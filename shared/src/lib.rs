//! Wire protocol shared by backend and client.
//!
//! Invariants (do not relax):
//!   1. No outbound actions, ever. Backend reads in / responds out only.
//!   2. Raw input is ephemeral. The Security Preprocessor is the only thing
//!      that sees it.
//!   3. Everything stored is sanitized.
//!   8. Configuration payloads (`ClientMessage::ConfigPayload`) bypass the
//!      Preprocessor AND never reach long-term memory. They are handled by
//!      a dedicated config dispatcher (see SPEC §11.6 / §19).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Message {
        payload: MessagePayload,
        metadata: Metadata,
        /// HAZMAT escape hatch. When true, the Security Preprocessor is
        /// skipped entirely and this message goes directly to the Assistant.
        /// Use only when the user has consciously chosen to bypass for a
        /// specific input (e.g. they want the raw text reasoned over
        /// verbatim). The backend logs a WARN and tags the resulting memory
        /// item with "hazmat" so the bypass is auditable.
        ///
        /// Wire field is `bypass_preprocessor`; the older name
        /// `bypass_sanitizer` is accepted as a deserialization alias for
        /// back-compat with older clients (forward-compatible reads
        /// invariant).
        #[serde(default, alias = "bypass_sanitizer")]
        bypass_preprocessor: bool,
        /// When true, skip the default Sonnet path and route the Assistant
        /// call directly to the configured escalation model (Opus). Used
        /// when the user knows the question deserves the heavier model.
        #[serde(default)]
        force_opus: bool,
    },
    Ping,
    /// Sensitive configuration payload. Routed to the config dispatcher;
    /// does NOT pass through the Preprocessor and does NOT land in long-term
    /// memory (Invariant #8).
    ConfigPayload {
        payload: ConfigPayloadKind,
    },
}

/// Discriminated union of all configuration payloads the client can send.
/// New variants should ONLY be added with explicit security review — every
/// variant here bypasses the Preprocessor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigPayloadKind {
    /// User provided the OAuth client_secret.json for a connector. Contents
    /// are written atomically to `<memory-dir>/connectors/<name>/client_secret.json`.
    ConnectorClientSecret {
        connector: String,
        /// Raw JSON text exactly as the user provided. Backend validates
        /// the shape (must look like a Google OAuth Desktop client) and
        /// rejects anything else.
        contents: String,
    },
    /// Client has bound an OAuth loopback listener at this port and is
    /// ready to receive the authorization redirect from Google. Backend
    /// uses the port to mint the auth URL.
    ConnectorLoopbackReady {
        connector: String,
        port: u16,
    },
    /// Client's loopback listener received the OAuth redirect. Backend
    /// validates `state` against its pending entry and exchanges the code
    /// for tokens.
    ConnectorOAuthCallback {
        connector: String,
        state: String,
        code: String,
    },
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
    /// Structured ask for the client to perform a configuration step
    /// (open a file picker, bind a loopback, launch a browser). Driven by
    /// the assistant's config markers — see SPEC §19.
    ConfigRequest { request: ConfigRequestKind },
    /// Result of a ConfigPayload the client just sent. Rendered in the
    /// transcript as a system note.
    ConfigStatus {
        connector: String,
        ok: bool,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigRequestKind {
    /// Ask the user to pick a file from disk. The client should respond
    /// with `ConfigPayload::ConnectorClientSecret`.
    RequestFile {
        connector: String,
        filename: String,
        /// Human-readable hint to render in the UI (what the file is for,
        /// where the user gets it).
        hint: String,
    },
    /// Begin an OAuth handshake. The client should bind a 127.0.0.1
    /// loopback listener and reply with
    /// `ConfigPayload::ConnectorLoopbackReady`. The backend will then
    /// build the auth URL and send `OpenBrowser`.
    BeginOAuth {
        connector: String,
        /// Informational only — the actual scope is enforced server-side
        /// at OAuth time.
        scope: String,
    },
    /// Open this URL in the user's browser. The client should also continue
    /// listening on the loopback bound in the BeginOAuth step.
    OpenBrowser {
        url: String,
        hint: String,
    },
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
