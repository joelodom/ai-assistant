//! WebSocket worker. Runs on its own tokio runtime and bridges to the egui
//! thread via std::sync::mpsc channels.

use futures::{SinkExt, StreamExt};
use shared::{ClientMessage, ServerMessage};
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

        loop {
            tokio::select! {
                inbound = ws.next() => {
                    match inbound {
                        Some(Ok(Message::Text(t))) => {
                            match serde_json::from_str::<ServerMessage>(&t) {
                                Ok(m) => { let _ = tx.send(NetToUi::Frame(m)); }
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
                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    // Poll the std::sync::mpsc for outbound UI messages. We
                    // can't await a std mpsc, so we tick. 50ms is invisible.
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
