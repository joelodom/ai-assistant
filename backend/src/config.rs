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
    pub model: String,
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
            timeout_secs: 180,
            scout_allowed_tools: vec!["WebSearch".to_string(), "WebFetch".to_string()],
        }
    }
}

impl Default for ScoutCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_minutes: 10,
            topics: vec![
                "world news headlines".to_string(),
                "technology news".to_string(),
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
