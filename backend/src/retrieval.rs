//! Hybrid retrieval. Combines vector similarity (semantic), keyword search
//! (exact-name lookups), and per-item axes (recency, importance) into a
//! single ranking.
//!
//! Replaces the old recent(20) + search(8) split. With recency in the score
//! itself, very-new items naturally float to the top even with low semantic
//! match — and very-old items with strong matches still surface.
//!
//! Score formula (all axes normalized to [0, 1]):
//!
//!   final = α · relevance + β · recency + γ · importance
//!
//!   relevance = max(vector_cosine, keyword_rank_score)
//!   recency   = exp(-age_days / half_life_days)
//!   importance = sidecar.importance
//!
//! Default weights: α=0.6, β=0.25, γ=0.15, half_life=30d. Tune via
//! `config.toml [retrieval]`.
//!
//! The candidate pool unions: vector top-K, keyword top-K, and a recent-window
//! fallback. The fallback keeps freshly-arrived items findable even before
//! they're embedded — which matters because embedding happens async via the
//! Indexer and there's a short window where a new item has no `.vec`.

use crate::embedder::Embedder;
use crate::memory::{ItemKind, MemoryItem, MemoryStore};
use crate::vector_index::VectorIndex;
use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetrievalWeights {
    pub alpha: f32,
    pub beta: f32,
    pub gamma: f32,
    pub half_life_days: f32,
    /// How many vector candidates to over-fetch before re-ranking. Tuned for
    /// recall — a small over-fetch lets recency/importance shuffle items that
    /// were just outside the vector top-K.
    pub vector_candidates: usize,
    /// How many keyword candidates to consider.
    pub keyword_candidates: usize,
    /// How many recent items to fold into the candidate pool. Keeps newly
    /// arrived items findable before they're embedded.
    pub recent_candidates: usize,
}

impl Default for RetrievalWeights {
    fn default() -> Self {
        Self {
            alpha: 0.6,
            beta: 0.25,
            gamma: 0.15,
            half_life_days: 30.0,
            vector_candidates: 50,
            keyword_candidates: 20,
            recent_candidates: 20,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScoredItem {
    pub item: MemoryItem,
    pub final_score: f32,
    pub relevance: f32,
    pub recency: f32,
    pub importance: f32,
}

/// Hybrid retrieval. Returns the top-`k` scored items. Always excludes
/// ForgottenStub tombstones.
#[tracing::instrument(skip_all, fields(query_len = query.len(), k))]
pub async fn retrieve(
    memory: &MemoryStore,
    embedder: &dyn Embedder,
    index: &VectorIndex,
    weights: &RetrievalWeights,
    query: &str,
    k: usize,
) -> Result<Vec<ScoredItem>> {
    if k == 0 {
        return Ok(vec![]);
    }

    let mut vector_scores: HashMap<String, f32> = HashMap::new();
    let mut keyword_scores: HashMap<String, f32> = HashMap::new();
    let mut candidates: HashMap<String, MemoryItem> = HashMap::new();

    // 1. Vector candidates (if we can embed; if the embedder fails, fall
    //    through gracefully — keyword + recency still produce a result).
    match embedder.embed(query).await {
        Ok(qv) => {
            let hits = index.search(&qv, weights.vector_candidates.max(k));
            for h in hits {
                vector_scores.insert(h.item_id.clone(), h.score.max(0.0));
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "embedder failed; retrieving without vector leg");
        }
    }

    // 2. Keyword candidates.
    if let Ok(hits) = memory.search(query, weights.keyword_candidates.max(k)) {
        let total = hits.len().max(1);
        for (i, item) in hits.iter().enumerate() {
            // Rank-based normalization in [0, 1]; rank 0 → 1.0, last → ~0.
            let s = 1.0 - (i as f32 / total as f32);
            keyword_scores.insert(item.sidecar.id.clone(), s);
        }
        for item in hits {
            candidates.entry(item.sidecar.id.clone()).or_insert(item);
        }
    }

    // 3. Recency fallback — pull in the freshest items so brand-new
    //    pre-embedding items still show up.
    if let Ok(recent) = memory.recent(weights.recent_candidates.max(k)) {
        for item in recent {
            candidates.entry(item.sidecar.id.clone()).or_insert(item);
        }
    }

    // 4. Hydrate any vector-only hits we haven't loaded yet.
    let need_ids: Vec<String> = vector_scores
        .keys()
        .filter(|id| !candidates.contains_key(*id))
        .cloned()
        .collect();
    for id in need_ids {
        if let Ok(Some(item)) = memory.get(&id) {
            candidates.insert(id, item);
        }
    }

    // 5. Score every candidate.
    let now = Utc::now();
    let half_life = weights.half_life_days.max(0.1);
    let mut scored: Vec<ScoredItem> = candidates
        .into_iter()
        // Exclude ForgottenStub tombstones and auto-generated Briefing items.
        // A briefing is a meta-summary OF memory, not a fact, and its high
        // recency would otherwise surface it in unrelated queries; the startup
        // greeting and `SEARCH: briefing` read briefings directly instead.
        .filter(|(_, item)| {
            !matches!(
                item.sidecar.kind,
                ItemKind::ForgottenStub | ItemKind::Briefing
            )
        })
        .map(|(id, item)| {
            let v = vector_scores.get(&id).copied().unwrap_or(0.0);
            let w = keyword_scores.get(&id).copied().unwrap_or(0.0);
            let relevance = v.max(w);
            let age_seconds = (now - item.sidecar.created_at).num_seconds() as f32;
            let age_days = (age_seconds / 86_400.0).max(0.0);
            let recency = (-age_days / half_life).exp();
            let importance = item.sidecar.importance.clamp(0.0, 1.0);
            let final_score =
                weights.alpha * relevance + weights.beta * recency + weights.gamma * importance;
            ScoredItem {
                item,
                final_score,
                relevance,
                recency,
                importance,
            }
        })
        .collect();

    scored.sort_by(|a, b| {
        b.final_score
            .partial_cmp(&a.final_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(k);
    // Top-5 item IDs let post-hoc log analysis cross-reference with the
    // memory directory to evaluate "did the right thing get retrieved?".
    // Item IDs are not sensitive on their own (random UUID-ish strings);
    // resolving them to content requires the on-disk sidecar.
    let top_ids: Vec<&str> = scored
        .iter()
        .take(5)
        .map(|s| s.item.sidecar.id.as_str())
        .collect();
    tracing::debug!(
        n_vector_candidates = vector_scores.len(),
        n_keyword_candidates = keyword_scores.len(),
        n_returned = scored.len(),
        top_score = scored.first().map(|s| s.final_score),
        top_relevance = scored.first().map(|s| s.relevance),
        top_recency = scored.first().map(|s| s.recency),
        top_importance = scored.first().map(|s| s.importance),
        top_ids = ?top_ids,
        "retrieve_done"
    );
    Ok(scored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::MockEmbedder;
    use crate::vector_index::VectorIndex;
    use chrono::Duration as ChronoDuration;
    use std::sync::Arc;
    use tempfile::TempDir;

    async fn build() -> (
        TempDir,
        Arc<MemoryStore>,
        Arc<MockEmbedder>,
        Arc<VectorIndex>,
    ) {
        let td = TempDir::new().unwrap();
        let mem = Arc::new(MemoryStore::open(td.path().to_path_buf()).await.unwrap());
        let emb = Arc::new(MockEmbedder::new());
        let idx =
            Arc::new(VectorIndex::open(mem.root(), emb.model_name(), emb.dimension()).unwrap());
        (td, mem, emb, idx)
    }

    async fn add_with_age(
        memory: &MemoryStore,
        embedder: &MockEmbedder,
        index: &VectorIndex,
        body: &str,
        importance: f32,
        days_old: i64,
    ) -> String {
        let sc = memory
            .add(
                body,
                ItemKind::Ingestion,
                importance,
                None,
                "".into(),
                vec![],
            )
            .await
            .unwrap();
        let item = memory.get(&sc.id).unwrap().unwrap();
        // Backdate.
        memory
            .update_item(&item, None, |s| {
                s.created_at = Utc::now() - ChronoDuration::days(days_old);
            })
            .await
            .unwrap();
        // Embed + index.
        let v = embedder.embed(body).await.unwrap();
        memory.write_vector(&item, &v).await.unwrap();
        index.upsert(&sc.id, v).unwrap();
        sc.id
    }

    #[tokio::test]
    async fn strong_match_old_still_returned() {
        let (_td, mem, emb, idx) = build().await;
        // Old item that should match the query semantically (shared tokens).
        let old_id =
            add_with_age(&mem, &emb, &idx, "dentist appointment scheduling", 0.5, 300).await;
        // Recent noise.
        let _new = add_with_age(&mem, &emb, &idx, "buy groceries milk eggs", 0.5, 0).await;

        let w = RetrievalWeights::default();
        let hits = retrieve(&mem, &*emb, &idx, &w, "dentist appointment", 5)
            .await
            .unwrap();
        let ids: Vec<_> = hits.iter().map(|s| s.item.sidecar.id.clone()).collect();
        assert!(
            ids.contains(&old_id),
            "old strong match should be in results: {ids:?}"
        );
    }

    #[tokio::test]
    async fn weak_match_new_still_returned() {
        let (_td, mem, emb, idx) = build().await;
        // Brand-new item that has no semantic relation to the query but
        // should be findable via recency.
        let new_id = add_with_age(&mem, &emb, &idx, "buy groceries milk eggs", 0.5, 0).await;
        // Old strong match.
        let _old = add_with_age(&mem, &emb, &idx, "dentist appointment scheduling", 0.5, 365).await;

        let w = RetrievalWeights::default();
        let hits = retrieve(&mem, &*emb, &idx, &w, "totally unrelated text", 5)
            .await
            .unwrap();
        let ids: Vec<_> = hits.iter().map(|s| s.item.sidecar.id.clone()).collect();
        assert!(
            ids.contains(&new_id),
            "new item should be in results via recency: {ids:?}"
        );
    }

    #[tokio::test]
    async fn weak_match_old_buried() {
        let (_td, mem, emb, idx) = build().await;
        let weak_old = add_with_age(&mem, &emb, &idx, "ancient unrelated text", 0.2, 400).await;
        let _strong_new =
            add_with_age(&mem, &emb, &idx, "dentist tuesday appointment", 0.8, 0).await;

        let w = RetrievalWeights::default();
        let hits = retrieve(&mem, &*emb, &idx, &w, "dentist appointment", 1)
            .await
            .unwrap();
        // Top hit should NOT be the weak old item.
        assert!(!hits.is_empty());
        assert_ne!(hits[0].item.sidecar.id, weak_old);
    }

    #[tokio::test]
    async fn importance_boosts_ranking() {
        let (_td, mem, emb, idx) = build().await;
        // Same body for both so vector relevance ties; same age so recency
        // ties. Use a query that does NOT match the body so keyword scores
        // are also zero for both (the rank-based keyword tiebreak would
        // otherwise dominate). Both items reach the candidate pool via the
        // recency fallback. With relevance + recency identical, importance
        // is the only differentiator.
        let body = "meeting with team about q3 plans and timeline";
        let lo = add_with_age(&mem, &emb, &idx, body, 0.05, 30).await;
        let hi = add_with_age(&mem, &emb, &idx, body, 0.95, 30).await;

        let w = RetrievalWeights::default();
        let hits = retrieve(&mem, &*emb, &idx, &w, "totally unrelated xyz topic", 5)
            .await
            .unwrap();
        let hi_rank = hits
            .iter()
            .position(|s| s.item.sidecar.id == hi)
            .expect("hi missing");
        let lo_rank = hits
            .iter()
            .position(|s| s.item.sidecar.id == lo)
            .expect("lo missing");
        assert!(
            hi_rank < lo_rank,
            "high-importance item should rank above low-importance (hi at {hi_rank}, lo at {lo_rank})"
        );
    }

    #[tokio::test]
    async fn forgotten_items_excluded() {
        let (_td, mem, emb, idx) = build().await;
        let id = add_with_age(&mem, &emb, &idx, "secret thing", 0.5, 0).await;
        mem.forget(&id).await.unwrap();
        // The forget call removed the vector from the file, but the in-memory
        // index still has it; remove from the index too as production code
        // does.
        idx.remove(&id);

        let w = RetrievalWeights::default();
        let hits = retrieve(&mem, &*emb, &idx, &w, "secret thing", 10)
            .await
            .unwrap();
        let ids: Vec<_> = hits.iter().map(|s| s.item.sidecar.id.clone()).collect();
        assert!(!ids.contains(&id));
    }

    #[tokio::test]
    async fn briefing_items_excluded() {
        let (_td, mem, emb, idx) = build().await;
        // Same body for a normal item and a Briefing item; without the
        // exclusion the briefing would be just as strong a candidate.
        let normal = add_with_age(&mem, &emb, &idx, "roof inspector follow up", 0.5, 0).await;
        let sc = mem
            .add(
                "roof inspector follow up",
                ItemKind::Briefing,
                0.1,
                None,
                "".into(),
                vec![],
            )
            .await
            .unwrap();
        let item = mem.get(&sc.id).unwrap().unwrap();
        let v = emb.embed(&item.body).await.unwrap();
        mem.write_vector(&item, &v).await.unwrap();
        idx.upsert(&sc.id, v).unwrap();

        let w = RetrievalWeights::default();
        let hits = retrieve(&mem, &*emb, &idx, &w, "roof inspector follow up", 10)
            .await
            .unwrap();
        assert!(
            !hits.iter().any(|s| s.item.sidecar.kind == ItemKind::Briefing),
            "briefing items must be excluded from retrieval"
        );
        assert!(
            hits.iter().any(|s| s.item.sidecar.id == normal),
            "the normal item should still be retrievable"
        );
    }

    #[tokio::test]
    async fn embedder_failure_falls_back_to_keyword_and_recency() {
        struct FailEmb;
        #[async_trait::async_trait]
        impl Embedder for FailEmb {
            fn dimension(&self) -> usize {
                384
            }
            fn model_name(&self) -> &str {
                "fail"
            }
            async fn embed(&self, _: &str) -> Result<Vec<f32>> {
                Err(anyhow::anyhow!("embedding broken"))
            }
        }
        let (_td, mem, _emb, idx) = build().await;
        // Add an item via the real (mock) embedder.
        let _id = add_with_age(
            &mem,
            &MockEmbedder::new(),
            &idx,
            "dentist appointment",
            0.5,
            0,
        )
        .await;

        // Now query with a failing embedder — should still return results.
        let fail = FailEmb;
        let w = RetrievalWeights::default();
        let hits = retrieve(&mem, &fail, &idx, &w, "dentist", 5).await.unwrap();
        assert!(!hits.is_empty(), "should fall back to keyword search");
    }
}
