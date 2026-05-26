//! WebSocket front door.
//!
//! Two kinds of inbound traffic:
//!
//!   1. `ClientMessage::Message` — a regular user turn. Goes through the
//!      Preprocessor (or HAZMAT bypass) and then the Assistant.
//!
//!   2. `ClientMessage::ConfigPayload` — a sensitive config handshake
//!      (uploading client_secret.json, OAuth callback, etc.). Routes
//!      directly to `ConfigProtocol` and BYPASSES both the Preprocessor
//!      and long-term memory (Invariant #8). The config dispatcher
//!      typically returns frames to send + a continuation prompt that we
//!      feed into a synthesized assistant turn so the conversation moves
//!      forward.
//!
//! On connect, we push the introduction immediately as a `reply_done`
//! frame so a brand-new user sees who/what this is.

use crate::assistant::Assistant;
use crate::config_protocol::{ConfigProtocol, ConfigResponse};
use crate::preprocessor::{InputProvenance, Preprocessor, PreprocessorResult};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::{
    sink::SinkExt,
    stream::{SplitSink, StreamExt},
};
use shared::{ClientMessage, Metadata, ReplyMeta, ServerMessage, Tier};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub preprocessor: Arc<Preprocessor>,
    pub assistant: Arc<Assistant>,
    pub config_protocol: Arc<ConfigProtocol>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/ws", get(ws_handler))
        .route("/health", get(health))
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

fn short_err(e: &anyhow::Error) -> String {
    let s = e.to_string();
    s.lines().next().unwrap_or(&s).to_string()
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();

    let intro_text = state.assistant.introduction().await;
    let intro = ServerMessage::ReplyDone {
        text: Some(intro_text),
        meta: Some(ReplyMeta {
            tier_summary: Some("introduction".into()),
            sources: vec![],
        }),
    };
    let _ = send_frame(&mut sender, &intro).await;

    while let Some(msg) = receiver.next().await {
        let Ok(msg) = msg else { break };
        let text = match msg {
            Message::Text(t) => t,
            Message::Ping(p) => {
                let _ = sender.send(Message::Pong(p)).await;
                continue;
            }
            Message::Close(_) => break,
            _ => continue,
        };

        let Ok(client_msg) = serde_json::from_str::<ClientMessage>(&text) else {
            let err = ServerMessage::Error {
                text: "Could not parse client message as JSON.".into(),
            };
            let _ = send_frame(&mut sender, &err).await;
            continue;
        };

        match client_msg {
            ClientMessage::Ping => {
                let _ = send_frame(&mut sender, &ServerMessage::Pong).await;
            }
            ClientMessage::Message {
                payload,
                metadata,
                bypass_preprocessor,
                force_opus,
            } => {
                let mut bundle = payload.content.clone();
                if !payload.attachments.is_empty() {
                    bundle.push_str("\n\n[attachments]\n");
                    for a in &payload.attachments {
                        let extracted = crate::attachments::extract_text(a);
                        bundle.push_str(&format!(
                            "--- attachment: {:?}{} ({}) ---\n{}\n",
                            a.kind,
                            a.name
                                .as_deref()
                                .map(|n| format!(" \"{n}\""))
                                .unwrap_or_default(),
                            a.mime,
                            extracted,
                        ));
                    }
                }

                let preprocess_result = if bypass_preprocessor {
                    tracing::warn!(
                        bundle_len = bundle.chars().count(),
                        "HAZMAT BYPASS: user invoked direct-to-assistant path; \
                         Preprocessor skipped for this message"
                    );
                    Ok(PreprocessorResult {
                        tier: shared::Tier::Pass,
                        output: bundle.clone(),
                        redaction_report:
                            "HAZMAT BYPASS — Preprocessor skipped at user request"
                                .to_string(),
                        importance: 0.8,
                        importance_reason: Some(
                            "HAZMAT bypass — user explicitly elevated".into(),
                        ),
                    })
                } else {
                    state
                        .preprocessor
                        .preprocess(&bundle, InputProvenance::Personal)
                        .await
                };

                let preprocessed = match preprocess_result {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "preprocessor failed");
                        let note = format!(
                            "Preprocessor failed at {}. Input was dropped without inspection \
                             (length: {} chars). Likely causes: out of Claude tokens, CLI not \
                             found, network timeout, or LLM returned malformed JSON. Underlying \
                             error: {}",
                            chrono::Utc::now().to_rfc3339(),
                            bundle.chars().count(),
                            e
                        );
                        let _ = state
                            .assistant
                            .memory
                            .add(
                                &note,
                                crate::memory::ItemKind::PreprocessorError,
                                0.6,
                                Some(metadata.clone()),
                                String::new(),
                                vec!["error".into(), "preprocessor".into()],
                            )
                            .await;
                        let notice = ServerMessage::StubNotice {
                            text: format!(
                                "The Preprocessor failed and your message was dropped without \
                                 inspection. Reason: {}. I saved a note about this; you can ask me \
                                 about it later. (If this keeps happening, check your Claude token \
                                 budget or the backend log.)",
                                short_err(&e)
                            ),
                        };
                        let _ = send_frame(&mut sender, &notice).await;
                        continue;
                    }
                };

                if preprocessed.tier == Tier::Drop {
                    let _ = state
                        .assistant
                        .memory
                        .add_stub(&preprocessed.output, preprocessed.redaction_report.clone())
                        .await;
                    let notice = ServerMessage::StubNotice {
                        text: preprocessed.output.clone(),
                    };
                    let _ = send_frame(&mut sender, &notice).await;
                    continue;
                }
                run_assistant_turn(&state, &mut sender, preprocessed, metadata, force_opus).await;
            }
            ClientMessage::ConfigPayload { payload } => {
                // Sensitive payload — bypass Preprocessor + memory (Invariant #8).
                // Dispatch directly to the config protocol handler.
                tracing::info!("config: received payload");
                let resp = state.config_protocol.handle(payload).await;
                match resp {
                    Ok(ConfigResponse::Frames(frames)) => {
                        for f in frames {
                            let _ = send_frame(&mut sender, &f).await;
                        }
                    }
                    Ok(ConfigResponse::FramesAndContinue {
                        frames,
                        continuation,
                    }) => {
                        for f in frames {
                            let _ = send_frame(&mut sender, &f).await;
                        }
                        // Synthesize a continuation turn. The continuation
                        // text is a trusted system event — we manufacture
                        // the PreprocessorResult instead of routing
                        // through the real Preprocessor.
                        let synthetic = PreprocessorResult {
                            tier: Tier::Pass,
                            output: continuation,
                            redaction_report: "config-continuation".into(),
                            importance: 0.2,
                            importance_reason: Some("system config event".into()),
                        };
                        let metadata = synthetic_metadata();
                        run_assistant_turn(&state, &mut sender, synthetic, metadata, false).await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "config: dispatch failed");
                        let err = ServerMessage::ConfigStatus {
                            connector: "unknown".into(),
                            ok: false,
                            message: format!("config: {e}"),
                        };
                        let _ = send_frame(&mut sender, &err).await;
                    }
                }
            }
        }
    }
}

/// One assistant turn: call respond, stream reply, send any ConfigRequest
/// frames the assistant emitted, send ReplyDone. Used both for normal user
/// turns AND for the synthetic continuation turns after a ConfigPayload.
async fn run_assistant_turn(
    state: &AppState,
    sender: &mut SplitSink<WebSocket, Message>,
    preprocessed: PreprocessorResult,
    metadata: Metadata,
    force_opus: bool,
) {
    match state
        .assistant
        .respond(&preprocessed, &metadata, force_opus)
        .await
    {
        Ok(outcome) => {
            if outcome.escalated {
                let prefix = if let Some(r) = &outcome.escalation_reason {
                    format!(
                        "🧠 Handing off to {} for deeper reasoning — {}\n\n",
                        outcome.model_used, r
                    )
                } else {
                    format!(
                        "🧠 Handing off to {} for deeper reasoning…\n\n",
                        outcome.model_used
                    )
                };
                let _ = send_frame(sender, &ServerMessage::ReplyChunk { text: prefix }).await;
            }
            let reply = &outcome.text;
            let chunks: Vec<&str> = if reply.contains("\n\n") {
                reply.split("\n\n").collect()
            } else {
                vec![reply.as_str()]
            };
            for c in chunks {
                let _ = send_frame(
                    sender,
                    &ServerMessage::ReplyChunk {
                        text: format!("{c}\n\n"),
                    },
                )
                .await;
            }

            // Send any config_requests the assistant emitted (CONFIG_REQUEST_FILE,
            // CONFIG_BEGIN_OAUTH). These follow the reply text so the user sees
            // the assistant's framing first, then their client UI reacts (file
            // picker, browser launch, etc.).
            for req in outcome.config_requests {
                let _ = send_frame(sender, &ServerMessage::ConfigRequest { request: req }).await;
            }

            let mut tier_summary = match preprocessed.tier {
                Tier::Redact => "redact".to_string(),
                Tier::Pass => "pass".to_string(),
                Tier::Drop => "drop".to_string(),
            };
            tier_summary.push_str(&format!(" · model={}", outcome.model_used));
            if outcome.escalated {
                tier_summary.push_str(" · escalated");
            }
            if force_opus {
                tier_summary.push_str(" · force_opus");
            }
            if outcome.forgotten_item_id.is_some() {
                tier_summary.push_str(" · forget-action");
            }
            let done = ServerMessage::ReplyDone {
                text: None,
                meta: Some(ReplyMeta {
                    tier_summary: Some(tier_summary),
                    sources: vec![],
                }),
            };
            let _ = send_frame(sender, &done).await;
        }
        Err(e) => {
            tracing::warn!(error = %e, "assistant failed");
            let note = format!(
                "Assistant failed at {}. The user's preceding message is in memory (search recent \
                 for context). Likely causes: out of Claude tokens, CLI not found, network timeout. \
                 Underlying error: {}",
                chrono::Utc::now().to_rfc3339(),
                e
            );
            let _ = state
                .assistant
                .memory
                .add(
                    &note,
                    crate::memory::ItemKind::AssistantError,
                    0.6,
                    Some(metadata.clone()),
                    String::new(),
                    vec!["error".into(), "assistant".into()],
                )
                .await;
            let err = ServerMessage::Error {
                text: format!(
                    "I hit an error generating a response: {}. I saved a note so I'll remember \
                     this happened — you can ask about it later. (Common cause: the Claude CLI is \
                     rate-limited or out of tokens.)",
                    short_err(&e)
                ),
            };
            let _ = send_frame(sender, &err).await;
        }
    }
}

async fn send_frame(
    sender: &mut SplitSink<WebSocket, Message>,
    frame: &ServerMessage,
) -> Result<(), axum::Error> {
    let json = serde_json::to_string(frame).map_err(|e| axum::Error::new(e))?;
    sender.send(Message::Text(json)).await
}

fn synthetic_metadata() -> Metadata {
    Metadata {
        datetime_iso: chrono::Utc::now().to_rfc3339(),
        geolocation: None,
        freeform: serde_json::json!({"source": "config_continuation"}),
    }
}
