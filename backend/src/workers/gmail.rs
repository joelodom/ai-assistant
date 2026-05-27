//! Gmail worker. Read-only search + autonomous tick.
//!
//! Auth scope (hardcoded): `https://www.googleapis.com/auth/gmail.readonly`.
//! Enforced both in our trait (no write verbs exposed) and by Google
//! at OAuth time (any write attempt would 403).
//!
//! Two modes of operation:
//!
//! - **search()**: assistant-initiated. Lists matching messages, fetches
//!   each in full, streams them through `WorkerContext::ingest_one`
//!   while emitting `SearchEvent::Ingested` / `Dropped` / `Failed`
//!   along the way. Each message body goes through the Preprocessor —
//!   the Preprocessor (not the Gmail worker) decides whether to drop,
//!   redact, or pass; importance score and reason come from there too.
//!
//! - **tick()**: every minute. Lists messages newer than the last seen
//!   timestamp, fetches them, runs the same ingestion pipeline. The
//!   `last_seen.json` file in `<memory-dir>/connectors/gmail/` is the
//!   only persistent state (besides credentials), and is written
//!   atomically (Invariant #6 restart-safety). First-ever tick after
//!   setup seeds the cursor to "now" and ingests nothing — we don't
//!   want a huge backfill on first run.

use crate::workers::oauth::OAuthClient;
use crate::workers::{RawWorkerResult, SearchEvent, Worker, WorkerContext};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use base64::Engine;
use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use shared::Metadata;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

pub const GMAIL_SCOPE: &str = "https://www.googleapis.com/auth/gmail.readonly";

/// Default tick cadence. One minute matches Gmail's typical
/// inbox-pull expectation while staying well within free-tier quotas.
const DEFAULT_TICK: Duration = Duration::from_secs(60);

/// Maximum messages a single tick will fetch + preprocess. Bounds the
/// blast radius if someone forwards a 5000-message mailing-list digest
/// at us.
const TICK_FETCH_CAP: usize = 25;

pub struct GmailWorker {
    auth: Arc<OAuthClient>,
    http: reqwest::Client,
    memory_root: PathBuf,
}

impl GmailWorker {
    /// Open the Gmail worker. Returns Ok(None) if not yet configured.
    pub fn open(memory_root: &Path) -> Result<Option<Self>> {
        let auth = match OAuthClient::open(memory_root, "gmail")? {
            Some(a) => Arc::new(a),
            None => return Ok(None),
        };
        let http = reqwest::Client::builder()
            .user_agent("ai-assistant/0.1 (gmail-worker)")
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Some(Self {
            auth,
            http,
            memory_root: memory_root.to_path_buf(),
        }))
    }

    fn last_seen_path(&self) -> PathBuf {
        self.memory_root
            .join("connectors")
            .join("gmail")
            .join("last_seen.json")
    }

    fn read_last_seen(&self) -> Option<LastSeen> {
        let bytes = std::fs::read(self.last_seen_path()).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    async fn write_last_seen(&self, ls: &LastSeen) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(ls)?;
        crate::memory::atomic_write(&self.last_seen_path(), &bytes).await?;
        Ok(())
    }

    /// Hit the Gmail messages.list endpoint and return the list of IDs.
    async fn list_message_ids(&self, query: &str, limit: usize) -> Result<Vec<String>> {
        let access = self.auth.access_token().await?;
        let url = format!(
            "https://gmail.googleapis.com/gmail/v1/users/me/messages?maxResults={}&q={}",
            limit,
            urlencoding::encode(query),
        );
        let list: ListResponse = self
            .http
            .get(&url)
            .bearer_auth(&access)
            .send()
            .await
            .context("Gmail messages.list HTTP failed")?
            .error_for_status()
            .context("Gmail messages.list status")?
            .json()
            .await
            .context("Gmail messages.list JSON")?;
        Ok(list
            .messages
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.id)
            .collect())
    }

    async fn fetch_message(&self, id: &str) -> Result<RawWorkerResult> {
        let access = self.auth.access_token().await?;
        let url = format!(
            "https://gmail.googleapis.com/gmail/v1/users/me/messages/{}?format=full",
            urlencoding::encode(id)
        );
        let m: GmailMessage = self
            .http
            .get(&url)
            .bearer_auth(access)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let payload = m.payload.unwrap_or_default();
        let headers = header_map(&payload.headers);
        let from = headers.get("From").cloned().unwrap_or_default();
        let to = headers.get("To").cloned().unwrap_or_default();
        let subject = headers.get("Subject").cloned().unwrap_or_default();
        let date_header = headers.get("Date").cloned().unwrap_or_default();

        let body_text = extract_text(&payload).unwrap_or_default();
        let body = if body_text.trim().is_empty() {
            m.snippet.clone().unwrap_or_default()
        } else {
            body_text
        };

        let content =
            format!("From: {from}\nTo: {to}\nSubject: {subject}\nDate: {date_header}\n\n{body}");

        let at = m
            .internal_date
            .as_deref()
            .and_then(|s| s.parse::<i64>().ok())
            .and_then(|ms| DateTime::<Utc>::from_timestamp_millis(ms));

        Ok(RawWorkerResult {
            source_id: format!("gmail:{}", m.id),
            source_url: Some(format!("https://mail.google.com/mail/u/0/#all/{}", m.id)),
            content,
            at,
        })
    }

    /// Run the standard fetch+ingest pipeline over a list of message
    /// IDs. Fans out per-message preprocessing using the context's
    /// configured concurrency.
    async fn ingest_messages(
        &self,
        ids: Vec<String>,
        ctx: Arc<WorkerContext>,
        metadata: Metadata,
        tx: &UnboundedSender<SearchEvent>,
        provenance: crate::preprocessor::InputProvenance,
    ) -> IngestionTally {
        let total = ids.len();
        let _ = tx.send(SearchEvent::Started {
            worker: self.name().to_string(),
            expected_total: Some(total),
            detail: Some(format!("gmail: {total} messages, fetching + preprocessing")),
        });
        let concurrency = ctx.preprocess_concurrency.max(1);

        use std::sync::atomic::{AtomicUsize, Ordering};
        let kept = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));

        stream::iter(ids.into_iter())
            .map(|id| {
                let ctx = ctx.clone();
                let metadata = metadata.clone();
                let tx = tx.clone();
                let kept = kept.clone();
                let dropped = dropped.clone();
                let failed = failed.clone();
                let completed = completed.clone();
                let provenance = provenance;
                async move {
                    // Fetch step. Failures here are network/quota issues —
                    // emit a Failed event and move on rather than aborting
                    // the whole search.
                    let raw = match self.fetch_message(&id).await {
                        Ok(r) => r,
                        Err(e) => {
                            failed.fetch_add(1, Ordering::Relaxed);
                            let _ = tx.send(SearchEvent::Failed {
                                worker: self.name().to_string(),
                                error: format!("gmail fetch {id}: {e}"),
                            });
                            return;
                        }
                    };
                    // Ingest step. ingest_one emits Ingested / Dropped /
                    // Failed itself; tally locally.
                    let id_before = raw.source_id.clone();
                    match ctx
                        .ingest_one(
                            self.name(),
                            &raw,
                            metadata,
                            provenance,
                            &tx,
                        )
                        .await
                    {
                        Some(_) => {
                            kept.fetch_add(1, Ordering::Relaxed);
                        }
                        None => {
                            // dropped or failed — ingest_one already
                            // emitted the right event; we don't know
                            // which, so bump dropped as the optimistic
                            // counter and let Failed events naturally
                            // outnumber if it was a failure. (For
                            // logging only — the per-event tally on
                            // the receiving side is authoritative.)
                            dropped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    let _ = id_before;
                    let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    let _ = tx.send(SearchEvent::Progress {
                        worker: self.name().to_string(),
                        completed: done,
                        total: Some(total),
                        detail: Some(format!("gmail: {done}/{total}")),
                    });
                }
            })
            .buffer_unordered(concurrency)
            .for_each(|_| async {})
            .await;

        IngestionTally {
            total,
            kept: kept.load(Ordering::Relaxed),
            dropped: dropped.load(Ordering::Relaxed),
            failed: failed.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct IngestionTally {
    total: usize,
    kept: usize,
    dropped: usize,
    failed: usize,
}

#[async_trait]
impl Worker for GmailWorker {
    fn name(&self) -> &'static str {
        "gmail"
    }

    fn description(&self) -> &'static str {
        "Searches the user's Gmail account, READ-ONLY (Google enforces the \
         scope server-side — this worker has no ability to send, delete, \
         or modify mail). Also runs autonomously: polls for new mail every \
         minute and pushes each new message through the Preprocessor so \
         the Preprocessor can decide what to keep (full body, summary, or \
         drop) before anything reaches long-term memory. \
         Search query syntax: `from:dr.patel`, `subject:invoice`, \
         `before:2024/06/01 after:2024/05/01`, `has:attachment`, free \
         text, or any combination."
    }

    fn is_available(&self) -> bool {
        true
    }

    fn tick_interval(&self) -> Option<Duration> {
        Some(DEFAULT_TICK)
    }

    async fn tick(&self, ctx: Arc<WorkerContext>) -> Result<()> {
        let now = Utc::now();
        let last = self.read_last_seen();

        // First tick after setup: seed the cursor to "now" and ingest
        // nothing. We don't want to back-fill the entire mailbox.
        let cutoff = match last {
            Some(ls) => ls.cutoff,
            None => {
                tracing::info!("gmail tick: seeding last_seen cursor, no backfill");
                self.write_last_seen(&LastSeen { cutoff: now }).await?;
                return Ok(());
            }
        };

        // Gmail's `after:` takes a date (YYYY/MM/DD). To get strictly
        // new mail we use the unix timestamp form: `after:<seconds>`.
        let query = format!("after:{}", cutoff.timestamp());
        let ids = self.list_message_ids(&query, TICK_FETCH_CAP).await?;

        if ids.is_empty() {
            tracing::debug!("gmail tick: no new messages since {cutoff}");
            self.write_last_seen(&LastSeen { cutoff: now }).await?;
            return Ok(());
        }

        // Synthesize a metadata for ingested items — autonomous tick
        // has no user request to borrow from. "now" + no geolocation.
        let metadata = Metadata {
            datetime_iso: now.to_rfc3339(),
            geolocation: None,
            freeform: serde_json::json!({
                "worker": self.name(),
                "via": "tick",
            }),
        };

        // Throw a sender into a void — we don't have a status channel
        // for background ingestion, just logs. But ingest_one wants a
        // channel, so give it one and drain it inline.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SearchEvent>();
        let drain = tokio::spawn(async move {
            let mut kept = 0;
            let mut dropped = 0;
            let mut failed = 0;
            while let Some(ev) = rx.recv().await {
                match ev {
                    SearchEvent::Ingested { .. } => kept += 1,
                    SearchEvent::Dropped { .. } => dropped += 1,
                    SearchEvent::Failed { error, .. } => {
                        tracing::warn!(error = %error, "gmail tick: ingest failed");
                        failed += 1;
                    }
                    _ => {}
                }
            }
            (kept, dropped, failed)
        });

        let started = std::time::Instant::now();
        let tally = self
            .ingest_messages(
                ids,
                ctx,
                metadata,
                &tx,
                // Tick-ingested mail is the user's own data arriving
                // via an external API. Use Personal provenance so the
                // Preprocessor applies the personal-data ruleset rather
                // than the more aggressive PublicWeb one.
                crate::preprocessor::InputProvenance::Personal,
            )
            .await;
        drop(tx);
        let (kept, dropped, failed) = drain.await.unwrap_or((0, 0, 0));

        tracing::info!(
            n_listed = tally.total,
            kept,
            dropped,
            failed,
            duration_ms = started.elapsed().as_millis() as u64,
            "gmail_tick_ingest_done"
        );

        // Always advance the cursor. If a future tick re-ingests a
        // message because Gmail's indexing was slow, ingest_one's
        // memory.add_with_reason path will store it again with a fresh
        // item id — duplication is recoverable, but losing the cursor
        // and re-ingesting the whole mailbox is not.
        self.write_last_seen(&LastSeen { cutoff: now }).await?;
        Ok(())
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
        tracing::debug!(query_len = query.len(), limit, "gmail_search_start");
        let limit = limit.clamp(1, 25);
        let ids = match self.list_message_ids(query, limit).await {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.send(SearchEvent::Failed {
                    worker: self.name().to_string(),
                    error: format!("gmail list: {e}"),
                });
                let _ = tx.send(SearchEvent::Finished {
                    worker: self.name().to_string(),
                    kept: 0,
                    dropped: 0,
                    failed: 1,
                    duration_ms: started.elapsed().as_millis() as u64,
                });
                return Err(anyhow!("gmail list failed: {e}"));
            }
        };
        let tally = self
            .ingest_messages(
                ids,
                ctx,
                metadata,
                &tx,
                // External-API personal data: Personal provenance.
                crate::preprocessor::InputProvenance::Personal,
            )
            .await;
        let _ = tx.send(SearchEvent::Finished {
            worker: self.name().to_string(),
            kept: tally.kept,
            dropped: tally.dropped,
            failed: tally.failed,
            duration_ms: started.elapsed().as_millis() as u64,
        });
        tracing::info!(
            n_listed = tally.total,
            kept = tally.kept,
            dropped = tally.dropped,
            failed = tally.failed,
            duration_ms = started.elapsed().as_millis() as u64,
            "gmail_search_done"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LastSeen {
    cutoff: DateTime<Utc>,
}

fn header_map(headers: &[GmailHeader]) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    for h in headers {
        m.insert(h.name.clone(), h.value.clone());
    }
    m
}

fn extract_text(payload: &GmailPayload) -> Option<String> {
    if let Some(t) = first_part_of(payload, "text/plain") {
        return decode_b64url(&t.body?.data?).ok();
    }
    if let Some(t) = first_part_of(payload, "text/html") {
        let raw = decode_b64url(&t.body?.data?).ok()?;
        return Some(strip_html(&raw));
    }
    None
}

fn first_part_of(payload: &GmailPayload, mime: &str) -> Option<GmailPayload> {
    if payload.mime_type.as_deref() == Some(mime) {
        return Some(payload.clone());
    }
    for p in payload.parts.iter().flatten() {
        if let Some(found) = first_part_of(p, mime) {
            return Some(found);
        }
    }
    None
}

fn decode_b64url(s: &str) -> Result<String> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(s))
        .map_err(|e| anyhow!("base64 decode failed: {e}"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if in_tag => {}
            _ => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

// --- Gmail API DTOs ---

#[derive(Debug, Deserialize)]
struct ListResponse {
    #[serde(default)]
    messages: Option<Vec<ListedMessage>>,
}

#[derive(Debug, Deserialize)]
struct ListedMessage {
    id: String,
    #[allow(dead_code)]
    #[serde(default, rename = "threadId")]
    thread_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GmailMessage {
    id: String,
    #[serde(default)]
    snippet: Option<String>,
    #[serde(default, rename = "internalDate")]
    internal_date: Option<String>,
    #[serde(default)]
    payload: Option<GmailPayload>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GmailPayload {
    #[serde(default, rename = "mimeType")]
    mime_type: Option<String>,
    #[serde(default)]
    headers: Vec<GmailHeader>,
    #[serde(default)]
    body: Option<GmailBody>,
    #[serde(default)]
    parts: Option<Vec<GmailPayload>>,
}

#[derive(Debug, Clone, Deserialize)]
struct GmailHeader {
    name: String,
    value: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GmailBody {
    #[serde(default)]
    data: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    size: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_drops_tags_and_collapses_whitespace() {
        let s = "<html><body>Hello <b>world</b>\n\n  and stuff</body></html>";
        assert_eq!(strip_html(s), "Hello world and stuff");
    }

    #[test]
    fn decode_b64url_roundtrip() {
        let s = "aGVsbG8sIHdvcmxk";
        let out = decode_b64url(s).unwrap();
        assert_eq!(out, "hello, world");
    }

    #[test]
    fn open_returns_none_when_unconfigured() {
        let td = tempfile::TempDir::new().unwrap();
        let r = GmailWorker::open(td.path()).unwrap();
        assert!(r.is_none());
    }
}
