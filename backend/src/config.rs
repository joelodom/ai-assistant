use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerCfg,
    pub memory: MemoryCfg,
    pub claude: ClaudeCfg,
    pub scout: ScoutCfg,
    pub curator: CuratorCfg,
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
    /// How many recent items to surface to the assistant by default.
    pub recent_window: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeCfg {
    pub binary: String,
    /// Default model for roles that don't override. The roles below
    /// individually override when set; leaving them None means "use `model`".
    pub model: String,
    /// Sanitizer runs on every turn and does tight pattern-recognition +
    /// structured JSON output — Haiku is fast and reliable for this.
    pub sanitizer_model: Option<String>,
    /// Assistant Core does memory-aware reasoning; default to the main model
    /// (typically the smartest).
    pub assistant_model: Option<String>,
    /// Curator summarizes aging items — Haiku is plenty.
    pub curator_model: Option<String>,
    /// Scout browses the web and synthesizes findings.
    pub scout_model: Option<String>,
    /// Per-call timeout in seconds.
    pub timeout_secs: u64,
    /// Tools to allow the Scout and Assistant (sanitizer never gets tools).
    pub scout_allowed_tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScoutCfg {
    pub enabled: bool,
    pub interval_minutes: u64,
    pub topics: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CuratorCfg {
    pub enabled: bool,
    pub interval_minutes: u64,
    /// Items younger than this are always Fresh.
    pub fresh_age_hours: u64,
    /// Items older than fresh but younger than this can be summarized.
    pub aging_age_days: u64,
    /// Items older than this can be collapsed to a one-liner.
    pub stale_age_days: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerCfg::default(),
            memory: MemoryCfg::default(),
            claude: ClaudeCfg::default(),
            scout: ScoutCfg::default(),
            curator: CuratorCfg::default(),
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
            sanitizer_model: Some("claude-haiku-4-5".to_string()),
            assistant_model: None,
            // Sonnet, not Haiku: the Curator destructively rewrites items
            // (the summary replaces the original body). Its mistakes are
            // silent and permanent, so we err toward smarter compression
            // over speed. The Curator runs in the background every 60 min;
            // latency isn't load-bearing the way it is for the Sanitizer.
            curator_model: Some("claude-sonnet-4-6".to_string()),
            // Sonnet, not Opus: Scout summarizes news/web findings into a
            // short bulleted list. That's a tractable extraction-and-
            // summarization task — Opus is wasted spend here. Sonnet
            // handles web-tool use and triage cleanly.
            scout_model: Some("claude-sonnet-4-6".to_string()),
            timeout_secs: 180,
            scout_allowed_tools: vec!["WebSearch".to_string(), "WebFetch".to_string()],
        }
    }
}

impl ClaudeCfg {
    pub fn model_for_sanitizer(&self) -> String {
        self.sanitizer_model.clone().unwrap_or_else(|| self.model.clone())
    }
    pub fn model_for_assistant(&self) -> String {
        self.assistant_model.clone().unwrap_or_else(|| self.model.clone())
    }
    pub fn model_for_curator(&self) -> String {
        self.curator_model.clone().unwrap_or_else(|| self.model.clone())
    }
    pub fn model_for_scout(&self) -> String {
        self.scout_model.clone().unwrap_or_else(|| self.model.clone())
    }
}

impl Default for ScoutCfg {
    fn default() -> Self {
        Self {
            // Off by default — Scout silently spends tokens in the background
            // and dumps findings into memory before the user has expressed
            // any topic preferences. Flip on after you've tuned the topic
            // list below to what you actually care about.
            enabled: false,
            interval_minutes: 10,
            // Broad starter set — meant to be edited. The user's actual
            // interests get learned over time through preference statements
            // ("stop telling me about crypto"), but the Scout needs some
            // seed list to query on a fresh install.
            topics: vec![
                "world news headlines".to_string(),
                "US national news".to_string(),
                "technology and AI news".to_string(),
                "science and space".to_string(),
                "local weather and severe-weather alerts".to_string(),
                "notable events in the user's region".to_string(),
            ],
        }
    }
}

impl Default for CuratorCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_minutes: 60,
            fresh_age_hours: 48,
            aging_age_days: 14,
            stale_age_days: 90,
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
