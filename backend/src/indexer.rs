//! The Indexer. Mechanical maintenance worker for the vector store.
//! NO LLM calls — replaces the old Curator (which did destructive
//! LLM-mediated summarization). Jobs:
//!
//!  1. **Embedding backfill** — find items with no `.vec` sidecar, embed
//!     them via the local `Embedder`, write sidecars atomically, upsert
//!     into the vector index.
//!  2. **Model change handling** — if the active embedder's model name has
//!     changed since the last `embedding_model.json` record, treat all
//!     existing vectors as stale and re-embed.
//!  3. **Manifest checkpoint** — periodically save the vector index
//!     manifest so cold-start can verify it's in sync with sidecars.
//!  4. **Stats snapshot** — log counts for observability.
//!
//! All jobs are idempotent and crash-safe. If the process dies mid-backfill,
//! the next tick picks up where it left off (the file system is the
//! checkpoint).

use crate::config::IndexerCfg;
use crate::embedder::Embedder;
use crate::memory::MemoryStore;
use crate::vector_index::VectorIndex;
use std::sync::Arc;
use std::time::Duration;

pub struct Indexer {
    pub memory: Arc<MemoryStore>,
    pub embedder: Arc<dyn Embedder>,
    pub vector_index: Arc<VectorIndex>,
    pub cfg: IndexerCfg,
}

impl Indexer {
    pub fn spawn(self) {
        if !self.cfg.enabled {
            tracing::info!("indexer disabled");
            return;
        }
        tokio::spawn(async move { self.run().await });
    }

    async fn run(self) {
        // Run one tick immediately on startup to warm the index, then enter
        // the steady-state loop. This means a fresh install with prior data
        // gets a fast cold-start backfill.
        let interval = Duration::from_secs(self.cfg.interval_minutes.saturating_mul(60).max(60));
        tracing::info!(?interval, batch = self.cfg.batch_size, "indexer: running");
        loop {
            if let Err(e) = self.tick().await {
                tracing::warn!(error = %e, "indexer tick failed");
            }
            tokio::time::sleep(interval).await;
        }
    }

    pub async fn tick(&self) -> anyhow::Result<()> {
        // 1. Check model change. If the configured embedder's model differs
        //    from the recorded one, every existing vector is now stale.
        //    Clear the index in-memory and wipe `.vec` sidecars so the next
        //    backfill regenerates them with the new model. The bodies/
        //    metadata are untouched.
        let current_record = self.memory.read_embedding_model();
        let current_model = self.embedder.model_name().to_string();
        let current_dim = self.embedder.dimension();
        let model_changed = match &current_record {
            Some(r) => r.model != current_model || r.dim != current_dim,
            None => false,
        };
        if model_changed {
            tracing::warn!(
                old_model = %current_record.as_ref().map(|r| r.model.clone()).unwrap_or_default(),
                new_model = %current_model,
                "indexer: embedder model changed; re-embedding all items"
            );
            self.vector_index.clear();
            for item in self.memory.scan_all().unwrap_or_default() {
                let p = item.vector_path();
                if p.exists() {
                    let _ = tokio::fs::remove_file(&p).await;
                }
            }
            self.memory
                .write_embedding_model(&current_model, current_dim)
                .await
                .ok();
        } else if current_record.is_none() {
            // First run — record the active model.
            self.memory
                .write_embedding_model(&current_model, current_dim)
                .await
                .ok();
        }

        // 2. Backfill missing vectors in batches so we don't burn the
        //    embedder on a huge import all at once.
        let missing = self.memory.items_missing_vectors().unwrap_or_default();
        let batch_size = self.cfg.batch_size.max(1);
        let mut backfilled = 0usize;
        let mut failed = 0usize;
        for item in missing.iter().take(batch_size) {
            // Skip empty bodies — embedding an empty string is meaningless.
            if item.body.trim().is_empty() {
                continue;
            }
            match self.embedder.embed(&item.body).await {
                Ok(v) => {
                    if let Err(e) = self.memory.write_vector(item, &v).await {
                        tracing::warn!(error = %e, id = %item.sidecar.id, "indexer: write_vector failed");
                        failed += 1;
                        continue;
                    }
                    if let Err(e) = self.vector_index.upsert(&item.sidecar.id, v) {
                        tracing::warn!(error = %e, id = %item.sidecar.id, "indexer: upsert failed");
                        failed += 1;
                        continue;
                    }
                    backfilled += 1;
                }
                Err(e) => {
                    tracing::warn!(error = %e, id = %item.sidecar.id, "indexer: embed failed");
                    failed += 1;
                }
            }
        }

        // 3. Checkpoint manifest.
        if backfilled > 0 || model_changed {
            let _ = self.vector_index.save_manifest();
        }

        // 4. Stats snapshot.
        let stats = self.memory.stats();
        tracing::info!(
            backfilled,
            failed,
            remaining = missing.len().saturating_sub(backfilled),
            total_items = stats.get("total").copied().unwrap_or(0),
            with_vector = stats.get("with_vector").copied().unwrap_or(0),
            index_size = self.vector_index.len(),
            "indexer tick"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::MockEmbedder;
    use crate::memory::ItemKind;
    use tempfile::TempDir;

    fn cfg() -> IndexerCfg {
        IndexerCfg {
            enabled: true,
            interval_minutes: 5,
            batch_size: 100,
        }
    }

    #[tokio::test]
    async fn backfill_writes_vec_and_upserts_index() {
        let td = TempDir::new().unwrap();
        let mem = Arc::new(MemoryStore::open(td.path().to_path_buf()).await.unwrap());
        let emb: Arc<dyn Embedder> = Arc::new(MockEmbedder::new());
        let idx =
            Arc::new(VectorIndex::open(mem.root(), emb.model_name(), emb.dimension()).unwrap());

        let sc = mem
            .add(
                "hello world",
                ItemKind::Ingestion,
                0.5,
                None,
                "".into(),
                vec![],
            )
            .await
            .unwrap();
        assert!(idx.is_empty());
        assert!(!mem.get(&sc.id).unwrap().unwrap().vector_path().exists());

        let indexer = Indexer {
            memory: mem.clone(),
            embedder: emb.clone(),
            vector_index: idx.clone(),
            cfg: cfg(),
        };
        indexer.tick().await.unwrap();

        assert_eq!(idx.len(), 1);
        assert!(mem.get(&sc.id).unwrap().unwrap().vector_path().exists());
    }

    #[tokio::test]
    async fn backfill_skips_forgotten_items() {
        let td = TempDir::new().unwrap();
        let mem = Arc::new(MemoryStore::open(td.path().to_path_buf()).await.unwrap());
        let emb: Arc<dyn Embedder> = Arc::new(MockEmbedder::new());
        let idx =
            Arc::new(VectorIndex::open(mem.root(), emb.model_name(), emb.dimension()).unwrap());

        let sc = mem
            .add("private", ItemKind::Ingestion, 0.5, None, "".into(), vec![])
            .await
            .unwrap();
        mem.forget(&sc.id).await.unwrap();

        let indexer = Indexer {
            memory: mem.clone(),
            embedder: emb.clone(),
            vector_index: idx.clone(),
            cfg: cfg(),
        };
        indexer.tick().await.unwrap();
        assert!(idx.is_empty());
    }

    #[tokio::test]
    async fn model_change_triggers_re_embed() {
        let td = TempDir::new().unwrap();
        let mem = Arc::new(MemoryStore::open(td.path().to_path_buf()).await.unwrap());
        let emb: Arc<dyn Embedder> = Arc::new(MockEmbedder::new());
        let idx =
            Arc::new(VectorIndex::open(mem.root(), emb.model_name(), emb.dimension()).unwrap());

        // Record an OLD model name on disk to simulate a model change.
        mem.write_embedding_model("old-model-v0", 384)
            .await
            .unwrap();

        let sc = mem
            .add("hi", ItemKind::Ingestion, 0.5, None, "".into(), vec![])
            .await
            .unwrap();
        let item = mem.get(&sc.id).unwrap().unwrap();
        // Pre-populate a vector + index so we can verify they're wiped + re-built.
        mem.write_vector(&item, &vec![0.0; 384]).await.unwrap();
        idx.upsert(&sc.id, vec![0.0; 384]).unwrap();

        let indexer = Indexer {
            memory: mem.clone(),
            embedder: emb.clone(),
            vector_index: idx.clone(),
            cfg: cfg(),
        };
        indexer.tick().await.unwrap();

        // After re-embed, the record should match the new model and the
        // vector should exist (re-written).
        let rec = mem.read_embedding_model().unwrap();
        assert_eq!(rec.model, "mock-bag-of-words-v1");
        assert!(mem.get(&sc.id).unwrap().unwrap().vector_path().exists());
    }

    #[tokio::test]
    async fn second_tick_is_no_op_when_caught_up() {
        let td = TempDir::new().unwrap();
        let mem = Arc::new(MemoryStore::open(td.path().to_path_buf()).await.unwrap());
        let emb: Arc<dyn Embedder> = Arc::new(MockEmbedder::new());
        let idx =
            Arc::new(VectorIndex::open(mem.root(), emb.model_name(), emb.dimension()).unwrap());

        mem.add("x", ItemKind::Ingestion, 0.5, None, "".into(), vec![])
            .await
            .unwrap();

        let indexer = Indexer {
            memory: mem.clone(),
            embedder: emb.clone(),
            vector_index: idx.clone(),
            cfg: cfg(),
        };
        indexer.tick().await.unwrap();
        let n_after_first = idx.len();
        indexer.tick().await.unwrap();
        assert_eq!(idx.len(), n_after_first);
    }
}
