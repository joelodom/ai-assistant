//! The Scout. Periodically asks Claude (with WebSearch/WebFetch enabled) to
//! summarize what's notable on the user's configured topics, then funnels the
//! result through the Sanitizer (PublicWeb provenance) and stores it.

use crate::assistant::Assistant;
use crate::claude::{LlmClient, LlmOptions};
use crate::config::ScoutCfg;
use crate::memory::ItemKind;
use crate::sanitizer::{InputProvenance, Sanitizer};
use chrono::Utc;
use std::sync::Arc;
use std::time::Duration;

pub struct Scout {
    pub llm: Arc<dyn LlmClient>,
    pub sanitizer: Arc<Sanitizer>,
    pub assistant: Arc<Assistant>,
    pub cfg: ScoutCfg,
    pub allowed_tools: Vec<String>,
    pub model: Option<String>,
}

impl Scout {
    pub fn spawn(self) {
        if !self.cfg.enabled {
            tracing::info!("scout disabled");
            return;
        }
        tokio::spawn(async move { self.run().await });
    }

    async fn run(self) {
        let interval = Duration::from_secs(self.cfg.interval_minutes.saturating_mul(60).max(60));
        tracing::info!(?interval, topics = ?self.cfg.topics, "scout: running");
        loop {
            if let Err(e) = self.tick().await {
                tracing::warn!(error = %e, "scout tick failed");
            }
            tokio::time::sleep(interval).await;
        }
    }

    async fn tick(&self) -> anyhow::Result<()> {
        let now = Utc::now();
        let topics = self.cfg.topics.join(", ");
        let prefs = self.assistant.memory.preferences().await;
        let prefs_block = if prefs.statements.is_empty() {
            String::new()
        } else {
            let mut s = String::from("\nUSER PREFERENCES (respect these — skip filtered topics):\n");
            for p in &prefs.statements {
                s.push_str(&format!("- {}\n", p.text));
            }
            s
        };

        let prompt = format!(
            r#"SCOUT_TASK

Right now: {now}

You are the Scout. Briefly browse the web for noteworthy items in the user's
topics below. Return a short bulleted list — five bullets max, one line each.
Skip filler. If nothing notable today, say so.{prefs_block}

TOPICS: {topics}
"#,
        );
        let opts = LlmOptions {
            allowed_tools: self.allowed_tools.clone(),
            model: self.model.clone(),
            ..Default::default()
        };
        let body = self.llm.oneshot(&prompt, opts).await?;

        let san = self
            .sanitizer
            .sanitize(&body, InputProvenance::PublicWeb)
            .await?;

        // Drop or store — same rules.
        match san.tier {
            shared::Tier::Drop => {
                self.assistant
                    .memory
                    .add_stub(&san.output, san.redaction_report)
                    .await?;
            }
            shared::Tier::Pass | shared::Tier::Redact => {
                self.assistant
                    .memory
                    .add(
                        &san.output,
                        ItemKind::ScoutFinding,
                        0.4,
                        None,
                        san.redaction_report,
                        vec!["scout".into()],
                    )
                    .await?;
            }
        }
        Ok(())
    }
}
