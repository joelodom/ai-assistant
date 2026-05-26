//! LLM access. Production path: shell out to the `claude` CLI so we use the
//! user's Claude Max budget rather than the API. Tests use `MockLlmClient`.
//!
//! Every `oneshot` is a brand-new subprocess: no shared session state, no
//! `--continue`. The sanitizer relies on this — its context is the process,
//! and the process dies after the call.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

#[async_trait]
pub trait LlmClient: Send + Sync {
    /// One-shot prompt → full text response. No streaming, no shared context.
    async fn oneshot(&self, prompt: &str, opts: LlmOptions) -> Result<String>;
}

#[derive(Debug, Clone, Default)]
pub struct LlmOptions {
    pub allowed_tools: Vec<String>,
    /// Override the configured timeout. None = use client default.
    pub timeout: Option<Duration>,
    /// Override the configured model. None = use client default.
    pub model: Option<String>,
}

pub struct ClaudeCliClient {
    pub binary: String,
    pub model: String,
    pub default_timeout: Duration,
}

impl ClaudeCliClient {
    pub fn new(binary: String, model: String, default_timeout: Duration) -> Self {
        Self {
            binary,
            model,
            default_timeout,
        }
    }
}

#[async_trait]
impl LlmClient for ClaudeCliClient {
    async fn oneshot(&self, prompt: &str, opts: LlmOptions) -> Result<String> {
        let model = opts.model.unwrap_or_else(|| self.model.clone());
        let to = opts.timeout.unwrap_or(self.default_timeout);

        let mut cmd = Command::new(&self.binary);
        cmd.arg("-p")
            .arg("--model")
            .arg(&model)
            .arg("--output-format")
            .arg("text");

        if opts.allowed_tools.is_empty() {
            // No tools requested → disable all built-in tools so a stray tool
            // call can't fire. This is what the Sanitizer and Curator want.
            cmd.arg("--tools").arg("");
        } else {
            // Tools requested → allow them AND set permission-mode dontAsk
            // so claude doesn't try to prompt a non-existent human for
            // approval. Without this flag, `-p` will quietly skip tool use.
            cmd.arg("--allowedTools")
                .arg(opts.allowed_tools.join(","))
                .arg("--permission-mode")
                .arg("dontAsk");
        }

        // Prompt via stdin to avoid argv length limits and shell quoting.
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn().with_context(|| {
            format!("failed to spawn `{}` — is the Claude CLI installed and on PATH?", self.binary)
        })?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(prompt.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        let output = match timeout(to, child.wait_with_output()).await {
            Ok(res) => res?,
            Err(_) => {
                return Err(anyhow!("claude CLI timed out after {:?}", to));
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(anyhow!(
                "claude CLI exited with {}: {}",
                output.status,
                stderr.trim()
            ));
        }

        let stdout = String::from_utf8(output.stdout)
            .map_err(|e| anyhow!("claude CLI returned non-UTF8: {e}"))?;
        Ok(stdout)
    }
}

/// Deterministic, free, offline LLM stand-in for tests and the
/// `AI_ASSISTANT_MOCK_CLAUDE=1` mode. It looks at the prompt prefix to decide
/// what shape of response to return so each pipeline stage gets something it
/// can parse.
pub struct MockLlmClient {
    inner: Arc<std::sync::Mutex<MockState>>,
}

#[derive(Default)]
struct MockState {
    pub calls: Vec<MockCall>,
    pub overrides: Vec<MockOverride>,
}

#[derive(Debug, Clone)]
pub struct MockCall {
    pub prompt: String,
    pub allowed_tools: Vec<String>,
}

struct MockOverride {
    matcher: String,
    response: String,
}

impl MockLlmClient {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(std::sync::Mutex::new(MockState::default())),
        })
    }

    /// Force a specific response for any prompt containing `matcher`.
    pub fn respond_when(&self, matcher: &str, response: &str) {
        let mut g = self.inner.lock().unwrap();
        g.overrides.push(MockOverride {
            matcher: matcher.to_string(),
            response: response.to_string(),
        });
    }

    pub fn calls(&self) -> Vec<MockCall> {
        self.inner.lock().unwrap().calls.clone()
    }

    fn default_response(prompt: &str) -> String {
        // Sanitizer prompt looks for `SANITIZER_TASK` marker (we control it
        // in sanitizer.rs). Return canonical Tier 3 JSON.
        if prompt.contains("SANITIZER_TASK") {
            // Echo the input back as Tier 3 (pass) by default.
            // The sanitizer prompt embeds the raw input between BEGIN/END markers.
            let echoed = extract_between(prompt, "<<<BEGIN_INPUT>>>", "<<<END_INPUT>>>")
                .unwrap_or_else(|| "".to_string());
            serde_json::json!({
                "tier": "pass",
                "output": echoed.trim(),
                "redaction_report": ""
            })
            .to_string()
        } else if prompt.contains("CURATOR_TASK") {
            // Generic single-line summary.
            "[mock] collapsed summary of an aging memory item.".to_string()
        } else if prompt.contains("SCOUT_TASK") {
            "[mock] No fresh items today.".to_string()
        } else {
            // Default assistant response.
            "[mock] Acknowledged.".to_string()
        }
    }
}

#[async_trait]
impl LlmClient for MockLlmClient {
    async fn oneshot(&self, prompt: &str, opts: LlmOptions) -> Result<String> {
        let mut g = self.inner.lock().unwrap();
        g.calls.push(MockCall {
            prompt: prompt.to_string(),
            allowed_tools: opts.allowed_tools.clone(),
        });
        for ov in &g.overrides {
            if prompt.contains(&ov.matcher) {
                return Ok(ov.response.clone());
            }
        }
        drop(g);
        Ok(Self::default_response(prompt))
    }
}

fn extract_between(text: &str, start: &str, end: &str) -> Option<String> {
    let s = text.find(start)? + start.len();
    let e = text[s..].find(end)?;
    Some(text[s..s + e].to_string())
}

/// A client that always errors. Useful for testing failure paths.
pub struct FailingLlmClient {
    pub message: String,
}

#[async_trait]
impl LlmClient for FailingLlmClient {
    async fn oneshot(&self, _prompt: &str, _opts: LlmOptions) -> Result<String> {
        Err(anyhow!(self.message.clone()))
    }
}

/// Build a real or mock client based on env. Backend `main` calls this.
pub fn make_client_from_env(cfg: &crate::config::ClaudeCfg) -> Arc<dyn LlmClient> {
    if std::env::var("AI_ASSISTANT_MOCK_CLAUDE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        tracing::warn!("AI_ASSISTANT_MOCK_CLAUDE=1 — using mock LLM, responses will be canned");
        MockLlmClient::new()
    } else {
        Arc::new(ClaudeCliClient::new(
            cfg.binary.clone(),
            cfg.model.clone(),
            Duration::from_secs(cfg.timeout_secs),
        ))
    }
}
