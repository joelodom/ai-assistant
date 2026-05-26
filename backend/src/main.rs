//! Backend entry point.
//!
//! INVARIANTS:
//!   1. No outbound actions, ever. Read in / respond out only.
//!   2. Raw input is ephemeral. The Security Preprocessor is the only thing
//!      that ever sees it.
//!   3. The Preprocessor sees everything, including the user's own queries
//!      (one explicit user-controlled exception: HAZMAT mode).
//!   4. Tier-1 content is never stored or forwarded — only a content-free stub.
//!   5. The memory store contains sanitized data only.
//!   6. The backend is restart-safe at any time.
//!   7. Forward-compatible reads: old memory directories and old config
//!      files must continue to load. Derived caches (HNSW graph) are
//!      rebuildable from sidecars.
//!
//! ONE flag only: `--config <path>` to point at a TOML config file. With
//! no flag, looks for `./config.toml` in cwd; if absent, uses built-in
//! defaults. Everything else (memory dir, listen address, model choices,
//! retrieval weights, scout toggles, etc.) lives in the TOML.
//!
//! All runtime configuration (connector setup, etc.) is driven from the
//! client via the WebSocket. Just start the backend and talk to the
//! assistant.
//!
//! Env knobs (testing-only; not used in production):
//!   AI_ASSISTANT_MOCK_CLAUDE=1     Use the canned mock LLM (for offline testing).
//!   AI_ASSISTANT_MOCK_EMBEDDER=1   Use the deterministic hash-based mock embedder.
//!   RUST_LOG                       Log filter (default: info).

use anyhow::Result;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Default)]
struct CliArgs {
    config_path: Option<PathBuf>,
}

fn parse_args() -> CliArgs {
    let mut args = std::env::args().skip(1);
    let mut out = CliArgs::default();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => out.config_path = args.next().map(PathBuf::from),
            "-h" | "--help" => {
                println!(
                    "ai-assistant-backend [--config PATH]\n\
                     \n\
                     With no --config, looks for ./config.toml in the current directory;\n\
                     if absent, uses built-in defaults. All other configuration lives in\n\
                     the TOML. Runtime config (connector setup, etc.) flows from the client\n\
                     over the WebSocket."
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
    }
    out
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let cli = parse_args();
    let cfg_path = cli
        .config_path
        .clone()
        .or_else(|| {
            let p = PathBuf::from("./config.toml");
            p.exists().then_some(p)
        });
    if let Some(p) = &cfg_path {
        tracing::info!(config = %p.display(), "loading config");
    } else {
        tracing::info!("no config file; using built-in defaults");
    }
    let cfg = backend::config::Config::load(cfg_path.as_deref())?;

    tracing::info!(
        memory_dir = %cfg.memory.dir.display(),
        addr = %cfg.server.addr,
        model = %cfg.claude.model,
        scout_enabled = cfg.scout.enabled,
        indexer_enabled = cfg.indexer.enabled,
        "starting ai-assistant backend"
    );

    let built = backend::build_app(cfg).await?;

    // Spawn Scout (opt-in) + Indexer (mechanical, no LLM). They no-op if disabled.
    backend::scout::Scout {
        llm: built.llm.clone(),
        sanitizer: built.state.preprocessor.clone(),
        assistant: built.state.assistant.clone(),
        cfg: built.cfg.scout.clone(),
        allowed_tools: built.cfg.claude.scout_allowed_tools.clone(),
        model: Some(built.cfg.claude.model_for_scout()),
    }
    .spawn();
    backend::indexer::Indexer {
        memory: built.memory.clone(),
        embedder: built.embedder.clone(),
        vector_index: built.vector_index.clone(),
        cfg: built.cfg.indexer.clone(),
    }
    .spawn();

    let addr = built.cfg.server.addr.clone();
    let app = backend::ws::router(built.state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on ws://{addr}/ws  (health: /health)");
    axum::serve(listener, app).await?;
    Ok(())
}
