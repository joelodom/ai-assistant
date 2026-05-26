//! Self-knowledge: the assistant should be able to answer questions about
//! itself — what models it uses, how it works, why design decisions were
//! made.
//!
//! Two layers:
//!   1. **Static memory items**, seeded on startup. These describe stable
//!      facts: the diode, the Security Preprocessor, RAG architecture, what the assistant
//!      CAN'T do. They live as ordinary `SelfKnowledge` items in memory
//!      so the assistant finds them through its normal retrieval pipeline.
//!      Idempotent via a stable `self:<slug>` tag.
//!   2. **Runtime facts block**, recomputed per turn. Captures things that
//!      change with config or runtime state — current models per role,
//!      intervals, embedding model, retrieval weights.

use crate::config::{ClaudeCfg, IndexerCfg, MemoryCfg, ScoutCfg, ServerCfg};
use crate::memory::{ItemKind, MemoryStore};
use crate::retrieval::RetrievalWeights;
use anyhow::Result;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct SystemFacts {
    pub preprocessor_model: String,
    pub assistant_model: String,
    pub assistant_escalation_model: String,
    pub scout_model: String,
    pub indexer_enabled: bool,
    pub indexer_interval_minutes: u64,
    pub indexer_batch_size: usize,
    pub scout_enabled: bool,
    pub scout_interval_minutes: u64,
    pub scout_pinned_topics: Vec<String>,
    pub memory_dir: PathBuf,
    pub server_addr: String,
    pub embedding_model: String,
    pub embedding_dim: usize,
    pub retrieval_alpha: f32,
    pub retrieval_beta: f32,
    pub retrieval_gamma: f32,
    pub retrieval_half_life_days: f32,
    pub build_version: String,
}

impl SystemFacts {
    pub fn from_cfg(
        claude: &ClaudeCfg,
        memory: &MemoryCfg,
        indexer: &IndexerCfg,
        scout: &ScoutCfg,
        server: &ServerCfg,
        retrieval: &RetrievalWeights,
        embedding_model: &str,
        embedding_dim: usize,
    ) -> Self {
        Self {
            preprocessor_model: claude.model_for_preprocessor(),
            assistant_model: claude.model_for_assistant(),
            assistant_escalation_model: claude.model_for_assistant_escalation(),
            scout_model: claude.model_for_scout(),
            indexer_enabled: indexer.enabled,
            indexer_interval_minutes: indexer.interval_minutes,
            indexer_batch_size: indexer.batch_size,
            scout_enabled: scout.enabled,
            scout_interval_minutes: scout.interval_minutes,
            scout_pinned_topics: scout.pinned_topics.clone(),
            memory_dir: memory.dir.clone(),
            server_addr: server.addr.clone(),
            embedding_model: embedding_model.to_string(),
            embedding_dim,
            retrieval_alpha: retrieval.alpha,
            retrieval_beta: retrieval.beta,
            retrieval_gamma: retrieval.gamma,
            retrieval_half_life_days: retrieval.half_life_days,
            build_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    pub fn placeholder() -> Self {
        Self {
            preprocessor_model: "(unset)".into(),
            assistant_model: "(unset)".into(),
            assistant_escalation_model: "(unset)".into(),
            scout_model: "(unset)".into(),
            indexer_enabled: false,
            indexer_interval_minutes: 0,
            indexer_batch_size: 0,
            scout_enabled: false,
            scout_interval_minutes: 0,
            scout_pinned_topics: vec![],
            memory_dir: PathBuf::from("(unset)"),
            server_addr: "(unset)".into(),
            embedding_model: "(unset)".into(),
            embedding_dim: 0,
            retrieval_alpha: 0.6,
            retrieval_beta: 0.25,
            retrieval_gamma: 0.15,
            retrieval_half_life_days: 30.0,
            build_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    pub fn render_prompt_block(&self, memory_item_count: usize) -> String {
        let mut s = String::from("SYSTEM SELF-KNOWLEDGE (current runtime configuration — accurate as of this turn):\n");
        s.push_str(&format!("  • Build version: {}\n", self.build_version));
        s.push_str(&format!(
            "  • Security Preprocessor model: {}\n",
            self.preprocessor_model
        ));
        s.push_str(&format!("  • Assistant (Core) primary model: {}\n", self.assistant_model));
        s.push_str(&format!("  • Assistant escalation model: {}\n", self.assistant_escalation_model));
        s.push_str(&format!(
            "  • Scout model: {}  ({}, every {} min)\n",
            self.scout_model,
            if self.scout_enabled { "enabled" } else { "disabled" },
            self.scout_interval_minutes,
        ));
        s.push_str(&format!(
            "  • Indexer: {}, every {} min, batch {} (mechanical, no LLM)\n",
            if self.indexer_enabled { "enabled" } else { "disabled" },
            self.indexer_interval_minutes,
            self.indexer_batch_size,
        ));
        s.push_str(&format!(
            "  • Embedder: {} ({}-dim)\n",
            self.embedding_model, self.embedding_dim
        ));
        s.push_str(&format!(
            "  • Retrieval weights: α(relevance)={:.2}, β(recency)={:.2}, γ(importance)={:.2}, half-life={} days\n",
            self.retrieval_alpha,
            self.retrieval_beta,
            self.retrieval_gamma,
            self.retrieval_half_life_days,
        ));
        s.push_str(&format!("  • Memory directory: {}\n", self.memory_dir.display()));
        s.push_str(&format!("  • Memory item count: {}\n", memory_item_count));
        s.push_str(&format!("  • Listening on: ws://{}/ws\n", self.server_addr));
        s
    }
}

/// Developer-authored baseline self-knowledge. Stable across restarts.
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
            "I am composed of these components running on a backend server:\n\
             1. **Security Preprocessor** (Preprocessor for short) — the first layer every message \
                passes through. Three-tier classification (drop / redact / pass), in-line redaction, \
                AND an importance score on [0, 1] that the retrieval system uses for ranking.\n\
             2. **Assistant Core** — the only component the user talks to. Reads relevant memory \
                via hybrid retrieval (vector + keyword + recency + importance), calls the LLM, \
                returns a reply. Has read-only web access (WebSearch, WebFetch).\n\
             3. **Embedder** — a local model (fastembed-rs) that turns sanitized text into \
                vectors. Runs in-process, no external API calls.\n\
             4. **Vector Index (HNSW)** — fast nearest-neighbor search. The graph file is a \
                derived cache; the source of truth is the per-item `.vec` sidecars.\n\
             5. **Indexer** — periodic mechanical worker (no LLM). Backfills missing `.vec` \
                sidecars, compacts the HNSW graph, snapshots stats. Replaces the old Curator, \
                which destructively summarized memory items.\n\
             6. **Scout** — opt-in periodic web/news worker.\n\
             A native Mac client connects via WebSocket."
                .to_string(),
        ),
        (
            "rag-retrieval",
            "I use hybrid retrieval to decide which memory items to surface for each turn. The \
             score for each candidate item is:\n\
             \n\
             final = α · relevance + β · recency + γ · importance\n\
             \n\
             where relevance = max(vector_cosine, keyword_rank), recency = exp(-age_days / \
             half_life), and importance is the score the Preprocessor assigned at ingest time. \
             Default weights are α=0.6, β=0.25, γ=0.15 with half_life=30 days; configurable in \
             [retrieval] of config.toml.\n\
             \n\
             This means: a very strong semantic match from a year ago will still surface; a weak \
             match from yesterday will still surface; a weak match from a year ago will be \
             filtered out; a flagged-important item floats up regardless."
                .to_string(),
        ),
        (
            "hazmat-bypass",
            "There is an explicit user-controlled escape hatch called HAZMAT mode. When the user \
             ticks the \"☢ HAZMAT\" checkbox in the client and sends a message, the Security \
             Preprocessor is skipped entirely for that message and the raw content goes directly \
             to the Assistant. The wire field is `bypass_preprocessor` (the older name `bypass_sanitizer` \
             is accepted as a deserialization alias). Memory items written from a bypass carry \
             the `hazmat` tag and an elevated importance (0.8). NO code path may set the bypass \
             flag programmatically — only the human-pressed checkbox can."
                .to_string(),
        ),
        (
            "explicit-forget",
            "I never silently forget things. The user can ask me to forget a specific memory \
             (\"forget that\"), and I emit a `FORGET:` marker to the backend. The backend then \
             tombstones the item: its body becomes `[forgotten <timestamp>]`, its kind becomes \
             `ForgottenStub`, its `.vec` sidecar is deleted, and it's removed from the HNSW \
             index. The sidecar metadata stays as audit. Reversible only from backup. Background \
             workers do NOT delete or rewrite item bodies on their own."
                .to_string(),
        ),
        (
            "ephemeral-preprocessor",
            "Critical security property: every time the Security Preprocessor runs, it gets a \
             brand-new subprocess with NO shared session state. The raw input only lives on \
             the request stack and inside that one subprocess; when the subprocess exits, the \
             raw input is gone. It is never written to disk, never logged, never reaches the \
             Assistant Core, the Embedder, or long-term memory. Only the Preprocessor's \
             structured output (tier classification, sanitized text, redaction report, \
             importance score) moves downstream."
                .to_string(),
        ),
        (
            "preprocessor-model-choice",
            "The Preprocessor uses Claude Haiku 4.5 by default. Reasoning: it runs on EVERY \
             message, so latency compounds; its job is pattern recognition (OTP codes, reset \
             links, account numbers) plus structured JSON output (with an importance score) — \
             both well within Haiku's capabilities. Configurable via `[claude].preprocessor_model` \
             (the legacy field `sanitizer_model` is also accepted)."
                .to_string(),
        ),
        (
            "assistant-model-routing",
            "The Assistant defaults to Claude Sonnet 4.6. Two ways to invoke the heavier Opus 4.7:\n\
             1. Self-escalation: Sonnet replies with exactly `ESCALATE_TO_OPUS: <reason>` as its \
                entire response. The backend detects the marker, re-runs the same prompt against \
                Opus, and the user receives Opus's answer.\n\
             2. User-forced: the client exposes a \"🧠 Opus\" checkbox. When ticked, the message \
                goes with `force_opus=true`; the backend skips Sonnet and routes straight to \
                Opus."
                .to_string(),
        ),
        (
            "indexer-design",
            "The Indexer is a small periodic mechanical worker. It does NOT call the LLM, does \
             NOT destructively rewrite memory items, does NOT delete things on its own. Its \
             jobs: (1) find items missing a `.vec` sidecar and embed them via the local \
             Embedder; (2) detect embedding-model changes and trigger a re-embed; (3) \
             checkpoint the HNSW manifest; (4) log stats. It runs every few minutes; the \
             interval and batch size are in `[indexer]` of config.toml."
                .to_string(),
        ),
        (
            "scout-design-choices",
            "The Scout is OPT-IN (disabled by default). On enable, each tick it reads recent \
             memory + stored preferences, infers what the user cares about, and searches the \
             web. Findings go through the Preprocessor (with PublicWeb provenance) before \
             landing in memory."
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
             - Full bank account numbers, card numbers, routing numbers, wire/ACH identifiers\n\
             It IS OK to remember and reason about: birthdays, family schedules, vacation dates, \
             job interviews, calendar events, names of banks/companies, rough dollar amounts \
             when not tied to an actionable identifier."
                .to_string(),
        ),
        (
            "no-automatic-decay",
            "I do not silently forget things. There is no Curator anymore — the old design \
             destructively summarized aging items, which meant any small fact that turned out \
             to matter later might just vanish. With RAG retrieval, large memory doesn't \
             pollute the prompt (the assistant only sees the top-K retrieved items per turn), \
             so the original motivation for decay is gone."
                .to_string(),
        ),
        (
            "how-to-use-me",
            "Hand me anything you want me to remember — paste an email, jot a note, drop a \
             calendar entry, describe a document. If your message doesn't contain a question, I \
             treat it as data. Ask me anything about your life or the world. Tell me to forget \
             things and I'll save that as a preference; ask me to forget a specific memory and \
             I'll tombstone it."
                .to_string(),
        ),
        (
            "error-handling",
            "When the Preprocessor fails (out of tokens, malformed JSON, network error), I drop \
             the input WITHOUT inspecting it — preserving the ephemerality guarantee — and write \
             an audit record (kind=preprocessor_error). When the Assistant fails after a \
             successful preprocess, the user's sanitized message is in memory; I add an \
             `assistant_error` record paired with it."
                .to_string(),
        ),
        (
            "where-data-lives",
            "Everything I remember lives in a single directory on disk. Each item is a plain-text \
             body file, a small JSON sidecar of metadata, and a tiny `.vec` binary sidecar \
             holding its embedding vector. Stubs (drop notices) live in a separate `stubs/` \
             directory. A `hnsw/` directory holds the vector search index — but that's a \
             DERIVED CACHE, rebuildable from the `.vec` sidecars; you can delete it without \
             losing data. All writes are atomic (temp file + rename). Backup is just `tar czf \
             data.tgz <memory-dir>`. Restores work even when partial — the Indexer rebuilds \
             whatever's missing."
                .to_string(),
        ),
    ]
}

pub async fn seed_baseline(memory: &MemoryStore) -> Result<()> {
    let existing = memory.scan_all().unwrap_or_default();
    for (slug, body) in baseline() {
        let tag = format!("self:{slug}");
        let existing_for_slug = existing
            .iter()
            .find(|it| it.sidecar.kind == ItemKind::SelfKnowledge && it.sidecar.tags.contains(&tag));
        match existing_for_slug {
            Some(item) if item.body == body => { /* already current */ }
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
    fn runtime_block_lists_embedder_and_retrieval() {
        let f = SystemFacts::placeholder();
        let s = f.render_prompt_block(42);
        assert!(s.contains("Embedder"));
        assert!(s.contains("Retrieval weights"));
        assert!(s.contains("42"));
    }
}
