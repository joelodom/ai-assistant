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
use backend::config::LoggingCfg;
use std::path::{Path, PathBuf};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt::writer::MakeWriterExt, EnvFilter};

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

/// Initialize tracing per the TOML logging config. Composes a stdout
/// writer (if enabled) AND a daily-rotated file writer (if enabled) into
/// one subscriber. Uses JSON formatter when `format = "json"`, the human
/// `fmt` formatter otherwise. RUST_LOG still overrides the configured
/// level.
///
/// Returns the WorkerGuard for the non-blocking file appender — caller
/// MUST hold it for the lifetime of the process or buffered writes get
/// dropped at shutdown.
fn init_logging(cfg: &LoggingCfg, memory_dir: &Path) -> Result<Option<WorkerGuard>> {
    // Filter: RUST_LOG wins; else the configured level.
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(cfg.level.as_str()));

    // Optional file appender.
    let mut guard: Option<WorkerGuard> = None;
    let file_writer = if cfg.file {
        let log_dir = cfg.dir.clone().unwrap_or_else(|| memory_dir.join("logs"));
        std::fs::create_dir_all(&log_dir)?;
        let appender = tracing_appender::rolling::daily(&log_dir, &cfg.file_prefix);
        let (nb, g) = tracing_appender::non_blocking(appender);
        guard = Some(g);
        Some(nb)
    } else {
        None
    };

    // Compose writers per the (stdout, file) matrix. Cases collapse to:
    //   (true, true)   stdout + file
    //   (true, false)  stdout only
    //   (false, true)  file only
    //   (false, false) silent (we still install a subscriber so events are
    //                  defined but discarded)
    let is_json = cfg.format.eq_ignore_ascii_case("json");

    match (cfg.stdout, file_writer) {
        (true, Some(fw)) => {
            let w = std::io::stdout.and(fw);
            if is_json {
                tracing_subscriber::fmt()
                    .json()
                    .with_env_filter(env_filter)
                    .with_writer(w)
                    .with_target(true)
                    .init();
            } else {
                tracing_subscriber::fmt()
                    .with_env_filter(env_filter)
                    .with_writer(w)
                    .with_target(true)
                    .init();
            }
        }
        (true, None) => {
            if is_json {
                tracing_subscriber::fmt()
                    .json()
                    .with_env_filter(env_filter)
                    .with_writer(std::io::stdout)
                    .with_target(true)
                    .init();
            } else {
                tracing_subscriber::fmt()
                    .with_env_filter(env_filter)
                    .with_writer(std::io::stdout)
                    .with_target(true)
                    .init();
            }
        }
        (false, Some(fw)) => {
            if is_json {
                tracing_subscriber::fmt()
                    .json()
                    .with_env_filter(env_filter)
                    .with_writer(fw)
                    .with_target(true)
                    .init();
            } else {
                tracing_subscriber::fmt()
                    .with_env_filter(env_filter)
                    .with_writer(fw)
                    .with_target(true)
                    .init();
            }
        }
        (false, None) => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_writer(std::io::sink)
                .init();
        }
    }

    install_panic_hook();
    Ok(guard)
}

/// Replace the default Rust panic hook with one that routes through
/// `tracing::error!` so panics land in the log file alongside everything
/// else. We chain to the default hook afterward so panic output still
/// reaches stderr the way users expect.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let msg: &str = if let Some(s) = payload.downcast_ref::<&str>() {
            s
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.as_str()
        } else {
            "(non-string panic payload)"
        };
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "(unknown)".to_string());
        let thread = std::thread::current().name().unwrap_or("<unnamed>").to_string();
        tracing::error!(
            panic.message = msg,
            panic.location = %location,
            panic.thread = %thread,
            "panic"
        );
        default(info);
    }));
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = parse_args();
    let cfg_path = cli
        .config_path
        .clone()
        .or_else(|| {
            let p = PathBuf::from("./config.toml");
            p.exists().then_some(p)
        });
    let cfg = backend::config::Config::load(cfg_path.as_deref())?;

    // Initialize logging AFTER loading the config so the level/format/
    // destinations come from the user's TOML. Held guard keeps the
    // non-blocking file appender alive until process exit.
    let _log_guard = init_logging(&cfg.logging, &cfg.memory.dir)?;

    match &cfg_path {
        Some(p) => tracing::info!(config = %p.display(), "loaded config"),
        None => tracing::info!("no config file; using built-in defaults"),
    }

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
