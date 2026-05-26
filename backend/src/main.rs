//! Backend entry point.
//!
//! INVARIANTS:
//!   1. No outbound actions, ever. Read in / respond out only.
//!   2. Raw input is ephemeral. Sanitizer is the only thing that ever sees it.
//!   3. The Sanitizer sees everything, including the user's own queries.
//!   4. Tier-1 content is never stored or forwarded — only a content-free stub.
//!   5. The memory store contains sanitized data only.
//!
//! Flags (overrides take precedence over config.toml):
//!   --config <path>         Use this TOML config (else ./config.toml if it exists, else defaults).
//!   --memory-dir <path>     Store memory in this directory (also AI_ASSISTANT_MEMORY_DIR).
//!   --addr <ip:port>        Listen address (also AI_ASSISTANT_ADDR).
//!
//! Env knobs:
//!   AI_ASSISTANT_MOCK_CLAUDE=1   Use the canned mock LLM (for offline testing).
//!   RUST_LOG                     Log filter (default: info).

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Default)]
struct CliArgs {
    config_path: Option<PathBuf>,
    memory_dir_override: Option<PathBuf>,
    addr_override: Option<String>,
}

fn parse_args() -> CliArgs {
    let mut args = std::env::args().skip(1);
    let mut out = CliArgs::default();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => out.config_path = args.next().map(PathBuf::from),
            "--memory-dir" => out.memory_dir_override = args.next().map(PathBuf::from),
            "--addr" => out.addr_override = args.next(),
            "-h" | "--help" => {
                println!("ai-assistant-backend [--config PATH] [--memory-dir PATH] [--addr IP:PORT]");
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
    }
    if out.memory_dir_override.is_none() {
        if let Ok(v) = std::env::var("AI_ASSISTANT_MEMORY_DIR") {
            out.memory_dir_override = Some(PathBuf::from(v));
        }
    }
    if out.addr_override.is_none() {
        if let Ok(v) = std::env::var("AI_ASSISTANT_ADDR") {
            out.addr_override = Some(v);
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
    let mut cfg = backend::config::Config::load(cfg_path.as_deref())?;
    if let Some(d) = cli.memory_dir_override {
        cfg.memory.dir = d;
    }
    if let Some(a) = cli.addr_override {
        cfg.server.addr = a;
    }

    tracing::info!(
        memory_dir = %cfg.memory.dir.display(),
        addr = %cfg.server.addr,
        model = %cfg.claude.model,
        scout_enabled = cfg.scout.enabled,
        curator_enabled = cfg.curator.enabled,
        "starting ai-assistant backend"
    );

    let built = backend::build_app(cfg).await?;

    // Spawn Scout + Curator. They no-op if disabled.
    backend::scout::Scout {
        llm: built.llm.clone(),
        sanitizer: built.state.sanitizer.clone(),
        assistant: built.state.assistant.clone(),
        cfg: built.cfg.scout.clone(),
        allowed_tools: built.cfg.claude.scout_allowed_tools.clone(),
        model: Some(built.cfg.claude.model_for_scout()),
    }
    .spawn();
    backend::curator::Curator {
        llm: built.llm.clone(),
        memory: built.memory.clone(),
        cfg: built.cfg.curator.clone(),
        model: Some(built.cfg.claude.model_for_curator()),
    }
    .spawn();

    let addr = built.cfg.server.addr.clone();
    let app = backend::ws::router(built.state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on ws://{addr}/ws  (health: /health)");
    axum::serve(listener, app).await?;
    let _ = Arc::new(()); // keep types happy if compiler ever inlines
    Ok(())
}
