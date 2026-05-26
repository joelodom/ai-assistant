//! File-based memory store. Everything in here is sanitized output — raw
//! input never lands here.
//!
//! Layout:
//!   <root>/
//!     items/
//!       YYYY-MM-DD/
//!         <ulid-ish-id>.txt      # sanitized body
//!         <ulid-ish-id>.json     # sidecar: metadata, importance, tags, state
//!     preferences.json           # learned user preferences
//!     stubs/                     # tier-1 drop notices (content-free)
//!
//! The index is "just rescan the sidecars" — the prototype is small enough
//! that this is fine, and it keeps the on-disk format trivial to inspect.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shared::Metadata;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::sync::RwLock;
use uuid::Uuid;
use walkdir::WalkDir;

/// Atomic write: write to a sibling temp file, fsync (best-effort), then
/// rename into place. On POSIX, rename within the same filesystem is atomic,
/// so a crash mid-write can never leave a partially-written sidecar/body.
async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow::anyhow!("path has no parent: {path:?}"))?;
    tokio::fs::create_dir_all(parent).await?;
    let tmp = parent.join(format!(
        ".{}.tmp.{}.{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("write"),
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    // Write + fsync the temp file.
    {
        let mut f = tokio::fs::File::create(&tmp).await?;
        use tokio::io::AsyncWriteExt;
        f.write_all(bytes).await?;
        // Best-effort sync; ignore errors on filesystems that don't support it.
        let _ = f.sync_all().await;
    }
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    UserMessage,
    Ingestion,
    ScoutFinding,
    AssistantNote,
    SanitizerStub,
    /// The Sanitizer itself failed (e.g. out of tokens, CLI not found,
    /// timeout). Body describes the failure; raw user input is NOT here —
    /// it was dropped without inspection per the ephemerality invariant.
    SanitizerError,
    /// The Assistant Core failed after the user message was already saved.
    /// Body describes the failure; the preceding user item is in memory.
    AssistantError,
    /// Self-knowledge: facts about the system itself, seeded on startup or
    /// added by the assistant during conversation. Searchable like any
    /// other memory.
    SelfKnowledge,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DecayStage {
    Fresh,
    Aging,
    Summarized,
    Stale,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sidecar {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub kind: ItemKind,
    pub importance: f32,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Preferences {
    /// Free-form user statements ("don't tell me about crypto news") collected
    /// over time. We keep the original phrasing and a timestamp.
    pub statements: Vec<PreferenceStatement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreferenceStatement {
    pub text: String,
    pub created_at: DateTime<Utc>,
}

pub struct MemoryStore {
    root: PathBuf,
    /// Coarse mutex protects preferences.json reads/writes.
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

    pub async fn add(
        &self,
        body: &str,
        kind: ItemKind,
        importance: f32,
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

        let sidecar = Sidecar {
            id: id.clone(),
            created_at: now,
            updated_at: now,
            kind,
            importance,
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

    /// Scan all sidecars. Sync because it's only used by Curator/recent which
    /// already block. For the prototype's volume this is fine.
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
                Err(_) => continue,
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

    /// Very simple substring search across body + tags. Good enough for v1;
    /// swap for a real index later if volume grows.
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
            atomic_write(&item.body_path, b.as_bytes()).await?;
        }
        atomic_write(
            &item.sidecar_path,
            serde_json::to_string_pretty(&sidecar)?.as_bytes(),
        )
        .await?;
        Ok(())
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
        let mut by_stage: HashMap<&'static str, usize> = HashMap::new();
        if let Ok(all) = self.scan_all() {
            for item in &all {
                let k = match item.sidecar.decay_stage {
                    DecayStage::Fresh => "fresh",
                    DecayStage::Aging => "aging",
                    DecayStage::Summarized => "summarized",
                    DecayStage::Stale => "stale",
                };
                *by_stage.entry(k).or_insert(0) += 1;
            }
            by_stage.insert("total", all.len());
        }
        by_stage
    }
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
        // Invariant #6: restart-safe. Simulate a hard restart by dropping
        // the store and opening a fresh one at the same path.
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
            // store dropped here
        };

        // No graceful shutdown, no flush — just open and read.
        let after = MemoryStore::open(path).await.unwrap();
        let items = after.recent(10).unwrap();
        assert_eq!(items.len(), 2);
        assert!(items.iter().any(|i| i.body == "first"));
        assert!(items.iter().any(|i| i.body == "second"));
        let prefs = after.preferences().await;
        assert_eq!(prefs.statements.len(), 1);
        // The sidecar from before should round-trip byte-identical metadata.
        let matched = items.iter().find(|i| i.sidecar.id == before_sidecar.id).unwrap();
        assert_eq!(matched.sidecar.importance, 0.7);
        assert_eq!(matched.sidecar.redaction_report, "rep");
        assert!(matched.sidecar.tags.contains(&"b".to_string()));
    }

    #[tokio::test]
    async fn preferences_persist() {
        let td = TempDir::new().unwrap();
        let path = td.path().to_path_buf();
        {
            let s = MemoryStore::open(path.clone()).await.unwrap();
            s.add_preference("stop telling me about crypto").await.unwrap();
        }
        let s2 = MemoryStore::open(path).await.unwrap();
        let p = s2.preferences().await;
        assert_eq!(p.statements.len(), 1);
        assert!(p.statements[0].text.contains("crypto"));
    }

    #[tokio::test]
    async fn stub_creates_file_without_content_leak() {
        let (td, store) = fresh_store().await;
        store
            .add_stub(
                "Received and dropped an email that appeared to be only a security message.",
                "likely 2FA code".to_string(),
            )
            .await
            .unwrap();
        let stubs: Vec<_> = std::fs::read_dir(td.path().join("stubs"))
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(stubs.len(), 1);
        let text = std::fs::read_to_string(stubs[0].path()).unwrap();
        // Body text was never passed in here — we only ever stored the stub.
        assert!(text.contains("security message"));
        assert!(!text.contains("482194")); // pretend OTP we never gave it
    }
}
