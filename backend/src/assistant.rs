//! The Assistant Core. Receives only sanitized content (or HAZMAT-bypassed
//! content from the explicit user opt-in).
//!
//! Pipeline per turn:
//!  1. Persist the user message (sanitized output + Preprocessor importance).
//!  2. Embed the message, upsert into the vector index.
//!  3. Hybrid retrieve: vector + keyword + recency + importance.
//!  4. Build prompt with persona, metadata, retrieved memory, preferences,
//!     and the AVAILABLE CONNECTORS block.
//!  5. Call LLM. Handle three kinds of markers in a single loop:
//!       - SEARCH: <connector> <query>  → run search, ingest, re-call
//!       - ESCALATE_TO_OPUS: <reason>   → re-call with escalation model
//!       - FORGET: <item_id>            → tombstone (handled post-loop)
//!  6. Persist assistant note.
//!
//! The search loop is bounded by `max_search_rounds` (default 2) so the
//! assistant can't recurse forever. ESCALATE is one-shot (Sonnet hands off
//! to Opus; Opus answers, no further escalation).

use crate::claude::{LlmClient, LlmOptions};
use crate::connectors::ConnectorRegistry;
use crate::embedder::Embedder;
use crate::manual::Manual;
use crate::memory::{ItemKind, MemoryStore};
use crate::preprocessor::{InputProvenance, Preprocessor, PreprocessorResult};
use crate::retrieval::{retrieve, RetrievalWeights, ScoredItem};
use crate::vector_index::VectorIndex;
use anyhow::Result;
use shared::{Metadata, Tier};
use std::sync::Arc;

pub struct Assistant {
    pub llm: Arc<dyn LlmClient>,
    pub memory: Arc<MemoryStore>,
    pub embedder: Arc<dyn Embedder>,
    pub vector_index: Arc<VectorIndex>,
    pub preprocessor: Arc<Preprocessor>,
    pub connectors: Arc<ConnectorRegistry>,
    pub manual: Arc<Manual>,
    pub model: Option<String>,
    /// Heavier model Sonnet hands off to when it judges a question needs
    /// deeper reasoning, or when the user sets `force_opus`.
    pub escalation_model: Option<String>,
    pub retrieval_weights: RetrievalWeights,
    /// Maximum SEARCH rounds per turn. 2 = one search + one re-call;
    /// beyond that the assistant answers with what it has.
    pub max_search_rounds: usize,
    /// Maximum total READ_MANUAL fetches per turn. Counts each section
    /// pulled, across however many rounds.
    pub max_manual_reads: usize,
    pub system_facts: Arc<crate::self_knowledge::SystemFacts>,
}

pub const ESCALATION_MARKER: &str = "ESCALATE_TO_OPUS:";

/// Marker the assistant emits when the user asks to forget a specific item.
pub const FORGET_MARKER: &str = "FORGET:";

/// Marker the assistant emits to request a connector search.
/// Format: `SEARCH: <connector_name> <free-form query>`.
pub const SEARCH_MARKER: &str = "SEARCH:";

/// Marker the assistant emits to fetch a section of the system manual.
/// Format: `READ_MANUAL: <section>`, or bare `READ_MANUAL` for the TOC.
pub const READ_MANUAL_MARKER: &str = "READ_MANUAL";

/// Marker the assistant emits to request a file from the user during
/// connector setup. Format: `CONFIG_REQUEST_FILE: <connector> <filename>`.
pub const CONFIG_REQUEST_FILE_MARKER: &str = "CONFIG_REQUEST_FILE:";

/// Marker the assistant emits to begin the OAuth handshake for a connector.
/// Format: `CONFIG_BEGIN_OAUTH: <connector>`.
pub const CONFIG_BEGIN_OAUTH_MARKER: &str = "CONFIG_BEGIN_OAUTH:";

#[derive(Debug, Clone)]
pub struct RespondOutcome {
    pub text: String,
    /// Config-flow requests the assistant emitted in this turn (parsed
    /// from CONFIG_REQUEST_FILE / CONFIG_BEGIN_OAUTH markers). The WS
    /// handler converts each to a `ServerMessage::ConfigRequest` and
    /// sends it to the client.
    pub config_requests: Vec<shared::ConfigRequestKind>,
    pub model_used: String,
    pub escalated: bool,
    pub escalation_reason: Option<String>,
    /// Item id that was tombstoned, if a FORGET marker was acted on.
    pub forgotten_item_id: Option<String>,
    /// One-line summaries of any connector searches executed this turn.
    /// Used by the WS handler to prepend a preamble to the user's reply
    /// so they can see what the assistant did under the hood.
    pub search_log: Vec<String>,
    /// Names of manual sections the assistant read this turn. Surfaced
    /// in the transcript so the user sees what the assistant consulted.
    pub manual_reads: Vec<String>,
}

impl Assistant {
    /// Minimal test constructor. Wires an empty connector registry and a
    /// Preprocessor backed by the same mock LLM. Production callers should
    /// use `build()` instead.
    pub fn new(llm: Arc<dyn LlmClient>, memory: Arc<MemoryStore>) -> Self {
        let embedder: Arc<dyn Embedder> = Arc::new(crate::embedder::MockEmbedder::new());
        let vector_index = Arc::new(
            VectorIndex::open(memory.root(), embedder.model_name(), embedder.dimension())
                .expect("open vector index"),
        );
        let preprocessor = Arc::new(Preprocessor::new(llm.clone()));
        let manual = Arc::new(Manual::open_or_seed(memory.root()).expect("seed manual"));
        Self {
            llm,
            memory,
            embedder,
            vector_index,
            preprocessor,
            connectors: Arc::new(ConnectorRegistry::empty()),
            manual,
            model: None,
            escalation_model: None,
            retrieval_weights: RetrievalWeights::default(),
            max_search_rounds: 2,
            max_manual_reads: 4,
            system_facts: Arc::new(crate::self_knowledge::SystemFacts::placeholder()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn build(
        llm: Arc<dyn LlmClient>,
        memory: Arc<MemoryStore>,
        embedder: Arc<dyn Embedder>,
        vector_index: Arc<VectorIndex>,
        preprocessor: Arc<Preprocessor>,
        connectors: Arc<ConnectorRegistry>,
        manual: Arc<Manual>,
        model: Option<String>,
        escalation_model: Option<String>,
        retrieval_weights: RetrievalWeights,
        max_search_rounds: usize,
        max_manual_reads: usize,
        system_facts: Arc<crate::self_knowledge::SystemFacts>,
    ) -> Self {
        Self {
            llm,
            memory,
            embedder,
            vector_index,
            preprocessor,
            connectors,
            manual,
            model,
            escalation_model,
            retrieval_weights,
            max_search_rounds,
            max_manual_reads,
            system_facts,
        }
    }

    /// Back-compat for old tests that took (llm, memory, model, facts).
    pub fn with_model_and_facts(
        llm: Arc<dyn LlmClient>,
        memory: Arc<MemoryStore>,
        model: Option<String>,
        system_facts: Arc<crate::self_knowledge::SystemFacts>,
    ) -> Self {
        let mut a = Self::new(llm, memory);
        a.model = model;
        a.system_facts = system_facts;
        a
    }

    /// Back-compat for old tests.
    pub fn with_models_and_facts(
        llm: Arc<dyn LlmClient>,
        memory: Arc<MemoryStore>,
        model: Option<String>,
        escalation_model: Option<String>,
        system_facts: Arc<crate::self_knowledge::SystemFacts>,
    ) -> Self {
        let mut a = Self::new(llm, memory);
        a.model = model;
        a.escalation_model = escalation_model;
        a.system_facts = system_facts;
        a
    }

    /// Test seam: override the connector registry on an already-built
    /// Assistant. Useful when wiring a MockConnector in unit tests.
    pub fn with_connectors(mut self, connectors: Arc<ConnectorRegistry>) -> Self {
        self.connectors = connectors;
        self
    }

    pub async fn introduction(&self) -> String {
        let prefs = self.memory.preferences().await;
        let user_items: usize = self
            .memory
            .scan_all()
            .unwrap_or_default()
            .iter()
            .filter(|i| i.sidecar.kind != ItemKind::SelfKnowledge)
            .count();
        let bootstrap = user_items == 0 && prefs.statements.is_empty();
        if bootstrap {
            BOOTSTRAP_INTRO.to_string()
        } else {
            format!(
                "Welcome back. I have {user_items} item(s) in memory and {} stored preference(s). \
                 Ask me what you need to be thinking about, or hand me more to remember.",
                prefs.statements.len()
            )
        }
    }

    pub async fn respond(
        &self,
        preprocessed: &PreprocessorResult,
        metadata: &Metadata,
        force_opus: bool,
    ) -> Result<RespondOutcome> {
        // 1. Persist user-side first.
        let user_kind = if looks_like_question(&preprocessed.output) {
            ItemKind::UserMessage
        } else {
            ItemKind::Ingestion
        };
        let is_hazmat = preprocessed.redaction_report.contains("HAZMAT BYPASS");
        let user_importance = if is_hazmat { 0.8 } else { preprocessed.importance };
        let mut tags: Vec<String> = vec![];
        if is_hazmat {
            tags.push("hazmat".to_string());
        }
        let sidecar = self
            .memory
            .add_with_reason(
                &preprocessed.output,
                user_kind,
                user_importance,
                preprocessed.importance_reason.clone(),
                Some(metadata.clone()),
                preprocessed.redaction_report.clone(),
                tags,
            )
            .await?;

        // 2. Embed + upsert.
        if let Ok(vec) = self.embedder.embed(&preprocessed.output).await {
            if let Some(item) = self.memory.get(&sidecar.id).ok().flatten() {
                let _ = self.memory.write_vector(&item, &vec).await;
                let _ = self.vector_index.upsert(&sidecar.id, vec);
            }
        }

        // 3. The big loop: READ_MANUAL + SEARCH rounds + ESCALATE + final reply.
        let primary_model = if force_opus {
            self.escalation_model.clone().or_else(|| self.model.clone())
        } else {
            self.model.clone()
        };
        let mut current_model = primary_model.clone();
        let mut escalated = false;
        let mut escalation_reason: Option<String> = None;
        let mut search_rounds = 0usize;
        let mut search_log: Vec<String> = vec![];
        // Manual excerpts accumulated this turn — injected into the next
        // re-prompt as a RECENTLY READ MANUAL SECTIONS block.
        let mut manual_excerpts: Vec<(String, String)> = vec![];
        let mut manual_reads_log: Vec<String> = vec![];

        let final_reply = loop {
            // Rebuild prompt every iteration — memory may have grown via
            // SEARCH ingestion, and manual_excerpts may have grown via
            // READ_MANUAL in the previous round.
            let retrieved = retrieve(
                &self.memory,
                &*self.embedder,
                &self.vector_index,
                &self.retrieval_weights,
                &preprocessed.output,
                15,
            )
            .await
            .unwrap_or_default();
            let prefs = self.memory.preferences().await;
            let item_count = self.memory.stats().get("total").copied().unwrap_or(0);
            let facts_block = self.system_facts.render_prompt_block(item_count);
            let connectors_block = self.connectors.render_prompt_block();
            let manual_block = render_manual_pointer(&self.manual);
            let excerpts_block = render_manual_excerpts(&manual_excerpts);
            let prompt = build_prompt(
                metadata,
                &preprocessed.output,
                preprocessed.tier,
                &retrieved,
                &prefs.statements,
                &facts_block,
                &connectors_block,
                &manual_block,
                &excerpts_block,
            );

            let opts = LlmOptions {
                allowed_tools: vec!["WebSearch".into(), "WebFetch".into()],
                model: current_model.clone(),
                ..Default::default()
            };
            let raw = self.llm.oneshot(&prompt, opts).await?;
            let reply = raw.trim().to_string();

            // Check ESCALATE first — it short-circuits the rest of the
            // reply per the marker's contract.
            if !force_opus && !escalated && reply.starts_with(ESCALATION_MARKER) {
                let reason = reply
                    .trim_start_matches(ESCALATION_MARKER)
                    .trim()
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                escalation_reason = if reason.is_empty() { None } else { Some(reason) };
                let esc = self.escalation_model.clone().or_else(|| self.model.clone());
                current_model = esc;
                escalated = true;
                continue;
            }

            // READ_MANUAL — fetch sections before SEARCH, since manual
            // content often informs whether a search is needed.
            let manual_requests = parse_manual_markers(&reply);
            if !manual_requests.is_empty()
                && manual_excerpts.len() < self.max_manual_reads
            {
                let budget = self.max_manual_reads - manual_excerpts.len();
                for section in manual_requests.into_iter().take(budget) {
                    let (display_name, body) = if section.is_empty() {
                        ("table-of-contents".to_string(), self.manual.render_toc())
                    } else {
                        match self.manual.read_section(&section) {
                            Some(b) => (section.clone(), b),
                            None => (
                                section.clone(),
                                format!(
                                    "(no such section: \"{section}\". Use READ_MANUAL with no args to see the TOC.)"
                                ),
                            ),
                        }
                    };
                    manual_reads_log.push(display_name.clone());
                    manual_excerpts.push((display_name, body));
                }
                continue;
            }

            // Then SEARCH — markers may appear interspersed with prose.
            let searches = parse_search_markers(&reply);
            if !searches.is_empty() && search_rounds < self.max_search_rounds {
                search_rounds += 1;
                for (conn_name, query) in searches {
                    let summary = self
                        .execute_search(&conn_name, &query, metadata)
                        .await;
                    search_log.push(summary);
                }
                continue;
            }

            // No more markers (or hit a cap). This is the final reply.
            break reply;
        };

        // 4a. Parse any CONFIG_REQUEST_FILE / CONFIG_BEGIN_OAUTH markers
        //     out of the reply text. These tell the WS handler to send a
        //     ConfigRequest frame to the client; they don't get echoed to
        //     the user as-is.
        let (after_config_strip, config_requests) = strip_config_markers(&final_reply);

        // 4b. Handle FORGET markers in the (now config-stripped) final reply.
        let (final_text, forgotten_item_id) =
            self.handle_forget_markers(&after_config_strip).await;

        // 5. Persist assistant note.
        let model_used = current_model.clone().unwrap_or_default();
        let mut note_tags = vec!["assistant".to_string()];
        if escalated {
            note_tags.push("escalated".into());
        }
        if force_opus {
            note_tags.push("force-opus".into());
        }
        if forgotten_item_id.is_some() {
            note_tags.push("forget-action".into());
        }
        if !search_log.is_empty() {
            note_tags.push("search-action".into());
        }
        let note_body = if escalated {
            format!(
                "(assistant reply via {model_used}, escalated from {}{}) {final_text}",
                self.model.clone().unwrap_or_else(|| "primary".into()),
                escalation_reason
                    .as_deref()
                    .map(|r| format!(" — reason: {r}"))
                    .unwrap_or_default()
            )
        } else {
            format!("(assistant reply via {model_used}) {final_text}")
        };
        self.memory
            .add(
                &note_body,
                ItemKind::AssistantNote,
                0.2,
                Some(metadata.clone()),
                String::new(),
                note_tags,
            )
            .await
            .ok();

        if let Some(pref) = detect_preference(&preprocessed.output) {
            self.memory.add_preference(&pref).await.ok();
        }

        Ok(RespondOutcome {
            text: final_text,
            config_requests,
            model_used,
            escalated,
            escalation_reason,
            forgotten_item_id,
            search_log,
            manual_reads: manual_reads_log,
        })
    }

    /// Execute one SEARCH: marker. Each connector result goes through the
    /// Preprocessor (Invariant #3 — external data passes the Gate first),
    /// non-drop results land in memory, embedded, and indexed.
    /// Returns a one-line summary suitable for surfacing in the user-
    /// visible preamble.
    async fn execute_search(
        &self,
        connector_name: &str,
        query: &str,
        metadata: &Metadata,
    ) -> String {
        let Some(connector) = self.connectors.get(connector_name) else {
            return format!("(no such connector: {connector_name})");
        };
        if !connector.is_available() {
            return format!(
                "({connector_name} not configured — run `ai-assistant-backend connect {connector_name}`)"
            );
        }
        let results = match connector.search(query, 10).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, connector_name, query, "connector search failed");
                return format!("(search {connector_name} \"{query}\" failed: {e})");
            }
        };
        let total = results.len();
        let mut kept = 0usize;
        let mut dropped = 0usize;
        for r in results {
            // External data → PublicWeb provenance hint. Gmail content is
            // technically personal, but it came through an external API —
            // we want the Preprocessor to apply normal redaction without
            // dropping aggressively.
            let pp = match self
                .preprocessor
                .preprocess(&r.content, InputProvenance::PublicWeb)
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "preprocessor failed on connector result");
                    continue;
                }
            };
            if pp.tier == Tier::Drop {
                dropped += 1;
                let _ = self
                    .memory
                    .add_stub(&pp.output, pp.redaction_report.clone())
                    .await;
                continue;
            }
            // Tag with provenance so the audit trail is clear.
            let tags = vec![
                "connector".into(),
                format!("connector:{connector_name}"),
                format!("source:{}", r.source_id),
            ];
            let mut item_metadata = metadata.clone();
            // Stuff the source_url into freeform so the assistant can cite it.
            if let Some(url) = r.source_url {
                let mut extras = serde_json::Map::new();
                extras.insert("source_url".into(), serde_json::Value::String(url));
                extras.insert(
                    "source_id".into(),
                    serde_json::Value::String(r.source_id.clone()),
                );
                extras.insert(
                    "connector".into(),
                    serde_json::Value::String(connector_name.to_string()),
                );
                item_metadata.freeform = serde_json::Value::Object(extras);
            }
            let added = self
                .memory
                .add_with_reason(
                    &pp.output,
                    ItemKind::ConnectorFinding,
                    pp.importance,
                    pp.importance_reason.clone(),
                    Some(item_metadata),
                    pp.redaction_report.clone(),
                    tags,
                )
                .await;
            match added {
                Ok(sc) => {
                    kept += 1;
                    // Embed + index inline so the very next retrieve() call
                    // surfaces this result.
                    if let Ok(v) = self.embedder.embed(&pp.output).await {
                        if let Some(item) = self.memory.get(&sc.id).ok().flatten() {
                            let _ = self.memory.write_vector(&item, &v).await;
                            let _ = self.vector_index.upsert(&sc.id, v);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to add connector result to memory");
                }
            }
        }
        format!(
            "🔍 {connector_name}: searched \"{query}\" → {total} results, kept {kept}, dropped {dropped}"
        )
    }

    /// Scan the reply for FORGET: markers, act on any valid ones, and
    /// substitute confirmation text inline.
    async fn handle_forget_markers(&self, reply: &str) -> (String, Option<String>) {
        if !reply.contains(FORGET_MARKER) {
            return (reply.to_string(), None);
        }
        let mut last_forgotten: Option<String> = None;
        let mut out = String::with_capacity(reply.len());
        for line in reply.lines() {
            if let Some(rest) = line.trim().strip_prefix(FORGET_MARKER) {
                let id = rest.trim().split_whitespace().next().unwrap_or("");
                if id.is_empty() {
                    out.push_str(line);
                    out.push('\n');
                    continue;
                }
                match self.memory.forget(id).await {
                    Ok(true) => {
                        last_forgotten = Some(id.to_string());
                        self.vector_index.remove(id);
                        out.push_str(&format!(
                            "(forgot item {id} — body zeroed, vector removed, audit trail kept)\n"
                        ));
                    }
                    Ok(false) => {
                        out.push_str(&format!(
                            "(could not find item with id {id} to forget; nothing changed)\n"
                        ));
                    }
                    Err(e) => {
                        out.push_str(&format!(
                            "(forget for {id} failed: {e}; nothing changed)\n"
                        ));
                    }
                }
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        (out.trim_end().to_string(), last_forgotten)
    }
}

/// Walk an LLM reply, pull out CONFIG_REQUEST_FILE / CONFIG_BEGIN_OAUTH
/// marker lines, and return (stripped_text, requests).
fn strip_config_markers(text: &str) -> (String, Vec<shared::ConfigRequestKind>) {
    let mut out_lines: Vec<String> = Vec::with_capacity(text.lines().count());
    let mut requests: Vec<shared::ConfigRequestKind> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(CONFIG_REQUEST_FILE_MARKER) {
            let mut parts = rest.trim().splitn(2, char::is_whitespace);
            let connector = parts.next().unwrap_or("").trim().to_string();
            let filename = parts.next().unwrap_or("").trim().to_string();
            if !connector.is_empty() && !filename.is_empty() {
                requests.push(shared::ConfigRequestKind::RequestFile {
                    connector: connector.clone(),
                    filename: filename.clone(),
                    hint: format!(
                        "{connector} needs {filename}. Pick the file you downloaded \
                         from Google Cloud Console."
                    ),
                });
                continue; // strip the marker line
            }
        }
        if let Some(rest) = trimmed.strip_prefix(CONFIG_BEGIN_OAUTH_MARKER) {
            let connector = rest.trim().split_whitespace().next().unwrap_or("").to_string();
            if !connector.is_empty() {
                let scope = crate::connectors::scope_for(&connector)
                    .unwrap_or("(unknown-scope)")
                    .to_string();
                requests.push(shared::ConfigRequestKind::BeginOAuth {
                    connector,
                    scope,
                });
                continue;
            }
        }
        out_lines.push(line.to_string());
    }
    let stripped = out_lines.join("\n");
    let stripped = stripped.trim_end().to_string();
    (stripped, requests)
}

/// Parse SEARCH: markers from an LLM reply. Returns (connector_name,
/// query) pairs in the order they appeared. Tolerates extra whitespace and
/// missing args.
fn parse_search_markers(text: &str) -> Vec<(String, String)> {
    let mut out = vec![];
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix(SEARCH_MARKER) else { continue };
        let rest = rest.trim();
        if rest.is_empty() {
            continue;
        }
        let mut split = rest.splitn(2, char::is_whitespace);
        let connector = split.next().unwrap_or("").trim().to_string();
        let query = split.next().unwrap_or("").trim().to_string();
        if connector.is_empty() || query.is_empty() {
            continue;
        }
        out.push((connector, query));
    }
    out
}

const BOOTSTRAP_INTRO: &str = "\
Hi — I'm your personal assistant. I'm starting completely fresh: no memories of you yet, no notes, no preferences.

Here's how this works:
  • Hand me anything you want me to remember — paste an email, jot a note, drop a calendar entry, describe a document. If you don't ask a question, I'll treat it as data to keep.
  • Ask me anything about your life that I might know from what you've given me, or about the world. Try \"what should I be thinking about right now?\"
  • Tell me to forget things or to stop telling you about a topic — I'll save that as a preference.

Two important things I will never do:
  • Take any action in the outside world on your behalf — I only read in and respond out.
  • Store sensitive identifiers (2FA codes, reset links, full account numbers) — they get dropped or redacted before I see them.

Whenever you're ready, send me something.";

fn looks_like_question(text: &str) -> bool {
    text.contains('?')
        || text
            .split_whitespace()
            .next()
            .map(|w| {
                matches!(
                    w.to_lowercase().as_str(),
                    "what" | "when" | "where" | "who" | "why" | "how" | "did" | "do" | "can" | "should" | "is" | "are"
                )
            })
            .unwrap_or(false)
}

fn detect_preference(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let triggers = [
        "stop telling me about ",
        "don't tell me about ",
        "do not tell me about ",
        "i don't care about ",
        "ignore ",
        "never bring up ",
    ];
    for t in triggers {
        if let Some(i) = lower.find(t) {
            let tail = &text[i..];
            return Some(tail.trim().to_string());
        }
    }
    None
}

/// Parse READ_MANUAL markers from an LLM reply. Each `READ_MANUAL:
/// <section>` line becomes a section-name request; a bare `READ_MANUAL`
/// line on its own becomes an empty-string request (the loop interprets
/// that as "give me the TOC"). Returns request strings in document order.
fn parse_manual_markers(text: &str) -> Vec<String> {
    let mut out = vec![];
    for line in text.lines() {
        let trimmed = line.trim();
        // Match either "READ_MANUAL: <section>" or bare "READ_MANUAL".
        if let Some(rest) = trimmed.strip_prefix(READ_MANUAL_MARKER) {
            let after = rest.trim();
            if let Some(section) = after.strip_prefix(':') {
                out.push(section.trim().to_string());
            } else if after.is_empty() {
                // Bare marker: request the TOC.
                out.push(String::new());
            }
            // Otherwise: not a real marker (e.g. "READ_MANUALLY"); skip.
        }
    }
    out
}

/// Render the always-on manual pointer block. Includes the section TOC
/// so the assistant can pick a section in one shot without an extra
/// READ_MANUAL round-trip. Cheap (a few lines).
fn render_manual_pointer(manual: &Manual) -> String {
    let toc = manual.toc();
    let mut s = String::from(
        "SYSTEM MANUAL — a reference document is available. Use it any time you need to be \
         certain about marker syntax, procedural steps (especially connector setup), the \
         system's invariants, or to walk the user through something confidently. To consult, \
         include a line of EXACTLY this form anywhere in your reply (multiple allowed):\n\
         \n  READ_MANUAL: <section-name>          (fetch one section)\n\
           READ_MANUAL                          (returns the table of contents)\n\
         \n",
    );
    if !toc.is_empty() {
        s.push_str("Sections available: ");
        s.push_str(&toc.join(", "));
        s.push_str(".\n");
    }
    s.push_str(
        "Bound: 4 manual reads per turn total. Prefer reading the manual over guessing.\n\n",
    );
    s
}

/// Render the RECENTLY READ MANUAL SECTIONS block — populated only when
/// the assistant has fetched sections this turn, injected on the next
/// re-prompt iteration.
fn render_manual_excerpts(excerpts: &[(String, String)]) -> String {
    if excerpts.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "RECENTLY READ MANUAL SECTIONS (you fetched these earlier in this turn):\n\n",
    );
    for (name, body) in excerpts {
        s.push_str(&format!("### {name}\n{body}\n\n"));
    }
    s
}

fn build_prompt(
    metadata: &Metadata,
    user_text: &str,
    tier: Tier,
    retrieved: &[ScoredItem],
    preferences: &[crate::memory::PreferenceStatement],
    system_facts_block: &str,
    connectors_block: &str,
    manual_block: &str,
    manual_excerpts_block: &str,
) -> String {
    let mut buf = String::new();
    buf.push_str(
        "You are the user's personal AI assistant. You are like a trusted human assistant: \
         you have read what the user has given you, remember it, and answer plainly. \
         You CANNOT take any action in the outside world — no sending email, no booking, \
         no transactions. If asked, explain why and offer to help the user do it themselves.\n\
         \n\
         You DO have read-only access to the web via two tools:\n\
           • WebSearch — search the web for current information\n\
           • WebFetch — fetch a specific URL the user gave you\n\
         Use them freely whenever the question benefits from current information.\n\
         \n\
         MODEL ESCALATION: You are running as the standard model (typically Sonnet). \
         For the vast majority of conversation — recall, light reasoning, factual lookup, \
         friendly chat — answer directly. But if a question GENUINELY needs deeper \
         reasoning, you can hand off to the heavier model by outputting EXACTLY this format \
         as your entire reply — no preamble, no answer:\n\
         \n\
           ESCALATE_TO_OPUS: <one short sentence saying why>\n\
         \n\
         FORGET ACTION: If the user asks you to forget a specific memory item that you can \
         see in the MEMORY block below, you can tombstone it. Each memory line starts with \
         its id (`id=...`). To forget one, include a line of EXACTLY this form anywhere in \
         your reply:\n\
         \n\
           FORGET: <the-item-id>\n\
         \n\
         The backend will replace the marker line with a confirmation. Use this only when \
         the user clearly asks (\"forget that\", \"don't remember X\"); never on your own \
         initiative.\n\n",
    );
    buf.push_str(system_facts_block);
    buf.push('\n');
    if !manual_block.is_empty() {
        buf.push_str(manual_block);
    }
    if !connectors_block.is_empty() {
        buf.push_str(connectors_block);
    }
    if !manual_excerpts_block.is_empty() {
        buf.push_str(manual_excerpts_block);
    }
    buf.push_str("\nWhen asked about yourself, use the SYSTEM SELF-KNOWLEDGE block above for \
                  runtime facts (model names, intervals, paths) and READ_MANUAL: <section> for \
                  procedural / architectural detail. Be specific; do not invent details.\n\n");
    buf.push_str(&format!("Right now: {}\n", metadata.datetime_iso));
    if let Some(geo) = &metadata.geolocation {
        let label = geo
            .label
            .clone()
            .unwrap_or_else(|| format!("{:.2},{:.2}", geo.lat, geo.lon));
        buf.push_str(&format!("Location: {label} ({:.4}, {:.4})\n", geo.lat, geo.lon));
    }
    buf.push('\n');

    if !preferences.is_empty() {
        buf.push_str("USER PREFERENCES (apply when relevant):\n");
        for p in preferences {
            buf.push_str(&format!(
                "  - [{}] {}\n",
                p.created_at.format("%Y-%m-%d"),
                p.text
            ));
        }
        buf.push('\n');
    }

    if !retrieved.is_empty() {
        buf.push_str("MEMORY (top hybrid-retrieved items for this turn — scored by relevance + recency + importance):\n");
        for s in retrieved {
            buf.push_str(&render_scored(s));
        }
        buf.push('\n');
    }

    let tier_note = match tier {
        Tier::Pass => "",
        Tier::Redact => "(Note: the user's message was redacted by the Preprocessor — dangerous identifiers replaced with [bracketed redactions]. Reason about it as-is.)\n",
        Tier::Drop => "(Note: this turn was Tier 1 — should not be visible here. Bug if you see it.)\n",
    };
    buf.push_str(tier_note);

    buf.push_str("USER MESSAGE:\n");
    buf.push_str(user_text);
    buf.push_str("\n\nRespond directly. If the message looks like data the user wants you to remember rather than a question, acknowledge briefly and confirm what you noted. If it's a question, answer it using memory above when possible. Be concise.\n");
    buf
}

fn render_scored(s: &ScoredItem) -> String {
    let when = s.item.sidecar.created_at.format("%Y-%m-%d %H:%M");
    let kind = format!("{:?}", s.item.sidecar.kind);
    let body = if s.item.body.len() > 800 {
        format!("{}…", &s.item.body[..800])
    } else {
        s.item.body.clone()
    };
    format!(
        "  - [id={}, when={when}, kind={kind}, score={:.2}, rel={:.2}, recency={:.2}, importance={:.2}] {body}\n",
        s.item.sidecar.id, s.final_score, s.relevance, s.recency, s.importance
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::MockLlmClient;
    use crate::connectors::mock::MockConnector;
    use crate::connectors::{ConnectorRegistry, RawConnectorResult};
    use tempfile::TempDir;

    async fn setup() -> (TempDir, Assistant) {
        let (td, a, _mock) = setup_with_mock().await;
        (td, a)
    }

    async fn setup_with_mock() -> (TempDir, Assistant, Arc<MockLlmClient>) {
        let td = TempDir::new().unwrap();
        let store = Arc::new(MemoryStore::open(td.path().to_path_buf()).await.unwrap());
        let llm = MockLlmClient::new();
        let assistant = Assistant::new(llm.clone(), store);
        (td, assistant, llm)
    }

    fn meta() -> Metadata {
        Metadata {
            datetime_iso: "2026-05-25T14:03:00-05:00".to_string(),
            geolocation: None,
            freeform: serde_json::Value::Null,
        }
    }

    fn pp_pass(text: &str) -> PreprocessorResult {
        PreprocessorResult {
            tier: Tier::Pass,
            output: text.to_string(),
            redaction_report: "".into(),
            importance: 0.5,
            importance_reason: None,
        }
    }

    #[tokio::test]
    async fn bootstrap_intro_when_empty() {
        let (_td, a) = setup().await;
        let i = a.introduction().await;
        assert!(i.contains("starting completely fresh"));
    }

    #[tokio::test]
    async fn intro_changes_after_first_item() {
        let (_td, a) = setup().await;
        a.respond(&pp_pass("Bought milk"), &meta(), false).await.unwrap();
        let i = a.introduction().await;
        assert!(i.contains("Welcome back"));
    }

    #[tokio::test]
    async fn preprocessor_importance_is_used() {
        let (_td, a) = setup().await;
        let pp = PreprocessorResult {
            tier: Tier::Pass,
            output: "important calendar item".into(),
            redaction_report: "".into(),
            importance: 0.91,
            importance_reason: Some("commitment with deadline".into()),
        };
        a.respond(&pp, &meta(), false).await.unwrap();
        let recent = a.memory.recent(5).unwrap();
        let user_item = recent
            .iter()
            .find(|i| {
                matches!(
                    i.sidecar.kind,
                    ItemKind::UserMessage | ItemKind::Ingestion
                ) && i.body.contains("calendar")
            })
            .expect("item not found");
        assert!((user_item.sidecar.importance - 0.91).abs() < 1e-5);
        assert_eq!(
            user_item.sidecar.importance_reason.as_deref(),
            Some("commitment with deadline")
        );
    }

    #[tokio::test]
    async fn questions_are_classified_as_user_messages() {
        let (_td, a) = setup().await;
        a.respond(
            &pp_pass("What should I be thinking about right now?"),
            &meta(),
            false,
        )
        .await
        .unwrap();
        let recent = a.memory.recent(5).unwrap();
        assert!(recent.iter().any(|i| i.sidecar.kind == ItemKind::UserMessage));
    }

    #[tokio::test]
    async fn preference_detected_and_stored() {
        let (_td, a) = setup().await;
        a.respond(&pp_pass("Please stop telling me about crypto news."), &meta(), false)
            .await
            .unwrap();
        let p = a.memory.preferences().await;
        assert_eq!(p.statements.len(), 1);
        assert!(p.statements[0].text.to_lowercase().contains("crypto"));
    }

    #[tokio::test]
    async fn assistant_passes_websearch_and_webfetch_to_llm() {
        let (_td, a, mock) = setup_with_mock().await;
        a.respond(&pp_pass("What's the weather in Lafayette today?"), &meta(), false)
            .await
            .unwrap();
        let calls = mock.calls();
        let turn = calls
            .iter()
            .find(|c| c.prompt.contains("weather in Lafayette"))
            .expect("expected an assistant LLM call");
        assert!(turn.allowed_tools.contains(&"WebSearch".to_string()));
        assert!(turn.allowed_tools.contains(&"WebFetch".to_string()));
    }

    #[tokio::test]
    async fn sonnet_self_escalation_triggers_opus_re_run() {
        let td = TempDir::new().unwrap();
        let store = Arc::new(MemoryStore::open(td.path().to_path_buf()).await.unwrap());
        let mock = MockLlmClient::new();
        let mut assistant = Assistant::new(mock.clone(), store);
        assistant.model = Some("claude-sonnet-4-6".into());
        assistant.escalation_model = Some("claude-opus-4-7".into());

        mock.respond_when(
            "USER MESSAGE",
            "ESCALATE_TO_OPUS: needs deep reasoning about a non-obvious tradeoff",
        );
        let outcome = assistant
            .respond(&pp_pass("Should I use eventual consistency for X?"), &meta(), false)
            .await
            .unwrap();
        assert!(outcome.escalated, "should have escalated");
        assert_eq!(outcome.model_used, "claude-opus-4-7");
        assert!(outcome.escalation_reason.is_some());
    }

    #[tokio::test]
    async fn force_opus_routes_directly_to_escalation_model() {
        let td = TempDir::new().unwrap();
        let store = Arc::new(MemoryStore::open(td.path().to_path_buf()).await.unwrap());
        let mock = MockLlmClient::new();
        let mut assistant = Assistant::new(mock.clone(), store);
        assistant.model = Some("claude-sonnet-4-6".into());
        assistant.escalation_model = Some("claude-opus-4-7".into());
        let outcome = assistant.respond(&pp_pass("anything"), &meta(), true).await.unwrap();
        assert!(!outcome.escalated);
        assert_eq!(outcome.model_used, "claude-opus-4-7");
    }

    #[tokio::test]
    async fn forget_marker_tombstones_target_item() {
        let (_td, a, mock) = setup_with_mock().await;
        let sc = a
            .memory
            .add("private note to forget", ItemKind::Ingestion, 0.5, None, "".into(), vec![])
            .await
            .unwrap();
        let id = sc.id.clone();
        mock.respond_when("USER MESSAGE", &format!("Sure, forgetting that.\nFORGET: {id}\n"));

        let outcome = a
            .respond(&pp_pass("forget the private note please"), &meta(), false)
            .await
            .unwrap();
        assert_eq!(outcome.forgotten_item_id.as_deref(), Some(id.as_str()));
        let item = a.memory.get(&id).unwrap().unwrap();
        assert_eq!(item.sidecar.kind, ItemKind::ForgottenStub);
        assert!(item.body.starts_with("[forgotten"));
    }

    #[tokio::test]
    async fn assistant_prompt_mentions_web_capabilities_and_forget_marker() {
        let (_td, a, mock) = setup_with_mock().await;
        a.respond(&pp_pass("hello"), &meta(), false).await.unwrap();
        let calls = mock.calls();
        let turn = calls.iter().find(|c| c.prompt.contains("USER MESSAGE")).unwrap();
        assert!(turn.prompt.contains("WebSearch"));
        assert!(turn.prompt.contains("FORGET:"));
    }

    #[tokio::test]
    async fn vector_is_written_on_respond() {
        let (_td, a) = setup().await;
        a.respond(&pp_pass("learn this fact"), &meta(), false).await.unwrap();
        let recent = a.memory.recent(5).unwrap();
        let item = recent
            .iter()
            .find(|i| i.body.contains("learn this fact"))
            .expect("missing item");
        assert!(
            item.vector_path().exists(),
            "expected .vec sidecar at {:?}",
            item.vector_path()
        );
    }

    // --- Connector-pathway tests ---

    #[test]
    fn parses_search_markers() {
        let s = "Let me look this up.\nSEARCH: gmail from:dr.patel implant\nsome prose\nSEARCH: calendar dentist 2024";
        let p = parse_search_markers(s);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0], ("gmail".to_string(), "from:dr.patel implant".to_string()));
        assert_eq!(p[1], ("calendar".to_string(), "dentist 2024".to_string()));
    }

    #[test]
    fn ignores_search_marker_without_query() {
        let s = "SEARCH: gmail\nSEARCH: \n";
        let p = parse_search_markers(s);
        assert!(p.is_empty());
    }

    #[tokio::test]
    async fn assistant_prompt_includes_connectors_block_when_registered() {
        let (_td, a_base, mock) = setup_with_mock().await;
        let m: Arc<dyn crate::connectors::Connector> = MockConnector::new("gmail");
        let registry = Arc::new(ConnectorRegistry::new(vec![m]));
        let a = a_base.with_connectors(registry);

        a.respond(&pp_pass("hi"), &meta(), false).await.unwrap();
        let calls = mock.calls();
        let assistant_call = calls
            .iter()
            .find(|c| c.prompt.contains("USER MESSAGE"))
            .unwrap();
        assert!(assistant_call.prompt.contains("EXTERNAL SEARCH"));
        assert!(assistant_call.prompt.contains("gmail"));
    }

    #[tokio::test]
    async fn search_marker_executes_and_ingests_results() {
        let (_td, a_base, mock) = setup_with_mock().await;
        // MockConnector returns one canned result for the query "implant".
        let m = MockConnector::new("gmail");
        m.respond_when(
            "implant",
            vec![RawConnectorResult {
                source_id: "gmail:abc123".into(),
                source_url: Some("https://mail.google.com/abc123".into()),
                content: "From: dr.patel@example.com\nSubject: implant\n\nThe implant looks good, follow up in 6 months.".into(),
                at: None,
            }],
        );
        let m_dyn: Arc<dyn crate::connectors::Connector> = m.clone();
        let registry = Arc::new(ConnectorRegistry::new(vec![m_dyn]));
        let a = a_base.with_connectors(registry);

        // Matchers are checked in registration order; first hit wins.
        //
        // On the second-pass assistant prompt, the retrieved memory block
        // will include a line containing "kind=ConnectorFinding" (the
        // ingested gmail result). That string never appears in the
        // Preprocessor's prompts or the first-pass assistant prompt — so
        // matching on it cleanly distinguishes the second assistant pass.
        mock.respond_when(
            "kind=ConnectorFinding",
            "Dr. Patel said the implant looks good and to follow up in 6 months.",
        );
        mock.respond_when(
            "EXTERNAL SEARCH",
            "Let me check your email.\nSEARCH: gmail implant\n",
        );

        let outcome = a
            .respond(&pp_pass("what did Dr. Patel say about the implant?"), &meta(), false)
            .await
            .unwrap();

        // Search log should record exactly one search.
        assert_eq!(outcome.search_log.len(), 1, "search_log = {:?}", outcome.search_log);
        assert!(outcome.search_log[0].contains("gmail"));
        assert!(outcome.search_log[0].contains("implant"));

        // Connector was called once.
        assert_eq!(m.calls().len(), 1);
        assert_eq!(m.calls()[0].0, "implant");

        // Result was ingested as a ConnectorFinding.
        let all = a.memory.scan_all().unwrap();
        let finding = all
            .iter()
            .find(|i| i.sidecar.kind == ItemKind::ConnectorFinding)
            .expect("no ConnectorFinding stored");
        assert!(finding.body.contains("implant"));
        assert!(finding.sidecar.tags.iter().any(|t| t == "connector:gmail"));
        assert!(finding
            .sidecar
            .tags
            .iter()
            .any(|t| t == "source:gmail:abc123"));

        // Final reply incorporates the search result.
        assert!(outcome.text.contains("implant") || outcome.text.contains("Patel"),
                "final reply should reflect what we found: {}", outcome.text);
    }

    #[tokio::test]
    async fn unknown_connector_in_search_marker_does_not_blow_up() {
        let (_td, a_base, mock) = setup_with_mock().await;
        let m: Arc<dyn crate::connectors::Connector> = MockConnector::new("gmail");
        let registry = Arc::new(ConnectorRegistry::new(vec![m]));
        let mut a = a_base.with_connectors(registry);
        // Cap rounds at 1 so the mock-always-says-SEARCH loop terminates
        // after one iteration. Without this the mock would emit SEARCH on
        // every iteration up to max_search_rounds.
        a.max_search_rounds = 1;

        mock.respond_when(
            "EXTERNAL SEARCH",
            "SEARCH: notreal anything goes here\n",
        );

        let outcome = a
            .respond(&pp_pass("search please"), &meta(), false)
            .await
            .unwrap();
        assert_eq!(outcome.search_log.len(), 1);
        assert!(outcome.search_log[0].contains("no such connector"));
    }

    #[tokio::test]
    async fn search_rounds_are_bounded() {
        let (_td, a_base, mock) = setup_with_mock().await;
        let m: Arc<dyn crate::connectors::Connector> = MockConnector::new("gmail");
        let registry = Arc::new(ConnectorRegistry::new(vec![m]));
        let mut a = a_base.with_connectors(registry);
        a.max_search_rounds = 1; // tighter for the test

        // The mock returns a SEARCH marker on every assistant call. Without
        // the depth bound, this would loop forever; with depth = 1, it
        // executes once and then accepts whatever the second call returns.
        mock.respond_when(
            "USER MESSAGE",
            "SEARCH: gmail anything\n",
        );

        let outcome = a
            .respond(&pp_pass("question"), &meta(), false)
            .await
            .unwrap();
        // Exactly one search round.
        assert_eq!(outcome.search_log.len(), 1);
    }

    // --- READ_MANUAL marker tests ---

    #[test]
    fn parses_read_manual_markers() {
        let s = "Let me check.\nREAD_MANUAL: invariants\nsome prose\nREAD_MANUAL\n";
        let p = parse_manual_markers(s);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0], "invariants");
        assert_eq!(p[1], ""); // bare marker = TOC request
    }

    #[test]
    fn ignores_lookalike_markers() {
        let s = "READ_MANUALLY check it\nread_manual: lowercase no\n";
        let p = parse_manual_markers(s);
        assert!(p.is_empty(), "got {p:?}");
    }

    #[tokio::test]
    async fn assistant_prompt_includes_manual_pointer_and_toc() {
        let (_td, a, mock) = setup_with_mock().await;
        a.respond(&pp_pass("hi"), &meta(), false).await.unwrap();
        let calls = mock.calls();
        let turn = calls.iter().find(|c| c.prompt.contains("USER MESSAGE")).unwrap();
        assert!(turn.prompt.contains("SYSTEM MANUAL"));
        assert!(turn.prompt.contains("READ_MANUAL"));
        // TOC includes section names from the embedded default manual.
        assert!(
            turn.prompt.contains("invariants") || turn.prompt.contains("markers"),
            "expected manual TOC entries in prompt"
        );
    }

    #[tokio::test]
    async fn read_manual_marker_fetches_section_and_re_prompts() {
        let (_td, a_base, mock) = setup_with_mock().await;
        let mut a = a_base;
        a.max_manual_reads = 2;

        // On the second pass (after manual section landed in context),
        // the prompt will contain the section body text. Match on a
        // distinctive line from the embedded default's "markers" section.
        mock.respond_when(
            "RECENTLY READ MANUAL SECTIONS",
            "Per the manual, FORGET is for explicit user requests.",
        );
        // First pass: emit READ_MANUAL marker for the "markers" section.
        mock.respond_when(
            "USER MESSAGE",
            "Let me check.\nREAD_MANUAL: markers\n",
        );

        let outcome = a
            .respond(&pp_pass("how do I forget something?"), &meta(), false)
            .await
            .unwrap();

        assert_eq!(outcome.manual_reads.len(), 1);
        assert_eq!(outcome.manual_reads[0], "markers");
        assert!(outcome.text.contains("FORGET"));
    }

    #[tokio::test]
    async fn read_manual_with_no_section_returns_toc() {
        let (_td, a_base, mock) = setup_with_mock().await;
        let mut a = a_base;
        a.max_manual_reads = 1;

        mock.respond_when(
            "RECENTLY READ MANUAL SECTIONS",
            "Got the TOC. Done.",
        );
        mock.respond_when(
            "USER MESSAGE",
            "READ_MANUAL\n",
        );

        let outcome = a
            .respond(&pp_pass("what sections are in the manual?"), &meta(), false)
            .await
            .unwrap();

        assert_eq!(outcome.manual_reads.len(), 1);
        assert_eq!(outcome.manual_reads[0], "table-of-contents");
    }

    #[tokio::test]
    async fn manual_reads_are_bounded() {
        let (_td, a_base, mock) = setup_with_mock().await;
        let mut a = a_base;
        a.max_manual_reads = 1;

        // Mock always emits a manual marker — without the bound this
        // would loop until something else stopped it.
        mock.respond_when(
            "USER MESSAGE",
            "READ_MANUAL: invariants\n",
        );

        let outcome = a
            .respond(&pp_pass("question"), &meta(), false)
            .await
            .unwrap();
        assert_eq!(outcome.manual_reads.len(), 1);
    }
}
