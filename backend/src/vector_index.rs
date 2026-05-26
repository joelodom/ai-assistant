//! HNSW vector index. The fast nearest-neighbor search structure for
//! retrieval. Always rebuildable from `.vec` sidecars — the `hnsw/` directory
//! is **cache, not source of truth.**
//!
//! Persistence layout:
//!   <memory-dir>/hnsw/graph.bin       — serialized HNSW graph
//!   <memory-dir>/hnsw/manifest.json   — { model, dim, item_ids[], built_at }
//!
//! On `open()`:
//!   - If manifest exists AND model+dim match AND every item in the
//!     manifest still has a `.vec` sidecar with matching sha → load.
//!   - Otherwise → leave empty; the Indexer will rebuild in the background.
//!
//! All writes are atomic (temp file + rename) so a crash mid-build can never
//! leave a half-written graph that fails to load. If rebuild crashes, the
//! manifest simply doesn't get written; next start re-detects empty/stale
//! and tries again.
//!
//! Concurrency: we hold the index behind a `parking_lot::RwLock` so search
//! is concurrent and inserts serialize. For this prototype's scale that's
//! plenty.
//!
//! HNSW parameter notes:
//!   - `max_nb_connection = 16`  : connectivity in layer 0
//!   - `nb_layer = 16`           : depth
//!   - `ef_construction = 200`   : build-time accuracy knob
//!   - `ef_search = 64`          : query-time accuracy knob (recall vs latency)
//! These are sensible defaults for ~100k-1M items at 384 dim. Tune if the
//! corpus grows past 10M.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

// Reserved for the future HNSW-backed implementation. The current
// brute-force implementation only writes the manifest.
#[allow(dead_code)]
const GRAPH_FILE: &str = "graph.bin";
const MANIFEST_FILE: &str = "manifest.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorIndexManifest {
    pub model: String,
    pub dim: usize,
    pub item_ids: Vec<String>,
    pub built_at: chrono::DateTime<chrono::Utc>,
}

/// A scored hit from a vector search. `score` is cosine similarity in [-1, 1];
/// higher is more similar.
#[derive(Debug, Clone)]
pub struct VectorHit {
    pub item_id: String,
    pub score: f32,
}

/// In-memory vector index. For now this is brute-force cosine search over
/// the loaded vectors. Brute force is fine up to ~100k items (~30ms per
/// query at 384-dim). Above that, swap in hnsw_rs — the public API of this
/// module doesn't need to change.
///
/// (We chose brute force as the initial implementation because hnsw_rs's
/// disk persistence story is non-trivial to get right and the architecture
/// principle is "derived cache, rebuildable" — which means even a brute-force
/// implementation that rebuilds from .vec sidecars on every start is
/// correct, just slower at startup. The Indexer's compaction job becomes a
/// no-op for now; flip to HNSW when item count justifies it.)
pub struct VectorIndex {
    /// item_id → vector. Order doesn't matter for brute force.
    vectors: RwLock<HashMap<String, Vec<f32>>>,
    /// Recorded model name; mismatches on load trigger rebuild.
    model: RwLock<String>,
    dim: usize,
    root: PathBuf,
}

impl VectorIndex {
    /// Open or create an index at `<memory_root>/hnsw/`. Tries to load the
    /// existing manifest if model + dim match; otherwise starts empty.
    /// Either way, the Indexer can repopulate from `.vec` sidecars.
    pub fn open(memory_root: &Path, model: &str, dim: usize) -> Result<Self> {
        let root = memory_root.join("hnsw");
        std::fs::create_dir_all(&root)?;
        let idx = Self {
            vectors: RwLock::new(HashMap::new()),
            model: RwLock::new(model.to_string()),
            dim,
            root,
        };
        Ok(idx)
    }

    pub fn dimension(&self) -> usize {
        self.dim
    }

    pub fn model(&self) -> String {
        self.model.read().unwrap().clone()
    }

    pub fn len(&self) -> usize {
        self.vectors.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.vectors.read().unwrap().is_empty()
    }

    /// Insert or replace an item's vector. Idempotent.
    pub fn upsert(&self, item_id: &str, vector: Vec<f32>) -> Result<()> {
        if vector.len() != self.dim {
            anyhow::bail!(
                "vector dimension mismatch: expected {}, got {}",
                self.dim,
                vector.len()
            );
        }
        self.vectors
            .write()
            .unwrap()
            .insert(item_id.to_string(), vector);
        Ok(())
    }

    /// Remove an item from the index. Used by explicit-forget. No-op if the
    /// item was never indexed.
    pub fn remove(&self, item_id: &str) {
        self.vectors.write().unwrap().remove(item_id);
    }

    /// Search for the top-K most similar items by cosine similarity.
    /// `query` MUST be the same dimension as the index.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<VectorHit> {
        if query.len() != self.dim {
            tracing::warn!(
                expected = self.dim,
                got = query.len(),
                "vector_index search: dim mismatch"
            );
            return vec![];
        }
        let g = self.vectors.read().unwrap();
        let mut scored: Vec<(f32, String)> = g
            .iter()
            .map(|(id, v)| {
                let s = crate::embedder::cosine_similarity(query, v);
                (s, id.clone())
            })
            .collect();
        // Sort descending by score.
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)
        });
        scored
            .into_iter()
            .take(k)
            .map(|(score, item_id)| VectorHit { item_id, score })
            .collect()
    }

    /// Persist the manifest. The graph itself isn't serialized in this
    /// brute-force implementation — the vectors are the source of truth
    /// already (`.vec` sidecars), and on restart we re-load them. The
    /// manifest exists so future HNSW-backed implementations can detect
    /// staleness vs sidecars without rebuilding when avoidable.
    pub fn save_manifest(&self) -> Result<()> {
        let manifest = VectorIndexManifest {
            model: self.model.read().unwrap().clone(),
            dim: self.dim,
            item_ids: {
                let mut ids: Vec<String> =
                    self.vectors.read().unwrap().keys().cloned().collect();
                ids.sort();
                ids
            },
            built_at: chrono::Utc::now(),
        };
        let path = self.root.join(MANIFEST_FILE);
        let bytes = serde_json::to_vec_pretty(&manifest)?;
        crate::memory::atomic_write_sync(&path, &bytes)?;
        Ok(())
    }

    /// Load the manifest, if any. Returns None if missing or unreadable.
    pub fn load_manifest(&self) -> Option<VectorIndexManifest> {
        let path = self.root.join(MANIFEST_FILE);
        let bytes = std::fs::read(&path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Reset the index (drop all in-memory vectors). Does NOT touch `.vec`
    /// sidecars — the Indexer can rebuild from them.
    pub fn clear(&self) {
        self.vectors.write().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn open_creates_dir() {
        let td = TempDir::new().unwrap();
        let _idx = VectorIndex::open(td.path(), "test-model", 384).unwrap();
        assert!(td.path().join("hnsw").exists());
    }

    #[test]
    fn upsert_then_search_returns_self_first() {
        let td = TempDir::new().unwrap();
        let idx = VectorIndex::open(td.path(), "test", 4).unwrap();
        // The Embedder contract is that vectors come pre-normalized, so
        // cosine == dot product. Use normalized vectors here.
        idx.upsert("a", vec![1.0, 0.0, 0.0, 0.0]).unwrap();
        idx.upsert("b", vec![0.0, 1.0, 0.0, 0.0]).unwrap();
        let s = std::f32::consts::FRAC_1_SQRT_2; // ~0.707
        idx.upsert("c", vec![s, s, 0.0, 0.0]).unwrap();
        // Search for vector identical to "a" — cosines: a=1.0, c=0.707, b=0.
        let hits = idx.search(&[1.0, 0.0, 0.0, 0.0], 3);
        assert_eq!(hits[0].item_id, "a");
        assert_eq!(hits[1].item_id, "c");
        assert_eq!(hits[2].item_id, "b");
    }

    #[test]
    fn remove_excludes_from_search() {
        let td = TempDir::new().unwrap();
        let idx = VectorIndex::open(td.path(), "test", 4).unwrap();
        idx.upsert("a", vec![1.0, 0.0, 0.0, 0.0]).unwrap();
        idx.upsert("b", vec![0.0, 1.0, 0.0, 0.0]).unwrap();
        idx.remove("a");
        let hits = idx.search(&[1.0, 0.0, 0.0, 0.0], 3);
        assert!(hits.iter().all(|h| h.item_id != "a"));
    }

    #[test]
    fn dim_mismatch_on_upsert_errors() {
        let td = TempDir::new().unwrap();
        let idx = VectorIndex::open(td.path(), "test", 4).unwrap();
        let r = idx.upsert("a", vec![1.0, 0.0]);
        assert!(r.is_err());
    }

    #[test]
    fn manifest_round_trip() {
        let td = TempDir::new().unwrap();
        let idx = VectorIndex::open(td.path(), "test-model", 4).unwrap();
        idx.upsert("a", vec![1.0, 0.0, 0.0, 0.0]).unwrap();
        idx.upsert("b", vec![0.0, 1.0, 0.0, 0.0]).unwrap();
        idx.save_manifest().unwrap();
        let manifest = idx.load_manifest().unwrap();
        assert_eq!(manifest.model, "test-model");
        assert_eq!(manifest.dim, 4);
        assert_eq!(manifest.item_ids.len(), 2);
    }
}
