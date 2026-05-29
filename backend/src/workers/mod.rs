//! Workers — the unified abstraction for everything that produces
//! external data for the assistant. Replaces the previous split between
//! Scout (autonomous web search) and Connectors (search-only adapters
//! for Gmail/Drive/...).
//!
//! A Worker may participate in either or both of two flows:
//!
//!   1. **Autonomous tick.** `Worker::tick()` is called on a cadence the
//!      worker chooses. Use this for background ingestion — e.g. the
//!      Gmail worker polls for new mail every minute, the WWW worker
//!      runs an interest-inferred web scan every N minutes. Workers
//!      without autonomous behavior return `None` from
//!      `tick_interval()` and the harness skips them.
//!
//!   2. **Core-initiated search.** `Worker::search()` is called when the
//!      assistant emits a `SEARCH: <worker> <query>` marker. The worker
//!      drives results through the Preprocessor and into memory ITSELF,
//!      emitting `SearchEvent` values on a channel as it makes
//!      progress. The Assistant subscribes to those events and uses
//!      them to update the user-visible status bar and to decide when
//!      it has enough to answer (or whether to abandon a stalled
//!      worker).
//!
//! ## Why workers, not connectors
//!
//! "Connector" suggested a synchronous request/response adapter. In
//! practice, two things wanted to live in the same architectural slot:
//! the polling-style Scout, and the search-style Gmail connector. Both
//! pull from the outside world; both produce items that need to flow
//! through the Preprocessor; both should be addressable by the
//! assistant via SEARCH markers. A Worker is just "a thing that
//! produces external data" — its interaction model (autonomous,
//! responsive, or both) is a property of the implementation, not the
//! abstraction.
//!
//! ## Security
//!
//! Every byte a Worker produces passes through the Preprocessor before
//! reaching memory (Invariant #3). Workers MUST use
//! `WorkerContext::ingest_one` (or equivalent) for this — never write
//! directly to `MemoryStore::add`. The Preprocessor decides tier,
//! redaction, and importance; the worker only chooses provenance hint.
//!
//! Workers are NEVER given write-capable handles to external services.
//! The `Worker::search` signature has no return path for "send" or
//! "delete" verbs — and at the API level, scopes are pinned at OAuth
//! time (Gmail uses `gmail.readonly`).

pub mod gdrive;
pub mod gmail;
pub mod oauth;
pub mod www;

use crate::embedder::Embedder;
use crate::memory::{ItemKind, MemoryStore};
use crate::preprocessor::{InputProvenance, Preprocessor};
use crate::vector_index::VectorIndex;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use shared::{Metadata, Tier};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

/// Lookup the OAuth scope a worker needs. Returns None for workers
/// that don't require OAuth (e.g. www). Used by `config_protocol`
/// when minting auth URLs and by the assistant when rendering
/// `CONFIG_BEGIN_OAUTH` requests.
pub fn scope_for(name: &str) -> Option<&'static str> {
    match name {
        "gmail" => Some(crate::workers::gmail::GMAIL_SCOPE),
        "gdrive" => Some(crate::workers::gdrive::DRIVE_SCOPE),
        _ => None,
    }
}

/// Statically-known worker kinds. Registered in the registry at
/// startup so the assistant prompt shows them as available-to-configure
/// even before they've been wired up. The how-to-set-up content lives
/// in the system manual (`READ_MANUAL: worker-setup-<name>`).
pub fn known_worker_kinds() -> Vec<KnownWorker> {
    vec![
        KnownWorker {
            name: "gmail",
            description: "Read-only Gmail search + autonomous polling for new mail. \
                          Supports `from:`, `subject:`, `before:`/`after:`, \
                          `has:attachment`, free text. For setup, read manual section: \
                          worker-setup-gmail.",
        },
        KnownWorker {
            name: "gdrive",
            description: "Read-only Google Drive full-text search. Downloads each \
                          matching file's text (Docs/Sheets/Slides exported, PDFs and \
                          text files extracted) through the Preprocessor into memory. \
                          Cannot modify Drive. For setup, read manual section: \
                          worker-setup-gdrive.",
        },
        KnownWorker {
            name: "www",
            description: "Open web search (WebSearch + WebFetch). Both autonomous \
                          (periodic interest-inferred scan) and on-demand via \
                          `SEARCH: www <query>`. No setup required.",
        },
    ]
}

/// One raw item produced by a worker, before any preprocessing. Used
/// internally by workers when shaping API responses into a uniform
/// shape; the public ingestion path is `WorkerContext::ingest_one`.
#[derive(Debug, Clone)]
pub struct RawWorkerResult {
    /// Provider-side identifier (e.g. Gmail message id).
    pub source_id: String,
    /// Optional direct link back to the item.
    pub source_url: Option<String>,
    /// The raw text to feed to the Preprocessor.
    pub content: String,
    /// When the item was produced at the source.
    pub at: Option<DateTime<Utc>>,
}

/// Events streamed back from a worker's `search()` call.
///
/// The Assistant pumps these as Status frames to the client (so the
/// status bar shows live progress) and uses them to decide when the
/// worker has finished, has stalled, or has produced enough to answer.
#[derive(Debug, Clone)]
pub enum SearchEvent {
    /// Worker accepted the query and is preparing to fetch.
    /// `expected_total` is best-effort — None if the worker can't
    /// estimate.
    Started {
        worker: String,
        expected_total: Option<usize>,
        detail: Option<String>,
    },
    /// Worker made progress without (yet) producing a finished item.
    /// Useful for slow phases like "fetching N message bodies".
    Progress {
        worker: String,
        completed: usize,
        total: Option<usize>,
        detail: Option<String>,
    },
    /// Preprocessor approved one result and it landed in memory.
    Ingested {
        worker: String,
        item_id: String,
        importance: f32,
    },
    /// Preprocessor dropped one result (Tier::Drop). A content-free
    /// stub was written; the raw bytes are gone.
    Dropped { worker: String, reason: String },
    /// Preprocessor or storage failed for one result. The raw bytes
    /// were not retained (Invariant #2).
    Failed { worker: String, error: String },
    /// Worker has finished processing all available results.
    Finished {
        worker: String,
        kept: usize,
        dropped: usize,
        failed: usize,
        duration_ms: u64,
    },
}

impl SearchEvent {
    /// The worker name this event belongs to. Used by the Assistant to
    /// route events into per-worker Status slots and to detect when a
    /// specific worker has stalled.
    pub fn worker(&self) -> &str {
        match self {
            SearchEvent::Started { worker, .. }
            | SearchEvent::Progress { worker, .. }
            | SearchEvent::Ingested { worker, .. }
            | SearchEvent::Dropped { worker, .. }
            | SearchEvent::Failed { worker, .. }
            | SearchEvent::Finished { worker, .. } => worker,
        }
    }
}

/// Shared infrastructure handed to every worker. Bundles the
/// Preprocessor, memory store, embedder, and vector index so workers can
/// drive results into memory themselves.
pub struct WorkerContext {
    pub preprocessor: Arc<Preprocessor>,
    pub memory: Arc<MemoryStore>,
    pub embedder: Arc<dyn Embedder>,
    pub vector_index: Arc<VectorIndex>,
    /// Max concurrent Preprocessor calls a worker may fan out within a
    /// single search or tick. Each call spawns a fresh `claude`
    /// subprocess (Invariant #2), so they're independent and safe to
    /// run in parallel.
    pub preprocess_concurrency: usize,
}

impl WorkerContext {
    /// Run one raw result through the standard ingestion pipeline:
    /// Preprocessor → memory store (or stub if dropped) → embed →
    /// vector-index upsert. Emits an `Ingested`, `Dropped`, or `Failed`
    /// SearchEvent on the channel.
    ///
    /// Returns the new item id if it was kept, None otherwise.
    ///
    /// `worker_name` is stamped into tags and into the event payload.
    /// `base_metadata` is whatever the caller wants the item's metadata
    /// to derive from (typically the user's request metadata for
    /// on-demand searches, or a synthesized "now" metadata for
    /// autonomous ticks).
    pub async fn ingest_one(
        &self,
        worker_name: &str,
        raw: &RawWorkerResult,
        base_metadata: Metadata,
        provenance: InputProvenance,
        tx: &UnboundedSender<SearchEvent>,
    ) -> Option<String> {
        let pp = match self.preprocessor.preprocess(&raw.content, provenance).await {
            Ok(p) => p,
            Err(e) => {
                let _ = tx.send(SearchEvent::Failed {
                    worker: worker_name.to_string(),
                    error: format!("preprocessor: {e}"),
                });
                return None;
            }
        };
        if pp.tier == Tier::Drop {
            let _ = self
                .memory
                .add_stub(&pp.output, pp.redaction_report.clone())
                .await;
            let _ = tx.send(SearchEvent::Dropped {
                worker: worker_name.to_string(),
                reason: "preprocessor dropped".into(),
            });
            return None;
        }
        // Standard worker tag set. Old items carried `connector:<name>`;
        // new items carry `worker:<name>` AND `connector:<name>` so
        // existing queries that look for the old tag still work.
        let tags = vec![
            "worker".into(),
            format!("worker:{worker_name}"),
            format!("connector:{worker_name}"),
            format!("source:{}", raw.source_id),
        ];
        let mut item_metadata = base_metadata;
        let mut extras = serde_json::Map::new();
        if let Some(url) = &raw.source_url {
            extras.insert("source_url".into(), serde_json::Value::String(url.clone()));
        }
        extras.insert(
            "source_id".into(),
            serde_json::Value::String(raw.source_id.clone()),
        );
        extras.insert(
            "worker".into(),
            serde_json::Value::String(worker_name.to_string()),
        );
        item_metadata.freeform = serde_json::Value::Object(extras);

        let added = self
            .memory
            .add_with_reason(
                &pp.output,
                ItemKind::WorkerFinding,
                pp.importance,
                pp.importance_reason.clone(),
                Some(item_metadata),
                pp.redaction_report.clone(),
                tags,
            )
            .await;
        match added {
            Ok(sc) => {
                // Embed + index inline so the very next retrieve() call
                // surfaces this item.
                if let Ok(v) = self.embedder.embed(&pp.output).await {
                    if let Some(item) = self.memory.get(&sc.id).ok().flatten() {
                        let _ = self.memory.write_vector(&item, &v).await;
                        let _ = self.vector_index.upsert(&sc.id, v);
                    }
                }
                let importance = pp.importance;
                let id = sc.id.clone();
                let _ = tx.send(SearchEvent::Ingested {
                    worker: worker_name.to_string(),
                    item_id: id.clone(),
                    importance,
                });
                Some(id)
            }
            Err(e) => {
                let _ = tx.send(SearchEvent::Failed {
                    worker: worker_name.to_string(),
                    error: format!("memory: {e}"),
                });
                None
            }
        }
    }
}

/// The Worker trait. Implementors live in `workers::<name>` modules.
#[async_trait]
pub trait Worker: Send + Sync {
    /// Short, stable identifier. Used in SEARCH markers, tags, status
    /// slots. e.g. "gmail", "www".
    fn name(&self) -> &'static str;

    /// One-paragraph description rendered into the assistant prompt so
    /// the LLM knows what's available and how to query it.
    fn description(&self) -> &'static str;

    /// True if the worker is set up and ready to use. False for
    /// connectors that need credentials before they can run.
    fn is_available(&self) -> bool;

    /// How often the harness should call `tick()`. None = no autonomous
    /// behavior; only callable via `search()`.
    fn tick_interval(&self) -> Option<Duration> {
        None
    }

    /// Autonomous tick — periodic background work. Default no-op.
    /// Implementations should be idempotent across reasonable cadences;
    /// the harness does not synchronize ticks with in-flight searches.
    async fn tick(&self, _ctx: Arc<WorkerContext>) -> Result<()> {
        Ok(())
    }

    /// Run an on-demand search. The worker is expected to drive results
    /// through the Preprocessor + memory store ITSELF (typically via
    /// `WorkerContext::ingest_one`) and to emit `SearchEvent` values on
    /// `tx` as work progresses.
    ///
    /// The caller awaits a `Finished` event (with a watchdog timeout
    /// for stall detection). The worker MUST emit `Started` first and
    /// `Finished` last so the caller can pair them.
    ///
    /// `metadata` is whatever metadata new items should carry — for an
    /// on-demand SEARCH from a user turn, this is the user's request
    /// metadata; for synthesized contexts it can be a "now" metadata.
    async fn search(
        &self,
        query: &str,
        limit: usize,
        ctx: Arc<WorkerContext>,
        metadata: Metadata,
        tx: UnboundedSender<SearchEvent>,
    ) -> Result<()>;
}

/// Registry of all installed workers, keyed by name. Mutable at runtime
/// so the config-protocol handler can install a freshly-authorized
/// worker (e.g. Gmail after OAuth) without restarting.
pub struct WorkerRegistry {
    by_name: RwLock<HashMap<String, Arc<dyn Worker>>>,
    known: RwLock<Vec<KnownWorker>>,
    ctx: Arc<WorkerContext>,
}

/// Static metadata for a worker kind that may not yet be configured.
/// Used to render the AVAILABLE WORKERS block when no instance exists.
#[derive(Debug, Clone)]
pub struct KnownWorker {
    pub name: &'static str,
    pub description: &'static str,
}

impl WorkerRegistry {
    pub fn new(ctx: Arc<WorkerContext>, workers: Vec<Arc<dyn Worker>>) -> Self {
        let mut by_name = HashMap::new();
        for w in workers {
            by_name.insert(w.name().to_string(), w);
        }
        Self {
            by_name: RwLock::new(by_name),
            known: RwLock::new(Vec::new()),
            ctx,
        }
    }

    pub fn empty(ctx: Arc<WorkerContext>) -> Self {
        Self {
            by_name: RwLock::new(HashMap::new()),
            known: RwLock::new(Vec::new()),
            ctx,
        }
    }

    pub fn ctx(&self) -> Arc<WorkerContext> {
        self.ctx.clone()
    }

    /// Record that a worker kind exists even if no instance is
    /// currently configured. The assistant uses this to know what
    /// setup flows it can offer.
    pub fn register_kind(&self, k: KnownWorker) {
        let mut kg = self.known.write().unwrap();
        if !kg.iter().any(|x| x.name == k.name) {
            kg.push(k);
        }
    }

    /// Install or replace a worker instance at runtime. Called by the
    /// config-protocol handler once a setup flow completes.
    pub fn register(&self, w: Arc<dyn Worker>) {
        self.by_name
            .write()
            .unwrap()
            .insert(w.name().to_string(), w);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Worker>> {
        self.by_name.read().unwrap().get(name).cloned()
    }

    pub fn all(&self) -> Vec<Arc<dyn Worker>> {
        let g = self.by_name.read().unwrap();
        let mut v: Vec<_> = g.values().cloned().collect();
        v.sort_by_key(|w| w.name());
        v
    }

    pub fn known_kinds(&self) -> Vec<KnownWorker> {
        self.known.read().unwrap().clone()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.read().unwrap().is_empty() && self.known.read().unwrap().is_empty()
    }

    /// Render the AVAILABLE WORKERS block for the assistant's prompt.
    pub fn render_prompt_block(&self) -> String {
        let instances = self.all();
        let kinds = self.known_kinds();
        if instances.is_empty() && kinds.is_empty() {
            return String::new();
        }
        let mut s = String::from(
            "AVAILABLE WORKERS:\n\
             Workers are subsystems that fetch external data. You can dispatch one\n\
             via a SEARCH marker, anywhere in your reply (one per line, may have\n\
             several):\n\
             \n  SEARCH: <worker_name> <query>\n\n\
             The worker streams results into memory (each one going through the\n\
             Preprocessor first), and you'll be re-prompted with the new items\n\
             available via retrieval. Some workers also run autonomously (e.g.\n\
             Gmail polls for new mail, the web worker periodically scans for\n\
             interest-relevant news) — those items will simply appear in memory\n\
             over time. You may emit up to 2 rounds of searches per turn.\n\
             \n\
             CONFIGURATION: If the user wants to set up a worker marked NOT\n\
             CONFIGURED below, walk them through it conversationally:\n\
             \n  CONFIG_REQUEST_FILE: <worker_name> <filename>\n\
                  → ask the user for a file (e.g. OAuth client_secret.json)\n\
               CONFIG_BEGIN_OAUTH: <worker_name>\n\
                  → begin the browser OAuth handshake\n\
             \n\
             Available workers:\n",
        );
        let mut listed_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        for w in &instances {
            let status = if w.is_available() {
                "✓ ACTIVE"
            } else {
                "✗ NOT CONFIGURED"
            };
            s.push_str(&format!(
                "  • {} [{}]\n    {}\n",
                w.name(),
                status,
                w.description()
            ));
            listed_names.insert(w.name().to_string());
        }
        for k in &kinds {
            if listed_names.contains(k.name) {
                continue;
            }
            s.push_str(&format!(
                "  • {} [✗ NOT CONFIGURED]\n    {}\n",
                k.name, k.description
            ));
        }
        s.push('\n');
        s
    }

    /// Spawn one tokio task per worker that declares a `tick_interval`,
    /// each looping forever calling `tick()` at the chosen cadence.
    /// Workers with `tick_interval() == None` are skipped.
    ///
    /// The harness does NOT enforce single-tick-at-a-time; if a worker
    /// wants that, it should hold its own Mutex or use a single
    /// in-flight guard. Workers should also be safe to drop mid-tick
    /// (Invariant #6 restart-safety).
    pub fn spawn_tick_drivers(self: &Arc<Self>) {
        for w in self.all() {
            let interval = match w.tick_interval() {
                Some(d) if !d.is_zero() => d,
                _ => continue,
            };
            let ctx = self.ctx.clone();
            let name = w.name();
            tracing::info!(
                worker = name,
                interval_secs = interval.as_secs(),
                "worker tick driver starting"
            );
            tokio::spawn(async move {
                // Stagger the first tick slightly so we don't all
                // hammer dependencies at startup.
                tokio::time::sleep(Duration::from_secs(5)).await;
                loop {
                    let started = std::time::Instant::now();
                    match w.tick(ctx.clone()).await {
                        Ok(()) => {
                            tracing::debug!(
                                worker = w.name(),
                                duration_ms = started.elapsed().as_millis() as u64,
                                "worker_tick_done"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                worker = w.name(),
                                error = %e,
                                duration_ms = started.elapsed().as_millis() as u64,
                                "worker_tick_failed"
                            );
                        }
                    }
                    tokio::time::sleep(interval).await;
                }
            });
        }
    }
}

/// Test-only worker that can either echo canned `SearchEvent`s (for
/// testing the Assistant's stream-consumption logic) OR push canned
/// `RawWorkerResult`s through the real `ctx.ingest_one()` pipeline
/// (for tests that need the assistant to see ingested items in memory).
#[cfg(test)]
pub mod mock {
    use super::*;
    use std::sync::Mutex;

    pub struct MockWorker {
        name: &'static str,
        /// Canned event streams — replayed verbatim, no ingestion.
        canned_events: Mutex<HashMap<String, Vec<SearchEvent>>>,
        /// Canned raw results — driven through `ctx.ingest_one()` so
        /// real memory items appear in the store.
        canned_results: Mutex<HashMap<String, Vec<RawWorkerResult>>>,
        calls: Mutex<Vec<(String, usize)>>,
        available: bool,
    }

    impl MockWorker {
        pub fn new(name: &'static str) -> Arc<Self> {
            Arc::new(Self {
                name,
                canned_events: Mutex::new(HashMap::new()),
                canned_results: Mutex::new(HashMap::new()),
                calls: Mutex::new(Vec::new()),
                available: true,
            })
        }

        /// Replay these events verbatim when `query` is searched.
        /// Useful for testing the Assistant's stream-consumption + stall
        /// handling without actually committing items to memory.
        pub fn respond_when(&self, query: &str, events: Vec<SearchEvent>) {
            self.canned_events
                .lock()
                .unwrap()
                .insert(query.to_string(), events);
        }

        /// Push these raw results through the real ingest pipeline when
        /// `query` is searched. After the call, items will be in the
        /// memory store, embedded, and indexed — exactly as a real
        /// worker would have produced them.
        pub fn respond_with_results(&self, query: &str, results: Vec<RawWorkerResult>) {
            self.canned_results
                .lock()
                .unwrap()
                .insert(query.to_string(), results);
        }

        pub fn calls(&self) -> Vec<(String, usize)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Worker for MockWorker {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> &'static str {
            "Mock worker for tests."
        }
        fn is_available(&self) -> bool {
            self.available
        }
        async fn search(
            &self,
            query: &str,
            limit: usize,
            ctx: Arc<WorkerContext>,
            metadata: Metadata,
            tx: UnboundedSender<SearchEvent>,
        ) -> Result<()> {
            self.calls.lock().unwrap().push((query.to_string(), limit));

            // If raw results were canned, run them through the real
            // ingest_one pipeline so the assistant sees them in memory.
            let results = self.canned_results.lock().unwrap().get(query).cloned();
            if let Some(results) = results {
                let total = results.len();
                let _ = tx.send(SearchEvent::Started {
                    worker: self.name.to_string(),
                    expected_total: Some(total),
                    detail: None,
                });
                let mut kept = 0;
                let mut dropped = 0;
                for r in results {
                    match ctx
                        .ingest_one(
                            self.name,
                            &r,
                            metadata.clone(),
                            InputProvenance::PublicWeb,
                            &tx,
                        )
                        .await
                    {
                        Some(_) => kept += 1,
                        None => dropped += 1,
                    }
                }
                let _ = tx.send(SearchEvent::Finished {
                    worker: self.name.to_string(),
                    kept,
                    dropped,
                    failed: 0,
                    duration_ms: 0,
                });
                return Ok(());
            }

            // Otherwise replay canned events (defaults to Started +
            // Finished with zero results).
            let events = self
                .canned_events
                .lock()
                .unwrap()
                .get(query)
                .cloned()
                .unwrap_or_else(|| {
                    vec![
                        SearchEvent::Started {
                            worker: self.name.to_string(),
                            expected_total: Some(0),
                            detail: None,
                        },
                        SearchEvent::Finished {
                            worker: self.name.to_string(),
                            kept: 0,
                            dropped: 0,
                            failed: 0,
                            duration_ms: 0,
                        },
                    ]
                });
            for e in events {
                let _ = tx.send(e);
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::MockLlmClient;
    use crate::embedder::MockEmbedder;
    use crate::memory::MemoryStore;
    use tempfile::TempDir;

    fn test_ctx() -> Arc<WorkerContext> {
        let tmp = TempDir::new().unwrap();
        let memory = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(MemoryStore::open(tmp.path().to_path_buf()))
            .unwrap();
        let memory = Arc::new(memory);
        let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder::new());
        let vector_index = Arc::new(
            VectorIndex::open(memory.root(), embedder.model_name(), embedder.dimension()).unwrap(),
        );
        let llm: Arc<dyn crate::claude::LlmClient> = MockLlmClient::new();
        let preprocessor = Arc::new(Preprocessor::new(llm));
        // Leak the tempdir for the duration of the process — these are
        // test-only helpers; we don't care about clean teardown.
        std::mem::forget(tmp);
        Arc::new(WorkerContext {
            preprocessor,
            memory,
            embedder,
            vector_index,
            preprocess_concurrency: 4,
        })
    }

    #[test]
    fn empty_registry_renders_empty_block() {
        let r = WorkerRegistry::empty(test_ctx());
        assert!(r.render_prompt_block().is_empty());
        assert!(r.is_empty());
    }

    #[test]
    fn registry_lists_workers_alphabetically() {
        let a: Arc<dyn Worker> = mock::MockWorker::new("alpha");
        let b: Arc<dyn Worker> = mock::MockWorker::new("beta");
        let r = WorkerRegistry::new(test_ctx(), vec![b, a]);
        let names: Vec<_> = r.all().iter().map(|w| w.name()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn registry_lookup_by_name() {
        let m: Arc<dyn Worker> = mock::MockWorker::new("foo");
        let r = WorkerRegistry::new(test_ctx(), vec![m]);
        assert!(r.get("foo").is_some());
        assert!(r.get("bar").is_none());
    }

    #[test]
    fn prompt_block_includes_each_worker() {
        let a: Arc<dyn Worker> = mock::MockWorker::new("alpha");
        let b: Arc<dyn Worker> = mock::MockWorker::new("beta");
        let r = WorkerRegistry::new(test_ctx(), vec![a, b]);
        let block = r.render_prompt_block();
        assert!(block.contains("SEARCH:"));
        assert!(block.contains("alpha"));
        assert!(block.contains("beta"));
        assert!(block.contains("Mock worker"));
    }
}
