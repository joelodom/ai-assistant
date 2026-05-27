//! Connectors — search-only adapters to external personal-data sources
//! (Gmail, Drive, Calendar, ...). The assistant emits `SEARCH: <connector>
//! <query>` markers when it wants to fetch from one; the backend runs the
//! search, passes each result through the Preprocessor (the security
//! invariant — connector data is "outside world" data), and ingests
//! sanitized results into memory.
//!
//! Connectors are **search-only** in v1. No push subscriptions, no change
//! notifications. The assistant decides when to look.
//!
//! Defense in depth: even if a connector has a bug, the upstream API's
//! authorization layer enforces the scope the user granted (e.g. Gmail
//! `gmail.readonly`). The connector trait deliberately exposes no
//! write-capable methods — there's no `.send()` for code to bug-call into
//! existence.

pub mod gmail;
pub mod oauth;

/// Lookup the OAuth scope a connector kind needs. Returns None for
/// unknown connector names. Used by config_protocol when minting auth
/// URLs and by the assistant when rendering BeginOAuth requests.
pub fn scope_for(name: &str) -> Option<&'static str> {
    match name {
        "gmail" => Some(crate::connectors::gmail::GMAIL_SCOPE),
        _ => None,
    }
}

/// Statically-known connector kinds. Registered in the ConnectorRegistry
/// at startup so the assistant prompt shows them as available-to-configure
/// even before they've been wired up. The procedural how-to-set-up content
/// lives in the system manual (`READ_MANUAL: connector-setup-<name>`),
/// not here — keeps the prompt lean and the manual the single source of
/// procedural truth.
pub fn known_connector_kinds() -> Vec<KnownConnector> {
    vec![KnownConnector {
        name: "gmail",
        description: "Read-only Gmail search. Supports `from:`, `subject:`, \
                      `before:`/`after:`, `has:attachment`, free text. \
                      For setup, read manual section: connector-setup-gmail.",
    }]
}

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// A single search hit from a connector. Pre-sanitization — this will be
/// fed to the Preprocessor before anything else sees it.
#[derive(Debug, Clone)]
pub struct RawConnectorResult {
    /// Provider-side identifier (e.g. Gmail message id). Lets the
    /// assistant cite "from Gmail message <id>" and lets the user dig
    /// deeper via the source_url.
    pub source_id: String,
    /// Optional direct link back to the item (e.g. https://mail.google.com/...).
    pub source_url: Option<String>,
    /// The raw text to sanitize. For Gmail, this is typically
    /// `From: ...\nSubject: ...\nDate: ...\n\n<body>`.
    pub content: String,
    /// When the item was produced at the source (sent date for an email,
    /// etc.). Used for the memory item's created_at if present.
    pub at: Option<DateTime<Utc>>,
}

/// What the assistant can do with this connector. The description text is
/// rendered into the assistant's prompt so the LLM knows what's available
/// and how to query it.
#[async_trait]
pub trait Connector: Send + Sync {
    /// Short, stable identifier — used in the SEARCH marker. e.g. "gmail".
    fn name(&self) -> &'static str;

    /// One-paragraph description of what this connector can search and the
    /// query syntax it accepts. Goes into the assistant prompt.
    fn description(&self) -> &'static str;

    /// True if the connector is configured and has valid credentials. False
    /// if the user hasn't run setup yet. Unavailable connectors are still
    /// listed (so the assistant knows the option exists) but their SEARCH
    /// markers will surface a "not configured" note instead of executing.
    fn is_available(&self) -> bool;

    /// Run a search. Returns up to `limit` results. Errors propagate; the
    /// assistant will get a note that the search failed.
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<RawConnectorResult>>;
}

/// Registry of all installed connectors, keyed by name. Mutable at runtime
/// so the config-protocol handler can install a freshly-authorized
/// connector without a backend restart.
pub struct ConnectorRegistry {
    by_name: RwLock<HashMap<String, Arc<dyn Connector>>>,
    /// Names of every connector kind known to the system, even if not
    /// currently configured. Used so the assistant prompt can show "✗ NOT
    /// CONFIGURED" entries — letting the assistant guide the user to set
    /// them up.
    known: RwLock<Vec<KnownConnector>>,
}

/// Static metadata for a connector kind that may not yet be configured.
/// Used to render the AVAILABLE CONNECTORS block when no instance exists.
#[derive(Debug, Clone)]
pub struct KnownConnector {
    pub name: &'static str,
    pub description: &'static str,
}

impl ConnectorRegistry {
    pub fn new(connectors: Vec<Arc<dyn Connector>>) -> Self {
        let mut by_name = HashMap::new();
        for c in connectors {
            by_name.insert(c.name().to_string(), c);
        }
        Self {
            by_name: RwLock::new(by_name),
            known: RwLock::new(Vec::new()),
        }
    }

    pub fn empty() -> Self {
        Self {
            by_name: RwLock::new(HashMap::new()),
            known: RwLock::new(Vec::new()),
        }
    }

    /// Record that a connector *kind* exists in the system even if no
    /// instance is currently configured. The assistant uses this to know
    /// what setup flows it can offer.
    pub fn register_kind(&self, k: KnownConnector) {
        let mut kg = self.known.write().unwrap();
        if !kg.iter().any(|x| x.name == k.name) {
            kg.push(k);
        }
    }

    /// Install or replace a connector instance at runtime. Called by the
    /// config-protocol handler once a setup flow completes.
    pub fn register(&self, c: Arc<dyn Connector>) {
        self.by_name
            .write()
            .unwrap()
            .insert(c.name().to_string(), c);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Connector>> {
        self.by_name.read().unwrap().get(name).cloned()
    }

    /// All currently-instantiated connectors, sorted by name.
    pub fn all(&self) -> Vec<Arc<dyn Connector>> {
        let g = self.by_name.read().unwrap();
        let mut v: Vec<_> = g.values().cloned().collect();
        v.sort_by_key(|c| c.name());
        v
    }

    pub fn known_kinds(&self) -> Vec<KnownConnector> {
        self.known.read().unwrap().clone()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.read().unwrap().is_empty() && self.known.read().unwrap().is_empty()
    }

    /// Render the AVAILABLE CONNECTORS block for the assistant's prompt.
    /// Lists both configured connector instances AND known kinds that
    /// haven't been set up yet — so the assistant can tell the user what
    /// setup flows are available.
    pub fn render_prompt_block(&self) -> String {
        let instances = self.all();
        let kinds = self.known_kinds();
        if instances.is_empty() && kinds.is_empty() {
            return String::new();
        }
        let mut s = String::from(
            "EXTERNAL SEARCH (connectors):\n\
             Some external personal-data sources are wired in. To search one in service of\n\
             answering a question, include a line of EXACTLY this form anywhere in your reply\n\
             (one per line, may have several):\n\
             \n  SEARCH: <connector_name> <query>\n\n\
             Each search executes, each result passes through the Preprocessor (sanitization\n\
             + importance scoring), non-drop results land in memory, and you'll be re-prompted\n\
             with the new items available via retrieval. Search when the user asks something\n\
             whose answer is likely in one of these sources. Don't search speculatively — it\n\
             costs latency and API quota. You may emit up to 2 rounds of searches per turn.\n\
             \n\
             CONFIGURATION: If the user wants to set up a connector that's marked NOT\n\
             CONFIGURED below, walk them through it conversationally. Use these markers\n\
             (one per turn — wait for the user to complete each step before the next):\n\
             \n  CONFIG_REQUEST_FILE: <connector_name> <filename>\n\
                  → ask the user for a file (e.g. their OAuth client_secret.json)\n\
               CONFIG_BEGIN_OAUTH: <connector_name>\n\
                  → begin the browser OAuth handshake\n\
             \n\
             Available connectors:\n",
        );
        let mut listed_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        for c in &instances {
            let status = if c.is_available() {
                "✓ ACTIVE"
            } else {
                "✗ NOT CONFIGURED"
            };
            s.push_str(&format!(
                "  • {} [{}]\n    {}\n",
                c.name(),
                status,
                c.description()
            ));
            listed_names.insert(c.name().to_string());
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
}

/// A test-only connector that returns canned results. The Mock checks for
/// exact-match queries first, then falls back to a single "no results" hit.
#[cfg(test)]
pub mod mock {
    use super::*;
    use std::sync::Mutex;

    pub struct MockConnector {
        name: &'static str,
        canned: Mutex<HashMap<String, Vec<RawConnectorResult>>>,
        calls: Mutex<Vec<(String, usize)>>,
        available: bool,
    }

    impl MockConnector {
        pub fn new(name: &'static str) -> Arc<Self> {
            Arc::new(Self {
                name,
                canned: Mutex::new(HashMap::new()),
                calls: Mutex::new(Vec::new()),
                available: true,
            })
        }

        pub fn respond_when(&self, query: &str, results: Vec<RawConnectorResult>) {
            self.canned
                .lock()
                .unwrap()
                .insert(query.to_string(), results);
        }

        pub fn calls(&self) -> Vec<(String, usize)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Connector for MockConnector {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> &'static str {
            "Mock connector for tests."
        }
        fn is_available(&self) -> bool {
            self.available
        }
        async fn search(&self, query: &str, limit: usize) -> Result<Vec<RawConnectorResult>> {
            self.calls.lock().unwrap().push((query.to_string(), limit));
            if let Some(hits) = self.canned.lock().unwrap().get(query) {
                return Ok(hits.clone());
            }
            Ok(vec![])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_renders_empty_block() {
        let r = ConnectorRegistry::empty();
        assert!(r.render_prompt_block().is_empty());
        assert!(r.is_empty());
    }

    #[test]
    fn registry_lists_connectors_alphabetically() {
        let a: Arc<dyn Connector> = mock::MockConnector::new("alpha");
        let b: Arc<dyn Connector> = mock::MockConnector::new("beta");
        // Insert in reverse order.
        let r = ConnectorRegistry::new(vec![b, a]);
        let names: Vec<_> = r.all().iter().map(|c| c.name()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn registry_lookup_by_name() {
        let m: Arc<dyn Connector> = mock::MockConnector::new("foo");
        let r = ConnectorRegistry::new(vec![m]);
        assert!(r.get("foo").is_some());
        assert!(r.get("bar").is_none());
    }

    #[test]
    fn prompt_block_includes_each_connector() {
        let a: Arc<dyn Connector> = mock::MockConnector::new("alpha");
        let b: Arc<dyn Connector> = mock::MockConnector::new("beta");
        let r = ConnectorRegistry::new(vec![a, b]);
        let block = r.render_prompt_block();
        assert!(block.contains("SEARCH:"));
        assert!(block.contains("alpha"));
        assert!(block.contains("beta"));
        assert!(block.contains("Mock connector for tests"));
    }
}
