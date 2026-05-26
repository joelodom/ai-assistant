//! Runtime configuration. Read from `config.toml` if present, else use
//! built-in defaults. All fields are `#[serde(default)]` and tolerate
//! unknown keys (Invariant #7: forward-compatible reads — old config files
//! continue to load).
//!
//! The legacy `[curator]` section is accepted on load and ignored — the
//! Curator has been removed in favor of the Indexer, which is configured
//! under `[indexer]`. Same story for `sanitizer_model` (now
//! `preprocessor_model`).

use crate::retrieval::RetrievalWeights;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerCfg,
    pub memory: MemoryCfg,
    pub claude: ClaudeCfg,
    pub scout: ScoutCfg,
    pub indexer: IndexerCfg,
    pub retrieval: RetrievalWeights,
    pub logging: LoggingCfg,
    /// Legacy section. Accepted on load and ignored — the Curator is gone.
    /// Kept as a typed-but-unused field so old TOML files with a `[curator]`
    /// section still deserialize cleanly.
    #[serde(default, alias = "curator")]
    pub _legacy_curator: Option<toml::Value>,
}

/// Logging configuration. The TOML controls the default level; `RUST_LOG`
/// env var still overrides if set (standard `tracing_subscriber::EnvFilter`
/// behavior).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingCfg {
    /// `off | error | warn | info | debug | trace`. Also accepts the
    /// full env-filter directive syntax (e.g. `info,backend::retrieval=trace`).
    pub level: String,
    /// `json` (machine-parseable, for analysis) or `text` (human-readable).
    pub format: String,
    /// Write logs to stdout.
    pub stdout: bool,
    /// Write logs to a rotating file (daily, one file per UTC day, no
    /// automatic deletion — left for the user).
    pub file: bool,
    /// Directory for log files. None → `<memory-dir>/logs`. The directory
    /// is created if missing.
    pub dir: Option<PathBuf>,
    /// Filename prefix for the rotating log files. The active file is
    /// `<prefix>.YYYY-MM-DD`; a new file opens at midnight UTC.
    pub file_prefix: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerCfg {
    pub addr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryCfg {
    pub dir: PathBuf,
    /// How many recent items to surface (kept for back-compat — retrieval
    /// now folds recency into a single score, but this is still used as a
    /// safety floor in some paths).
    pub recent_window: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeCfg {
    pub binary: String,
    pub model: String,
    /// Security Preprocessor model. Runs on every turn; latency matters.
    /// Accepts the legacy field name `sanitizer_model` for back-compat.
    #[serde(alias = "sanitizer_model")]
    pub preprocessor_model: Option<String>,
    /// Assistant Core does memory-aware reasoning. Default Sonnet, with
    /// self-escalation to the escalation model when needed.
    pub assistant_model: Option<String>,
    /// Escalation target when Sonnet self-escalates or the user forces Opus.
    pub assistant_escalation_model: Option<String>,
    /// Scout: web summarization + triage.
    pub scout_model: Option<String>,
    /// Per-call timeout in seconds.
    pub timeout_secs: u64,
    /// Tools to allow the Scout and Assistant.
    pub scout_allowed_tools: Vec<String>,
    /// Legacy: ignored. The Curator no longer exists.
    #[serde(default)]
    pub curator_model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScoutCfg {
    pub enabled: bool,
    pub interval_minutes: u64,
    pub pinned_topics: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexerCfg {
    pub enabled: bool,
    pub interval_minutes: u64,
    /// How many items to embed per tick. Keeps a bulk Gmail import from
    /// hammering the embedder in a single burst.
    pub batch_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerCfg::default(),
            memory: MemoryCfg::default(),
            claude: ClaudeCfg::default(),
            scout: ScoutCfg::default(),
            indexer: IndexerCfg::default(),
            retrieval: RetrievalWeights::default(),
            logging: LoggingCfg::default(),
            _legacy_curator: None,
        }
    }
}

impl Default for LoggingCfg {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            format: "json".to_string(),
            stdout: true,
            file: true,
            dir: None,
            file_prefix: "ai-assistant.log".to_string(),
        }
    }
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self {
            addr: "127.0.0.1:8765".to_string(),
        }
    }
}

impl Default for MemoryCfg {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("./memory"),
            recent_window: 20,
        }
    }
}

impl Default for ClaudeCfg {
    fn default() -> Self {
        Self {
            binary: "claude".to_string(),
            model: "claude-opus-4-7".to_string(),
            // Preprocessor runs on every turn and does pattern recognition +
            // structured JSON output — Haiku is fast and reliable.
            preprocessor_model: Some("claude-haiku-4-5".to_string()),
            // Assistant default: Sonnet. Self-escalates via ESCALATE_TO_OPUS
            // when it judges Opus would meaningfully outperform.
            assistant_model: Some("claude-sonnet-4-6".to_string()),
            assistant_escalation_model: Some("claude-opus-4-7".to_string()),
            // Scout: web summarization. Sonnet is plenty.
            scout_model: Some("claude-sonnet-4-6".to_string()),
            timeout_secs: 180,
            scout_allowed_tools: vec!["WebSearch".to_string(), "WebFetch".to_string()],
            curator_model: None,
        }
    }
}

impl ClaudeCfg {
    pub fn model_for_preprocessor(&self) -> String {
        self.preprocessor_model.clone().unwrap_or_else(|| self.model.clone())
    }
    /// Back-compat alias.
    pub fn model_for_sanitizer(&self) -> String {
        self.model_for_preprocessor()
    }
    pub fn model_for_assistant(&self) -> String {
        self.assistant_model.clone().unwrap_or_else(|| self.model.clone())
    }
    pub fn model_for_assistant_escalation(&self) -> String {
        self.assistant_escalation_model
            .clone()
            .unwrap_or_else(|| self.model.clone())
    }
    pub fn model_for_scout(&self) -> String {
        self.scout_model.clone().unwrap_or_else(|| self.model.clone())
    }
}

impl Default for ScoutCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_minutes: 10,
            pinned_topics: vec![],
        }
    }
}

impl Default for IndexerCfg {
    fn default() -> Self {
        Self {
            // On by default — purely mechanical, no LLM cost. Backfills
            // missing .vec sidecars in the background.
            enabled: true,
            interval_minutes: 5,
            batch_size: 50,
        }
    }
}

impl Config {
    pub fn load(path: Option<&std::path::Path>) -> anyhow::Result<Self> {
        match path {
            Some(p) if p.exists() => {
                let text = std::fs::read_to_string(p)?;
                Ok(toml::from_str(&text)?)
            }
            _ => Ok(Self::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_curator_section_loads_and_is_ignored() {
        // Invariant #7: old config files still load.
        let legacy = r#"
[server]
addr = "127.0.0.1:8765"

[claude]
binary = "claude"
model = "claude-opus-4-7"
sanitizer_model = "claude-haiku-4-5"
curator_model = "claude-sonnet-4-6"

[curator]
enabled = true
interval_minutes = 60
fresh_age_hours = 48
aging_age_days = 14
stale_age_days = 90
"#;
        let cfg: Config = toml::from_str(legacy).unwrap();
        assert_eq!(cfg.claude.model_for_preprocessor(), "claude-haiku-4-5");
        assert!(cfg.claude.curator_model.is_some());
        assert!(cfg._legacy_curator.is_some());
    }

    #[test]
    fn default_config_has_indexer_enabled() {
        let cfg = Config::default();
        assert!(cfg.indexer.enabled);
    }

    #[test]
    fn default_retrieval_weights_sane() {
        let cfg = Config::default();
        let r = &cfg.retrieval;
        assert!(r.alpha > 0.0 && r.alpha < 1.0);
        assert!(r.beta > 0.0 && r.beta < 1.0);
        assert!(r.gamma > 0.0 && r.gamma < 1.0);
        assert!((r.alpha + r.beta + r.gamma - 1.0).abs() < 0.01);
        assert!(r.half_life_days > 0.0);
    }
}
