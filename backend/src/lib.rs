//! Backend library. The binary in `main.rs` is a thin wrapper around `run()`.
//! Integration tests in `tests/` exercise the modules directly.

pub mod assistant;
pub mod attachments;
pub mod claude;
pub mod config;
pub mod curator;
pub mod memory;
pub mod sanitizer;
pub mod scout;
pub mod self_knowledge;
pub mod ws;

use std::sync::Arc;

/// Construct the full app graph from a config. Returns the axum router and
/// the long-lived components so callers can keep their handles for tests.
pub struct Built {
    pub state: ws::AppState,
    pub memory: Arc<memory::MemoryStore>,
    pub llm: Arc<dyn claude::LlmClient>,
    pub cfg: config::Config,
}

pub async fn build_app(cfg: config::Config) -> anyhow::Result<Built> {
    let memory = Arc::new(memory::MemoryStore::open(cfg.memory.dir.clone()).await?);
    let llm = claude::make_client_from_env(&cfg.claude);

    // Seed baseline self-knowledge (idempotent).
    self_knowledge::seed_baseline(&memory).await?;

    let facts = Arc::new(self_knowledge::SystemFacts::from_cfg(
        &cfg.claude,
        &cfg.memory,
        &cfg.curator,
        &cfg.scout,
        &cfg.server,
    ));

    let sanitizer = Arc::new(sanitizer::Sanitizer::with_model(
        llm.clone(),
        Some(cfg.claude.model_for_sanitizer()),
    ));
    let assistant = Arc::new(assistant::Assistant::with_model_and_facts(
        llm.clone(),
        memory.clone(),
        Some(cfg.claude.model_for_assistant()),
        facts,
    ));
    let state = ws::AppState {
        sanitizer,
        assistant,
    };
    Ok(Built {
        state,
        memory,
        llm,
        cfg,
    })
}
