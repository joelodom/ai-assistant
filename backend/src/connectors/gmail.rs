//! Gmail connector. Read-only search + fetch via the Gmail REST API.
//!
//! Auth scope (hardcoded): `https://www.googleapis.com/auth/gmail.readonly`.
//! This is enforced both:
//!   - In our own code: the trait only exposes `search`; there's no
//!     `.send()` or `.delete()` for a bug to call into.
//!   - By Google's authorization server: any non-read API would 403
//!     immediately, because the OAuth token is scope-bound at issuance.
//!
//! The connector reads `client_secret.json` and `token.json` from
//! `<memory-dir>/connectors/gmail/`. If either is missing, the connector
//! reports itself unavailable (the assistant still sees it listed, with
//! "NOT CONFIGURED", and can guide the user to run `connect gmail`).

use super::{Connector, RawConnectorResult};
use crate::connectors::oauth::OAuthClient;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;

pub const GMAIL_SCOPE: &str = "https://www.googleapis.com/auth/gmail.readonly";

pub struct GmailConnector {
    auth: Arc<OAuthClient>,
    http: reqwest::Client,
}

impl GmailConnector {
    /// Open the Gmail connector. Returns Ok(None) if not yet configured
    /// (no client_secret.json or no token.json).
    pub fn open(memory_root: &Path) -> Result<Option<Self>> {
        let auth = match OAuthClient::open(memory_root, "gmail")? {
            Some(a) => Arc::new(a),
            None => return Ok(None),
        };
        let http = reqwest::Client::builder()
            .user_agent("ai-assistant/0.1 (gmail-connector)")
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Some(Self { auth, http }))
    }
}

#[async_trait]
impl Connector for GmailConnector {
    fn name(&self) -> &'static str {
        "gmail"
    }

    fn description(&self) -> &'static str {
        "Searches the user's Gmail account, READ-ONLY (Google enforces the \
         scope server-side — this connector has no ability to send, delete, \
         or modify mail). Accepts Gmail query syntax: `from:dr.patel`, \
         `subject:invoice`, `before:2024/06/01 after:2024/05/01`, \
         `has:attachment`, free text, or any combination. Returns up to N \
         messages, each as: From, To, Subject, Date, and as much body text \
         as fits. Use this when the user asks something whose answer is \
         likely in their inbox (\"what did Dr. Patel say about the implant\", \
         \"when did the inspector reply\", \"summarize my correspondence \
         with the contractor\")."
    }

    fn is_available(&self) -> bool {
        true
    }

    async fn search(&self, query: &str, limit: usize) -> Result<Vec<RawConnectorResult>> {
        let started = std::time::Instant::now();
        // NOTE: never log `query` verbatim — it may contain personal names
        // / dates from the user's memory. Lengths and counts are fine.
        tracing::debug!(query_len = query.len(), limit, "gmail_search_start");
        let access = self.auth.access_token().await?;
        let limit = limit.clamp(1, 25);

        // 1) List matching message IDs.
        let list_url = format!(
            "https://gmail.googleapis.com/gmail/v1/users/me/messages?maxResults={}&q={}",
            limit,
            urlencoding::encode(query),
        );
        let list: ListResponse = self
            .http
            .get(&list_url)
            .bearer_auth(&access)
            .send()
            .await
            .context("Gmail messages.list HTTP failed")?
            .error_for_status()
            .context("Gmail messages.list status")?
            .json()
            .await
            .context("Gmail messages.list JSON")?;

        let Some(messages) = list.messages else {
            tracing::info!(
                n_hits = 0,
                duration_ms = started.elapsed().as_millis() as u64,
                "gmail_search_done"
            );
            return Ok(vec![]);
        };
        tracing::debug!(
            n_hits = messages.len(),
            list_duration_ms = started.elapsed().as_millis() as u64,
            "gmail_list_done"
        );

        // 2) Fetch each message in full. Sequentially for v1 — concurrent
        // fetches are easy to add later but rarely the bottleneck (the
        // expensive part is downstream sanitization).
        let n_messages = messages.len().min(limit);
        let mut out = Vec::with_capacity(n_messages);
        let mut n_failed = 0usize;
        for m in messages.into_iter().take(limit) {
            match self.fetch_message(&m.id, &access).await {
                Ok(r) => out.push(r),
                Err(e) => {
                    n_failed += 1;
                    tracing::warn!(error = %e, id = %m.id, "gmail_fetch_failed");
                }
            }
        }
        tracing::info!(
            n_listed = n_messages,
            n_fetched = out.len(),
            n_failed,
            duration_ms = started.elapsed().as_millis() as u64,
            "gmail_search_done"
        );
        Ok(out)
    }
}

impl GmailConnector {
    async fn fetch_message(&self, id: &str, access: &str) -> Result<RawConnectorResult> {
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
        // Fallback to snippet if we couldn't extract any text.
        let body = if body_text.trim().is_empty() {
            m.snippet.clone().unwrap_or_default()
        } else {
            body_text
        };

        // Compose a human-readable block. The Preprocessor will see this
        // verbatim — the From/To/Subject lines are exactly the structured
        // signal it needs for importance scoring.
        let content =
            format!("From: {from}\nTo: {to}\nSubject: {subject}\nDate: {date_header}\n\n{body}");

        let at = m
            .internal_date
            .as_deref()
            .and_then(|s| s.parse::<i64>().ok())
            .and_then(|ms| DateTime::<Utc>::from_timestamp_millis(ms));

        Ok(RawConnectorResult {
            source_id: format!("gmail:{}", m.id),
            source_url: Some(format!("https://mail.google.com/mail/u/0/#all/{}", m.id)),
            content,
            at,
        })
    }
}

fn header_map(headers: &[GmailHeader]) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    for h in headers {
        m.insert(h.name.clone(), h.value.clone());
    }
    m
}

/// Walk a Gmail payload tree and return the best text representation we can
/// find. Prefers text/plain. Falls back to a naive HTML-stripping pass on
/// text/html. Returns None if neither is present.
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

/// Extremely naive HTML stripper. Removes tags, collapses whitespace.
/// Good enough for "feed this to a Preprocessor that just needs to score
/// importance"; not good enough for high-fidelity rendering.
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
    // Collapse runs of whitespace.
    let collapsed: String = out.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
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
        // "hello, world" → "aGVsbG8sIHdvcmxk"
        let s = "aGVsbG8sIHdvcmxk";
        let out = decode_b64url(s).unwrap();
        assert_eq!(out, "hello, world");
    }

    #[test]
    fn extract_text_finds_plain_part() {
        let payload = GmailPayload {
            mime_type: Some("multipart/alternative".into()),
            parts: Some(vec![
                GmailPayload {
                    mime_type: Some("text/plain".into()),
                    body: Some(GmailBody {
                        data: Some("aGVsbG8sIHdvcmxk".into()),
                        size: Some(12),
                    }),
                    ..Default::default()
                },
                GmailPayload {
                    mime_type: Some("text/html".into()),
                    body: Some(GmailBody {
                        data: Some("PGI-aGk8L2I-".into()),
                        size: Some(12),
                    }),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        let s = extract_text(&payload).unwrap();
        assert_eq!(s, "hello, world");
    }

    #[test]
    fn extract_text_falls_back_to_html_when_no_plain() {
        let payload = GmailPayload {
            mime_type: Some("text/html".into()),
            // "<b>hi</b>" URL-safe base64 (no padding): PGI-aGk8L2I-
            body: Some(GmailBody {
                data: Some("PGI-aGk8L2I-".into()),
                size: Some(12),
            }),
            ..Default::default()
        };
        let s = extract_text(&payload).unwrap();
        assert_eq!(s.trim(), "hi");
    }

    #[test]
    fn header_map_builds_lookup() {
        let h = vec![
            GmailHeader {
                name: "From".into(),
                value: "a@b".into(),
            },
            GmailHeader {
                name: "Subject".into(),
                value: "hi".into(),
            },
        ];
        let m = header_map(&h);
        assert_eq!(m.get("From"), Some(&"a@b".to_string()));
        assert_eq!(m.get("Subject"), Some(&"hi".to_string()));
    }

    #[test]
    fn open_returns_none_when_unconfigured() {
        let td = tempfile::TempDir::new().unwrap();
        let r = GmailConnector::open(td.path()).unwrap();
        assert!(r.is_none());
    }
}
