//! OAuth 2.0 runtime machinery for Google-family connectors.
//!
//! The interactive setup flow lives in `backend/src/config_protocol.rs`
//! (driven by the client over the WS connection — see SPEC §19). This
//! module owns:
//!
//! - Parsing the Google Cloud Console `client_secret.json` download.
//! - Persisting / loading `token.json`.
//! - `OAuthClient`: a runtime token holder used by connectors. Loads
//!   from disk at startup, refreshes silently when access tokens expire.
//!
//! Files on disk (under `<memory-dir>/connectors/<name>/`):
//!   - `client_secret.json` — Google Cloud Console download. Written by
//!     the config dispatcher when the user uploads it via the client.
//!   - `token.json` — written by the config dispatcher after OAuth
//!     completes; updated by `OAuthClient::access_token()` on every refresh.
//!     Atomic writes only.
//!
//! Security: tokens are scope-bound at issuance. The runtime never widens
//! scope on refresh; Google enforces this server-side as well.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use oauth2::basic::BasicClient;
use oauth2::reqwest::async_http_client;
use oauth2::{AuthUrl, ClientId, ClientSecret, RefreshToken, TokenResponse, TokenUrl};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;

const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Subset of the Google Cloud Console "OAuth 2.0 Client IDs" download we
/// actually use. The download wraps either of two top-level keys
/// (`installed` for Desktop clients, `web` for web clients); we accept
/// `installed`.
#[derive(Debug, Deserialize)]
struct ClientSecretFile {
    installed: Option<ClientSecretInner>,
    web: Option<ClientSecretInner>,
}

#[derive(Debug, Deserialize)]
struct ClientSecretInner {
    client_id: String,
    client_secret: String,
}

#[derive(Debug, Clone)]
pub struct ClientSecretData {
    pub client_id: String,
    pub client_secret: String,
}

pub fn load_client_secret(path: &Path) -> Result<ClientSecretData> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let parsed: ClientSecretFile = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "parsing {} as a Google OAuth client_secret JSON",
            path.display()
        )
    })?;
    let inner = parsed
        .installed
        .or(parsed.web)
        .context("client_secret.json has neither `installed` nor `web` section")?;
    Ok(ClientSecretData {
        client_id: inner.client_id,
        client_secret: inner.client_secret,
    })
}

/// Token persistence format. Includes the scope so a future runtime can
/// detect mismatches between the configured connector and the actual
/// authorization the user granted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredToken {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: DateTime<Utc>,
    pub scope: String,
    pub authorized_at: DateTime<Utc>,
}

pub fn token_path(memory_root: &Path, connector_name: &str) -> PathBuf {
    memory_root
        .join("connectors")
        .join(connector_name)
        .join("token.json")
}

pub fn client_secret_path(memory_root: &Path, connector_name: &str) -> PathBuf {
    memory_root
        .join("connectors")
        .join(connector_name)
        .join("client_secret.json")
}

pub fn load_stored_token(path: &Path) -> Result<StoredToken> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let token: StoredToken =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    Ok(token)
}
/// Runtime token holder. Owns the StoredToken and a way to refresh it.
/// Cloneable via Arc.
pub struct OAuthClient {
    client_id: String,
    client_secret: String,
    token_path: PathBuf,
    /// Guarded so concurrent connector calls don't race on refresh.
    token: Mutex<StoredToken>,
}

impl OAuthClient {
    /// Load an OAuth client from disk. Returns Ok(None) if the connector
    /// hasn't been set up yet (token.json missing).
    pub fn open(memory_root: &Path, connector_name: &str) -> Result<Option<Self>> {
        let token_p = token_path(memory_root, connector_name);
        let secret_p = client_secret_path(memory_root, connector_name);
        if !token_p.exists() || !secret_p.exists() {
            return Ok(None);
        }
        let cs = load_client_secret(&secret_p)?;
        let token = load_stored_token(&token_p)?;
        Ok(Some(Self {
            client_id: cs.client_id,
            client_secret: cs.client_secret,
            token_path: token_p,
            token: Mutex::new(token),
        }))
    }

    /// Return a current access token, refreshing if it's within 60s of
    /// expiring. Writes the refreshed token to disk atomically.
    pub async fn access_token(&self) -> Result<String> {
        let mut g = self.token.lock().await;
        if g.expires_at - Utc::now() > ChronoDuration::seconds(60) {
            return Ok(g.access_token.clone());
        }
        // Refresh via Google.
        let client = BasicClient::new(
            ClientId::new(self.client_id.clone()),
            Some(ClientSecret::new(self.client_secret.clone())),
            AuthUrl::new(GOOGLE_AUTH_URL.to_string())?,
            Some(TokenUrl::new(GOOGLE_TOKEN_URL.to_string())?),
        );
        let refresh = RefreshToken::new(g.refresh_token.clone());
        let new = client
            .exchange_refresh_token(&refresh)
            .request_async(async_http_client)
            .await
            .map_err(|e| anyhow!("OAuth refresh failed: {e}"))?;

        let expires_in = new
            .expires_in()
            .unwrap_or_else(|| std::time::Duration::from_secs(3600));
        g.access_token = new.access_token().secret().clone();
        g.expires_at =
            Utc::now() + ChronoDuration::from_std(expires_in).unwrap_or(ChronoDuration::hours(1));
        // Google sometimes rotates refresh tokens.
        if let Some(rt) = new.refresh_token() {
            g.refresh_token = rt.secret().clone();
        }
        crate::memory::atomic_write_sync(&self.token_path, &serde_json::to_vec_pretty(&*g)?)?;
        Ok(g.access_token.clone())
    }

    pub async fn scope(&self) -> String {
        self.token.lock().await.scope.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parses_desktop_client_secret() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("client_secret.json");
        let body = r#"{
          "installed": {
            "client_id": "abc.apps.googleusercontent.com",
            "client_secret": "xyz",
            "auth_uri": "https://accounts.google.com/o/oauth2/auth",
            "token_uri": "https://oauth2.googleapis.com/token"
          }
        }"#;
        std::fs::write(&p, body).unwrap();
        let cs = load_client_secret(&p).unwrap();
        assert_eq!(cs.client_id, "abc.apps.googleusercontent.com");
        assert_eq!(cs.client_secret, "xyz");
    }

    #[test]
    fn parses_web_client_secret_too() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("client_secret.json");
        let body = r#"{
          "web": {
            "client_id": "web.apps.googleusercontent.com",
            "client_secret": "ws"
          }
        }"#;
        std::fs::write(&p, body).unwrap();
        let cs = load_client_secret(&p).unwrap();
        assert_eq!(cs.client_id, "web.apps.googleusercontent.com");
    }

    #[test]
    fn rejects_unrecognized_shape() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("client_secret.json");
        std::fs::write(&p, r#"{"other":{}}"#).unwrap();
        assert!(load_client_secret(&p).is_err());
    }

    #[test]
    fn token_path_under_connector_dir() {
        let p = token_path(Path::new("/tmp/mem"), "gmail");
        assert_eq!(p, PathBuf::from("/tmp/mem/connectors/gmail/token.json"));
    }

    #[test]
    fn open_returns_none_when_not_configured() {
        let td = TempDir::new().unwrap();
        let r = OAuthClient::open(td.path(), "gmail").unwrap();
        assert!(r.is_none());
    }
}
