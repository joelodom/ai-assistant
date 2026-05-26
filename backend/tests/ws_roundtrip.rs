//! End-to-end: spin up the backend with the mock LLM, connect a real
//! WebSocket client, send a message, assert the reply frames.

use shared::{ClientMessage, MessagePayload, Metadata, ServerMessage};
use std::time::Duration;
use tempfile::TempDir;

#[tokio::test]
async fn full_roundtrip_with_mock_llm() {
    std::env::set_var("AI_ASSISTANT_MOCK_CLAUDE", "1");

    let td = TempDir::new().unwrap();

    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);

    let memory_dir = td.path().to_path_buf();
    let addr_str = addr.to_string();
    let mut cfg = backend::config::Config::default();
    cfg.memory.dir = memory_dir;
    cfg.server.addr = addr_str.clone();
    cfg.scout.enabled = false;
    cfg.curator.enabled = false;

    tokio::spawn(async move {
        let built = backend::build_app(cfg).await.unwrap();
        let app = backend::ws::router(built.state);
        let listener = tokio::net::TcpListener::bind(&addr_str).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    });

    tokio::time::sleep(Duration::from_millis(300)).await;

    let url = format!("ws://{addr}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let intro = ws.next().await.expect("intro frame").expect("ok intro");
    let intro_text = match intro {
        Message::Text(t) => t,
        other => panic!("unexpected intro frame: {other:?}"),
    };
    let parsed: ServerMessage = serde_json::from_str(&intro_text).unwrap();
    match parsed {
        ServerMessage::ReplyDone { text: Some(t), .. } => {
            assert!(t.contains("personal assistant"), "intro: {t}");
        }
        other => panic!("expected intro ReplyDone, got {other:?}"),
    }

    let msg = ClientMessage::Message {
        payload: MessagePayload {
            content: "Remind me: I bought milk".into(),
            attachments: vec![],
        },
        metadata: Metadata {
            datetime_iso: "2026-05-25T14:03:00-05:00".into(),
            geolocation: None,
            freeform: serde_json::Value::Null,
        },
    };
    ws.send(Message::Text(serde_json::to_string(&msg).unwrap()))
        .await
        .unwrap();

    let mut chunks: Vec<String> = Vec::new();
    let mut got_done = false;
    while let Some(frame) = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .ok()
        .flatten()
    {
        let txt = match frame.unwrap() {
            Message::Text(t) => t,
            _ => continue,
        };
        let parsed: ServerMessage = serde_json::from_str(&txt).unwrap();
        match parsed {
            ServerMessage::ReplyChunk { text } => chunks.push(text),
            ServerMessage::ReplyDone { .. } => {
                got_done = true;
                break;
            }
            ServerMessage::Error { text } => panic!("server error: {text}"),
            other => panic!("unexpected frame: {other:?}"),
        }
    }
    assert!(got_done, "never saw reply_done frame");
    let joined = chunks.join("");
    assert!(joined.contains("[mock]"), "joined reply: {joined:?}");
}

#[tokio::test]
async fn sanitizer_drop_path_emits_stub_notice_and_persists_only_stub() {
    std::env::set_var("AI_ASSISTANT_MOCK_CLAUDE", "1");

    let td = TempDir::new().unwrap();

    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);

    let memory_dir = td.path().to_path_buf();
    let addr_str = addr.to_string();
    let mut cfg = backend::config::Config::default();
    cfg.memory.dir = memory_dir.clone();
    cfg.server.addr = addr_str.clone();
    cfg.scout.enabled = false;
    cfg.curator.enabled = false;

    // Build app, override the LLM with a mock that forces Tier::Drop for the
    // sanitizer prompt.
    let built = backend::build_app(cfg).await.unwrap();
    let mock = backend::claude::MockLlmClient::new();
    mock.respond_when(
        "SANITIZER_TASK",
        r#"{"tier":"drop","output":"Received and dropped a security message.","redaction_report":"likely 2FA"}"#,
    );
    let sanitizer = std::sync::Arc::new(backend::sanitizer::Sanitizer::new(mock.clone()));
    let assistant = std::sync::Arc::new(backend::assistant::Assistant::new(
        mock.clone(),
        built.memory.clone(),
    ));
    let state = backend::ws::AppState { sanitizer, assistant };

    tokio::spawn(async move {
        let app = backend::ws::router(state);
        let listener = tokio::net::TcpListener::bind(&addr_str).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    // Drain intro.
    let _ = ws.next().await;

    let msg = ClientMessage::Message {
        payload: MessagePayload {
            content: "Your one-time code is 482194. Do not share it.".into(),
            attachments: vec![],
        },
        metadata: Metadata {
            datetime_iso: "2026-05-25T14:03:00-05:00".into(),
            geolocation: None,
            freeform: serde_json::Value::Null,
        },
    };
    ws.send(Message::Text(serde_json::to_string(&msg).unwrap()))
        .await
        .unwrap();

    let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let txt = match frame {
        Message::Text(t) => t,
        other => panic!("unexpected: {other:?}"),
    };
    let parsed: ServerMessage = serde_json::from_str(&txt).unwrap();
    match parsed {
        ServerMessage::StubNotice { text } => {
            assert!(text.contains("dropped"));
            assert!(!text.contains("482194"), "OTP leaked: {text}");
        }
        other => panic!("expected StubNotice, got {other:?}"),
    }

    // Walk the memory dir: the OTP must not appear anywhere on disk.
    let mut leaked = false;
    for entry in walkdir::WalkDir::new(&memory_dir).into_iter().flatten() {
        if entry.file_type().is_file() {
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                if text.contains("482194") {
                    leaked = true;
                    eprintln!("leak in {:?}: {text}", entry.path());
                }
            }
        }
    }
    assert!(!leaked, "OTP appeared in memory store on disk");
}

#[tokio::test]
async fn sanitizer_failure_drops_input_persists_audit_and_notifies_user() {
    std::env::set_var("AI_ASSISTANT_MOCK_CLAUDE", "1");

    let td = TempDir::new().unwrap();
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);

    let memory_dir = td.path().to_path_buf();
    let addr_str = addr.to_string();

    // Wire the app with a guaranteed-failing LLM so the sanitizer errors.
    let mut cfg = backend::config::Config::default();
    cfg.memory.dir = memory_dir.clone();
    cfg.server.addr = addr_str.clone();
    cfg.scout.enabled = false;
    cfg.curator.enabled = false;
    let memory = std::sync::Arc::new(
        backend::memory::MemoryStore::open(cfg.memory.dir.clone()).await.unwrap()
    );
    let failing: std::sync::Arc<dyn backend::claude::LlmClient> =
        std::sync::Arc::new(backend::claude::FailingLlmClient {
            message: "credit balance is too low".into(),
        });
    let sanitizer = std::sync::Arc::new(backend::sanitizer::Sanitizer::new(failing.clone()));
    let assistant = std::sync::Arc::new(backend::assistant::Assistant::new(
        failing.clone(),
        memory.clone(),
    ));
    let state = backend::ws::AppState { sanitizer, assistant };

    tokio::spawn(async move {
        let app = backend::ws::router(state);
        let listener = tokio::net::TcpListener::bind(&addr_str).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap();
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let _ = ws.next().await; // intro

    let msg = ClientMessage::Message {
        payload: MessagePayload {
            content: "secret personal data".into(),
            attachments: vec![],
        },
        metadata: Metadata {
            datetime_iso: "2026-05-25T14:03:00-05:00".into(),
            geolocation: None,
            freeform: serde_json::Value::Null,
        },
    };
    ws.send(Message::Text(serde_json::to_string(&msg).unwrap()))
        .await
        .unwrap();

    let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let txt = match frame {
        Message::Text(t) => t,
        other => panic!("unexpected: {other:?}"),
    };
    let parsed: ServerMessage = serde_json::from_str(&txt).unwrap();
    match parsed {
        ServerMessage::StubNotice { text } => {
            assert!(text.contains("Gate"), "stub: {text}");
            assert!(text.contains("dropped"), "stub: {text}");
        }
        other => panic!("expected StubNotice on sanitizer failure, got {other:?}"),
    }

    // Audit record is on disk, raw input is NOT.
    let mut saw_audit = false;
    let mut leaked = false;
    for entry in walkdir::WalkDir::new(&memory_dir).into_iter().flatten() {
        if entry.file_type().is_file() {
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                if text.contains("Sanitizer failed") {
                    saw_audit = true;
                }
                if text.contains("secret personal data") {
                    leaked = true;
                    eprintln!("leak in {:?}: {text}", entry.path());
                }
            }
        }
    }
    assert!(saw_audit, "no audit record found in {memory_dir:?}");
    assert!(!leaked, "raw input leaked despite sanitizer failure");
}
