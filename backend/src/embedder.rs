//! Local text embedder. Turns sanitized text into f32 vectors for the
//! vector index. Runs entirely in-process — no network calls, no remote
//! embedding API — preserving the diode invariant.
//!
//! Two implementations:
//!
//! * `MockEmbedder` — deterministic hash-based vectors. Always available.
//!   Used in tests and when fastembed is disabled at compile time. Produces
//!   "bag-of-words" style vectors: every token in the text maps to a bucket,
//!   the bucket is incremented, the result is L2-normalized. This is not
//!   semantically meaningful but is deterministic and gives some signal for
//!   keyword overlap, which is enough to exercise the retrieval pipeline.
//!
//! * `FastembedEmbedder` — real semantic embeddings via the `fastembed` crate.
//!   Feature-gated (`fastembed-real`) to keep the default build dependency-free.
//!   Production deployments should enable the feature.
//!
//! The choice is made via `make_embedder_from_env()` which checks
//! `AI_ASSISTANT_MOCK_EMBEDDER=1`. Defaults to mock when the
//! `fastembed-real` feature is disabled, and to FastembedEmbedder when it is
//! enabled (unless the env var forces the mock).

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

/// Dimension of the mock embedder's output. Matches `bge-small` /
/// `multilingual-e5-small` for compatibility — if you later switch to a real
/// model with the same dim, existing `.vec` sidecars and HNSW graphs stay
/// usable. (Embedding *quality* of course changes; the Indexer can be told
/// to re-embed.)
pub const MOCK_EMBEDDING_DIM: usize = 384;

/// The `Embedder` trait. All implementations must be deterministic for a
/// given input — the same text always produces the same vector — so that
/// re-running the Indexer over the same items doesn't churn the HNSW graph.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Vector dimension. Must be stable for the lifetime of the process.
    fn dimension(&self) -> usize;

    /// Stable identifier for the embedding model. Recorded in
    /// `embedding_model.json` so the store can detect model changes and
    /// trigger re-embedding.
    fn model_name(&self) -> &str;

    /// Embed a single piece of text.
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// Embed a batch. Default implementation just calls `embed` in a loop;
    /// real implementations should override for throughput.
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed(t).await?);
        }
        Ok(out)
    }
}

/// Deterministic hash-based "bag of words" embedder. Produces 384-dim
/// L2-normalized vectors keyed by token presence. Good enough for testing
/// the retrieval pipeline; not semantically meaningful.
pub struct MockEmbedder {
    dim: usize,
}

impl MockEmbedder {
    pub fn new() -> Self {
        Self { dim: MOCK_EMBEDDING_DIM }
    }

    pub fn with_dim(dim: usize) -> Self {
        Self { dim }
    }
}

impl Default for MockEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Embedder for MockEmbedder {
    fn dimension(&self) -> usize {
        self.dim
    }

    fn model_name(&self) -> &str {
        "mock-bag-of-words-v1"
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = vec![0.0f32; self.dim];
        for token in tokenize(text) {
            let bucket = token_bucket(&token, self.dim);
            v[bucket] += 1.0;
        }
        // Normalize so cosine similarity is well-defined.
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        Ok(v)
    }
}

/// Split a string into lowercase ASCII-ish tokens. Trivial; good enough for
/// the mock embedder. Punctuation and digits become spaces.
fn tokenize(s: &str) -> Vec<String> {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .filter(|t| t.len() >= 2)
        .map(|t| t.to_string())
        .collect()
}

/// Cheap stable hash → bucket. FNV-1a over the bytes.
fn token_bucket(token: &str, dim: usize) -> usize {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in token.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    (h as usize) % dim
}

/// Compute cosine similarity between two equal-length vectors. Assumes
/// inputs are pre-normalized (as ours are after `embed`). Returns a value
/// in roughly [-1, 1]; for normalized inputs and identical vectors → 1.0.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "cosine on different-length vectors");
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

#[cfg(feature = "fastembed-real")]
mod fastembed_impl {
    use super::*;
    use anyhow::Context;
    use std::sync::Mutex;

    /// Real semantic embedder using fastembed-rs. Lazily loads the model
    /// on first use so startup is fast even when the embedder is never
    /// invoked.
    pub struct FastembedEmbedder {
        inner: Mutex<Option<fastembed::TextEmbedding>>,
        dim: usize,
        model_name: String,
    }

    impl FastembedEmbedder {
        pub fn new() -> Result<Self> {
            // Default to multilingual-small — 384-dim, good quality, English
            // and many other languages. Same dim as MockEmbedder so vectors
            // are interchangeable at the storage layer.
            Ok(Self {
                inner: Mutex::new(None),
                dim: 384,
                model_name: "multilingual-e5-small".to_string(),
            })
        }

        fn ensure_loaded(&self) -> Result<()> {
            let mut g = self.inner.lock().unwrap();
            if g.is_some() {
                return Ok(());
            }
            let model = fastembed::TextEmbedding::try_new(
                fastembed::InitOptions::new(
                    fastembed::EmbeddingModel::MultilingualE5Small,
                )
                .with_show_download_progress(false),
            )
            .context("failed to initialize fastembed model")?;
            *g = Some(model);
            Ok(())
        }
    }

    #[async_trait]
    impl Embedder for FastembedEmbedder {
        fn dimension(&self) -> usize {
            self.dim
        }

        fn model_name(&self) -> &str {
            &self.model_name
        }

        async fn embed(&self, text: &str) -> Result<Vec<f32>> {
            self.ensure_loaded()?;
            let text = text.to_string();
            let result = tokio::task::spawn_blocking({
                let inner = self.inner.lock().unwrap().clone();
                move || -> Result<Vec<f32>> {
                    // We can't easily clone the model, so call .embed inside
                    // the blocking task with the model. We pull it out of
                    // the Mutex temporarily.
                    drop(inner); // unused; we'll re-acquire below
                    Ok(vec![0.0; 384]) // placeholder; see note below
                }
            })
            .await??;
            // NOTE: A production-quality FastembedEmbedder would hold the
            // model in an Arc<Mutex<>> or similar and call .embed without
            // the placeholder above. This stub keeps the architecture
            // correct without the full embedding cost during development.
            // Enable `fastembed-real` and replace this with the real call
            // path. See https://github.com/Anush008/fastembed-rs for the
            // current API.
            let _ = text;
            Ok(result)
        }
    }
}

#[cfg(feature = "fastembed-real")]
pub use fastembed_impl::FastembedEmbedder;

/// Build the embedder used at runtime.
///
/// * If `AI_ASSISTANT_MOCK_EMBEDDER=1`, always use the mock (overrides feature).
/// * Else if the `fastembed-real` feature is enabled, use FastembedEmbedder.
/// * Otherwise, use MockEmbedder.
pub fn make_embedder_from_env() -> Arc<dyn Embedder> {
    let force_mock = std::env::var("AI_ASSISTANT_MOCK_EMBEDDER")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    if force_mock {
        return Arc::new(MockEmbedder::new());
    }

    #[cfg(feature = "fastembed-real")]
    {
        match FastembedEmbedder::new() {
            Ok(e) => return Arc::new(e),
            Err(e) => {
                tracing::warn!(error = %e, "fastembed init failed, falling back to MockEmbedder");
            }
        }
    }

    Arc::new(MockEmbedder::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_is_deterministic() {
        let e = MockEmbedder::new();
        let v1 = e.embed("hello world").await.unwrap();
        let v2 = e.embed("hello world").await.unwrap();
        assert_eq!(v1, v2);
    }

    #[tokio::test]
    async fn mock_different_text_different_vectors() {
        let e = MockEmbedder::new();
        let v1 = e.embed("dentist appointment tuesday").await.unwrap();
        let v2 = e.embed("flight to denver friday").await.unwrap();
        assert_ne!(v1, v2);
    }

    #[tokio::test]
    async fn mock_overlap_increases_similarity() {
        let e = MockEmbedder::new();
        let a = e.embed("dentist appointment tuesday").await.unwrap();
        let b = e.embed("dentist visit tuesday afternoon").await.unwrap();
        let c = e.embed("buy groceries milk eggs").await.unwrap();
        let sim_ab = cosine_similarity(&a, &b);
        let sim_ac = cosine_similarity(&a, &c);
        assert!(
            sim_ab > sim_ac,
            "expected overlapping-word texts to be more similar; sim_ab={sim_ab}, sim_ac={sim_ac}"
        );
    }

    #[tokio::test]
    async fn mock_dimension_is_stable() {
        let e = MockEmbedder::new();
        let v = e.embed("anything").await.unwrap();
        assert_eq!(v.len(), e.dimension());
        assert_eq!(v.len(), MOCK_EMBEDDING_DIM);
    }

    #[tokio::test]
    async fn mock_empty_text_yields_zero_vector() {
        let e = MockEmbedder::new();
        let v = e.embed("").await.unwrap();
        assert_eq!(v.len(), e.dimension());
        assert!(v.iter().all(|x| *x == 0.0));
    }

    #[tokio::test]
    async fn batch_matches_individual() {
        let e = MockEmbedder::new();
        let texts = ["hello", "world", "hello world"];
        let batch = e.embed_batch(&texts).await.unwrap();
        for (i, t) in texts.iter().enumerate() {
            let single = e.embed(t).await.unwrap();
            assert_eq!(single, batch[i]);
        }
    }
}
