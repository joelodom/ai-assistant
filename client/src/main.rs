//! Native Mac client. Single typed-chat surface that handles both ingestion
//! and conversation. Talks to the backend over WebSocket using the shared
//! protocol types.
//!
//! Threading model:
//!  - egui owns the UI thread.
//!  - A dedicated std::thread hosts a tokio runtime that runs the WebSocket
//!    task. The runtime and the UI exchange messages via std::sync::mpsc.
//!
//! Args:
//!   --url ws://host:port/ws   Backend URL (default ws://127.0.0.1:8765/ws)

mod app;
mod geo;
mod net;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt::init();

    let mut url = std::env::var("AI_ASSISTANT_URL").unwrap_or_else(|_| "ws://127.0.0.1:8765/ws".to_string());
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--url" {
            if let Some(u) = args.next() {
                url = u;
            }
        }
    }

    let (ui_tx, net_rx) = std::sync::mpsc::channel::<net::UiToNet>();
    let (net_tx, ui_rx) = std::sync::mpsc::channel::<net::NetToUi>();

    let url_clone = url.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(net::run(url_clone, net_rx, net_tx));
    });

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([800.0, 720.0])
            .with_title("AI Assistant"),
        ..Default::default()
    };
    eframe::run_native(
        "AI Assistant",
        native_options,
        Box::new(move |cc| Box::new(app::AssistantApp::new(cc, url, ui_tx, ui_rx))),
    )
}
