//! File-based memory store. Everything in here is sanitized output — raw
//! input never lands here.
//!
//! Layout:
//!   <root>/
//!     items/
//!       YYYY-MM-DD/
//!         <ulid-ish-id>.txt      # sanitized body (source of truth)
//!         <ulid-ish-id>.json     # sidecar: metadata, importance, tags
//!         <ulid-ish-id>.vec      # N × f32 packed LE (source of truth)
//!     preferences.json           # learned user preferences
//!     stubs/                     # tier-1 drop notices (content-free)
//!     embedding_model.json       # active embedding model record
//!     hnsw/                      # derived cache (rebuildable)
//!
//! Forward-compatible reads (Invariant #7): items written by older versions
//! still load. Unknown fields are tolerated; missing optional fields default
//! cleanly; the legacy `decay_stage` field is accepted and ignored.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use shared::Metadata;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::sync::RwLock;
use uuid::Uuid;
use walkdir::WalkDir;

/// Atomic write: write to a sibling temp file, fsync (best-effort), then
/// rename into place. On POSIX, rename within the same filesystem is atomic,
/// so a crash mid-write can never leave a partially-written sidecar/body.
pub async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow::anyhow!("path has no parent: {path:?}"))?;
    tokio::fs::create_dir_all(parent).await?;
    let tmp = parent.join(format!(
        ".{}.tmp.{}.{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("write"),
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    {
        let mut f = tokio::fs::File::create(&tmp).await?;
        use tokio::io::AsyncWriteExt;
        f.write_all(bytes).await?;
        let _ = f.sync_all().await;
    }
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// Synchronous version of `atomic_write`. Used by code paths that don't run
/// in an async context (e.g. VectorIndex manifest writes during graph
/// rebuild). Same atomicity story: temp file → fsync → rename.
pub fn atomic_write_sync(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow::anyhow!("path has no parent: {path:?}"))?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp.{}.{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("write"),
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        let _ = f.sync_all();
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    UserMessage,
    Ingestion,
    ScoutFinding,
    AssistantNote,
    /// The Preprocessor emitted a content-free stub for a Tier-1 (drop)
    /// input. Body describes WHAT was dropped, not the dropped content
    /// itself.
    PreprocessorStub,
    /// The Preprocessor itself failed. Raw input was dropped without
    /// inspection; body describes the failure only.
    PreprocessorError,
    /// Legacy: the same kinds as above, from before the Sanitizer → Preprocessor
    /// rename. Kept here so old items still deserialize. New items use the
    /// `Preprocessor*` variants.
    SanitizerStub,
    SanitizerError,
    /// The Assistant Core failed after the user message was already saved.
    /// Body describes the failure; the preceding user item is in memory.
    AssistantError,
    /// Self-knowledge: facts about the system itself, seeded on startup or
    /// added by the assistant during conversation.
    SelfKnowledge,
    /// Tombstone for an item the user explicitly asked to forget. The body
    /// is zeroed (`[forgotten <ts>]`); the sidecar remains for audit.
    ForgottenStub,
    /// A result fetched from a connector (e.g. Gmail, Drive, Calendar) in
    /// response to an assistant-initiated SEARCH. Body is the
    /// Preprocessor-sanitized content; tags include `connector:<name>` and
    /// the source ID.
    ConnectorFinding,
}

/// Legacy decay stage. The Curator used to advance items through these
/// stages and destructively summarize them. The Curator has been removed
/// (its mechanical jobs moved to the Indexer); this enum stays only so
/// existing on-disk items continue to deserialize cleanly (Invariant #7).
/// New code does not read or write this field.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecayStage {
    Fresh,
    Aging,
    Summarized,
    Stale,
}

impl Default for DecayStage {
    fn default() -> Self {
        DecayStage::Fresh
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sidecar {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub kind: ItemKind,
    pub importance: f32,
    /// Brief reason the Preprocessor gave for the importance score. Used in
    /// audit ("show me everything you marked important"). Optional for
    /// back-compat with items written before the field existed.
    #[serde(default)]
    pub importance_reason: Option<String>,
    /// SHA-256 of the body file as it was at write time. Optional so older
    /// sidecars (without it) still load. The Indexer can detect corruption
    /// by recomputing.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Legacy field — kept for back-compat. Defaulted on missing. New code
    /// does not read or write it; the Curator is gone.
    #[serde(default)]
    pub decay_stage: DecayStage,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub redaction_report: String,
    /// "done", "dismissed", "active"
    #[serde(default = "default_state")]
    pub state: String,
    #[serde(default)]
    pub metadata: Option<Metadata>,
}

fn default_state() -> String {
    "active".to_string()
}

#[derive(Debug, Clone)]
pub struct MemoryItem {
    pub sidecar: Sidecar,
    pub body: String,
    pub body_path: PathBuf,
    pub sidecar_path: PathBuf,
}

impl MemoryItem {
    /// Path to the `.vec` sidecar for this item (may or may not exist).
    pub fn vector_path(&self) -> PathBuf {
        self.body_path.with_extension("vec")
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Preferences {
    pub statements: Vec<PreferenceStatement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreferenceStatement {
    pub text: String,
    pub created_at: DateTime<Utc>,
}

/// Record of the embedding model that produced the `.vec` sidecars. If the
/// configured model changes, the Indexer will trigger a full re-embed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingModelRecord {
    pub model: String,
    pub dim: usize,
    pub recorded_at: DateTime<Utc>,
}

pub struct MemoryStore {
    root: PathBuf,
    prefs: RwLock<Preferences>,
}

impl MemoryStore {
    pub async fn open(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(root.join("items"))?;
        std::fs::create_dir_all(root.join("stubs"))?;
        let prefs_path = root.join("preferences.json");
        let prefs = if prefs_path.exists() {
            let text = tokio::fs::read_to_string(&prefs_path).await?;
            serde_json::from_str(&text).unwrap_or_default()
        } else {
            Preferences::default()
        };
        Ok(Self {
            root,
            prefs: RwLock::new(prefs),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Add a new item. Computes sha256 of the body at write time so future
    /// integrity checks can detect corruption.
    pub async fn add(
        &self,
        body: &str,
        kind: ItemKind,
        importance: f32,
        metadata: Option<Metadata>,
        redaction_report: String,
        tags: Vec<String>,
    ) -> Result<Sidecar> {
        self.add_with_reason(body, kind, importance, None, metadata, redaction_report, tags)
            .await
    }

    /// As `add`, but also records an `importance_reason` (typically the
    /// Preprocessor's one-line justification for the score).
    pub async fn add_with_reason(
        &self,
        body: &str,
        kind: ItemKind,
        importance: f32,
        importance_reason: Option<String>,
        metadata: Option<Metadata>,
        redaction_report: String,
        tags: Vec<String>,
    ) -> Result<Sidecar> {
        let now = Utc::now();
        let id = format!(
            "{}-{}",
            now.format("%Y%m%dT%H%M%SZ"),
            Uuid::new_v4().simple()
        );
        let day = now.format("%Y-%m-%d").to_string();
        let dir = self.root.join("items").join(&day);
        tokio::fs::create_dir_all(&dir).await?;

        let body_path = dir.join(format!("{id}.txt"));
        let sidecar_path = dir.join(format!("{id}.json"));

        let sha = sha256_hex(body.as_bytes());

        let sidecar = Sidecar {
            id: id.clone(),
            created_at: now,
            updated_at: now,
            kind,
            importance,
            importance_reason,
            sha256: Some(sha),
            decay_stage: DecayStage::Fresh,
            tags,
            redaction_report,
            state: "active".to_string(),
            metadata,
        };
        let sidecar_json = serde_json::to_string_pretty(&sidecar)?;
        atomic_write(&body_path, body.as_bytes()).await?;
        atomic_write(&sidecar_path, sidecar_json.as_bytes()).await?;
        Ok(sidecar)
    }

    pub async fn add_stub(&self, note: &str, redaction_report: String) -> Result<()> {
        let now = Utc::now();
        let id = format!(
            "{}-{}",
            now.format("%Y%m%dT%H%M%SZ"),
            Uuid::new_v4().simple()
        );
        let path = self.root.join("stubs").join(format!("{id}.json"));
        let stub = serde_json::json!({
            "id": id,
            "created_at": now,
            "note": note,
            "redaction_report": redaction_report,
        });
        atomic_write(&path, serde_json::to_string_pretty(&stub)?.as_bytes()).await?;
        Ok(())
    }

    /// Scan all sidecars. For Invariant #7, gracefully handles items whose
    /// sidecars are missing or unparseable (skipped, logged at warn).
    pub fn scan_all(&self) -> Result<Vec<MemoryItem>> {
        let mut out = Vec::new();
        let items_root = self.root.join("items");
        if !items_root.exists() {
            return Ok(out);
        }
        for entry in WalkDir::new(&items_root)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let text = match std::fs::read_to_string(p) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let sidecar: Sidecar = match serde_json::from_str(&text) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(path = %p.display(), error = %e, "skipping unreadable sidecar");
                    continue;
                }
            };
            let body_path = p.with_extension("txt");
            let body = std::fs::read_to_string(&body_path).unwrap_or_default();
            out.push(MemoryItem {
                sidecar,
                body,
                body_path,
                sidecar_path: p.to_path_buf(),
            });
        }
        out.sort_by(|a, b| a.sidecar.created_at.cmp(&b.sidecar.created_at));
        Ok(out)
    }

    pub fn recent(&self, n: usize) -> Result<Vec<MemoryItem>> {
        let mut all = self.scan_all()?;
        all.reverse();
        all.truncate(n);
        Ok(all)
    }

    /// Look up a single item by id. None if not found.
    pub fn get(&self, id: &str) -> Result<Option<MemoryItem>> {
        for item in self.scan_all()? {
            if item.sidecar.id == id {
                return Ok(Some(item));
            }
        }
        Ok(None)
    }

    /// Substring keyword search across body + tags. Kept alongside vector
    /// retrieval so hybrid scoring can use it as the keyword leg.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryItem>> {
        let q = query.to_lowercase();
        let mut hits: Vec<(usize, MemoryItem)> = Vec::new();
        for item in self.scan_all()? {
            let body_lc = item.body.to_lowercase();
            let mut score = 0usize;
            for term in q.split_whitespace() {
                if body_lc.contains(term) {
                    score += 2;
                }
                if item.sidecar.tags.iter().any(|t| t.to_lowercase().contains(term)) {
                    score += 3;
                }
            }
            if score > 0 {
                hits.push((score, item));
            }
        }
        hits.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(hits.into_iter().take(limit).map(|(_, i)| i).collect())
    }

    pub async fn update_item(
        &self,
        item: &MemoryItem,
        new_body: Option<&str>,
        mutate: impl FnOnce(&mut Sidecar),
    ) -> Result<()> {
        let mut sidecar = item.sidecar.clone();
        mutate(&mut sidecar);
        sidecar.updated_at = Utc::now();
        if let Some(b) = new_body {
            sidecar.sha256 = Some(sha256_hex(b.as_bytes()));
            atomic_write(&item.body_path, b.as_bytes()).await?;
        }
        atomic_write(
            &item.sidecar_path,
            serde_json::to_string_pretty(&sidecar)?.as_bytes(),
        )
        .await?;
        Ok(())
    }

    /// Explicit forget. Tombstones the item: body becomes
    /// `[forgotten <ts>]`, sidecar kind becomes `ForgottenStub`, vector
    /// sidecar deleted. The sidecar metadata is preserved (with the original
    /// kind moved to a tag) so an audit ("what did I forget?") still finds
    /// it. Reversible only from backup.
    pub async fn forget(&self, item_id: &str) -> Result<bool> {
        let Some(item) = self.get(item_id)? else {
            return Ok(false);
        };
        let now = Utc::now();
        let tombstone_body = format!("[forgotten {}]", now.to_rfc3339());
        let original_kind_tag = format!("forgotten-from:{:?}", item.sidecar.kind);
        let mut new_tags = item.sidecar.tags.clone();
        if !new_tags.iter().any(|t| t == &original_kind_tag) {
            new_tags.push(original_kind_tag);
        }
        if !new_tags.iter().any(|t| t == "forgotten") {
            new_tags.push("forgotten".to_string());
        }
        self.update_item(&item, Some(&tombstone_body), |s| {
            s.kind = ItemKind::ForgottenStub;
            s.tags = new_tags;
            s.importance = 0.0;
            s.redaction_report = format!("forgotten at user request on {}", now.to_rfc3339());
        })
        .await?;
        // Delete the .vec sidecar if present.
        let vec_path = item.vector_path();
        if vec_path.exists() {
            let _ = tokio::fs::remove_file(&vec_path).await;
        }
        Ok(true)
    }

    /// Write a `.vec` sidecar atomically for an item. Vectors are packed
    /// little-endian f32. Called by the Indexer (and the WS handler for
    /// fresh items if we ever inline-embed).
    pub async fn write_vector(&self, item: &MemoryItem, vector: &[f32]) -> Result<()> {
        let bytes = vector_to_bytes(vector);
        atomic_write(&item.vector_path(), &bytes).await
    }

    /// Read a `.vec` sidecar. Returns None if the file is missing or
    /// truncated to a non-multiple of 4 bytes.
    pub fn read_vector(&self, item: &MemoryItem) -> Option<Vec<f32>> {
        let path = item.vector_path();
        let bytes = std::fs::read(&path).ok()?;
        bytes_to_vector(&bytes)
    }

    /// Iterate all items that DO have a `.vec` sidecar. Useful for warming
    /// the VectorIndex on startup.
    pub fn items_with_vectors(&self) -> Result<Vec<(MemoryItem, Vec<f32>)>> {
        let mut out = Vec::new();
        for item in self.scan_all()? {
            if let Some(v) = self.read_vector(&item) {
                out.push((item, v));
            }
        }
        Ok(out)
    }

    /// Iterate all items that do NOT have a `.vec` sidecar. Used by the
    /// Indexer for backfill.
    pub fn items_missing_vectors(&self) -> Result<Vec<MemoryItem>> {
        let mut out = Vec::new();
        for item in self.scan_all()? {
            if !item.vector_path().exists()
                && item.sidecar.kind != ItemKind::ForgottenStub
            {
                out.push(item);
            }
        }
        Ok(out)
    }

    /// Read the embedding model record, if any.
    pub fn read_embedding_model(&self) -> Option<EmbeddingModelRecord> {
        let path = self.root.join("embedding_model.json");
        let bytes = std::fs::read(&path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Write the embedding model record. If the model name or dim has
    /// changed since the previous record, callers should treat existing
    /// `.vec` sidecars as stale and trigger re-embedding.
    pub async fn write_embedding_model(&self, model: &str, dim: usize) -> Result<()> {
        let record = EmbeddingModelRecord {
            model: model.to_string(),
            dim,
            recorded_at: Utc::now(),
        };
        let path = self.root.join("embedding_model.json");
        atomic_write(&path, serde_json::to_vec_pretty(&record)?.as_slice()).await
    }

    pub async fn preferences(&self) -> Preferences {
        self.prefs.read().await.clone()
    }

    pub async fn add_preference(&self, text: &str) -> Result<()> {
        let mut g = self.prefs.write().await;
        g.statements.push(PreferenceStatement {
            text: text.to_string(),
            created_at: Utc::now(),
        });
        let path = self.root.join("preferences.json");
        atomic_write(&path, serde_json::to_string_pretty(&*g)?.as_bytes()).await?;
        Ok(())
    }

    pub fn stats(&self) -> HashMap<&'static str, usize> {
        let mut out: HashMap<&'static str, usize> = HashMap::new();
        if let Ok(all) = self.scan_all() {
            for item in &all {
                let k = match item.sidecar.kind {
                    ItemKind::UserMessage => "user_message",
                    ItemKind::Ingestion => "ingestion",
                    ItemKind::ScoutFinding => "scout_finding",
                    ItemKind::ConnectorFinding => "connector_finding",
                    ItemKind::AssistantNote => "assistant_note",
                    ItemKind::PreprocessorStub | ItemKind::SanitizerStub => "preprocessor_stub",
                    ItemKind::PreprocessorError | ItemKind::SanitizerError => "preprocessor_error",
                    ItemKind::AssistantError => "assistant_error",
                    ItemKind::SelfKnowledge => "self_knowledge",
                    ItemKind::ForgottenStub => "forgotten",
                };
                *out.entry(k).or_insert(0) += 1;
            }
            out.insert("total", all.len());
            let with_vec = all
                .iter()
                .filter(|i| i.vector_path().exists())
                .count();
            out.insert("with_vector", with_vec);
        }
        out
    }
}

/// SHA-256 → lowercase hex. Used for body integrity field.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    hex_lower(&digest)
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

/// Pack a vector as little-endian f32 bytes.
pub fn vector_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Unpack little-endian f32 bytes into a vector. Returns None on bad length.
pub fn bytes_to_vector(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn fresh_store() -> (TempDir, MemoryStore) {
        let td = TempDir::new().unwrap();
        let store = MemoryStore::open(td.path().to_path_buf()).await.unwrap();
        (td, store)
    }

    #[tokio::test]
    async fn add_and_recent_roundtrip() {
        let (_td, store) = fresh_store().await;
        store
            .add("first item", ItemKind::UserMessage, 0.5, None, "".into(), vec![])
            .await
            .unwrap();
        store
            .add("second item", ItemKind::Ingestion, 0.5, None, "".into(), vec!["email".into()])
            .await
            .unwrap();
        let r = store.recent(10).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].body, "second item");
        assert_eq!(r[1].body, "first item");
    }

    #[tokio::test]
    async fn sha256_is_recorded_on_add() {
        let (_td, store) = fresh_store().await;
        let sc = store
            .add("hello world", ItemKind::Ingestion, 0.5, None, "".into(), vec![])
            .await
            .unwrap();
        let expected = sha256_hex(b"hello world");
        assert_eq!(sc.sha256, Some(expected));
    }

    #[tokio::test]
    async fn search_finds_by_body_and_tags() {
        let (_td, store) = fresh_store().await;
        store
            .add("dentist appointment on Tuesday", ItemKind::UserMessage, 0.5, None, "".into(), vec![])
            .await
            .unwrap();
        store
            .add("car maintenance receipt", ItemKind::Ingestion, 0.5, None, "".into(), vec!["car".into()])
            .await
            .unwrap();
        let hits = store.search("dentist", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].body.contains("dentist"));

        let hits = store.search("car", 10).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn store_survives_drop_and_reopen() {
        let td = TempDir::new().unwrap();
        let path = td.path().to_path_buf();

        let before_sidecar = {
            let s = MemoryStore::open(path.clone()).await.unwrap();
            s.add("first", ItemKind::UserMessage, 0.5, None, "".into(), vec!["a".into()])
                .await
                .unwrap();
            let sc = s
                .add("second", ItemKind::Ingestion, 0.7, None, "rep".into(), vec!["b".into()])
                .await
                .unwrap();
            s.add_preference("don't tell me about sports").await.unwrap();
            s.add_stub("dropped a 2FA message", "OTP".into()).await.unwrap();
            sc
        };

        let after = MemoryStore::open(path).await.unwrap();
        let items = after.recent(10).unwrap();
        assert_eq!(items.len(), 2);
        let matched = items.iter().find(|i| i.sidecar.id == before_sidecar.id).unwrap();
        assert_eq!(matched.sidecar.importance, 0.7);
        assert!(matched.sidecar.tags.contains(&"b".to_string()));
    }

    #[tokio::test]
    async fn forget_zeros_body_and_tombstones() {
        let (_td, store) = fresh_store().await;
        let sc = store
            .add("private note", ItemKind::Ingestion, 0.6, None, "".into(), vec!["personal".into()])
            .await
            .unwrap();
        let ok = store.forget(&sc.id).await.unwrap();
        assert!(ok);
        let item = store.get(&sc.id).unwrap().unwrap();
        assert_eq!(item.sidecar.kind, ItemKind::ForgottenStub);
        assert!(item.body.starts_with("[forgotten"));
        assert!(item.sidecar.tags.contains(&"forgotten".to_string()));
        assert_eq!(item.sidecar.importance, 0.0);
    }

    #[tokio::test]
    async fn forget_is_idempotent_and_returns_false_for_unknown() {
        let (_td, store) = fresh_store().await;
        let ok = store.forget("nope-not-an-id").await.unwrap();
        assert!(!ok);
    }

    #[tokio::test]
    async fn forget_removes_vec_sidecar() {
        let (_td, store) = fresh_store().await;
        let sc = store
            .add("data", ItemKind::Ingestion, 0.5, None, "".into(), vec![])
            .await
            .unwrap();
        let item = store.get(&sc.id).unwrap().unwrap();
        store.write_vector(&item, &[0.1, 0.2, 0.3]).await.unwrap();
        assert!(item.vector_path().exists());
        store.forget(&sc.id).await.unwrap();
        let item = store.get(&sc.id).unwrap().unwrap();
        assert!(!item.vector_path().exists());
    }

    #[tokio::test]
    async fn vector_roundtrip() {
        let (_td, store) = fresh_store().await;
        let sc = store
            .add("data", ItemKind::Ingestion, 0.5, None, "".into(), vec![])
            .await
            .unwrap();
        let item = store.get(&sc.id).unwrap().unwrap();
        let v = vec![1.0f32, -2.5, 3.25, 0.0, 1e-3];
        store.write_vector(&item, &v).await.unwrap();
        let item = store.get(&sc.id).unwrap().unwrap();
        let v2 = store.read_vector(&item).unwrap();
        assert_eq!(v, v2);
    }

    #[tokio::test]
    async fn items_missing_vectors_excludes_forgotten_and_with_vectors() {
        let (_td, store) = fresh_store().await;
        let a = store
            .add("a", ItemKind::Ingestion, 0.5, None, "".into(), vec![])
            .await
            .unwrap();
        let b = store
            .add("b", ItemKind::Ingestion, 0.5, None, "".into(), vec![])
            .await
            .unwrap();
        let c = store
            .add("c", ItemKind::Ingestion, 0.5, None, "".into(), vec![])
            .await
            .unwrap();
        let item_a = store.get(&a.id).unwrap().unwrap();
        store.write_vector(&item_a, &[0.1, 0.2]).await.unwrap();
        store.forget(&c.id).await.unwrap();
        let missing = store.items_missing_vectors().unwrap();
        let missing_ids: Vec<String> = missing.iter().map(|i| i.sidecar.id.clone()).collect();
        assert!(missing_ids.contains(&b.id));
        assert!(!missing_ids.contains(&a.id));
        assert!(!missing_ids.contains(&c.id));
    }

    #[tokio::test]
    async fn legacy_sidecar_with_decay_stage_still_loads() {
        // Invariant #7: a sidecar JSON written by an older version (with
        // `decay_stage` and without `sha256`/`importance_reason`) must
        // continue to deserialize cleanly.
        let td = TempDir::new().unwrap();
        let root = td.path().to_path_buf();
        std::fs::create_dir_all(root.join("items/2025-01-01")).unwrap();
        let legacy = serde_json::json!({
            "id": "20250101T120000Z-abc",
            "created_at": "2025-01-01T12:00:00Z",
            "updated_at": "2025-01-01T12:00:00Z",
            "kind": "ingestion",
            "importance": 0.4,
            "decay_stage": "aging",
            "tags": ["legacy"],
            "redaction_report": "",
            "state": "active"
        });
        std::fs::write(
            root.join("items/2025-01-01/20250101T120000Z-abc.json"),
            serde_json::to_vec_pretty(&legacy).unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.join("items/2025-01-01/20250101T120000Z-abc.txt"),
            "old body",
        )
        .unwrap();
        let store = MemoryStore::open(root).await.unwrap();
        let items = store.scan_all().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].body, "old body");
        assert_eq!(items[0].sidecar.sha256, None);
        assert_eq!(items[0].sidecar.importance_reason, None);
    }

    #[tokio::test]
    async fn legacy_sanitizer_kind_still_loads() {
        // Invariant #7: items with the old `sanitizer_stub` / `sanitizer_error`
        // kind values must continue to load.
        let td = TempDir::new().unwrap();
        let root = td.path().to_path_buf();
        std::fs::create_dir_all(root.join("items/2025-01-01")).unwrap();
        for kind_str in &["sanitizer_stub", "sanitizer_error"] {
            let sc = serde_json::json!({
                "id": format!("legacy-{kind_str}"),
                "created_at": "2025-01-01T12:00:00Z",
                "updated_at": "2025-01-01T12:00:00Z",
                "kind": kind_str,
                "importance": 0.3,
                "tags": []
            });
            std::fs::write(
                root.join(format!("items/2025-01-01/legacy-{kind_str}.json")),
                serde_json::to_vec_pretty(&sc).unwrap(),
            )
            .unwrap();
            std::fs::write(
                root.join(format!("items/2025-01-01/legacy-{kind_str}.txt")),
                "legacy body",
            )
            .unwrap();
        }
        let store = MemoryStore::open(root).await.unwrap();
        let items = store.scan_all().unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn vector_byte_roundtrip() {
        let v = vec![1.0f32, -2.0, 3.5, 0.0];
        let b = vector_to_bytes(&v);
        assert_eq!(b.len(), v.len() * 4);
        let v2 = bytes_to_vector(&b).unwrap();
        assert_eq!(v, v2);
    }

    #[test]
    fn bytes_to_vector_rejects_bad_length() {
        assert!(bytes_to_vector(&[1, 2, 3]).is_none());
    }
}
