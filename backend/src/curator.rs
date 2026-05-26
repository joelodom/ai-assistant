//! The Curator. Periodically walks the memory store, advances items through
//! decay stages (Fresh → Aging → Summarized → Stale), and uses Claude to
//! collapse aging items into short, durable summaries.
//!
//! Decay is by importance *and* age — low-importance items collapse sooner.

use crate::claude::{LlmClient, LlmOptions};
use crate::config::CuratorCfg;
use crate::memory::{DecayStage, MemoryStore};
use chrono::{Duration as ChronoDuration, Utc};
use std::sync::Arc;
use std::time::Duration;

pub struct Curator {
    pub llm: Arc<dyn LlmClient>,
    pub memory: Arc<MemoryStore>,
    pub cfg: CuratorCfg,
    pub model: Option<String>,
}

impl Curator {
    pub fn spawn(self) {
        if !self.cfg.enabled {
            tracing::info!("curator disabled");
            return;
        }
        tokio::spawn(async move { self.run().await });
    }

    async fn run(self) {
        let interval = Duration::from_secs(self.cfg.interval_minutes.saturating_mul(60).max(60));
        tracing::info!(?interval, "curator: running");
        loop {
            if let Err(e) = self.tick().await {
                tracing::warn!(error = %e, "curator tick failed");
            }
            tokio::time::sleep(interval).await;
        }
    }

    pub async fn tick(&self) -> anyhow::Result<()> {
        let items = self.memory.scan_all()?;
        let now = Utc::now();
        let fresh_cutoff = now - ChronoDuration::hours(self.cfg.fresh_age_hours as i64);
        let aging_cutoff = now - ChronoDuration::days(self.cfg.aging_age_days as i64);
        let stale_cutoff = now - ChronoDuration::days(self.cfg.stale_age_days as i64);

        let mut promoted = 0usize;
        let mut summarized = 0usize;
        let mut staled = 0usize;

        for item in items {
            let age = item.sidecar.created_at;
            // Stage advancement.
            let next_stage = if age < stale_cutoff && item.sidecar.importance < 0.7 {
                Some(DecayStage::Stale)
            } else if age < aging_cutoff
                && item.sidecar.importance < 0.7
                && item.sidecar.decay_stage == DecayStage::Aging
            {
                Some(DecayStage::Summarized)
            } else if age < fresh_cutoff && item.sidecar.decay_stage == DecayStage::Fresh {
                Some(DecayStage::Aging)
            } else {
                None
            };

            let Some(target_stage) = next_stage else { continue };

            match target_stage {
                DecayStage::Aging => {
                    self.memory
                        .update_item(&item, None, |s| s.decay_stage = DecayStage::Aging)
                        .await?;
                    promoted += 1;
                }
                DecayStage::Summarized => {
                    let prompt = build_summary_prompt(&item.body);
                    let opts = LlmOptions {
                        model: self.model.clone(),
                        ..Default::default()
                    };
                    let summary = self
                        .llm
                        .oneshot(&prompt, opts)
                        .await
                        .unwrap_or_else(|_| {
                            // Fall back to a deterministic truncation if the
                            // LLM call fails — never lose the memory.
                            truncate_summary(&item.body)
                        });
                    let summary = summary.trim().to_string();
                    self.memory
                        .update_item(&item, Some(&summary), |s| {
                            s.decay_stage = DecayStage::Summarized
                        })
                        .await?;
                    summarized += 1;
                }
                DecayStage::Stale => {
                    let stub = format!(
                        "[stale, {}] {}",
                        item.sidecar.created_at.format("%Y-%m-%d"),
                        first_line(&item.body)
                    );
                    self.memory
                        .update_item(&item, Some(&stub), |s| s.decay_stage = DecayStage::Stale)
                        .await?;
                    staled += 1;
                }
                DecayStage::Fresh => {}
            }
        }

        tracing::info!(promoted, summarized, staled, "curator tick");
        Ok(())
    }
}

fn build_summary_prompt(body: &str) -> String {
    format!(
        r#"CURATOR_TASK

Collapse the following memory item into a single short paragraph (≤ 3 sentences).
Keep durable, useful nuggets: names, dates, what kind of event it was, key facts.
Drop verbose body text, marketing language, and anything not worth carrying forward.
Respond with ONLY the summary text — no preamble.

ITEM:
{body}
"#
    )
}

fn truncate_summary(body: &str) -> String {
    let one_line = first_line(body);
    if one_line.len() > 200 {
        format!("{}…", &one_line[..200])
    } else {
        one_line.to_string()
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::MockLlmClient;
    use crate::memory::{ItemKind, MemoryStore};
    use tempfile::TempDir;

    #[tokio::test]
    async fn curator_promotes_fresh_to_aging() {
        let td = TempDir::new().unwrap();
        let memory = Arc::new(MemoryStore::open(td.path().to_path_buf()).await.unwrap());
        memory
            .add(
                "old item",
                ItemKind::Ingestion,
                0.3,
                None,
                String::new(),
                vec![],
            )
            .await
            .unwrap();

        // Backdate the item by hand.
        let item = memory.recent(1).unwrap().pop().unwrap();
        memory
            .update_item(&item, None, |s| {
                s.created_at = Utc::now() - ChronoDuration::hours(72);
            })
            .await
            .unwrap();

        let curator = Curator {
            llm: MockLlmClient::new(),
            memory: memory.clone(),
            cfg: CuratorCfg {
                enabled: true,
                interval_minutes: 60,
                fresh_age_hours: 48,
                aging_age_days: 14,
                stale_age_days: 90,
            },
            model: None,
        };
        curator.tick().await.unwrap();

        let item = memory.recent(1).unwrap().pop().unwrap();
        assert_eq!(item.sidecar.decay_stage, DecayStage::Aging);
    }

    #[tokio::test]
    async fn curator_summarizes_aging_items() {
        let td = TempDir::new().unwrap();
        let memory = Arc::new(MemoryStore::open(td.path().to_path_buf()).await.unwrap());
        memory
            .add(
                "a long boring marketing email full of text about a privacy policy update",
                ItemKind::Ingestion,
                0.2,
                None,
                String::new(),
                vec![],
            )
            .await
            .unwrap();

        let item = memory.recent(1).unwrap().pop().unwrap();
        // Backdate it past the aging cutoff AND set it to Aging already.
        memory
            .update_item(&item, None, |s| {
                s.created_at = Utc::now() - ChronoDuration::days(30);
                s.decay_stage = DecayStage::Aging;
            })
            .await
            .unwrap();

        let llm = MockLlmClient::new();
        llm.respond_when("CURATOR_TASK", "Privacy policy email — noted.");
        let curator = Curator {
            llm,
            memory: memory.clone(),
            cfg: CuratorCfg {
                enabled: true,
                interval_minutes: 60,
                fresh_age_hours: 48,
                aging_age_days: 14,
                stale_age_days: 90,
            },
            model: None,
        };
        curator.tick().await.unwrap();

        let item = memory.recent(1).unwrap().pop().unwrap();
        assert_eq!(item.sidecar.decay_stage, DecayStage::Summarized);
        assert!(item.body.contains("Privacy policy"));
    }
}
