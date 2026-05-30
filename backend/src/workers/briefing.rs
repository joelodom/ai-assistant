//! Briefing worker. A background *producer*: every `interval_minutes` it reads
//! the user's memory, asks the LLM (with NO tools) to synthesize what's
//! important / time-sensitive / actionable right now, and stores the result as
//! a single low-importance `Briefing` memory item tagged `auto-briefing`.
//!
//! Security note — why this worker does NOT use the Preprocessor: every other
//! worker fetches EXTERNAL data and must drive it through the Preprocessor
//! before it touches memory (the diode). This worker fetches nothing external.
//! Its LLM call is given an empty tool list, and its only input is a digest of
//! memory that was ALREADY sanitized when first ingested. The briefing is
//! therefore a pure function of sanitized data — exactly like an
//! `AssistantNote` — and is stored directly. No new raw-input path is created,
//! so the diode is preserved. (If this worker ever gains tools or an external
//! fetch, that output MUST go back through the Preprocessor.)
//!
//! The briefing is the producer behind the startup "what's new" greeting:
//! `Assistant::introduction` reads the latest briefing (if fresh) and has a
//! cheap model summarize it. Briefings are excluded from the assistant's
//! contextual retrieval (they're meta-summaries of memory, not facts).

use crate::claude::{LlmClient, LlmOptions};
use crate::config::BriefingCfg;
use crate::memory::{ItemKind, Sidecar};
use crate::workers::{SearchEvent, Worker, WorkerContext};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use shared::Metadata;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

/// Tag applied to every briefing item so the assistant — and the startup
/// summarizer — can recognize an auto-generated briefing.
pub const BRIEFING_TAG: &str = "auto-briefing";

/// Importance for briefing items — deliberately low. They're excluded from
/// contextual retrieval anyway; this keeps them out of importance-ranked
/// views too.
const BRIEFING_IMPORTANCE: f32 = 0.1;

/// How many recent, user-relevant memory items to feed the synthesis prompt.
const DIGEST_ITEMS: usize = 40;

pub struct BriefingWorker {
    llm: Arc<dyn LlmClient>,
    cfg: BriefingCfg,
    model: Option<String>,
}

impl BriefingWorker {
    pub fn new(llm: Arc<dyn LlmClient>, cfg: BriefingCfg, model: Option<String>) -> Self {
        Self { llm, cfg, model }
    }

    /// Build the synthesis prompt from a digest of recent, user-relevant
    /// memory. Excludes system/meta kinds — including prior briefings, to
    /// avoid a feedback loop.
    async fn build_prompt(&self, memory: &crate::memory::MemoryStore) -> String {
        let now = Utc::now();
        let prefs = memory.preferences().await;
        let all = memory.scan_all().unwrap_or_default();
        let user_items: Vec<_> = all
            .iter()
            .filter(|i| {
                !matches!(
                    i.sidecar.kind,
                    ItemKind::SelfKnowledge
                        | ItemKind::AssistantNote
                        | ItemKind::Briefing
                        | ItemKind::ForgottenStub
                        | ItemKind::PreprocessorStub
                        | ItemKind::PreprocessorError
                        | ItemKind::SanitizerStub
                        | ItemKind::SanitizerError
                        | ItemKind::AssistantError
                )
            })
            .collect();
        let user_item_count = user_items.len();

        let mut memory_digest = String::new();
        let recent_slice: Vec<_> = user_items.iter().rev().take(DIGEST_ITEMS).collect();
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
            memory_digest.push_str("(memory is empty — nothing to brief on yet)\n");
        }

        let mut prefs_block = String::new();
        if !prefs.statements.is_empty() {
            prefs_block.push_str("USER PREFERENCES (respect these):\n");
            for p in &prefs.statements {
                prefs_block.push_str(&format!("- {}\n", p.text));
            }
        }

        format!(
            r#"BRIEFING_WORKER_TICK

Right now: {now}

You are the briefing worker — a background subsystem for a personal AI
assistant. You have NO tools and NO web access. Work ONLY from the memory
digest below; do not invent facts.

Your job: produce a short "what should I be thinking about" briefing for the
user, as if greeting them when they sit back down. Surface what is:
  - time-sensitive (deadlines, appointments, anything dated soon),
  - newly added or recently changed,
  - high-stakes or high-importance (commitments, people, obligations),
  - an open loop the user may want to follow up on.

Rules:
  - 3-6 bullets max, one line each. Lead with the most pressing.
  - Be concrete and specific; reference the actual items.
  - Respect the preferences below.
  - If there is genuinely nothing worth surfacing, say exactly:
    "Nothing pressing right now."
  - Do not pad. No preamble, no sign-off — just the bullets.

User memory digest ({user_item_count} item(s), most recent last):
{memory_digest}
{prefs_block}
Output: just the briefing bullets."#,
        )
    }

    /// Synthesize a briefing and store it as a low-importance `Briefing`
    /// memory item (embedded for uniformity, but excluded from retrieval).
    /// Returns the stored item's sidecar.
    async fn build_and_store(&self, ctx: &Arc<WorkerContext>) -> Result<Sidecar> {
        let prompt = self.build_prompt(&ctx.memory).await;
        // NO tools — guarantees the output is derived only from the sanitized
        // memory digest, never from any external fetch.
        let opts = LlmOptions {
            allowed_tools: vec![],
            model: self.model.clone(),
            ..Default::default()
        };
        let text = self.llm.oneshot(&prompt, opts).await?.trim().to_string();
        if text.is_empty() {
            anyhow::bail!("briefing worker: empty LLM reply");
        }

        let built_at = Utc::now();
        let body = format!(
            "(auto-generated briefing built {}; synthesized from memory, not user-stated)\n{}",
            built_at.to_rfc3339(),
            text
        );
        let metadata = Metadata {
            datetime_iso: built_at.to_rfc3339(),
            geolocation: None,
            freeform: serde_json::json!({ "worker": self.name(), "via": "tick" }),
        };

        // Store directly — NOT through the Preprocessor (see module docs).
        let sc = ctx
            .memory
            .add(
                &body,
                ItemKind::Briefing,
                BRIEFING_IMPORTANCE,
                Some(metadata),
                String::new(),
                vec![BRIEFING_TAG.to_string()],
            )
            .await?;

        // Embed for uniformity with the rest of the store. Retrieval excludes
        // Briefing items, so this never pollutes the assistant's recall.
        if let Ok(vec) = ctx.embedder.embed(&body).await {
            if let Ok(Some(item)) = ctx.memory.get(&sc.id) {
                let _ = ctx.memory.write_vector(&item, &vec).await;
                let _ = ctx.vector_index.upsert(&sc.id, vec);
            }
        }

        tracing::info!(
            item_id = %sc.id,
            briefing_len = text.chars().count(),
            "briefing_built"
        );
        Ok(sc)
    }
}

#[async_trait]
impl Worker for BriefingWorker {
    fn name(&self) -> &'static str {
        "briefing"
    }

    fn description(&self) -> &'static str {
        "Auto-briefing. Every few minutes it reviews your memory and writes a \
         short 'what's important right now' briefing — the one that greets you \
         on startup. SEARCH it to force a fresh briefing on demand."
    }

    fn is_available(&self) -> bool {
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
        self.build_and_store(&ctx).await?;
        Ok(())
    }

    async fn search(
        &self,
        _query: &str,
        _limit: usize,
        ctx: Arc<WorkerContext>,
        _metadata: Metadata,
        tx: UnboundedSender<SearchEvent>,
    ) -> Result<()> {
        let started = std::time::Instant::now();
        let _ = tx.send(SearchEvent::Started {
            worker: self.name().to_string(),
            expected_total: Some(1),
            detail: Some("briefing: synthesizing from memory…".into()),
        });

        let result = self.build_and_store(&ctx).await;
        let duration_ms = started.elapsed().as_millis() as u64;
        let (kept, failed) = match &result {
            Ok(sc) => {
                let _ = tx.send(SearchEvent::Ingested {
                    worker: self.name().to_string(),
                    item_id: sc.id.clone(),
                    importance: sc.importance,
                });
                (1, 0)
            }
            Err(e) => {
                let _ = tx.send(SearchEvent::Failed {
                    worker: self.name().to_string(),
                    error: e.to_string(),
                });
                (0, 1)
            }
        };
        let _ = tx.send(SearchEvent::Finished {
            worker: self.name().to_string(),
            kept,
            dropped: 0,
            failed,
            duration_ms,
        });
        result.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::MockLlmClient;
    use crate::embedder::{Embedder, MockEmbedder};
    use crate::memory::MemoryStore;
    use crate::preprocessor::Preprocessor;
    use crate::vector_index::VectorIndex;
    use tempfile::TempDir;

    async fn ctx_with(llm: Arc<MockLlmClient>) -> (TempDir, Arc<WorkerContext>) {
        let td = TempDir::new().unwrap();
        let memory = Arc::new(MemoryStore::open(td.path().to_path_buf()).await.unwrap());
        let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder::new());
        let vector_index = Arc::new(
            VectorIndex::open(memory.root(), embedder.model_name(), embedder.dimension()).unwrap(),
        );
        let preprocessor = Arc::new(Preprocessor::new(llm));
        let ctx = Arc::new(WorkerContext {
            preprocessor,
            memory,
            embedder,
            vector_index,
            preprocess_concurrency: 4,
        });
        (td, ctx)
    }

    fn cfg(enabled: bool) -> BriefingCfg {
        BriefingCfg {
            enabled,
            interval_minutes: 10,
            staleness_minutes: 30,
        }
    }

    #[tokio::test]
    async fn tick_stores_a_low_importance_marked_briefing() {
        let llm = MockLlmClient::new();
        llm.respond_when(
            "BRIEFING_WORKER_TICK",
            "- Roof inspector follow-up is still open\n- Recital is Saturday at 4pm",
        );
        let (_td, ctx) = ctx_with(llm.clone()).await;
        ctx.memory
            .add(
                "Roof inspector coming Tuesday",
                ItemKind::Ingestion,
                0.6,
                None,
                String::new(),
                vec![],
            )
            .await
            .unwrap();

        let worker = BriefingWorker::new(llm, cfg(true), None);
        worker.tick(ctx.clone()).await.unwrap();

        let all = ctx.memory.scan_all().unwrap();
        let briefing = all
            .iter()
            .find(|i| i.sidecar.kind == ItemKind::Briefing)
            .expect("a briefing item should have been stored");
        assert!(
            briefing.sidecar.importance < 0.2,
            "briefings are low importance"
        );
        assert!(briefing.sidecar.tags.iter().any(|t| t == BRIEFING_TAG));
        assert!(briefing.body.contains("auto-generated briefing"));
        assert!(briefing.body.contains("Roof inspector follow-up"));
    }

    #[test]
    fn tick_interval_none_when_disabled() {
        let llm = MockLlmClient::new();
        let worker = BriefingWorker::new(llm, cfg(false), None);
        assert!(worker.tick_interval().is_none());
    }

    #[tokio::test]
    async fn tick_interval_some_when_enabled() {
        let llm = MockLlmClient::new();
        let worker = BriefingWorker::new(llm, cfg(true), None);
        assert!(worker.tick_interval().is_some());
    }
}
