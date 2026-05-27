//! Self-knowledge: the assistant should be able to answer questions about
//! itself — what models it uses, how it works, why design decisions were
//! made.
//!
//! This module now owns ONLY the **runtime facts** layer — the snapshot of
//! current runtime config that goes into every prompt (model names per
//! role, intervals, embedding model, retrieval weights, etc.). That state
//! genuinely changes with config and can't be captured in a static doc.
//!
//! Procedural / architectural / "how does X work" knowledge has moved to
//! the system manual (`backend/src/manual.rs` + `SYSTEM_MANUAL.md` in the
//! memory directory), which the assistant consults on demand via the
//! `READ_MANUAL` marker. The previous SelfKnowledge seeding has been
//! removed; the `ItemKind::SelfKnowledge` enum variant is preserved only
//! for back-compat with items written by earlier versions of the system
//! (Invariant #7).

use crate::config::{ClaudeCfg, IndexerCfg, MemoryCfg, ScoutCfg, ServerCfg};
use crate::retrieval::RetrievalWeights;
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
        let mut s = String::from(
            "SYSTEM SELF-KNOWLEDGE (current runtime configuration — accurate as of this turn):\n",
        );
        s.push_str(&format!("  • Build version: {}\n", self.build_version));
        s.push_str(&format!(
            "  • Security Preprocessor model: {}\n",
            self.preprocessor_model
        ));
        s.push_str(&format!(
            "  • Assistant (Core) primary model: {}\n",
            self.assistant_model
        ));
        s.push_str(&format!(
            "  • Assistant escalation model: {}\n",
            self.assistant_escalation_model
        ));
        s.push_str(&format!(
            "  • Scout model: {}  ({}, every {} min)\n",
            self.scout_model,
            if self.scout_enabled {
                "enabled"
            } else {
                "disabled"
            },
            self.scout_interval_minutes,
        ));
        s.push_str(&format!(
            "  • Indexer: {}, every {} min, batch {} (mechanical, no LLM)\n",
            if self.indexer_enabled {
                "enabled"
            } else {
                "disabled"
            },
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
        s.push_str(&format!(
            "  • Memory directory: {}\n",
            self.memory_dir.display()
        ));
        s.push_str(&format!("  • Memory item count: {}\n", memory_item_count));
        s.push_str(&format!("  • Listening on: ws://{}/ws\n", self.server_addr));
        s
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_block_lists_embedder_and_retrieval() {
        let f = SystemFacts::placeholder();
        let s = f.render_prompt_block(42);
        assert!(s.contains("Embedder"));
        assert!(s.contains("Retrieval weights"));
        assert!(s.contains("42"));
    }
}
