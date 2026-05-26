//! Wire protocol shared by backend and client.
//!
//! Invariants (do not relax):
//!   1. No outbound actions, ever. Backend reads in / responds out only.
//!   2. Raw input is ephemeral. Sanitizer is the only thing that sees it.
//!   3. Everything stored is sanitized.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Message {
        payload: MessagePayload,
        metadata: Metadata,
        /// HAZMAT escape hatch. When true, the Sanitizer is skipped entirely
        /// and this message goes directly to the Assistant. Use only when
        /// the user has consciously chosen to bypass for a specific input
        /// (e.g. they want the raw text reasoned over verbatim). The backend
        /// logs a WARN and tags the resulting memory item with "hazmat" so
        /// the bypass is auditable.
        #[serde(default)]
        bypass_sanitizer: bool,
        /// When true, skip the default Sonnet path and route the Assistant
        /// call directly to the configured escalation model (Opus). Used
        /// when the user knows the question deserves the heavier model.
        #[serde(default)]
        force_opus: bool,
    },
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagePayload {
    pub content: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub kind: AttachmentKind,
    pub data: String, // base64 for binary; raw text otherwise
    pub mime: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Photo,
    Document,
    Email,
    Calendar,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    pub datetime_iso: String,
    #[serde(default)]
    pub geolocation: Option<Geolocation>,
    #[serde(default)]
    pub freeform: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Geolocation {
    pub lat: f64,
    pub lon: f64,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Streaming chunk of an assistant reply.
    ReplyChunk { text: String },
    /// End-of-reply marker. `text` may carry a final non-streamed payload
    /// (e.g. when the backend chose to send the whole thing at once).
    ReplyDone {
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        meta: Option<ReplyMeta>,
    },
    /// Sanitizer dropped or redacted something — let the user know without
    /// revealing the dropped content.
    StubNotice { text: String },
    /// Backend-side error surfaced to the user.
    Error { text: String },
    Pong,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReplyMeta {
    #[serde(default)]
    pub tier_summary: Option<String>,
    #[serde(default)]
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Drop entirely — only-security content (2FA, reset link).
    Drop,
    /// Redact dangerous identifiers, keep contextual meaning.
    Redact,
    /// Pass through unchanged.
    Pass,
}
