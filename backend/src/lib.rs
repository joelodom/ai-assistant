//! Backend library. The binary in `main.rs` is a thin wrapper around `run()`.
//! Integration tests in `tests/` exercise the modules directly.

pub mod assistant;
pub mod attachments;
pub mod claude;
pub mod config;
pub mod config_protocol;
pub mod connectors;
pub mod embedder;
pub mod indexer;
pub mod manual;
pub mod memory;
pub mod preprocessor;
pub mod retrieval;
pub mod scout;
pub mod self_knowledge;
pub mod vector_index;
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
    pub connectors: Arc<connectors::ConnectorRegistry>,
    pub cfg: config::Config,
}

pub async fn build_app(cfg: config::Config) -> anyhow::Result<Built> {
    let memory = Arc::new(memory::MemoryStore::open(cfg.memory.dir.clone()).await?);
    let llm = claude::make_client_from_env(&cfg.claude);
    let embedder = embedder::make_embedder_from_env();

    // Open or initialize the vector index. The actual graph contents are a
    // derived cache — Indexer will warm it from .vec sidecars in the
    // background. If the existing embedding_model record disagrees with the
    // current embedder, the Indexer will trigger a re-embed.
    let vector_index = Arc::new(vector_index::VectorIndex::open(
        memory.root(),
        embedder.model_name(),
        embedder.dimension(),
    )?);
    // Warm the index from any existing .vec sidecars synchronously so the
    // first retrieve() call after startup has something to work with.
    for (item, vec) in memory.items_with_vectors().unwrap_or_default() {
        let _ = vector_index.upsert(&item.sidecar.id, vec);
        let _ = item; // silence unused warning if the upsert is no-op'd
    }
    // Record the active embedding model. The Indexer reads this on each
    // tick to detect model changes.
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

    // Discover available connectors. Each connector reports Ok(None) if
    // not yet configured (no client_secret.json / token.json on disk) so
    // first-run is graceful — the assistant sees an empty connector list
    // and falls back to its normal behavior.
    let mut connector_list: Vec<Arc<dyn connectors::Connector>> = vec![];
    match connectors::gmail::GmailConnector::open(memory.root()) {
        Ok(Some(gmail)) => {
            tracing::info!("gmail connector loaded");
            connector_list.push(Arc::new(gmail));
        }
        Ok(None) => {
            tracing::info!("gmail connector not configured (no client_secret.json or token.json)");
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to open gmail connector");
        }
    }
    let connectors_registry = Arc::new(connectors::ConnectorRegistry::new(connector_list));
    // Register every connector *kind* so the assistant prompt lists them
    // even when not yet configured — letting the assistant offer setup.
    for k in connectors::known_connector_kinds() {
        connectors_registry.register_kind(k);
    }

    // Config protocol dispatcher — owns pending OAuth state, writes
    // client_secret.json / token.json atomically, registers new connector
    // instances live after OAuth completes.
    let config_protocol = Arc::new(config_protocol::ConfigProtocol::new(
        memory.root().to_path_buf(),
        connectors_registry.clone(),
    ));

    let manual = Arc::new(manual::Manual::open_or_seed(memory.root())?);

    let assistant = Arc::new(assistant::Assistant::build(
        llm.clone(),
        memory.clone(),
        embedder.clone(),
        vector_index.clone(),
        preprocessor.clone(),
        connectors_registry.clone(),
        manual,
        Some(cfg.claude.model_for_assistant()),
        Some(cfg.claude.model_for_assistant_escalation()),
        cfg.retrieval.clone(),
        2, // max_search_rounds — bound to keep latency + cost predictable
        4, // max_manual_reads
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
        connectors: connectors_registry,
        cfg,
    })
}
