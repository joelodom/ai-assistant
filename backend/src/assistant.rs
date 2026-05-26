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
}

impl Assistant {
    pub fn new(llm: Arc<dyn LlmClient>, memory: Arc<MemoryStore>) -> Self {
        Self { llm, memory }
    }

    /// Produce the introduction the client sees on connect. Pure function so
    /// it's easy to test and to render the same text in different surfaces.
    pub async fn introduction(&self) -> String {
        let prefs = self.memory.preferences().await;
        let stats = self.memory.stats();
        let total = stats.get("total").copied().unwrap_or(0);
        let bootstrap = total == 0 && prefs.statements.is_empty();
        if bootstrap {
            BOOTSTRAP_INTRO.to_string()
        } else {
            format!(
                "Welcome back. I have {total} item(s) in memory and {} stored preference(s). \
                 Ask me what you need to be thinking about, or hand me more to remember.",
                prefs.statements.len()
            )
        }
    }

    /// Run a single conversational turn. Returns the assistant's full reply
    /// text. Persists both the user message and the assistant's note.
    pub async fn respond(
        &self,
        sanitized: &SanitizerResult,
        metadata: &Metadata,
    ) -> Result<String> {
        // Persist user-side first (even if Tier::Drop, we already wrote a
        // stub from the WS handler — that path doesn't reach here).
        let user_importance = importance_hint(&sanitized.output);
        let user_kind = if looks_like_question(&sanitized.output) {
            ItemKind::UserMessage
        } else {
            ItemKind::Ingestion
        };
        self.memory
            .add(
                &sanitized.output,
                user_kind,
                user_importance,
                Some(metadata.clone()),
                sanitized.redaction_report.clone(),
                tag_guess(&sanitized.output),
            )
            .await?;

        // Pull context.
        let recent = self.memory.recent(20).unwrap_or_default();
        let keyword_hits = self
            .memory
            .search(&sanitized.output, 8)
            .unwrap_or_default();
        let prefs = self.memory.preferences().await;

        let prompt = build_prompt(metadata, &sanitized.output, sanitized.tier, &recent, &keyword_hits, &prefs.statements);

        // The assistant can use web tools when answering questions about the
        // outside world — they're read-only and consistent with the diode
        // invariant (we fetch in, we never push out).
        let opts = LlmOptions {
            allowed_tools: vec!["WebSearch".into(), "WebFetch".into()],
            ..Default::default()
        };
        let reply = self.llm.oneshot(&prompt, opts).await?;
        let reply = reply.trim().to_string();

        // Persist assistant note.
        self.memory
            .add(
                &format!("(assistant reply) {reply}"),
                ItemKind::AssistantNote,
                0.2,
                Some(metadata.clone()),
                String::new(),
                vec!["assistant".into()],
            )
            .await
            .ok();

        // Detect preference-shaped statements ("stop telling me…", "don't
        // tell me about…", "ignore X"). Cheap heuristic — Curator can
        // refine later.
        if let Some(pref) = detect_preference(&sanitized.output) {
            self.memory.add_preference(&pref).await.ok();
        }

        Ok(reply)
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
         in, you never push out.\n\n",
    );
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
        a.respond(&s, &meta()).await.unwrap();
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
        a.respond(&s, &meta()).await.unwrap();
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
        a.respond(&s, &meta()).await.unwrap();
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
        a.respond(&s, &meta()).await.unwrap();
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
        a.respond(&s, &meta()).await.unwrap();
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
    async fn assistant_prompt_mentions_web_capabilities() {
        let (_td, a, mock) = setup_with_mock().await;
        let s = SanitizerResult {
            tier: Tier::Pass,
            output: "hello".into(),
            redaction_report: "".into(),
        };
        a.respond(&s, &meta()).await.unwrap();
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
