//! Backend library. The binary in `main.rs` is a thin wrapper around `run()`.
//! Integration tests in `tests/` exercise the modules directly.

pub mod assistant;
pub mod attachments;
pub mod claude;
pub mod config;
pub mod config_protocol;
pub mod embedder;
pub mod indexer;
pub mod manual;
pub mod memory;
pub mod preprocessor;
pub mod retrieval;
pub mod self_knowledge;
pub mod vector_index;
pub mod workers;
pub mod ws;

/// Back-compat module alias — old code (and integration tests) imported
/// `backend::sanitizer::Sanitizer`. The type is the same; just routed
/// through the new module.
pub mod sanitizer {
    pub use crate::preprocessor::{
        InputProvenance, Preprocessor as Sanitizer, PreprocessorResult as SanitizerResult,
    };
}

use std::sync::Arc;

/// Construct the full app graph from a config. Returns the axum router and
/// the long-lived components so callers can keep their handles for tests.
pub struct Built {
    pub state: ws::AppState,
    pub memory: Arc<memory::MemoryStore>,
    pub llm: Arc<dyn claude::LlmClient>,
    pub embedder: Arc<dyn embedder::Embedder>,
    pub vector_index: Arc<vector_index::VectorIndex>,
    pub workers: Arc<workers::WorkerRegistry>,
    pub cfg: config::Config,
}

pub async fn build_app(cfg: config::Config) -> anyhow::Result<Built> {
    let memory = Arc::new(memory::MemoryStore::open(cfg.memory.dir.clone()).await?);
    let llm = claude::make_client_from_env(&cfg.claude);
    let embedder = embedder::make_embedder_from_env();

    let vector_index = Arc::new(vector_index::VectorIndex::open(
        memory.root(),
        embedder.model_name(),
        embedder.dimension(),
    )?);
    for (item, vec) in memory.items_with_vectors().unwrap_or_default() {
        let _ = vector_index.upsert(&item.sidecar.id, vec);
        let _ = item;
    }
    memory
        .write_embedding_model(embedder.model_name(), embedder.dimension())
        .await
        .ok();

    let facts = Arc::new(self_knowledge::SystemFacts::from_cfg(
        &cfg.claude,
        &cfg.memory,
        &cfg.indexer,
        &cfg.scout,
        &cfg.server,
        &cfg.retrieval,
        embedder.model_name(),
        embedder.dimension(),
    ));

    let preprocessor = Arc::new(preprocessor::Preprocessor::with_model(
        llm.clone(),
        Some(cfg.claude.model_for_preprocessor()),
    ));

    // Shared services every worker needs.
    let worker_ctx = Arc::new(workers::WorkerContext {
        preprocessor: preprocessor.clone(),
        memory: memory.clone(),
        embedder: embedder.clone(),
        vector_index: vector_index.clone(),
        preprocess_concurrency: 4,
    });

    // Discover available workers. Each reports Ok(None) if not yet
    // configured (no client_secret.json / token.json on disk) so first
    // run is graceful — the assistant sees an empty worker list and
    // falls back to its normal behavior.
    let mut worker_list: Vec<Arc<dyn workers::Worker>> = vec![];
    match workers::gmail::GmailWorker::open(memory.root()) {
        Ok(Some(gmail)) => {
            tracing::info!("gmail worker loaded");
            worker_list.push(Arc::new(gmail));
        }
        Ok(None) => {
            tracing::info!("gmail worker not configured (no client_secret.json or token.json)");
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to open gmail worker");
        }
    }
    // WWW worker is always present — WebSearch/WebFetch need no setup.
    // Autonomous tick is still gated on cfg.scout.enabled (kept under
    // the legacy `scout` section name for back-compat; renamed in a
    // future config schema bump).
    worker_list.push(Arc::new(workers::www::WwwWorker::new(
        llm.clone(),
        preprocessor.clone(),
        cfg.scout.clone(),
        cfg.claude.scout_allowed_tools.clone(),
        Some(cfg.claude.model_for_scout()),
    )));

    let workers_registry = Arc::new(workers::WorkerRegistry::new(worker_ctx, worker_list));
    for k in workers::known_worker_kinds() {
        workers_registry.register_kind(k);
    }

    // Config protocol dispatcher — owns pending OAuth state, writes
    // client_secret.json / token.json atomically, registers new worker
    // instances live after OAuth completes.
    let config_protocol = Arc::new(config_protocol::ConfigProtocol::new(
        memory.root().to_path_buf(),
        workers_registry.clone(),
    ));

    let manual = Arc::new(manual::Manual::open_or_seed(memory.root())?);

    let assistant = Arc::new(assistant::Assistant::build(
        llm.clone(),
        memory.clone(),
        embedder.clone(),
        vector_index.clone(),
        preprocessor.clone(),
        workers_registry.clone(),
        manual,
        Some(cfg.claude.model_for_assistant()),
        Some(cfg.claude.model_for_assistant_escalation()),
        cfg.retrieval.clone(),
        2, // max_search_rounds
        4, // max_manual_reads
        4, // preprocess_concurrency
        4, // connector_concurrency
        facts,
    ));
    let state = ws::AppState {
        preprocessor,
        assistant,
        config_protocol,
    };
    Ok(Built {
        state,
        memory,
        llm,
        embedder,
        vector_index,
        workers: workers_registry,
        cfg,
    })
}
