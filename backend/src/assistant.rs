//! The Assistant Core. Receives only sanitized content.
//!
//! Pipeline per turn:
//!  1. Sanitize the incoming message (caller already did this).
//!  2. Pull a window of recent memory + a keyword-search slice.
//!  3. Render a prompt with persona, metadata, memory, preferences, message.
//!  4. Call Claude (one-shot for v1; streaming chunked from the WS handler).
//!  5. Write a memory item for the incoming sanitized text (the assistant's
//!     own reply is also stored, as a lightweight assistant note).

use crate::claude::{LlmClient, LlmOptions};
use crate::memory::{ItemKind, MemoryItem, MemoryStore};
use crate::sanitizer::SanitizerResult;
use anyhow::Result;
use shared::{Metadata, Tier};
use std::sync::Arc;

pub struct Assistant {
    pub llm: Arc<dyn LlmClient>,
    pub memory: Arc<MemoryStore>,
    pub model: Option<String>,
    /// Heavier model Sonnet hands off to when it judges a question needs
    /// deeper reasoning, or when the user sets `force_opus` on the message.
    pub escalation_model: Option<String>,
    pub system_facts: Arc<crate::self_knowledge::SystemFacts>,
}

/// Marker Sonnet emits as the FIRST line of its reply to hand off to Opus.
/// Backend detects the prefix, discards the rest of Sonnet's text, and
/// re-runs the same prompt against the escalation model.
pub const ESCALATION_MARKER: &str = "ESCALATE_TO_OPUS:";

#[derive(Debug, Clone)]
pub struct RespondOutcome {
    pub text: String,
    /// Which model produced the final text (post-escalation if any).
    pub model_used: String,
    /// True if Sonnet handed off, false if the configured primary model
    /// answered directly (whether Sonnet, Opus-via-force, or other).
    pub escalated: bool,
    /// Short reason string Sonnet provided after the marker, if any.
    pub escalation_reason: Option<String>,
}

impl Assistant {
    pub fn new(llm: Arc<dyn LlmClient>, memory: Arc<MemoryStore>) -> Self {
        Self {
            llm,
            memory,
            model: None,
            escalation_model: None,
            system_facts: Arc::new(crate::self_knowledge::SystemFacts::placeholder()),
        }
    }

    pub fn with_model_and_facts(
        llm: Arc<dyn LlmClient>,
        memory: Arc<MemoryStore>,
        model: Option<String>,
        system_facts: Arc<crate::self_knowledge::SystemFacts>,
    ) -> Self {
        Self {
            llm,
            memory,
            model,
            escalation_model: None,
            system_facts,
        }
    }

    pub fn with_models_and_facts(
        llm: Arc<dyn LlmClient>,
        memory: Arc<MemoryStore>,
        model: Option<String>,
        escalation_model: Option<String>,
        system_facts: Arc<crate::self_knowledge::SystemFacts>,
    ) -> Self {
        Self {
            llm,
            memory,
            model,
            escalation_model,
            system_facts,
        }
    }

    /// Produce the introduction the client sees on connect. Pure function so
    /// it's easy to test and to render the same text in different surfaces.
    pub async fn introduction(&self) -> String {
        let prefs = self.memory.preferences().await;
        // Bootstrap state = no user data. SelfKnowledge items are seeded by
        // the system on every startup, so they don't count.
        let user_items: usize = self
            .memory
            .scan_all()
            .unwrap_or_default()
            .iter()
            .filter(|i| i.sidecar.kind != crate::memory::ItemKind::SelfKnowledge)
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

    /// Run a single conversational turn. Returns the assistant's full reply
    /// text plus metadata about which model produced it and whether it was
    /// escalated. Persists both the user message and the assistant's note.
    pub async fn respond(
        &self,
        sanitized: &SanitizerResult,
        metadata: &Metadata,
        force_opus: bool,
    ) -> Result<RespondOutcome> {
        // Persist user-side first (even if Tier::Drop, we already wrote a
        // stub from the WS handler — that path doesn't reach here).
        let user_importance = importance_hint(&sanitized.output);
        let user_kind = if looks_like_question(&sanitized.output) {
            ItemKind::UserMessage
        } else {
            ItemKind::Ingestion
        };
        let mut tags = tag_guess(&sanitized.output);
        let is_hazmat = sanitized.redaction_report.contains("HAZMAT BYPASS");
        if is_hazmat {
            tags.push("hazmat".to_string());
        }
        self.memory
            .add(
                &sanitized.output,
                user_kind,
                // Hazmat items get higher importance so they're easier to
                // find in a later audit ("show me everything I bypassed
                // the sanitizer for").
                if is_hazmat { 0.8 } else { user_importance },
                Some(metadata.clone()),
                sanitized.redaction_report.clone(),
                tags,
            )
            .await?;

        // Pull context.
        let recent = self.memory.recent(20).unwrap_or_default();
        let keyword_hits = self
            .memory
            .search(&sanitized.output, 8)
            .unwrap_or_default();
        let prefs = self.memory.preferences().await;

        let item_count = self.memory.stats().get("total").copied().unwrap_or(0);
        let facts_block = self.system_facts.render_prompt_block(item_count);
        let prompt = build_prompt(
            metadata,
            &sanitized.output,
            sanitized.tier,
            &recent,
            &keyword_hits,
            &prefs.statements,
            &facts_block,
        );

        // The assistant can use web tools when answering questions about the
        // outside world — they're read-only and consistent with the diode
        // invariant (we fetch in, we never push out).
        //
        // Model routing:
        //   force_opus=true → straight to escalation model, no Sonnet pre-pass.
        //   force_opus=false → primary model (Sonnet); if it self-escalates
        //                       via ESCALATE_TO_OPUS, we re-run with the
        //                       escalation model.
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

        // Check for self-escalation (only when not already forced).
        let (final_text, model_used, escalated, reason) = if !force_opus
            && raw_reply.starts_with(ESCALATION_MARKER)
        {
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
                primary_model.unwrap_or_default(),
                false,
                None,
            )
        };

        // Persist assistant note. Tag escalations and Opus-forced turns so
        // the audit trail makes routing decisions visible.
        let mut note_tags = vec!["assistant".to_string()];
        if escalated {
            note_tags.push("escalated".into());
        }
        if force_opus {
            note_tags.push("force-opus".into());
        }
        let note_body = if escalated {
            format!(
                "(assistant reply via {model_used}, escalated from {}{}) {final_text}",
                self.model.clone().unwrap_or_else(|| "primary".into()),
                reason
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

        // Detect preference-shaped statements ("stop telling me…", "don't
        // tell me about…", "ignore X"). Cheap heuristic — Curator can
        // refine later.
        if let Some(pref) = detect_preference(&sanitized.output) {
            self.memory.add_preference(&pref).await.ok();
        }

        Ok(RespondOutcome {
            text: final_text,
            model_used,
            escalated,
            escalation_reason: reason,
        })
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

fn importance_hint(text: &str) -> f32 {
    // Trivial heuristic: longer + question-shaped messages score higher.
    // Curator can re-score later.
    let mut score: f32 = 0.3;
    if looks_like_question(text) {
        score += 0.2;
    }
    if text.len() > 400 {
        score += 0.2;
    }
    if text.len() > 2000 {
        score += 0.1;
    }
    score.min(0.95)
}

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

fn tag_guess(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut tags = Vec::new();
    for (needle, tag) in [
        ("appointment", "calendar"),
        ("meeting", "calendar"),
        ("flight", "travel"),
        ("hotel", "travel"),
        ("trip", "travel"),
        ("receipt", "finance"),
        ("invoice", "finance"),
        ("doctor", "health"),
        ("dentist", "health"),
        ("birthday", "people"),
        ("kids", "family"),
        ("wife", "family"),
        ("husband", "family"),
    ] {
        if lower.contains(needle) {
            tags.push(tag.to_string());
        }
    }
    tags.sort();
    tags.dedup();
    tags
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
    recent: &[MemoryItem],
    keyword_hits: &[MemoryItem],
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
         Use them freely whenever the question benefits from current information \
         (news, weather, prices, events, recent changes, anything time-sensitive). \
         Reading from the web is consistent with the diode invariant — you fetch \
         in, you never push out.\n\
         \n\
         MODEL ESCALATION: You are running as the standard model (typically Sonnet). \
         For the vast majority of conversation — recall, light reasoning, factual lookup, \
         friendly chat — answer directly. But if a question GENUINELY needs deeper \
         reasoning than you can reliably provide (e.g. subtle architectural trade-offs, \
         multi-step formal reasoning, careful security analysis, untangling a confusing \
         situation with many constraints, the user explicitly asking for Opus), you can \
         hand off to the heavier model. To do so, output EXACTLY this format as your \
         entire reply — no preamble, no answer:\n\
         \n\
           ESCALATE_TO_OPUS: <one short sentence saying why you're escalating>\n\
         \n\
         The backend will detect the marker, run the same prompt against Opus, and the \
         user will receive Opus's answer directly. Do NOT escalate routinely — only when \
         your honest assessment is that Opus would meaningfully outperform you. If the \
         user just asks for a fact, recalls a memory, or wants a casual response, answer \
         yourself.\n\n",
    );
    buf.push_str(system_facts_block);
    buf.push_str("\nWhen asked about yourself — what model you use, how you work, why you made \
                  a design choice — use the SYSTEM SELF-KNOWLEDGE block above for runtime facts \
                  AND the SelfKnowledge memory items (visible in the memory blocks below) for \
                  rationale and architecture. Be specific and accurate; do not invent details.\n\n");
    buf.push_str(&format!(
        "Right now: {}\n",
        metadata.datetime_iso
    ));
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

    if !keyword_hits.is_empty() {
        buf.push_str("MEMORY (keyword-relevant items):\n");
        for it in keyword_hits {
            buf.push_str(&render_item(it));
        }
        buf.push('\n');
    }

    if !recent.is_empty() {
        buf.push_str("MEMORY (most recent items, newest first):\n");
        for it in recent.iter().take(10) {
            buf.push_str(&render_item(it));
        }
        buf.push('\n');
    }

    let tier_note = match tier {
        Tier::Pass => "",
        Tier::Redact => "(Note: the user's message was redacted by the Gate — dangerous identifiers replaced with [bracketed redactions]. Reason about it as-is.)\n",
        Tier::Drop => "(Note: this turn was Tier 1 — should not be visible here. Bug if you see it.)\n",
    };
    buf.push_str(tier_note);

    buf.push_str("USER MESSAGE:\n");
    buf.push_str(user_text);
    buf.push_str("\n\nRespond directly. If the message looks like data the user wants you to remember rather than a question, acknowledge briefly and confirm what you noted. If it's a question, answer it using memory above when possible. Be concise.\n");
    buf
}

fn render_item(it: &MemoryItem) -> String {
    let when = it.sidecar.created_at.format("%Y-%m-%d %H:%M");
    let kind = format!("{:?}", it.sidecar.kind);
    let body = if it.body.len() > 800 {
        format!("{}…", &it.body[..800])
    } else {
        it.body.clone()
    };
    format!("  - [{when}] ({kind}) {body}\n")
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

    #[tokio::test]
    async fn bootstrap_intro_when_empty() {
        let (_td, a) = setup().await;
        let i = a.introduction().await;
        assert!(i.contains("starting completely fresh"));
        assert!(i.contains("never do"));
    }

    #[tokio::test]
    async fn intro_changes_after_first_item() {
        let (_td, a) = setup().await;
        let s = SanitizerResult {
            tier: Tier::Pass,
            output: "Bought milk".to_string(),
            redaction_report: "".into(),
        };
        a.respond(&s, &meta(), false).await.unwrap();
        let i = a.introduction().await;
        assert!(i.contains("Welcome back"));
    }

    #[tokio::test]
    async fn preference_detected_and_stored() {
        let (_td, a) = setup().await;
        let s = SanitizerResult {
            tier: Tier::Pass,
            output: "Please stop telling me about crypto news.".into(),
            redaction_report: "".into(),
        };
        a.respond(&s, &meta(), false).await.unwrap();
        let p = a.memory.preferences().await;
        assert_eq!(p.statements.len(), 1);
        assert!(p.statements[0].text.to_lowercase().contains("crypto"));
    }

    #[tokio::test]
    async fn tags_are_guessed() {
        let (_td, a) = setup().await;
        let s = SanitizerResult {
            tier: Tier::Pass,
            output: "Dentist appointment Tuesday at 3pm".into(),
            redaction_report: "".into(),
        };
        a.respond(&s, &meta(), false).await.unwrap();
        let recent = a.memory.recent(5).unwrap();
        // No question mark, no question-starter — classified as Ingestion.
        let item = recent.iter().find(|i| i.sidecar.kind == ItemKind::Ingestion).unwrap();
        assert!(item.sidecar.tags.contains(&"calendar".to_string()));
        assert!(item.sidecar.tags.contains(&"health".to_string()));
    }

    #[tokio::test]
    async fn questions_are_classified_as_user_messages() {
        let (_td, a) = setup().await;
        let s = SanitizerResult {
            tier: Tier::Pass,
            output: "What should I be thinking about right now?".into(),
            redaction_report: "".into(),
        };
        a.respond(&s, &meta(), false).await.unwrap();
        let recent = a.memory.recent(5).unwrap();
        assert!(recent.iter().any(|i| i.sidecar.kind == ItemKind::UserMessage));
    }

    #[test]
    fn detect_preference_handles_phrasings() {
        assert!(detect_preference("Stop telling me about sports").is_some());
        assert!(detect_preference("don't tell me about the weather").is_some());
        assert!(detect_preference("I love coffee").is_none());
    }

    #[tokio::test]
    async fn assistant_passes_websearch_and_webfetch_to_llm() {
        let (_td, a, mock) = setup_with_mock().await;
        let s = SanitizerResult {
            tier: Tier::Pass,
            output: "What's the weather in Lafayette today?".into(),
            redaction_report: "".into(),
        };
        a.respond(&s, &meta(), false).await.unwrap();
        // The assistant turn is the LLM call with the user message in its
        // prompt — find it and assert tools were allowed.
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
        let assistant = Assistant::with_models_and_facts(
            mock.clone(),
            store,
            Some("claude-sonnet-4-6".to_string()),
            Some("claude-opus-4-7".to_string()),
            Arc::new(crate::self_knowledge::SystemFacts::placeholder()),
        );

        // Sonnet responds with the escalation marker; the second call (Opus)
        // gets a real answer. Both are matched on prompt content so we can
        // distinguish by model used.
        let pulse = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let pulse_for_mock = pulse.clone();
        // Override: first call returns the marker, second returns a real answer.
        // The mock has no notion of call order, so use the model field to pick.
        // (model is in LlmOptions, not the prompt — so we route via a custom
        // matcher: when the prompt mentions USER MESSAGE we count, and the
        // mock responds based on which call it is.)
        // Simpler: respond differently to the same prompt across calls via a
        // counter in the override closure. The MockLlmClient doesn't expose
        // closure overrides, so we use a trick: respond_when prefers prompt
        // substrings, and we know Sonnet sees a USER MESSAGE prompt — but
        // we can't distinguish Sonnet's call from Opus's by prompt alone
        // since they're identical. Instead, we just call respond() and
        // verify by inspecting calls() — the mock returns the default
        // canned response for the assistant prompt (which doesn't contain
        // the marker), so we cannot easily test the escalation branch this
        // way. We rely on the integration test for that.
        let _ = (pulse, pulse_for_mock); // keep names alive

        // Direct unit test of the escalation parse path:
        mock.respond_when(
            "USER MESSAGE",
            "ESCALATE_TO_OPUS: needs deep reasoning about a non-obvious tradeoff",
        );
        let s = SanitizerResult {
            tier: Tier::Pass,
            output: "Should I use eventual consistency or strict ordering for X?".into(),
            redaction_report: "".into(),
        };
        // With the mock returning the marker, the assistant should re-call
        // the mock (now with the escalation model) — but the mock still
        // returns the marker on the second call too, so the final text
        // will start with the marker. That's fine; what we're testing is:
        //   - escalated == true
        //   - model_used == escalation model
        let outcome = assistant.respond(&s, &meta(), false).await.unwrap();
        assert!(outcome.escalated, "should have escalated");
        assert_eq!(outcome.model_used, "claude-opus-4-7");
        assert!(outcome.escalation_reason.is_some());
    }

    #[tokio::test]
    async fn force_opus_routes_directly_to_escalation_model_no_re_run() {
        let td = TempDir::new().unwrap();
        let store = Arc::new(MemoryStore::open(td.path().to_path_buf()).await.unwrap());
        let mock = MockLlmClient::new();
        let assistant = Assistant::with_models_and_facts(
            mock.clone(),
            store,
            Some("claude-sonnet-4-6".to_string()),
            Some("claude-opus-4-7".to_string()),
            Arc::new(crate::self_knowledge::SystemFacts::placeholder()),
        );
        let s = SanitizerResult {
            tier: Tier::Pass,
            output: "anything".into(),
            redaction_report: "".into(),
        };
        let outcome = assistant.respond(&s, &meta(), true).await.unwrap();
        // No escalation marker → no second call. Model used should be Opus
        // directly (force_opus path skips Sonnet).
        assert!(!outcome.escalated, "force_opus path doesn't go through escalation");
        assert_eq!(outcome.model_used, "claude-opus-4-7");

        // Verify only ONE assistant LLM call happened (i.e. no Sonnet pre-pass).
        let calls = mock.calls();
        let assistant_calls: Vec<_> = calls
            .iter()
            .filter(|c| c.prompt.contains("USER MESSAGE"))
            .collect();
        assert_eq!(assistant_calls.len(), 1, "expected exactly one assistant call");
        assert_eq!(
            assistant_calls[0].allowed_tools,
            vec!["WebSearch", "WebFetch"]
        );
    }

    #[tokio::test]
    async fn assistant_prompt_mentions_web_capabilities() {
        let (_td, a, mock) = setup_with_mock().await;
        let s = SanitizerResult {
            tier: Tier::Pass,
            output: "hello".into(),
            redaction_report: "".into(),
        };
        a.respond(&s, &meta(), false).await.unwrap();
        let calls = mock.calls();
        let turn = calls
            .iter()
            .find(|c| c.prompt.contains("USER MESSAGE"))
            .expect("expected an assistant LLM call");
        assert!(
            turn.prompt.contains("WebSearch"),
            "assistant prompt should advertise WebSearch capability"
        );
    }
}
