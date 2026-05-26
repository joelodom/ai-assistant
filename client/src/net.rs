//! WebSocket worker. Runs on its own tokio runtime and bridges to the egui
//! thread via std::sync::mpsc channels.
//!
//! This worker also handles the mechanical parts of the config protocol
//! that need a local network listener or a browser launch:
//!   - `ServerMessage::ConfigRequest::BeginOAuth` → bind a 127.0.0.1
//!     loopback, send back a `ConnectorLoopbackReady`, and spawn a
//!     one-shot listener that forwards the OAuth callback to the backend
//!     as a `ConnectorOAuthCallback`.
//!   - `ServerMessage::ConfigRequest::OpenBrowser` → launch the URL via
//!     `webbrowser::open()`. Also forwards the frame to the UI so the
//!     transcript can show "🌐 opening browser…".
//!
//! Other frames (RequestFile, ConfigStatus, the chat-stream variants) are
//! forwarded as-is to the UI.

use futures::{SinkExt, StreamExt};
use shared::{ClientMessage, ConfigPayloadKind, ConfigRequestKind, ServerMessage};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

pub enum UiToNet {
    Send(ClientMessage),
    Reconnect(String),
}

pub enum NetToUi {
    Connecting,
    Connected,
    Disconnected(String),
    Frame(ServerMessage),
}

pub async fn run(mut url: String, rx: Receiver<UiToNet>, tx: Sender<NetToUi>) {
    loop {
        let _ = tx.send(NetToUi::Connecting);
        let connect_result = tokio_tungstenite::connect_async(&url).await;
        let (mut ws, _) = match connect_result {
            Ok(pair) => pair,
            Err(e) => {
                let _ = tx.send(NetToUi::Disconnected(format!("connect failed: {e}")));
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let _ = tx.send(NetToUi::Connected);

        // Internal channel: spawned tasks (e.g. OAuth callback listeners)
        // push outbound ClientMessages here; the main loop forwards them
        // over the WebSocket.
        let (internal_tx, mut internal_rx) =
            tokio::sync::mpsc::unbounded_channel::<ClientMessage>();

        loop {
            tokio::select! {
                inbound = ws.next() => {
                    match inbound {
                        Some(Ok(Message::Text(t))) => {
                            match serde_json::from_str::<ServerMessage>(&t) {
                                Ok(m) => {
                                    handle_inbound_frame(m, &tx, &internal_tx);
                                }
                                Err(e) => {
                                    let _ = tx.send(NetToUi::Frame(ServerMessage::Error {
                                        text: format!("bad server frame: {e}")
                                    }));
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            let _ = tx.send(NetToUi::Disconnected("server closed".into()));
                            break;
                        }
                        Some(Err(e)) => {
                            let _ = tx.send(NetToUi::Disconnected(format!("ws error: {e}")));
                            break;
                        }
                        _ => {}
                    }
                }
                outbound = internal_rx.recv() => {
                    if let Some(msg) = outbound {
                        let json = serde_json::to_string(&msg).unwrap();
                        if let Err(e) = ws.send(Message::Text(json)).await {
                            let _ = tx.send(NetToUi::Disconnected(format!("send failed: {e}")));
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    // Poll the std::sync::mpsc for outbound UI messages.
                    while let Ok(cmd) = rx.try_recv() {
                        match cmd {
                            UiToNet::Send(m) => {
                                let json = serde_json::to_string(&m).unwrap();
                                if let Err(e) = ws.send(Message::Text(json)).await {
                                    let _ = tx.send(NetToUi::Disconnected(format!("send failed: {e}")));
                                    break;
                                }
                            }
                            UiToNet::Reconnect(new_url) => {
                                url = new_url;
                                let _ = ws.close(None).await;
                            }
                        }
                    }
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Dispatch one inbound frame. Most are forwarded straight to the UI.
/// Two ConfigRequest kinds are special-cased because they need to run
/// network or browser side effects on this thread:
///   - BeginOAuth: bind a loopback listener, send LoopbackReady, spawn
///     the callback waiter.
///   - OpenBrowser: webbrowser::open the URL (and also forward to UI for
///     the transcript note).
fn handle_inbound_frame(
    frame: ServerMessage,
    tx_to_ui: &Sender<NetToUi>,
    internal_tx: &tokio::sync::mpsc::UnboundedSender<ClientMessage>,
) {
    if let ServerMessage::ConfigRequest { request } = &frame {
        match request {
            ConfigRequestKind::BeginOAuth { connector, .. } => {
                let connector = connector.clone();
                let tx_to_ui = tx_to_ui.clone();
                let internal_tx = internal_tx.clone();
                std::thread::spawn(move || {
                    if let Err(e) = run_oauth_listener(&connector, &tx_to_ui, &internal_tx) {
                        let _ = tx_to_ui.send(NetToUi::Frame(ServerMessage::Error {
                            text: format!("OAuth listener failed for {connector}: {e}"),
                        }));
                    }
                });
                return;
            }
            ConfigRequestKind::OpenBrowser { url, .. } => {
                if let Err(e) = webbrowser::open(url) {
                    let _ = tx_to_ui.send(NetToUi::Frame(ServerMessage::Error {
                        text: format!("could not open browser: {e}"),
                    }));
                }
                // Fall through: also forward the frame to UI so the
                // transcript shows "🌐 opening browser…".
            }
            _ => {}
        }
    }
    let _ = tx_to_ui.send(NetToUi::Frame(frame));
}

/// One-shot loopback listener for an OAuth callback. Runs on a dedicated
/// std::thread (not on the tokio runtime) so blocking `accept` and
/// blocking `read_line` are fine — they're exactly what we want.
fn run_oauth_listener(
    connector: &str,
    tx_to_ui: &Sender<NetToUi>,
    internal_tx: &tokio::sync::mpsc::UnboundedSender<ClientMessage>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();

    // Tell the backend we're listening so it can mint the auth URL.
    let lr = ClientMessage::ConfigPayload {
        payload: ConfigPayloadKind::ConnectorLoopbackReady {
            connector: connector.to_string(),
            port,
        },
    };
    let _ = internal_tx.send(lr);

    // Inform the UI for transcript visibility.
    let _ = tx_to_ui.send(NetToUi::Frame(ServerMessage::ConfigStatus {
        connector: connector.to_string(),
        ok: true,
        message: format!("Listening on 127.0.0.1:{port} for the OAuth callback…"),
    }));

    // Block waiting for the redirect. We accept exactly one connection.
    let (stream, _) = listener.accept()?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        anyhow::bail!("malformed OAuth callback: {request_line:?}");
    }
    let query = parts[1].split_once('?').map(|(_, q)| q).unwrap_or("");

    let mut code: Option<String> = None;
    let mut state: Option<String> = None;
    let mut error: Option<String> = None;
    for kv in query.split('&') {
        let Some((k, v)) = kv.split_once('=') else { continue };
        let dec = urlencoding::decode(v)
            .unwrap_or(std::borrow::Cow::Borrowed(v))
            .into_owned();
        match k {
            "code" => code = Some(dec),
            "state" => state = Some(dec),
            "error" => error = Some(dec),
            _ => {}
        }
    }

    // Reply with a friendly HTML page so the user can close the tab.
    let body = if let Some(e) = &error {
        format!(
            "<html><body style='font-family:sans-serif;padding:2em'>\
             <h1>Authorization failed</h1><p>Google reported: <code>{e}</code></p>\
             <p>You can close this tab.</p></body></html>"
        )
    } else if code.is_some() {
        String::from(
            "<html><body style='font-family:sans-serif;padding:2em'>\
             <h1>✓ Authorized.</h1><p>You can close this tab. The assistant has received the callback.</p>\
             </body></html>"
        )
    } else {
        String::from("<html><body>Unexpected callback parameters.</body></html>")
    };
    let mut writer = stream;
    write!(
        writer,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )?;
    writer.flush()?;

    if let Some(e) = error {
        anyhow::bail!("OAuth flow returned error: {e}");
    }
    let code = code.ok_or_else(|| anyhow::anyhow!("OAuth callback missing code"))?;
    let state = state.ok_or_else(|| anyhow::anyhow!("OAuth callback missing state"))?;

    // Forward the code+state to the backend over the ws.
    let payload = ClientMessage::ConfigPayload {
        payload: ConfigPayloadKind::ConnectorOAuthCallback {
            connector: connector.to_string(),
            state,
            code,
        },
    };
    let _ = internal_tx.send(payload);

    let _ = tx_to_ui.send(NetToUi::Frame(ServerMessage::ConfigStatus {
        connector: connector.to_string(),
        ok: true,
        message: "OAuth callback received; backend is exchanging the code for tokens.".into(),
    }));
    Ok(())
}
