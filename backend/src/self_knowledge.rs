//! Self-knowledge: the assistant should be able to answer questions about
//! itself — what models it uses, how it works, why design decisions were
//! made.
//!
//! Two layers:
//!   1. **Static memory items**, seeded on startup. These describe stable
//!      facts: the diode architecture, the Sanitizer's role, why each model
//!      was chosen, what the assistant CAN'T do. They live as ordinary
//!      `SelfKnowledge` items in memory so the assistant finds them through
//!      its normal recent/search pipeline. Idempotent via a stable
//!      `self:<slug>` tag.
//!   2. **Runtime facts block**, recomputed per turn. Captures things that
//!      change with config or runtime state — current models per role,
//!      intervals, memory directory, item count. Injected into the
//!      assistant's prompt.
//!
//! The assistant is free to add its own `SelfKnowledge` items during a
//! conversation (e.g. "I made decision X today, here's why"); seeding only
//! covers the developer-authored baseline.

use crate::config::{ClaudeCfg, CuratorCfg, MemoryCfg, ScoutCfg, ServerCfg};
use crate::memory::{ItemKind, MemoryStore};
use anyhow::Result;
use std::path::PathBuf;

/// Runtime-snapshot facts the assistant gets in its prompt every turn. Cheap
/// to construct; we rebuild fresh per turn so changes to config (after
/// restart) are reflected immediately.
#[derive(Debug, Clone)]
pub struct SystemFacts {
    pub sanitizer_model: String,
    pub assistant_model: String,
    pub curator_model: String,
    pub scout_model: String,
    pub curator_enabled: bool,
    pub curator_interval_minutes: u64,
    pub scout_enabled: bool,
    pub scout_interval_minutes: u64,
    pub scout_pinned_topics: Vec<String>,
    pub memory_dir: PathBuf,
    pub server_addr: String,
    pub build_version: String,
}

impl SystemFacts {
    pub fn from_cfg(
        claude: &ClaudeCfg,
        memory: &MemoryCfg,
        curator: &CuratorCfg,
        scout: &ScoutCfg,
        server: &ServerCfg,
    ) -> Self {
        Self {
            sanitizer_model: claude.model_for_sanitizer(),
            assistant_model: claude.model_for_assistant(),
            curator_model: claude.model_for_curator(),
            scout_model: claude.model_for_scout(),
            curator_enabled: curator.enabled,
            curator_interval_minutes: curator.interval_minutes,
            scout_enabled: scout.enabled,
            scout_interval_minutes: scout.interval_minutes,
            scout_pinned_topics: scout.pinned_topics.clone(),
            memory_dir: memory.dir.clone(),
            server_addr: server.addr.clone(),
            build_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Used in tests and as a default before runtime config is wired in.
    pub fn placeholder() -> Self {
        Self {
            sanitizer_model: "(unset)".into(),
            assistant_model: "(unset)".into(),
            curator_model: "(unset)".into(),
            scout_model: "(unset)".into(),
            curator_enabled: false,
            curator_interval_minutes: 0,
            scout_enabled: false,
            scout_interval_minutes: 0,
            scout_pinned_topics: vec![],
            memory_dir: PathBuf::from("(unset)"),
            server_addr: "(unset)".into(),
            build_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Render a compact block to drop into the assistant's prompt. The
    /// assistant uses this to answer questions like "what model are you
    /// using?", "how often does the curator run?", "where is my data?".
    pub fn render_prompt_block(&self, memory_item_count: usize) -> String {
        let mut s = String::from("SYSTEM SELF-KNOWLEDGE (current runtime configuration — accurate as of this turn):\n");
        s.push_str(&format!("  • Build version: {}\n", self.build_version));
        s.push_str(&format!(
            "  • Sanitizer (the Gate) model: {}\n",
            self.sanitizer_model
        ));
        s.push_str(&format!("  • Assistant (Core) model: {}\n", self.assistant_model));
        s.push_str(&format!(
            "  • Curator model: {}  ({}, every {} min)\n",
            self.curator_model,
            if self.curator_enabled { "enabled" } else { "disabled" },
            self.curator_interval_minutes,
        ));
        s.push_str(&format!(
            "  • Scout model: {}  ({}, every {} min; topics inferred from memory{})\n",
            self.scout_model,
            if self.scout_enabled { "enabled" } else { "disabled" },
            self.scout_interval_minutes,
            if self.scout_pinned_topics.is_empty() {
                String::new()
            } else {
                format!("; pinned: {}", self.scout_pinned_topics.join(", "))
            },
        ));
        s.push_str(&format!("  • Memory directory: {}\n", self.memory_dir.display()));
        s.push_str(&format!("  • Memory item count: {}\n", memory_item_count));
        s.push_str(&format!("  • Listening on: ws://{}/ws\n", self.server_addr));
        s
    }
}

/// Developer-authored baseline self-knowledge. Stable across restarts.
/// Each entry is (slug, body). Slug becomes the tag `self:<slug>` so we can
/// find + update existing copies idempotently.
fn baseline() -> Vec<(&'static str, String)> {
    vec![
        (
            "what-i-am",
            "I am a personal AI assistant built around a strict one-way data flow (\"the diode\"). \
             Data flows IN — emails, notes, documents, calendar entries, photos. I accumulate \
             knowledge about the user over time. I only produce OUTPUTS — reminders, summaries, \
             answers. I CANNOT take any action in the outside world: I cannot send email, book \
             flights, move money, change settings, or call any write-capable API. This is a \
             deliberate, load-bearing security property of my design."
                .to_string(),
        ),
        (
            "architecture",
            "I am composed of four components running on a backend server:\n\
             1. **Sanitizer (\"the Gate\")** — the first layer every message passes through, including \
                the user's own questions. Three-tier: drop (security-only messages like 2FA codes), \
                redact (sensitive but useful messages, like a deposit confirmation), or pass.\n\
             2. **Assistant Core** — the only component the user actually talks to. Reads relevant \
                memory, calls the LLM, returns a reply. Has read-only web access (WebSearch, WebFetch).\n\
             3. **Curator** — runs periodically. Walks memory, ages items, summarizes aging items, \
                collapses stale ones. Decay is by importance, not a fixed calendar.\n\
             4. **Scout** — runs periodically (when enabled). Browses the web for the user's topics \
                of interest, funnels findings through the Sanitizer, files them in memory.\n\
             A native Mac client connects via WebSocket and provides a single chat surface for both \
             data ingestion and conversation."
                .to_string(),
        ),
        (
            "ephemeral-sanitizer",
            "Critical security property: every time the Sanitizer runs, it gets a brand-new \
             subprocess with NO shared session state. The raw input only lives on the request \
             stack and inside that one subprocess; when the subprocess exits, the raw input is \
             gone. It is never written to disk, never logged, never reaches the Assistant Core \
             or long-term memory. Only the Sanitizer's structured output (a tier classification, \
             a sanitized version of the message, and a non-sensitive redaction report) moves \
             downstream. This is why I cannot \"remember exactly what you said\" if the Gate \
             redacted parts of it — the original is gone."
                .to_string(),
        ),
        (
            "sanitizer-model-choice",
            "The Sanitizer uses Claude Haiku 4.5 by default. Reasoning: the Sanitizer runs on \
             EVERY message, so latency compounds; its job is pattern recognition (OTP codes, \
             reset links, account numbers) plus structured JSON output — both well within \
             Haiku's capabilities; and the threat model is defending against well-known patterns \
             of direct account takeover, not novel social engineering. A user worried about \
             subtle attacks can bump it to Sonnet via `[claude].sanitizer_model` in `config.toml`."
                .to_string(),
        ),
        (
            "scout-design-choices",
            "The Scout is OPT-IN (disabled by default). Reasons: it spends tokens silently in \
             the background on a ~10 min interval, and on a fresh install with no memory it \
             can't yet infer what the user cares about. Enable via `[scout].enabled = true` in \
             `config.toml` once the assistant has accumulated enough memory to be useful. \
             The model is Claude Sonnet 4.6 — web summarization and triage is well within \
             Sonnet's range; Opus would be wasted spend.\n\
             \n\
             The Scout has NO hardcoded topic list. Each tick it reads the user's recent memory \
             and stored preferences, infers what the user cares about, and searches accordingly. \
             If memory is thin, it falls back to base-rate human interests for the user's \
             location (major news, severe-weather alerts, broadly relevant science/tech) and \
             marks those bullets with \"[base rate — limited memory]\" so the user knows. \
             Time-sensitive items (severe weather, breaking news affecting the user's region) \
             are always included regardless of inferred interests. The user can pin specific \
             topics via `[scout].pinned_topics` if they want a guaranteed sweep that doesn't \
             rely on inference, but normally you'd just tell the assistant (\"keep an eye on \
             Boston Celtics news\") and the preference + memory pipeline picks it up next tick. \
             Findings go through the Sanitizer (with PublicWeb provenance) before landing in \
             memory."
                .to_string(),
        ),
        (
            "curator-model-choice",
            "The Curator uses Claude Sonnet 4.6 by default — NOT Haiku, even though it's a \
             summarization task. Reasoning: the Curator is the only component that destructively \
             rewrites memory items (the summary replaces the original body). Its mistakes are \
             silent and permanent — a buried name or offhand date that turns out to matter later \
             just vanishes. Sanitizer mistakes are noisy (the user notices an over-redaction and \
             re-sends), Assistant mistakes are correctable in the next turn, but Curator mistakes \
             are unrecoverable. The Curator runs every 60 min in the background, so latency \
             isn't load-bearing the way it is for the Sanitizer. Override via \
             `[claude].curator_model`."
                .to_string(),
        ),
        (
            "what-i-protect-against",
            "I am defending against sophisticated, financially motivated attackers whose goal \
             is account takeover or direct theft. I must NEVER let the following reach long-term \
             memory or the main reasoning model:\n\
             - 2FA / MFA / OTP codes\n\
             - Password reset links and tokens\n\
             - API keys, access tokens, session tokens, recovery codes\n\
             - Full bank account numbers, card numbers, routing numbers, wire/ACH/ETF identifiers\n\
             It IS OK to remember and reason about: birthdays, family schedules, vacation dates, \
             \"house empty next Tuesday\" implications, job interviews, calendar events, names of \
             banks/companies, types of events (\"a deposit was confirmed\"), and rough dollar \
             amounts when not tied to an actionable identifier."
                .to_string(),
        ),
        (
            "memory-decay",
            "I do not keep everything forever. The Curator runs periodically and walks all \
             memory items: Fresh items (recent) are kept in full. Aging items get summarized \
             into a short paragraph that preserves names, dates, and key facts but drops bulk. \
             Stale items (very old, low importance) collapse to a one-line pointer or get \
             dropped. Decay is driven by importance × age, not a fixed schedule, so important \
             items (a major life event) outlast trivial bulk (a privacy-policy email). Photos \
             eventually become text summaries; the pixels are discarded."
                .to_string(),
        ),
        (
            "how-to-use-me",
            "Hand me anything you want me to remember — paste an email, jot a note, drop a \
             calendar entry, describe a document. If your message doesn't contain a question, \
             I treat it as data to keep. Ask me anything about your life that I might know from \
             what you've given me, or about the world (I'll search the web when helpful). Tell \
             me to forget things (\"stop telling me about crypto news\") and I'll save that as \
             a persistent preference. Acknowledge things (\"I finished that task\") and I'll \
             update item state."
                .to_string(),
        ),
        (
            "error-handling",
            "When the Sanitizer fails (out of tokens, malformed JSON, network error), I drop the \
             input WITHOUT inspecting it — preserving the ephemerality guarantee — and write an \
             audit record (kind=sanitizer_error) so I can tell the user what happened. When the \
             Assistant Core fails (after the Sanitizer succeeded), the user's already-sanitized \
             message is in memory; I add an assistant_error record paired with it. You can ask \
             \"have you had any errors recently?\" and I'll find these records."
                .to_string(),
        ),
        (
            "where-data-lives",
            "Everything I remember lives in a single directory on disk (the \"memory directory\"). \
             Each item is a plain-text body file plus a small JSON sidecar of metadata. All writes \
             go through a temp-file-then-rename atomic pattern so a crash mid-write cannot corrupt \
             items. The backup procedure is `tar czf data.tgz <memory-dir>`. The backend can be \
             pointed at a different directory with `--memory-dir <path>` or `AI_ASSISTANT_MEMORY_DIR`."
                .to_string(),
        ),
    ]
}

/// Idempotently write/update the baseline self-knowledge items. Safe to call
/// on every startup; runs once and skips items that are already current.
pub async fn seed_baseline(memory: &MemoryStore) -> Result<()> {
    let existing = memory.scan_all().unwrap_or_default();
    for (slug, body) in baseline() {
        let tag = format!("self:{slug}");
        let existing_for_slug = existing
            .iter()
            .find(|it| it.sidecar.kind == ItemKind::SelfKnowledge && it.sidecar.tags.contains(&tag));
        match existing_for_slug {
            Some(item) if item.body == body => {
                // Up to date; nothing to do.
            }
            Some(item) => {
                memory.update_item(item, Some(&body), |_| {}).await?;
            }
            None => {
                memory
                    .add(
                        &body,
                        ItemKind::SelfKnowledge,
                        0.9,
                        None,
                        String::new(),
                        vec!["self".into(), tag, "self-knowledge".into()],
                    )
                    .await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn seeding_is_idempotent() {
        let td = TempDir::new().unwrap();
        let mem = MemoryStore::open(td.path().to_path_buf()).await.unwrap();
        seed_baseline(&mem).await.unwrap();
        let n1 = mem
            .scan_all()
            .unwrap()
            .iter()
            .filter(|i| i.sidecar.kind == ItemKind::SelfKnowledge)
            .count();
        seed_baseline(&mem).await.unwrap();
        let n2 = mem
            .scan_all()
            .unwrap()
            .iter()
            .filter(|i| i.sidecar.kind == ItemKind::SelfKnowledge)
            .count();
        assert_eq!(n1, n2);
        assert_eq!(n1, baseline().len());
    }

    #[test]
    fn runtime_block_lists_each_role_model() {
        let f = SystemFacts {
            sanitizer_model: "claude-haiku-4-5".into(),
            assistant_model: "claude-opus-4-7".into(),
            curator_model: "claude-haiku-4-5".into(),
            scout_model: "claude-opus-4-7".into(),
            curator_enabled: true,
            curator_interval_minutes: 60,
            scout_enabled: false,
            scout_interval_minutes: 10,
            scout_pinned_topics: vec!["tech news".into()],
            memory_dir: PathBuf::from("./memory"),
            server_addr: "127.0.0.1:8765".into(),
            build_version: "0.1.0".into(),
        };
        let s = f.render_prompt_block(42);
        assert!(s.contains("claude-haiku-4-5"));
        assert!(s.contains("Sanitizer"));
        assert!(s.contains("Curator"));
        assert!(s.contains("Scout"));
        assert!(s.contains("42"));
    }
}
