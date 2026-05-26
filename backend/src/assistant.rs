//! The Assistant Core. Receives only sanitized content (or HAZMAT-bypassed
//! content from the explicit user opt-in).
//!
//! Pipeline per turn:
//!  1. Persist the user message (sanitized output + Preprocessor importance).
//!  2. Embed the message, upsert into the vector index.
//!  3. Hybrid retrieve: vector + keyword + recency + importance.
//!  4. Build prompt with persona, metadata, retrieved memory, preferences.
//!  5. Call Sonnet. Handle ESCALATE_TO_OPUS or FORGET: markers.
//!  6. Persist assistant note.

use crate::claude::{LlmClient, LlmOptions};
use crate::embedder::Embedder;
use crate::memory::{ItemKind, MemoryStore};
use crate::preprocessor::PreprocessorResult;
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
    pub model: Option<String>,
    /// Heavier model Sonnet hands off to when it judges a question needs
    /// deeper reasoning, or when the user sets `force_opus` on the message.
    pub escalation_model: Option<String>,
    pub retrieval_weights: RetrievalWeights,
    pub system_facts: Arc<crate::self_knowledge::SystemFacts>,
}

/// Marker Sonnet emits as the FIRST line of its reply to hand off to Opus.
pub const ESCALATION_MARKER: &str = "ESCALATE_TO_OPUS:";

/// Marker the assistant emits when the user asks to forget a specific item.
/// The text after the colon is the item id (matched against the IDs the
/// assistant saw in its memory block).
pub const FORGET_MARKER: &str = "FORGET:";

#[derive(Debug, Clone)]
pub struct RespondOutcome {
    pub text: String,
    pub model_used: String,
    pub escalated: bool,
    pub escalation_reason: Option<String>,
    /// If the LLM emitted a FORGET marker that the backend acted on, this
    /// holds the item id and a human-readable confirmation. The
    /// confirmation has already been substituted into `text`; this field is
    /// for tests / audit.
    pub forgotten_item_id: Option<String>,
}

impl Assistant {
    /// Test-only constructor with mock-friendly defaults.
    pub fn new(llm: Arc<dyn LlmClient>, memory: Arc<MemoryStore>) -> Self {
        let embedder: Arc<dyn Embedder> = Arc::new(crate::embedder::MockEmbedder::new());
        let vector_index = Arc::new(
            VectorIndex::open(memory.root(), embedder.model_name(), embedder.dimension())
                .expect("open vector index"),
        );
        Self {
            llm,
            memory,
            embedder,
            vector_index,
            model: None,
            escalation_model: None,
            retrieval_weights: RetrievalWeights::default(),
            system_facts: Arc::new(crate::self_knowledge::SystemFacts::placeholder()),
        }
    }

    /// Full constructor used by `build_app`.
    pub fn build(
        llm: Arc<dyn LlmClient>,
        memory: Arc<MemoryStore>,
        embedder: Arc<dyn Embedder>,
        vector_index: Arc<VectorIndex>,
        model: Option<String>,
        escalation_model: Option<String>,
        retrieval_weights: RetrievalWeights,
        system_facts: Arc<crate::self_knowledge::SystemFacts>,
    ) -> Self {
        Self {
            llm,
            memory,
            embedder,
            vector_index,
            model,
            escalation_model,
            retrieval_weights,
            system_facts,
        }
    }

    /// Back-compat with old tests that took just (llm, memory, model, facts).
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

    /// Back-compat with old tests.
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
        // HAZMAT bypasses get a fixed importance (0.8) since no Preprocessor
        // ran. Otherwise use the Preprocessor's score.
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

        // 2. Embed + upsert (best-effort; Indexer catches up if this fails).
        if let Ok(vec) = self.embedder.embed(&preprocessed.output).await {
            if let Some(item) = self.memory.get(&sidecar.id).ok().flatten() {
                let _ = self.memory.write_vector(&item, &vec).await;
                let _ = self.vector_index.upsert(&sidecar.id, vec);
            }
        }

        // 3. Hybrid retrieve.
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
        let prompt = build_prompt(
            metadata,
            &preprocessed.output,
            preprocessed.tier,
            &retrieved,
            &prefs.statements,
            &facts_block,
        );

        // 4. Call LLM with model routing.
        let primary_model = if force_opus {
            self.escalation_model.clone().or_else(|| self.model.clone())
        } else {
            self.model.clone()
        };

        let opts = LlmOptions {
            allowed_tools: vec!["WebSearch".into(), "WebFetch".into()],
            model: primary_model.clone(),
            ..Default::default()
        };
        let raw_reply = self.llm.oneshot(&prompt, opts).await?;
        let raw_reply = raw_reply.trim().to_string();

        // 5a. Handle ESCALATE_TO_OPUS.
        let (after_escalation_text, model_used, escalated, escalation_reason) =
            if !force_opus && raw_reply.starts_with(ESCALATION_MARKER) {
                let reason = raw_reply
                    .trim_start_matches(ESCALATION_MARKER)
                    .trim()
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                let escalation_model = self
                    .escalation_model
                    .clone()
                    .or_else(|| self.model.clone());
                let opts2 = LlmOptions {
                    allowed_tools: vec!["WebSearch".into(), "WebFetch".into()],
                    model: escalation_model.clone(),
                    ..Default::default()
                };
                let opus_reply = self.llm.oneshot(&prompt, opts2).await?;
                (
                    opus_reply.trim().to_string(),
                    escalation_model.unwrap_or_default(),
                    true,
                    if reason.is_empty() { None } else { Some(reason) },
                )
            } else {
                (
                    raw_reply,
                    primary_model.clone().unwrap_or_default(),
                    false,
                    None,
                )
            };

        // 5b. Handle FORGET markers anywhere in the reply.
        let (final_text, forgotten_item_id) = self.handle_forget_markers(&after_escalation_text).await;

        // 6. Persist assistant note.
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

        // Preference detection — kept as a cheap regex heuristic. Anything
        // smarter belongs in the Preprocessor.
        if let Some(pref) = detect_preference(&preprocessed.output) {
            self.memory.add_preference(&pref).await.ok();
        }

        Ok(RespondOutcome {
            text: final_text,
            model_used,
            escalated,
            escalation_reason,
            forgotten_item_id,
        })
    }

    /// Scan the reply for FORGET: markers, act on any valid ones, and
    /// substitute confirmation text inline. Multiple markers are supported.
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

fn build_prompt(
    metadata: &Metadata,
    user_text: &str,
    tier: Tier,
    retrieved: &[ScoredItem],
    preferences: &[crate::memory::PreferenceStatement],
    system_facts_block: &str,
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
    buf.push_str("\nWhen asked about yourself, use the SYSTEM SELF-KNOWLEDGE block above for \
                  runtime facts AND the SelfKnowledge memory items (visible in the memory block \
                  below) for rationale and architecture. Be specific; do not invent details.\n\n");
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
        // Add an item we will then ask to forget.
        let sc = a
            .memory
            .add("private note to forget", ItemKind::Ingestion, 0.5, None, "".into(), vec![])
            .await
            .unwrap();
        let id = sc.id.clone();
        // Mock returns a FORGET marker for any USER MESSAGE prompt.
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
        let outcome = a.respond(&pp_pass("learn this fact"), &meta(), false).await;
        outcome.unwrap();
        let recent = a.memory.recent(5).unwrap();
        let item = recent
            .iter()
            .find(|i| i.body.contains("learn this fact"))
            .expect("missing item");
        // The Embedder ran inline → .vec sidecar should exist immediately.
        assert!(
            item.vector_path().exists(),
            "expected .vec sidecar at {:?}",
            item.vector_path()
        );
    }
}
