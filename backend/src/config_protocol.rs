//! Backend dispatcher for `ClientMessage::ConfigPayload` traffic.
//!
//! Per Invariant #8 (SPEC §11.6), config payloads bypass the Preprocessor
//! and never reach long-term memory. They are mechanical handshakes /
//! secrets, not personal data. This module is the *only* code that
//! consumes them, and the only path on the backend that writes to
//! `<memory-dir>/connectors/<name>/`.
//!
//! Concerns this module owns:
//!   - Validating each ConfigPayloadKind's structured shape.
//!   - Atomic writes for `client_secret.json` and `token.json`.
//!   - Pending OAuth state (PKCE verifier, CSRF nonce, scope), with a TTL.
//!   - Exchanging an authorization code for tokens with Google.
//!   - Instantiating the resulting connector and registering it live.
//!
//! Out of scope:
//!   - Speaking to the client. This module returns a typed `ConfigResponse`;
//!     the WS handler dispatches the frames.
//!   - Telling the assistant the conversation should continue. The WS
//!     handler synthesizes a continuation turn from the
//!     `ConfigResponse::FramesAndContinue` payload.

use crate::memory::atomic_write_sync;
use crate::workers::oauth::{
    client_secret_path, load_client_secret, token_path, ClientSecretData, StoredToken,
};
use crate::workers::{gmail::GmailWorker, Worker, WorkerRegistry};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use oauth2::basic::BasicClient;
use oauth2::reqwest::async_http_client;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use shared::{ConfigPayloadKind, ConfigRequestKind, ServerMessage};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const PENDING_TTL_SECS: i64 = 600; // 10 minutes
const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// What the WS handler should do after consuming a ConfigPayload.
#[derive(Debug)]
pub enum ConfigResponse {
    /// Send these frames to the client (in order). No continuation needed.
    Frames(Vec<ServerMessage>),
    /// Send these frames AND inject a continuation prompt into a fresh
    /// assistant turn. The continuation is a brief status note that lets
    /// the assistant resume the conversation conversationally.
    FramesAndContinue {
        frames: Vec<ServerMessage>,
        continuation: String,
    },
}

/// One pending OAuth flow, keyed by connector name.
struct PendingOAuth {
    csrf_state: String,
    pkce_verifier: String,
    client_id: String,
    client_secret: String,
    scope: String,
    redirect_url: String,
    expires_at: DateTime<Utc>,
}

pub struct ConfigProtocol {
    memory_root: PathBuf,
    registry: Arc<WorkerRegistry>,
    pending: Mutex<HashMap<String, PendingOAuth>>,
}

impl ConfigProtocol {
    pub fn new(memory_root: PathBuf, registry: Arc<WorkerRegistry>) -> Self {
        Self {
            memory_root,
            registry,
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Entry point: dispatch on payload kind.
    pub async fn handle(&self, payload: ConfigPayloadKind) -> Result<ConfigResponse> {
        // Logging discipline: payload kinds + connector names are SAFE to
        // log; the actual contents (secrets, tokens, codes) are NEVER
        // logged.
        let (kind_name, connector) = match &payload {
            ConfigPayloadKind::ConnectorClientSecret { connector, .. } => {
                ("connector_client_secret", connector.clone())
            }
            ConfigPayloadKind::ConnectorLoopbackReady { connector, .. } => {
                ("connector_loopback_ready", connector.clone())
            }
            ConfigPayloadKind::ConnectorOAuthCallback { connector, .. } => {
                ("connector_oauth_callback", connector.clone())
            }
        };
        tracing::info!(kind = kind_name, %connector, "config_payload_received");
        match payload {
            ConfigPayloadKind::ConnectorClientSecret {
                connector,
                contents,
            } => self.handle_client_secret(connector, contents).await,
            ConfigPayloadKind::ConnectorLoopbackReady { connector, port } => {
                self.handle_loopback_ready(connector, port).await
            }
            ConfigPayloadKind::ConnectorOAuthCallback {
                connector,
                state,
                code,
            } => self.handle_oauth_callback(connector, state, code).await,
        }
    }

    async fn handle_client_secret(
        &self,
        connector: String,
        contents: String,
    ) -> Result<ConfigResponse> {
        // Validate the JSON parses as a Google OAuth client secret (either
        // shape — Desktop or Web). We reject anything else to keep the
        // attack surface tight: the client cannot drop arbitrary bytes
        // onto disk via this channel.
        let parsed: serde_json::Value = serde_json::from_str(&contents)
            .context("config: client_secret.json is not valid JSON")?;
        let has_installed = parsed.get("installed").is_some();
        let has_web = parsed.get("web").is_some();
        if !has_installed && !has_web {
            bail!(
                "config: client_secret.json must have either 'installed' or 'web' top-level key \
                 (got: {})",
                parsed
                    .as_object()
                    .map(|o| o.keys().cloned().collect::<Vec<_>>().join(", "))
                    .unwrap_or_else(|| "(not an object)".into())
            );
        }
        // Verify the inner shape has client_id + client_secret.
        let inner = parsed
            .get("installed")
            .or_else(|| parsed.get("web"))
            .unwrap();
        if inner.get("client_id").and_then(|v| v.as_str()).is_none()
            || inner
                .get("client_secret")
                .and_then(|v| v.as_str())
                .is_none()
        {
            bail!("config: client_secret.json missing client_id or client_secret");
        }

        let path = client_secret_path(&self.memory_root, &connector);
        atomic_write_sync(&path, contents.as_bytes())?;
        tracing::info!(connector = %connector, "config: stored client_secret.json");

        Ok(ConfigResponse::FramesAndContinue {
            frames: vec![ServerMessage::ConfigStatus {
                connector: connector.clone(),
                ok: true,
                message: format!("Stored client_secret.json for {connector}."),
            }],
            continuation: format!(
                "(config event) The user has uploaded client_secret.json for the {connector} \
                 connector. The credentials file is now on disk. If you were walking the user \
                 through OAuth setup for {connector}, the next step is to begin the OAuth \
                 handshake — emit `CONFIG_BEGIN_OAUTH: {connector}` and tell them you're \
                 opening the browser."
            ),
        })
    }

    async fn handle_loopback_ready(&self, connector: String, port: u16) -> Result<ConfigResponse> {
        if !(1024..=65535).contains(&port) {
            bail!("config: loopback port out of range: {port}");
        }
        // Load the client_secret.json for this connector. If it's missing,
        // the user skipped a step.
        let secret_path = client_secret_path(&self.memory_root, &connector);
        if !secret_path.exists() {
            return Ok(ConfigResponse::FramesAndContinue {
                frames: vec![ServerMessage::ConfigStatus {
                    connector: connector.clone(),
                    ok: false,
                    message: format!(
                        "No client_secret.json on disk for {connector}. \
                         Ask the user to provide it first."
                    ),
                }],
                continuation: format!(
                    "(config event) OAuth could not start for {connector} because the \
                     client_secret.json file is missing. Ask the user to provide it first via \
                     CONFIG_REQUEST_FILE: {connector} client_secret.json"
                ),
            });
        }
        let cs: ClientSecretData = load_client_secret(&secret_path)?;

        // Determine scope from connector name.
        let scope = scope_for(&connector)?;
        let redirect_url = format!("http://127.0.0.1:{port}");

        // Build the OAuth client + auth URL.
        let client = BasicClient::new(
            ClientId::new(cs.client_id.clone()),
            Some(ClientSecret::new(cs.client_secret.clone())),
            AuthUrl::new(GOOGLE_AUTH_URL.to_string())?,
            Some(TokenUrl::new(GOOGLE_TOKEN_URL.to_string())?),
        )
        .set_redirect_uri(RedirectUrl::new(redirect_url.clone())?);

        let (pkce_chal, pkce_ver) = PkceCodeChallenge::new_random_sha256();

        let (auth_url, csrf_state) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new(scope.to_string()))
            .add_extra_param("access_type", "offline")
            .add_extra_param("prompt", "consent")
            .set_pkce_challenge(pkce_chal)
            .url();

        // Park the verifier + CSRF so the callback handler can validate.
        self.gc_expired();
        self.pending.lock().unwrap().insert(
            connector.clone(),
            PendingOAuth {
                csrf_state: csrf_state.secret().clone(),
                pkce_verifier: pkce_ver.secret().clone(),
                client_id: cs.client_id,
                client_secret: cs.client_secret,
                scope: scope.to_string(),
                redirect_url: redirect_url.clone(),
                expires_at: Utc::now() + ChronoDuration::seconds(PENDING_TTL_SECS),
            },
        );

        tracing::info!(
            connector = %connector,
            redirect = %redirect_url,
            "config: minted OAuth auth URL for connector"
        );

        Ok(ConfigResponse::Frames(vec![ServerMessage::ConfigRequest {
            request: ConfigRequestKind::OpenBrowser {
                url: auth_url.to_string(),
                hint: format!(
                    "Opening Google's consent screen for {connector}. \
                     Requested scope: {scope}. Click 'Allow' on the page."
                ),
            },
        }]))
    }

    async fn handle_oauth_callback(
        &self,
        connector: String,
        state: String,
        code: String,
    ) -> Result<ConfigResponse> {
        // Pull the pending state, validate CSRF, exchange.
        let pending = {
            let mut g = self.pending.lock().unwrap();
            g.remove(&connector)
        };
        let pending = pending.ok_or_else(|| {
            anyhow!("config: no pending OAuth for {connector} (timed out or never initiated?)")
        })?;
        if pending.expires_at < Utc::now() {
            bail!("config: pending OAuth for {connector} expired before the callback arrived");
        }
        if state != pending.csrf_state {
            bail!(
                "config: OAuth state mismatch for {connector} — possible CSRF (got {state}, expected {})",
                pending.csrf_state
            );
        }

        let client = BasicClient::new(
            ClientId::new(pending.client_id.clone()),
            Some(ClientSecret::new(pending.client_secret.clone())),
            AuthUrl::new(GOOGLE_AUTH_URL.to_string())?,
            Some(TokenUrl::new(GOOGLE_TOKEN_URL.to_string())?),
        )
        .set_redirect_uri(RedirectUrl::new(pending.redirect_url.clone())?);

        let token = client
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(PkceCodeVerifier::new(pending.pkce_verifier.clone()))
            .request_async(async_http_client)
            .await
            .map_err(|e| anyhow!("config: token exchange failed: {e}"))?;

        let refresh = token
            .refresh_token()
            .ok_or_else(|| {
                anyhow!(
                    "config: Google did not return a refresh_token. Re-run the flow and ensure \
                     `prompt=consent` is included in the auth URL."
                )
            })?
            .secret()
            .to_string();
        let expires_in = token
            .expires_in()
            .unwrap_or_else(|| std::time::Duration::from_secs(3600));
        let stored = StoredToken {
            access_token: token.access_token().secret().clone(),
            refresh_token: refresh,
            expires_at: Utc::now()
                + ChronoDuration::from_std(expires_in).unwrap_or(ChronoDuration::hours(1)),
            scope: pending.scope.clone(),
            authorized_at: Utc::now(),
        };
        let tp = token_path(&self.memory_root, &connector);
        atomic_write_sync(&tp, &serde_json::to_vec_pretty(&stored)?)?;
        tracing::info!(connector = %connector, scope = %pending.scope, "config: token stored");

        // Instantiate + register the live worker.
        let registered_msg = match connector.as_str() {
            "gmail" => match GmailWorker::open(&self.memory_root)? {
                Some(w) => {
                    self.registry.register(Arc::new(w) as Arc<dyn Worker>);
                    "Gmail is now active and searchable."
                }
                None => "Token saved, but worker failed to open. Check logs.",
            },
            other => {
                tracing::warn!("config: no live-register handler for worker kind: {other}");
                "Token saved; restart the backend to activate."
            }
        };

        Ok(ConfigResponse::FramesAndContinue {
            frames: vec![ServerMessage::ConfigStatus {
                connector: connector.clone(),
                ok: true,
                message: format!("Authorized {connector} (scope: {}).", pending.scope),
            }],
            continuation: format!(
                "(config event) OAuth completed successfully for the {connector} connector. \
                 Token saved with scope {}. {registered_msg} Tell the user this is done and \
                 suggest one or two example queries they could now ask.",
                pending.scope
            ),
        })
    }

    fn gc_expired(&self) {
        let now = Utc::now();
        let mut g = self.pending.lock().unwrap();
        g.retain(|_, p| p.expires_at >= now);
    }
}

fn scope_for(worker: &str) -> Result<&'static str> {
    match worker {
        "gmail" => Ok(crate::workers::gmail::GMAIL_SCOPE),
        other => bail!("config: unknown worker kind: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workers::WorkerContext;
    use tempfile::TempDir;

    async fn fixture() -> (TempDir, ConfigProtocol) {
        let td = TempDir::new().unwrap();
        let memory = Arc::new(
            crate::memory::MemoryStore::open(td.path().to_path_buf())
                .await
                .unwrap(),
        );
        let embedder: Arc<dyn crate::embedder::Embedder> =
            Arc::new(crate::embedder::MockEmbedder::new());
        let vector_index = Arc::new(
            crate::vector_index::VectorIndex::open(
                memory.root(),
                embedder.model_name(),
                embedder.dimension(),
            )
            .unwrap(),
        );
        let llm: Arc<dyn crate::claude::LlmClient> = crate::claude::MockLlmClient::new();
        let preprocessor = Arc::new(crate::preprocessor::Preprocessor::new(llm));
        let ctx = Arc::new(WorkerContext {
            preprocessor,
            memory,
            embedder,
            vector_index,
            preprocess_concurrency: 4,
        });
        let registry = Arc::new(WorkerRegistry::empty(ctx));
        let cp = ConfigProtocol::new(td.path().to_path_buf(), registry);
        (td, cp)
    }

    #[tokio::test]
    async fn client_secret_validates_shape() {
        let (_td, cp) = fixture().await;
        let bogus = "not json".to_string();
        let r = cp
            .handle(ConfigPayloadKind::ConnectorClientSecret {
                connector: "gmail".into(),
                contents: bogus,
            })
            .await;
        assert!(r.is_err());

        let wrong_shape = r#"{"other":{}}"#.to_string();
        let r = cp
            .handle(ConfigPayloadKind::ConnectorClientSecret {
                connector: "gmail".into(),
                contents: wrong_shape,
            })
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn client_secret_writes_atomically() {
        let (td, cp) = fixture().await;
        let body =
            r#"{"installed":{"client_id":"a","client_secret":"b","auth_uri":"x","token_uri":"y"}}"#;
        let res = cp
            .handle(ConfigPayloadKind::ConnectorClientSecret {
                connector: "gmail".into(),
                contents: body.into(),
            })
            .await
            .unwrap();
        match res {
            ConfigResponse::FramesAndContinue { frames, .. } => {
                let st = matches!(frames[0], ServerMessage::ConfigStatus { ok: true, .. });
                assert!(st);
            }
            _ => panic!("expected FramesAndContinue"),
        }
        let p = td.path().join("connectors/gmail/client_secret.json");
        assert!(p.exists());
    }

    #[tokio::test]
    async fn loopback_ready_errors_when_no_secret() {
        let (_td, cp) = fixture().await;
        let res = cp
            .handle(ConfigPayloadKind::ConnectorLoopbackReady {
                connector: "gmail".into(),
                port: 12345,
            })
            .await
            .unwrap();
        // Should return a "needs client_secret first" status, not bail.
        match res {
            ConfigResponse::FramesAndContinue { frames, .. } => {
                let bad_status =
                    matches!(&frames[0], ServerMessage::ConfigStatus { ok: false, .. });
                assert!(bad_status, "expected non-ok ConfigStatus");
            }
            _ => panic!("unexpected response"),
        }
    }

    #[tokio::test]
    async fn oauth_callback_without_pending_errors() {
        let (_td, cp) = fixture().await;
        let r = cp
            .handle(ConfigPayloadKind::ConnectorOAuthCallback {
                connector: "gmail".into(),
                state: "abc".into(),
                code: "def".into(),
            })
            .await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn loopback_port_validated() {
        let (_td, cp) = fixture().await;
        let r = cp
            .handle(ConfigPayloadKind::ConnectorLoopbackReady {
                connector: "gmail".into(),
                port: 22, // privileged port, not allowed
            })
            .await;
        assert!(r.is_err());
    }
}
