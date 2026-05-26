//! The Scout. Periodically asks Claude (with WebSearch/WebFetch enabled) to
//! infer what the user cares about from memory + preferences, search the web
//! for noteworthy items, funnel the result through the Sanitizer (PublicWeb
//! provenance), and store. Falls back to base-rate human interests when
//! memory is thin.

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
        tracing::info!(?interval, pinned_topics = ?self.cfg.pinned_topics, "scout: running");
        loop {
            if let Err(e) = self.tick().await {
                tracing::warn!(error = %e, "scout tick failed");
            }
            tokio::time::sleep(interval).await;
        }
    }

    async fn tick(&self) -> anyhow::Result<()> {
        let now = Utc::now();
        let prefs = self.assistant.memory.preferences().await;

        // Build a digest of what we know about the user so Scout can infer
        // interests. Exclude SelfKnowledge (system-seeded) and assistant
        // notes (they'd just echo the assistant's own voice back).
        let all = self.assistant.memory.scan_all().unwrap_or_default();
        let user_items: Vec<_> = all
            .iter()
            .filter(|i| {
                !matches!(
                    i.sidecar.kind,
                    crate::memory::ItemKind::SelfKnowledge
                        | crate::memory::ItemKind::AssistantNote
                )
            })
            .collect();
        let user_item_count = user_items.len();

        let mut memory_digest = String::new();
        // Include the 30 most recent items (newest first), truncated.
        let recent_slice: Vec<_> = user_items.iter().rev().take(30).collect();
        for item in &recent_slice {
            let body = if item.body.len() > 300 {
                format!("{}…", &item.body[..300])
            } else {
                item.body.clone()
            };
            memory_digest.push_str(&format!(
                "- [{}] ({:?}) {}\n",
                item.sidecar.created_at.format("%Y-%m-%d"),
                item.sidecar.kind,
                body.replace('\n', " ")
            ));
        }
        if memory_digest.is_empty() {
            memory_digest.push_str("(memory is empty — you have nothing to go on yet)\n");
        }

        let mut prefs_block = String::new();
        if !prefs.statements.is_empty() {
            prefs_block.push_str("USER PREFERENCES (respect these — skip filtered topics):\n");
            for p in &prefs.statements {
                prefs_block.push_str(&format!("- {}\n", p.text));
            }
        }

        // Try to pull a location hint from the most recent item that has one.
        let location_hint = recent_slice
            .iter()
            .find_map(|i| {
                i.sidecar
                    .metadata
                    .as_ref()
                    .and_then(|m| m.geolocation.as_ref())
                    .map(|g| {
                        g.label
                            .clone()
                            .unwrap_or_else(|| format!("{:.2},{:.2}", g.lat, g.lon))
                    })
            })
            .unwrap_or_else(|| "unknown".to_string());

        let pinned_block = if self.cfg.pinned_topics.is_empty() {
            String::new()
        } else {
            format!(
                "PINNED TOPICS (user explicitly asked to always watch these):\n{}\n",
                self.cfg
                    .pinned_topics
                    .iter()
                    .map(|t| format!("- {t}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };

        let prompt = format!(
            r#"SCOUT_TASK

Right now: {now}
User location (best guess): {location_hint}

You are the Scout — a background worker for a personal AI assistant. Your job:
  1. Figure out what THIS user would find noteworthy right now.
  2. Search the web for those things.
  3. Return a short bulleted list — five bullets max, one line each. Skip filler.

How to decide what to look for:
  - If the memory digest below gives you a clear sense of the user (their interests,
    location, profession, family, recurring themes), search for items in those areas.
  - If memory is thin or empty, fall back to base-rate human interests for an adult
    in {location_hint}: major world/national news, severe-weather alerts for their
    region, big science/tech stories of broad interest. Mark these bullets with
    "[base rate — limited memory]" so the user knows you were guessing.
  - ALWAYS include genuinely time-sensitive items even if outside inferred interests
    (severe weather, natural disasters, major breaking news affecting their area).
  - SKIP anything the preferences list asks you to skip.
  - Prefer fresh items (last 24-72 hours). Skip evergreen content unless newly relevant.

User memory digest ({user_item_count} item(s)):
{memory_digest}
{prefs_block}{pinned_block}
Output: just the bullets, one per line. If genuinely nothing notable, say
"Nothing notable today."
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
