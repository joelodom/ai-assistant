//! WWW worker. The open web — `WebSearch` + `WebFetch` via Claude's
//! tool-use, both autonomously (interest-inferred tick) and on-demand
//! (assistant emits `SEARCH: www <query>`).
//!
//! Replaces the old `Scout`. Same autonomous behavior; new on-demand
//! path means the assistant can also dispatch a fresh-news lookup mid-
//! conversation.
//!
//! Every result flows through the Preprocessor with PublicWeb
//! provenance — content from the open internet gets the stricter
//! sanitization pass, not the Personal one.

use crate::claude::{LlmClient, LlmOptions};
use crate::config::ScoutCfg;
use crate::memory::ItemKind;
use crate::preprocessor::{InputProvenance, Preprocessor};
use crate::workers::{RawWorkerResult, SearchEvent, Worker, WorkerContext};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use shared::{Metadata, Tier};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

pub struct WwwWorker {
    llm: Arc<dyn LlmClient>,
    sanitizer: Arc<Preprocessor>,
    cfg: ScoutCfg,
    allowed_tools: Vec<String>,
    model: Option<String>,
}

impl WwwWorker {
    pub fn new(
        llm: Arc<dyn LlmClient>,
        sanitizer: Arc<Preprocessor>,
        cfg: ScoutCfg,
        allowed_tools: Vec<String>,
        model: Option<String>,
    ) -> Self {
        Self {
            llm,
            sanitizer,
            cfg,
            allowed_tools,
            model,
        }
    }

    /// Build the autonomous-tick prompt. Pulls interests + preferences
    /// from memory and asks the LLM (with WebSearch/WebFetch) for a
    /// short bulleted summary.
    async fn build_tick_prompt(&self, memory: &crate::memory::MemoryStore) -> String {
        let now = Utc::now();
        let prefs = memory.preferences().await;
        let all = memory.scan_all().unwrap_or_default();
        let user_items: Vec<_> = all
            .iter()
            .filter(|i| {
                !matches!(
                    i.sidecar.kind,
                    ItemKind::SelfKnowledge | ItemKind::AssistantNote
                )
            })
            .collect();
        let user_item_count = user_items.len();

        let mut memory_digest = String::new();
        let recent_slice: Vec<_> = user_items.iter().rev().take(30).collect();
        for item in &recent_slice {
            let body = if item.body.chars().count() > 300 {
                crate::assistant::truncate_chars(&item.body, 300)
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

        format!(
            r#"WWW_WORKER_TICK

Right now: {now}
User location (best guess): {location_hint}

You are the WWW worker — a background subsystem for a personal AI assistant.
Your job:
  1. Figure out what THIS user would find noteworthy right now.
  2. Search the web for those things.
  3. Return a short bulleted list — five bullets max, one line each. Skip filler.

How to decide what to look for:
  - If the memory digest gives you a clear sense of the user, search their interests.
  - If memory is thin or empty, fall back to base-rate human interests for an adult
    in {location_hint}: major world/national news, severe-weather alerts, big
    science/tech stories. Mark these bullets with "[base rate — limited memory]".
  - ALWAYS include genuinely time-sensitive items even if outside inferred interests.
  - SKIP anything the preferences list asks you to skip.
  - Prefer fresh items (last 24-72 hours).

User memory digest ({user_item_count} item(s)):
{memory_digest}
{prefs_block}{pinned_block}
Output: just the bullets, one per line. If genuinely nothing notable, say
"Nothing notable today."
"#,
        )
    }

    /// Build the on-demand search prompt — focused on the user's
    /// question, not on interest inference.
    fn build_search_prompt(&self, query: &str) -> String {
        let now = Utc::now();
        format!(
            r#"WWW_WORKER_SEARCH

Right now: {now}

You are the WWW worker. The assistant has dispatched you to answer this query
by searching the open web. Use WebSearch / WebFetch as needed. Return:

  - 3-7 short bullet points covering what you found.
  - Each bullet should cite its source URL inline in parentheses.
  - If you can't find anything relevant, say so plainly.
  - No filler, no padding, no "I will now…" preambles.

Query: {query}
"#,
        )
    }

    /// Push one LLM-produced text chunk through the Preprocessor and
    /// then into memory. Used by both tick() and search().
    async fn ingest_body(
        &self,
        body: &str,
        ctx: Arc<WorkerContext>,
        metadata: Metadata,
        tx: Option<&UnboundedSender<SearchEvent>>,
    ) -> Result<()> {
        let san = self
            .sanitizer
            .preprocess(body, InputProvenance::PublicWeb)
            .await?;

        match san.tier {
            Tier::Drop => {
                ctx.memory
                    .add_stub(&san.output, san.redaction_report.clone())
                    .await?;
                if let Some(tx) = tx {
                    let _ = tx.send(SearchEvent::Dropped {
                        worker: self.name().to_string(),
                        reason: "preprocessor dropped".into(),
                    });
                }
            }
            Tier::Pass | Tier::Redact => {
                // The WWW worker produces one synthesized "bullets"
                // blob per call — there's no per-item source_id from
                // Claude's tool use. Reuse the standard ingest helper
                // by wrapping the body in a RawWorkerResult with a
                // synthesized id so the tagging is consistent.
                let raw = RawWorkerResult {
                    source_id: format!("www:{}", chrono::Utc::now().timestamp_millis()),
                    source_url: None,
                    content: san.output.clone(),
                    at: Some(chrono::Utc::now()),
                };
                if let Some(tx) = tx {
                    // Use the standard ingest path so the event is
                    // emitted consistently. ingest_one re-runs the
                    // preprocessor — slight waste, but it keeps the
                    // ingestion pipeline single-pathed. We could add
                    // a "preprocessed already" variant later.
                    let _ = ctx
                        .ingest_one(
                            self.name(),
                            &raw,
                            metadata,
                            InputProvenance::PublicWeb,
                            tx,
                        )
                        .await;
                } else {
                    // Tick path: no channel. Write directly using the
                    // already-sanitized body to avoid double-pp.
                    let tags = vec![
                        "worker".into(),
                        format!("worker:{}", self.name()),
                        "connector:www".into(),
                        format!("source:{}", raw.source_id),
                    ];
                    let mut item_metadata = metadata;
                    let mut extras = serde_json::Map::new();
                    extras.insert(
                        "source_id".into(),
                        serde_json::Value::String(raw.source_id.clone()),
                    );
                    extras.insert(
                        "worker".into(),
                        serde_json::Value::String(self.name().to_string()),
                    );
                    extras.insert("via".into(), serde_json::Value::String("tick".into()));
                    item_metadata.freeform = serde_json::Value::Object(extras);
                    let _ = ctx
                        .memory
                        .add_with_reason(
                            &san.output,
                            ItemKind::WorkerFinding,
                            san.importance,
                            san.importance_reason.clone(),
                            Some(item_metadata),
                            san.redaction_report.clone(),
                            tags,
                        )
                        .await;
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Worker for WwwWorker {
    fn name(&self) -> &'static str {
        "www"
    }

    fn description(&self) -> &'static str {
        "Open-web search. WebSearch + WebFetch via Claude tool-use. \
         Both autonomous (periodic interest-inferred scan when enabled) \
         and on-demand: use `SEARCH: www <query>` for time-sensitive or \
         fresh-news questions whose answer is not in memory yet. Returns \
         a short bulleted summary with inline source URLs."
    }

    fn is_available(&self) -> bool {
        // Always available — WebSearch/WebFetch require no per-user
        // setup. The autonomous tick is gated on cfg.enabled.
        true
    }

    fn tick_interval(&self) -> Option<Duration> {
        if !self.cfg.enabled {
            return None;
        }
        Some(Duration::from_secs(
            self.cfg.interval_minutes.saturating_mul(60).max(60),
        ))
    }

    async fn tick(&self, ctx: Arc<WorkerContext>) -> Result<()> {
        let prompt = self.build_tick_prompt(&ctx.memory).await;
        let opts = LlmOptions {
            allowed_tools: self.allowed_tools.clone(),
            model: self.model.clone(),
            ..Default::default()
        };
        let body = self.llm.oneshot(&prompt, opts).await?;
        let metadata = Metadata {
            datetime_iso: Utc::now().to_rfc3339(),
            geolocation: None,
            freeform: serde_json::json!({
                "worker": self.name(),
                "via": "tick",
            }),
        };
        self.ingest_body(&body, ctx, metadata, None).await?;
        Ok(())
    }

    async fn search(
        &self,
        query: &str,
        _limit: usize,
        ctx: Arc<WorkerContext>,
        metadata: Metadata,
        tx: UnboundedSender<SearchEvent>,
    ) -> Result<()> {
        let started = std::time::Instant::now();
        let _ = tx.send(SearchEvent::Started {
            worker: self.name().to_string(),
            expected_total: Some(1),
            detail: Some("www: searching the web…".into()),
        });
        let prompt = self.build_search_prompt(query);
        let opts = LlmOptions {
            allowed_tools: self.allowed_tools.clone(),
            model: self.model.clone(),
            ..Default::default()
        };
        let body = match self.llm.oneshot(&prompt, opts).await {
            Ok(b) => b,
            Err(e) => {
                let _ = tx.send(SearchEvent::Failed {
                    worker: self.name().to_string(),
                    error: format!("www llm: {e}"),
                });
                let _ = tx.send(SearchEvent::Finished {
                    worker: self.name().to_string(),
                    kept: 0,
                    dropped: 0,
                    failed: 1,
                    duration_ms: started.elapsed().as_millis() as u64,
                });
                return Err(e);
            }
        };
        self.ingest_body(&body, ctx, metadata, Some(&tx)).await?;
        let _ = tx.send(SearchEvent::Finished {
            worker: self.name().to_string(),
            kept: 1,
            dropped: 0,
            failed: 0,
            duration_ms: started.elapsed().as_millis() as u64,
        });
        tracing::info!(
            duration_ms = started.elapsed().as_millis() as u64,
            "www_search_done"
        );
        Ok(())
    }
}
