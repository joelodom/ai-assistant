//! Google Drive worker. Read-only file search + content download.
//!
//! Auth scope (hardcoded): `https://www.googleapis.com/auth/drive.readonly`.
//! Enforced both in our trait (no write verbs exposed — only `search`) and
//! by Google at OAuth time: any create/update/delete attempt would 403,
//! because the token is scope-bound at issuance. This worker cannot change
//! your Drive.
//!
//! Mode: **search() only** (on-demand). The assistant emits
//! `SEARCH: gdrive <query>`; we run a full-text `files.list`, then for each
//! match we pull the file's *text*:
//!   - Google Docs/Sheets/Slides are **exported** to text/plain or CSV.
//!   - PDFs are downloaded and text-extracted (`pdf-extract`).
//!   - text/markdown/csv/json/xml/rtf files are downloaded as text.
//!   - images, video, archives, other binaries, and folders are skipped
//!     (the text-only Preprocessor can't inspect them, so they aren't stored).
//! Each extracted file goes through `WorkerContext::ingest_one`, so the
//! Preprocessor — not this worker — decides keep / redact / drop and assigns
//! importance before anything reaches long-term memory.
//!
//! There is deliberately no autonomous `tick()`: we don't want to silently
//! pull an entire Drive into memory. Adding a Gmail-style "new/modified
//! since last seen" poll later is straightforward (give it a `tick_interval`
//! and a `last_seen.json` cursor).

use crate::workers::oauth::OAuthClient;
use crate::workers::{RawWorkerResult, SearchEvent, Worker, WorkerContext};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};
use serde::Deserialize;
use shared::Metadata;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

pub const DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive.readonly";

/// Max files a single search will fetch + preprocess. Bounds the blast
/// radius of a broad query.
const SEARCH_FETCH_CAP: usize = 25;

/// Skip downloadable files larger than this. Keeps one giant blob from
/// dominating memory (and the export/download/extract cost).
const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;

/// Clip extracted text so one enormous document can't flood memory.
const MAX_EXTRACTED_CHARS: usize = 50_000;

pub struct GoogleDriveWorker {
    auth: Arc<OAuthClient>,
    http: reqwest::Client,
}

impl GoogleDriveWorker {
    /// Open the Drive worker. Returns Ok(None) if not yet configured
    /// (no token on disk), matching Gmail's first-run-graceful behavior.
    pub fn open(memory_root: &Path) -> Result<Option<Self>> {
        let auth = match OAuthClient::open(memory_root, "gdrive")? {
            Some(a) => Arc::new(a),
            None => return Ok(None),
        };
        let http = reqwest::Client::builder()
            .user_agent("ai-assistant/0.1 (gdrive-worker)")
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Some(Self { auth, http }))
    }

    /// Drive `files.list` with a full-text query. Excludes folders and
    /// trashed files; newest first.
    async fn list_files(&self, query: &str, limit: usize) -> Result<Vec<DriveFile>> {
        let access = self.auth.access_token().await?;
        // The Drive query language quotes string literals with single
        // quotes, so backslash-escape backslashes then single quotes.
        let escaped = query.replace('\\', "\\\\").replace('\'', "\\'");
        let q = format!(
            "fullText contains '{escaped}' and trashed = false \
             and mimeType != 'application/vnd.google-apps.folder'"
        );
        let resp: FileList = self
            .http
            .get("https://www.googleapis.com/drive/v3/files")
            .bearer_auth(&access)
            .query(&[
                ("q", q.as_str()),
                ("pageSize", &limit.to_string()),
                ("spaces", "drive"),
                ("corpora", "user"),
                (
                    "fields",
                    "files(id,name,mimeType,modifiedTime,webViewLink,size)",
                ),
                ("orderBy", "modifiedTime desc"),
            ])
            .send()
            .await
            .context("Drive files.list HTTP failed")?
            .error_for_status()
            .context("Drive files.list status")?
            .json()
            .await
            .context("Drive files.list JSON")?;
        Ok(resp.files.unwrap_or_default())
    }

    /// Pull one file's text. Returns Ok(None) for unsupported types or
    /// oversized/empty files (skipped, not an error).
    async fn fetch_file(&self, f: &DriveFile) -> Result<Option<RawWorkerResult>> {
        let mime = f.mime_type.clone().unwrap_or_default();

        // Size guard applies to downloadable (non-Google-native) files;
        // Google-native docs report no `size`.
        if let Some(sz) = f.size.as_deref().and_then(|s| s.parse::<u64>().ok()) {
            if sz > MAX_FILE_BYTES {
                return Ok(None);
            }
        }

        let access = self.auth.access_token().await?;
        let body = match extract_target(&mime) {
            ExtractKind::Export(export_mime) => {
                let url = format!(
                    "https://www.googleapis.com/drive/v3/files/{}/export",
                    urlencoding::encode(&f.id)
                );
                let text = self
                    .http
                    .get(&url)
                    .bearer_auth(&access)
                    .query(&[("mimeType", export_mime)])
                    .send()
                    .await?
                    .error_for_status()?
                    .text()
                    .await?;
                clip(text.trim(), MAX_EXTRACTED_CHARS)
            }
            ExtractKind::DownloadText => {
                let bytes = self.download_media(&f.id, &access).await?;
                clip(String::from_utf8_lossy(&bytes).trim(), MAX_EXTRACTED_CHARS)
            }
            ExtractKind::DownloadPdf => {
                let bytes = self.download_media(&f.id, &access).await?;
                // pdf-extract is synchronous and can be slow; run it off
                // the async runtime.
                let text =
                    tokio::task::spawn_blocking(move || pdf_extract::extract_text_from_mem(&bytes))
                        .await
                        .context("pdf extraction task panicked")?
                        .map_err(|e| anyhow!("pdf extraction failed: {e}"))?;
                clip(text.trim(), MAX_EXTRACTED_CHARS)
            }
            ExtractKind::Skip => return Ok(None),
        };

        if body.trim().is_empty() {
            return Ok(None);
        }

        let name = f.name.clone().unwrap_or_else(|| f.id.clone());
        let modified = f.modified_time.clone().unwrap_or_default();
        let content =
            format!("Google Drive file: {name}\nType: {mime}\nModified: {modified}\n\n{body}");
        let at = f
            .modified_time
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc));

        Ok(Some(RawWorkerResult {
            source_id: format!("gdrive:{}", f.id),
            source_url: f.web_view_link.clone(),
            content,
            at,
        }))
    }

    async fn download_media(&self, id: &str, access: &str) -> Result<Vec<u8>> {
        let url = format!(
            "https://www.googleapis.com/drive/v3/files/{}?alt=media",
            urlencoding::encode(id)
        );
        let bytes = self
            .http
            .get(&url)
            .bearer_auth(access)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        Ok(bytes.to_vec())
    }
}

#[async_trait]
impl Worker for GoogleDriveWorker {
    fn name(&self) -> &'static str {
        "gdrive"
    }

    fn description(&self) -> &'static str {
        "Searches the user's Google Drive, READ-ONLY (Google enforces the \
         drive.readonly scope server-side — this worker has no ability to \
         create, edit, move, or delete files). Full-text search across the \
         user's documents; each matching file's text is downloaded (Google \
         Docs/Sheets/Slides exported to text, PDFs and text files extracted) \
         and pushed through the Preprocessor before anything is remembered. \
         Images, video, and other binaries are skipped. Query syntax: free \
         text — words/phrases that appear in the file's name or contents."
    }

    fn is_available(&self) -> bool {
        true
    }

    async fn search(
        &self,
        query: &str,
        limit: usize,
        ctx: Arc<WorkerContext>,
        metadata: Metadata,
        tx: UnboundedSender<SearchEvent>,
    ) -> Result<()> {
        let started = std::time::Instant::now();
        tracing::debug!(
            query_len = query.chars().count(),
            limit,
            "gdrive_search_start"
        );
        let limit = limit.clamp(1, SEARCH_FETCH_CAP);

        let files = match self.list_files(query, limit).await {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.send(SearchEvent::Failed {
                    worker: self.name().to_string(),
                    error: format!("gdrive list: {e}"),
                });
                let _ = tx.send(SearchEvent::Finished {
                    worker: self.name().to_string(),
                    kept: 0,
                    dropped: 0,
                    failed: 1,
                    duration_ms: started.elapsed().as_millis() as u64,
                });
                return Err(anyhow!("gdrive list failed: {e}"));
            }
        };

        let total = files.len();
        let _ = tx.send(SearchEvent::Started {
            worker: self.name().to_string(),
            expected_total: Some(total),
            detail: Some(format!("gdrive: {total} files, fetching + preprocessing")),
        });

        let concurrency = ctx.preprocess_concurrency.max(1);
        use std::sync::atomic::{AtomicUsize, Ordering};
        let kept = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));

        stream::iter(files.into_iter())
            .map(|f| {
                let ctx = ctx.clone();
                let metadata = metadata.clone();
                let tx = tx.clone();
                let kept = kept.clone();
                let dropped = dropped.clone();
                let failed = failed.clone();
                let completed = completed.clone();
                async move {
                    match self.fetch_file(&f).await {
                        Ok(Some(raw)) => {
                            match ctx
                                .ingest_one(
                                    self.name(),
                                    &raw,
                                    metadata,
                                    crate::preprocessor::InputProvenance::Personal,
                                    &tx,
                                )
                                .await
                            {
                                Some(_) => {
                                    kept.fetch_add(1, Ordering::Relaxed);
                                }
                                None => {
                                    dropped.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                        // Unsupported / oversized / empty — skipped silently
                        // (not a failure).
                        Ok(None) => {}
                        Err(e) => {
                            failed.fetch_add(1, Ordering::Relaxed);
                            let _ = tx.send(SearchEvent::Failed {
                                worker: self.name().to_string(),
                                error: format!("gdrive fetch {}: {e}", f.id),
                            });
                        }
                    }
                    let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    let _ = tx.send(SearchEvent::Progress {
                        worker: self.name().to_string(),
                        completed: done,
                        total: Some(total),
                        detail: Some(format!("gdrive: {done}/{total}")),
                    });
                }
            })
            .buffer_unordered(concurrency)
            .for_each(|_| async {})
            .await;

        let kept = kept.load(Ordering::Relaxed);
        let dropped = dropped.load(Ordering::Relaxed);
        let failed = failed.load(Ordering::Relaxed);
        let _ = tx.send(SearchEvent::Finished {
            worker: self.name().to_string(),
            kept,
            dropped,
            failed,
            duration_ms: started.elapsed().as_millis() as u64,
        });
        tracing::info!(
            n_listed = total,
            kept,
            dropped,
            failed,
            duration_ms = started.elapsed().as_millis() as u64,
            "gdrive_search_done"
        );
        Ok(())
    }
}

/// What to do with a given Drive file's MIME type.
enum ExtractKind {
    /// Google-native doc: export to this MIME type (text).
    Export(&'static str),
    /// Binary download → UTF-8 text.
    DownloadText,
    /// Binary download → PDF text extraction.
    DownloadPdf,
    /// Unsupported (image/video/archive/folder/etc.) — don't fetch.
    Skip,
}

fn extract_target(mime: &str) -> ExtractKind {
    match mime {
        "application/vnd.google-apps.document" => ExtractKind::Export("text/plain"),
        "application/vnd.google-apps.spreadsheet" => ExtractKind::Export("text/csv"),
        "application/vnd.google-apps.presentation" => ExtractKind::Export("text/plain"),
        "application/pdf" => ExtractKind::DownloadPdf,
        "application/json" | "application/xml" | "application/rtf" => ExtractKind::DownloadText,
        m if m.starts_with("text/") => ExtractKind::DownloadText,
        _ => ExtractKind::Skip,
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}\n[…truncated after {max} chars]")
    }
}

// --- Drive API DTOs ---

#[derive(Debug, Deserialize)]
struct FileList {
    #[serde(default)]
    files: Option<Vec<DriveFile>>,
}

#[derive(Debug, Clone, Deserialize)]
struct DriveFile {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "mimeType")]
    mime_type: Option<String>,
    #[serde(default, rename = "modifiedTime")]
    modified_time: Option<String>,
    #[serde(default, rename = "webViewLink")]
    web_view_link: Option<String>,
    // Drive returns size as a decimal string; absent for Google-native docs.
    #[serde(default)]
    size: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_returns_none_when_unconfigured() {
        let td = tempfile::TempDir::new().unwrap();
        let r = GoogleDriveWorker::open(td.path()).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn extract_target_maps_types() {
        assert!(matches!(
            extract_target("application/vnd.google-apps.document"),
            ExtractKind::Export("text/plain")
        ));
        assert!(matches!(
            extract_target("application/vnd.google-apps.spreadsheet"),
            ExtractKind::Export("text/csv")
        ));
        assert!(matches!(
            extract_target("application/pdf"),
            ExtractKind::DownloadPdf
        ));
        assert!(matches!(
            extract_target("text/markdown"),
            ExtractKind::DownloadText
        ));
        assert!(matches!(extract_target("image/png"), ExtractKind::Skip));
        assert!(matches!(
            extract_target("application/vnd.google-apps.folder"),
            ExtractKind::Skip
        ));
    }

    #[test]
    fn clip_truncates_long_text() {
        let s = "x".repeat(MAX_EXTRACTED_CHARS + 50);
        let out = clip(&s, MAX_EXTRACTED_CHARS);
        assert!(out.contains("truncated"));
        assert!(out.chars().count() < MAX_EXTRACTED_CHARS + 60);
    }
}
