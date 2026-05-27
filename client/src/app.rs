//! egui chat surface. Single-pane: scrollable transcript above, input below,
//! collapsible settings drawer.
//!
//! UX intent: any message is both an ingestion and a question — the same box
//! handles both. The user shouldn't have to think about which mode they're in.

use crate::geo;
use crate::net::{NetToUi, UiToNet};
use chrono::Local;
use eframe::egui;
use serde::{Deserialize, Serialize};
use shared::{
    Attachment, AttachmentKind, ClientMessage, ConfigPayloadKind, ConfigRequestKind, Geolocation,
    MessagePayload, Metadata, ServerMessage,
};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

/// Persistent user prefs for the client. Lives at `~/.ai-assistant-client.json`.
/// Use serde defaults so new fields don't break old files.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Prefs {
    ui_scale: f32,
}

impl Default for Prefs {
    fn default() -> Self {
        Self { ui_scale: 1.0 }
    }
}

const UI_SCALE_MIN: f32 = 0.7;
const UI_SCALE_MAX: f32 = 2.5;
const UI_SCALE_STEP: f32 = 0.1;

fn prefs_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".ai-assistant-client.json"))
}

fn load_prefs() -> Prefs {
    let Some(p) = prefs_path() else { return Prefs::default() };
    std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_prefs(prefs: &Prefs) {
    let Some(p) = prefs_path() else { return };
    if let Ok(json) = serde_json::to_string_pretty(prefs) {
        // Best-effort; we don't want a chat client to crash because prefs
        // couldn't be written.
        let _ = std::fs::write(p, json);
    }
}

#[derive(Clone, Debug)]
enum Turn {
    User { text: String, ts: String },
    Assistant { text: String, ts: String },
    Stub { text: String, ts: String },
    Error { text: String, ts: String },
    System { text: String, ts: String },
}

/// In-flight status for the current turn, populated by
/// `ServerMessage::Status` frames as the backend works. Cleared when
/// `ReplyDone` arrives (or when an `Error` frame ends the turn). The
/// elapsed-time counter in the status bar reads from
/// `AssistantApp.turn_started_at`, not from per-status timestamps —
/// it shows total turn elapsed, not per-phase elapsed.
#[derive(Debug, Clone)]
struct TurnStatus {
    phase: String,
    detail: Option<String>,
}

pub struct AssistantApp {
    url: String,
    pending_url_edit: String,
    ui_tx: Sender<UiToNet>,
    ui_rx: Receiver<NetToUi>,
    status: String,
    connected: bool,

    transcript: Vec<Turn>,
    /// Buffer accumulating reply chunks until ReplyDone.
    streaming_reply: String,
    /// Live status from the backend during a turn. None when idle.
    turn_status: Option<TurnStatus>,
    /// When the user pressed Send for the current turn — drives the
    /// elapsed-time counter in the status bar. Cleared on ReplyDone /
    /// Error / Disconnect.
    turn_started_at: Option<std::time::Instant>,

    input_buf: String,
    pending_attachments: Vec<Attachment>,

    show_settings: bool,

    /// HAZMAT toggle: when true, the next message bypasses the Security Preprocessor.
    /// Sticky within a session (does not auto-clear after send) so the user
    /// can run a sequence of bypass messages, but always resets to false
    /// on app startup. A bright red indicator in the UI makes the state
    /// impossible to miss.
    bypass_preprocessor: bool,

    /// Force the assistant to route directly to the heavier model (Opus),
    /// skipping the default Sonnet pre-pass.
    force_opus: bool,

    prefs: Prefs,
    /// Tracks last applied scale so we only push a new pixels_per_point when
    /// it changes (and only save prefs to disk on actual change).
    applied_scale: f32,

    location: Arc<Mutex<Option<Geolocation>>>,
    location_loading: Arc<Mutex<bool>>,
    location_label_edit: String,
    location_lat_edit: String,
    location_lon_edit: String,
}

impl AssistantApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        url: String,
        ui_tx: Sender<UiToNet>,
        ui_rx: Receiver<NetToUi>,
    ) -> Self {
        // Light theme by default; user can toggle in egui's built-in menu.
        cc.egui_ctx.set_visuals(egui::Visuals::dark());

        let mut prefs = load_prefs();
        prefs.ui_scale = prefs.ui_scale.clamp(UI_SCALE_MIN, UI_SCALE_MAX);
        cc.egui_ctx.set_pixels_per_point(prefs.ui_scale);

        let location = Arc::new(Mutex::new(None));
        let location_loading = Arc::new(Mutex::new(true));
        {
            let loc = location.clone();
            let flag = location_loading.clone();
            std::thread::spawn(move || {
                let g = geo::fetch_ip_geo();
                *loc.lock().unwrap() = g;
                *flag.lock().unwrap() = false;
            });
        }

        Self {
            pending_url_edit: url.clone(),
            url,
            ui_tx,
            ui_rx,
            status: "starting…".into(),
            connected: false,
            transcript: Vec::new(),
            streaming_reply: String::new(),
            turn_status: None,
            turn_started_at: None,
            input_buf: String::new(),
            pending_attachments: Vec::new(),
            show_settings: false,
            bypass_preprocessor: false,
            force_opus: false,
            applied_scale: prefs.ui_scale,
            prefs,
            location,
            location_loading,
            location_label_edit: String::new(),
            location_lat_edit: String::new(),
            location_lon_edit: String::new(),
        }
    }

    fn drain_net(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.ui_rx.try_recv() {
            match ev {
                NetToUi::Connecting => {
                    self.status = "connecting…".into();
                    self.connected = false;
                }
                NetToUi::Connected => {
                    self.status = "connected".into();
                    self.connected = true;
                }
                NetToUi::Disconnected(why) => {
                    self.status = format!("disconnected: {why}");
                    self.connected = false;
                    // Drop any in-flight status — the turn isn't coming
                    // back to us.
                    self.turn_status = None;
                    self.turn_started_at = None;
                }
                NetToUi::Frame(f) => match f {
                    ServerMessage::ReplyChunk { text } => {
                        self.streaming_reply.push_str(&text);
                        // If we never saw an explicit `replying` Status
                        // frame (e.g. backend skipped it, or this is the
                        // intro), seeing chunks is itself proof we're in
                        // the replying phase.
                        if self.turn_status.as_ref().map(|s| s.phase != "replying").unwrap_or(true)
                            && self.turn_started_at.is_some()
                        {
                            self.turn_status = Some(TurnStatus {
                                phase: "replying".into(),
                                detail: None,
                            });
                        }
                    }
                    ServerMessage::ReplyDone { text, .. } => {
                        if let Some(t) = text {
                            // Whole reply in one frame (used for the intro).
                            self.transcript.push(Turn::System {
                                text: t,
                                ts: now_str(),
                            });
                        } else if !self.streaming_reply.is_empty() {
                            let final_text = std::mem::take(&mut self.streaming_reply);
                            self.transcript.push(Turn::Assistant {
                                text: final_text.trim().to_string(),
                                ts: now_str(),
                            });
                        }
                        // Turn is over — clear the status bar.
                        self.turn_status = None;
                        self.turn_started_at = None;
                    }
                    ServerMessage::StubNotice { text } => {
                        self.transcript.push(Turn::Stub {
                            text,
                            ts: now_str(),
                        });
                        self.turn_status = None;
                        self.turn_started_at = None;
                    }
                    ServerMessage::Error { text } => {
                        self.transcript.push(Turn::Error {
                            text,
                            ts: now_str(),
                        });
                        self.turn_status = None;
                        self.turn_started_at = None;
                    }
                    ServerMessage::Pong => {}
                    ServerMessage::ConfigRequest { request } => {
                        self.handle_config_request(request);
                    }
                    ServerMessage::ConfigStatus { connector, ok, message } => {
                        let prefix = if ok { "✓" } else { "✗" };
                        self.transcript.push(Turn::System {
                            text: format!("{prefix} [{connector}] {message}"),
                            ts: now_str(),
                        });
                    }
                    ServerMessage::Status { phase, detail } => {
                        // Update the live status bar. The bar shows what
                        // the backend is currently doing; clears on
                        // ReplyDone or Error.
                        self.turn_status = Some(TurnStatus { phase, detail });
                    }
                },
            }
            ctx.request_repaint();
        }
    }

    fn current_metadata(&self) -> Metadata {
        let dt = Local::now();
        Metadata {
            datetime_iso: dt.to_rfc3339(),
            geolocation: self.location.lock().unwrap().clone(),
            freeform: serde_json::json!({
                "client": "ai-assistant-client",
                "os": std::env::consts::OS,
            }),
        }
    }

    fn send_current(&mut self) {
        if !self.connected {
            self.transcript.push(Turn::Error {
                text: "Not connected — message not sent.".into(),
                ts: now_str(),
            });
            return;
        }
        let text = std::mem::take(&mut self.input_buf);
        if text.trim().is_empty() && self.pending_attachments.is_empty() {
            return;
        }
        let attachments = std::mem::take(&mut self.pending_attachments);
        let hazmat = self.bypass_preprocessor;
        let force_opus = self.force_opus;
        // Mark hazmat / opus in the local transcript so the user sees what
        // routing applied — they should never wonder later whether a message
        // went through the Preprocessor or which model handled it.
        let body = render_user_outgoing(&text, &attachments);
        let display = match (hazmat, force_opus) {
            (true, true) => format!("☢ HAZMAT 🧠 OPUS\n{body}"),
            (true, false) => format!("☢ HAZMAT (Preprocessor bypassed)\n{body}"),
            (false, true) => format!("🧠 OPUS (forced)\n{body}"),
            (false, false) => body,
        };
        self.transcript.push(Turn::User {
            text: display,
            ts: now_str(),
        });
        // Light up the status bar immediately so the user sees activity
        // before the first server frame arrives.
        self.turn_started_at = Some(std::time::Instant::now());
        self.turn_status = Some(TurnStatus {
            phase: "sending".into(),
            detail: Some("Delivering your message…".into()),
        });
        let msg = ClientMessage::Message {
            payload: MessagePayload {
                content: text,
                attachments,
            },
            metadata: self.current_metadata(),
            bypass_preprocessor: hazmat,
            force_opus,
        };
        let _ = self.ui_tx.send(UiToNet::Send(msg));
    }

    /// Open a native file picker and queue every chosen file as an attachment.
    /// Backend asked us to do a config step. Dispatch by kind.
    /// BeginOAuth is handled entirely on the network thread (see net.rs);
    /// the other two land here.
    fn handle_config_request(&mut self, req: ConfigRequestKind) {
        match req {
            ConfigRequestKind::RequestFile {
                connector,
                filename,
                hint,
            } => {
                // Annotate the transcript so the user sees what's being asked.
                self.transcript.push(Turn::System {
                    text: format!("📎 [{connector}] {hint}"),
                    ts: now_str(),
                });
                let picked = rfd::FileDialog::new()
                    .set_title(&format!("Choose {filename} for {connector}"))
                    .pick_file();
                let Some(path) = picked else {
                    self.transcript.push(Turn::System {
                        text: format!("✗ [{connector}] file selection cancelled."),
                        ts: now_str(),
                    });
                    return;
                };
                let contents = match std::fs::read_to_string(&path) {
                    Ok(c) => c,
                    Err(e) => {
                        self.transcript.push(Turn::Error {
                            text: format!("could not read {}: {e}", path.display()),
                            ts: now_str(),
                        });
                        return;
                    }
                };
                let msg = ClientMessage::ConfigPayload {
                    payload: ConfigPayloadKind::ConnectorClientSecret {
                        connector: connector.clone(),
                        contents,
                    },
                };
                let _ = self.ui_tx.send(UiToNet::Send(msg));
                self.transcript.push(Turn::System {
                    text: format!("→ [{connector}] sent {} to backend", path.display()),
                    ts: now_str(),
                });
            }
            ConfigRequestKind::BeginOAuth { connector, scope } => {
                // The network thread handles BeginOAuth (binds loopback,
                // spawns listener). Render a transcript note so the user
                // sees the OAuth dance kicking off.
                self.transcript.push(Turn::System {
                    text: format!(
                        "🔐 [{connector}] starting OAuth handshake (scope: {scope})…"
                    ),
                    ts: now_str(),
                });
            }
            ConfigRequestKind::OpenBrowser { url, hint } => {
                // Browser launch was done by net.rs; this is just the
                // transcript echo.
                self.transcript.push(Turn::System {
                    text: format!("🌐 {hint}\n   {url}"),
                    ts: now_str(),
                });
            }
        }
    }

    fn pick_files(&mut self) {
        let files = rfd::FileDialog::new()
            .set_title("Attach files")
            .pick_files();
        if let Some(paths) = files {
            for p in paths {
                self.attach_from_path(&p);
            }
        }
    }

    fn attach_from_path(&mut self, path: &std::path::Path) {
        match read_and_classify(path) {
            Ok(att) => self.pending_attachments.push(att),
            Err(e) => self.transcript.push(Turn::Error {
                text: format!("Couldn't attach {}: {e}", path.display()),
                ts: now_str(),
            }),
        }
    }

    fn drain_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<egui::DroppedFile> = ctx.input(|i| i.raw.dropped_files.clone());
        for df in dropped {
            if let Some(p) = df.path {
                self.attach_from_path(&p);
            }
        }
    }
}

const MAX_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;

fn read_and_classify(path: &std::path::Path) -> Result<Attachment, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "file is {} MB; cap is {} MB",
            bytes.len() / 1024 / 1024,
            MAX_ATTACHMENT_BYTES / 1024 / 1024
        ));
    }
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string());
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default();
    let (mime, kind, is_text) = classify_extension(&ext);
    let data = if is_text {
        // Text-ish: send as UTF-8 string. The backend treats data as raw text.
        String::from_utf8(bytes).map_err(|_| "file is not valid UTF-8".to_string())?
    } else {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    };
    Ok(Attachment {
        kind,
        data,
        mime: mime.to_string(),
        name,
    })
}

fn classify_extension(ext: &str) -> (&'static str, AttachmentKind, bool) {
    match ext {
        // Plain text / source
        "txt" | "md" | "markdown" | "log" => ("text/plain", AttachmentKind::Document, true),
        "json" => ("application/json", AttachmentKind::Document, true),
        "csv" => ("text/csv", AttachmentKind::Document, true),
        "html" | "htm" => ("text/html", AttachmentKind::Document, true),
        "xml" => ("application/xml", AttachmentKind::Document, true),
        "yaml" | "yml" => ("application/yaml", AttachmentKind::Document, true),
        "rs" | "py" | "js" | "ts" | "go" | "c" | "cpp" | "h" | "rb" | "java" | "swift" => {
            ("text/x-source", AttachmentKind::Document, true)
        }
        // Email
        "eml" | "msg" => ("message/rfc822", AttachmentKind::Email, true),
        // Calendar
        "ics" | "ical" => ("text/calendar", AttachmentKind::Calendar, true),
        // PDF — binary, but backend extracts text
        "pdf" => ("application/pdf", AttachmentKind::Document, false),
        // Images
        "jpg" | "jpeg" => ("image/jpeg", AttachmentKind::Photo, false),
        "png" => ("image/png", AttachmentKind::Photo, false),
        "gif" => ("image/gif", AttachmentKind::Photo, false),
        "webp" => ("image/webp", AttachmentKind::Photo, false),
        "heic" | "heif" => ("image/heic", AttachmentKind::Photo, false),
        _ => ("application/octet-stream", AttachmentKind::Document, false),
    }
}

fn render_user_outgoing(text: &str, attachments: &[Attachment]) -> String {
    if attachments.is_empty() {
        text.to_string()
    } else {
        let mut s = text.to_string();
        for a in attachments {
            s.push_str(&format!(
                "\n[attached {:?}{} · {} · {}]",
                a.kind,
                a.name.as_deref().map(|n| format!(" {n}")).unwrap_or_default(),
                a.mime,
                approx_size(a),
            ));
        }
        s
    }
}

fn approx_size(a: &Attachment) -> String {
    // For text-ish mimes, data is raw text — use char count.
    // For binary, data is base64 — back out approx byte count.
    let bytes = if a.mime.starts_with("text/")
        || a.mime == "application/json"
        || a.mime == "application/xml"
        || a.mime == "application/yaml"
        || a.mime == "message/rfc822"
    {
        a.data.len()
    } else {
        // base64: 4 chars → 3 bytes (minus padding).
        a.data.len() * 3 / 4
    };
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn now_str() -> String {
    Local::now().format("%H:%M:%S").to_string()
}

impl AssistantApp {
    fn bump_scale(&mut self, delta: f32) {
        let next = (self.prefs.ui_scale + delta).clamp(UI_SCALE_MIN, UI_SCALE_MAX);
        // Snap to nearest 0.05 so the displayed percentage stays clean.
        self.prefs.ui_scale = (next * 20.0).round() / 20.0;
    }

    fn reset_scale(&mut self) {
        self.prefs.ui_scale = 1.0;
    }

    fn apply_scale_if_changed(&mut self, ctx: &egui::Context) {
        if (self.prefs.ui_scale - self.applied_scale).abs() > f32::EPSILON {
            ctx.set_pixels_per_point(self.prefs.ui_scale);
            self.applied_scale = self.prefs.ui_scale;
            save_prefs(&self.prefs);
        }
    }
}

impl eframe::App for AssistantApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_net(ctx);

        // ⌘+ / ⌘- / ⌘0 font-size shortcuts. Accept Ctrl on non-Mac.
        let (zoom_in, zoom_out, zoom_reset) = ctx.input(|i| {
            let mac_or_ctrl = i.modifiers.command || i.modifiers.mac_cmd || i.modifiers.ctrl;
            (
                mac_or_ctrl
                    && (i.key_pressed(egui::Key::Plus)
                        || i.key_pressed(egui::Key::Equals)),
                mac_or_ctrl && i.key_pressed(egui::Key::Minus),
                mac_or_ctrl && i.key_pressed(egui::Key::Num0),
            )
        });
        if zoom_in {
            self.bump_scale(UI_SCALE_STEP);
        }
        if zoom_out {
            self.bump_scale(-UI_SCALE_STEP);
        }
        if zoom_reset {
            self.reset_scale();
        }

        egui::TopBottomPanel::top("topbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("AI Assistant");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⚙ Settings").clicked() {
                        self.show_settings = !self.show_settings;
                    }
                    let color = if self.connected {
                        egui::Color32::from_rgb(80, 200, 120)
                    } else {
                        egui::Color32::from_rgb(220, 100, 100)
                    };
                    ui.colored_label(color, &self.status);
                });
            });
        });

        if self.show_settings {
            egui::SidePanel::right("settings")
                .resizable(true)
                .default_width(280.0)
                .show(ctx, |ui| {
                    ui.heading("Settings");
                    ui.separator();
                    ui.label("Backend URL");
                    ui.text_edit_singleline(&mut self.pending_url_edit);
                    if ui.button("Reconnect").clicked() {
                        self.url = self.pending_url_edit.clone();
                        let _ = self.ui_tx.send(UiToNet::Reconnect(self.url.clone()));
                    }
                    ui.separator();
                    ui.heading("Text size");
                    ui.horizontal(|ui| {
                        if ui.button("−").on_hover_text("⌘−").clicked() {
                            self.bump_scale(-UI_SCALE_STEP);
                        }
                        ui.label(format!("{:.0}%", self.prefs.ui_scale * 100.0));
                        if ui.button("+").on_hover_text("⌘+").clicked() {
                            self.bump_scale(UI_SCALE_STEP);
                        }
                        if ui.button("Reset").on_hover_text("⌘0").clicked() {
                            self.reset_scale();
                        }
                    });
                    let mut s = self.prefs.ui_scale;
                    if ui
                        .add(
                            egui::Slider::new(&mut s, UI_SCALE_MIN..=UI_SCALE_MAX)
                                .step_by(UI_SCALE_STEP as f64 / 2.0)
                                .text("scale"),
                        )
                        .changed()
                    {
                        self.prefs.ui_scale = s;
                    }
                    ui.weak("Saved to ~/.ai-assistant-client.json");
                    ui.separator();
                    ui.heading("Location");
                    let loading = *self.location_loading.lock().unwrap();
                    let current = self.location.lock().unwrap().clone();
                    if loading {
                        ui.label("auto-detecting via IP…");
                    } else if let Some(g) = &current {
                        ui.label(format!(
                            "auto: {} ({:.4}, {:.4})",
                            g.label.clone().unwrap_or_default(),
                            g.lat,
                            g.lon
                        ));
                    } else {
                        ui.label("no auto location");
                    }
                    ui.separator();
                    ui.label("Override (label, lat, lon)");
                    ui.text_edit_singleline(&mut self.location_label_edit);
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut self.location_lat_edit);
                        ui.text_edit_singleline(&mut self.location_lon_edit);
                    });
                    if ui.button("Apply override").clicked() {
                        if let (Ok(lat), Ok(lon)) = (
                            self.location_lat_edit.parse::<f64>(),
                            self.location_lon_edit.parse::<f64>(),
                        ) {
                            *self.location.lock().unwrap() = Some(Geolocation {
                                lat,
                                lon,
                                label: Some(self.location_label_edit.clone()),
                            });
                        }
                    }
                });
        }

        // Pick up files dropped on the window before painting the input panel.
        self.drain_dropped_files(ctx);

        egui::TopBottomPanel::bottom("input")
            .resizable(true)
            .min_height(160.0)
            .default_height(200.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);

                // Live status bar — populated by `ServerMessage::Status`
                // frames during a turn. Shows phase + detail + elapsed
                // seconds so the user can see the backend is alive even
                // during multi-second LLM calls. Cleared by ReplyDone /
                // Error / Disconnect.
                if let Some(ts) = self.turn_status.clone() {
                    let elapsed = self
                        .turn_started_at
                        .map(|t| t.elapsed())
                        .unwrap_or_default();
                    let icon = match ts.phase.as_str() {
                        "sending" => "📤",
                        "preprocessing" => "🛡",
                        "retrieving" | "re_retrieving" => "🔎",
                        "thinking" => "🧠",
                        "searching" => "📨",
                        "reading_manual" => "📖",
                        "escalating" => "⏫",
                        "replying" => "💬",
                        _ => "•",
                    };
                    let label = match &ts.detail {
                        Some(d) => format!("{icon} {} — {}", ts.phase, d),
                        None => format!("{icon} {}", ts.phase),
                    };
                    let elapsed_str = format!(" ({:.1}s)", elapsed.as_secs_f32());
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0));
                        ui.label(egui::RichText::new(label).color(
                            egui::Color32::from_rgb(150, 200, 255),
                        ));
                        ui.weak(elapsed_str);
                    });
                    ui.add_space(2.0);
                    // Keep the elapsed counter ticking even when the
                    // user isn't interacting.
                    ctx.request_repaint_after(std::time::Duration::from_millis(250));
                }

                // Pending attachments strip.
                if !self.pending_attachments.is_empty() {
                    let mut remove_idx: Option<usize> = None;
                    ui.horizontal_wrapped(|ui| {
                        for (i, a) in self.pending_attachments.iter().enumerate() {
                            let size = approx_size(a);
                            let label = format!(
                                "📎 {} ({}, {})",
                                a.name.clone().unwrap_or_else(|| "(unnamed)".into()),
                                a.mime,
                                size
                            );
                            ui.group(|ui| {
                                ui.label(label);
                                if ui.small_button("✕").clicked() {
                                    remove_idx = Some(i);
                                }
                            });
                        }
                    });
                    if let Some(i) = remove_idx {
                        self.pending_attachments.remove(i);
                    }
                    ui.add_space(2.0);
                }

                // Compute height reserve for the bottom row (buttons).
                let reserved = 36.0 + if self.pending_attachments.is_empty() { 0.0 } else { 4.0 };
                ui.add_sized(
                    [ui.available_width(), (ui.available_height() - reserved).max(40.0)],
                    egui::TextEdit::multiline(&mut self.input_buf)
                        .hint_text(
                            "Type a message, paste an email, drop a note. \
                             Drag files into the window or click 📎 to attach. \
                             Enter = newline · ⌘+Enter or Send = send.",
                        ),
                );

                let cmd_enter = ui.input(|i| {
                    i.key_pressed(egui::Key::Enter)
                        && (i.modifiers.command || i.modifiers.mac_cmd || i.modifiers.ctrl)
                });
                ui.horizontal(|ui| {
                    if ui
                        .button("📎 Attach")
                        .on_hover_text("Open file picker. Multiple selection allowed. Or just drag files into the window.")
                        .clicked()
                    {
                        self.pick_files();
                    }
                    // HAZMAT toggle. Red when on; impossible to miss.
                    let hazmat_label = if self.bypass_preprocessor {
                        egui::RichText::new("☢ HAZMAT ON — bypassing the Preprocessor ☢")
                            .color(egui::Color32::from_rgb(255, 60, 60))
                            .strong()
                    } else {
                        egui::RichText::new("☢ HAZMAT (bypass the Preprocessor)")
                            .color(egui::Color32::from_rgb(200, 140, 80))
                    };
                    ui.checkbox(&mut self.bypass_preprocessor, hazmat_label)
                        .on_hover_text(
                            "DANGEROUS: when on, the next messages skip the Security Preprocessor \
                             and go directly to the Assistant. Use only when you know what you're \
                             doing — the Preprocessor protects you from leaking secrets (2FA codes, \
                             reset links, account numbers) into long-term memory. The bypass is \
                             tagged in the memory audit trail. Toggle off when done.",
                        );
                    let opus_label = if self.force_opus {
                        egui::RichText::new("🧠 Opus (forced)")
                            .color(egui::Color32::from_rgb(180, 140, 255))
                            .strong()
                    } else {
                        egui::RichText::new("🧠 Opus")
                            .color(egui::Color32::from_rgb(160, 160, 200))
                    };
                    ui.checkbox(&mut self.force_opus, opus_label)
                        .on_hover_text(
                            "Force the heavier model (Opus) for the next messages. Default is \
                             Sonnet, which is faster and self-escalates when it judges a \
                             question genuinely needs Opus. Tick this to skip Sonnet entirely \
                             — useful when you already know the question is hard.",
                        );
                    ui.weak(format!(
                        "{} char · {} attached",
                        self.input_buf.chars().count(),
                        self.pending_attachments.len(),
                    ));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let send_label = if self.bypass_preprocessor {
                            egui::RichText::new("Send (HAZMAT)  ⌘↵")
                                .color(egui::Color32::from_rgb(255, 80, 80))
                                .strong()
                        } else {
                            egui::RichText::new("Send  ⌘↵")
                        };
                        let send_clicked = ui
                            .add_enabled(self.connected, egui::Button::new(send_label))
                            .clicked();
                        if send_clicked || cmd_enter {
                            self.send_current();
                        }
                    });
                });
                ui.add_space(4.0);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for turn in &self.transcript {
                        render_turn(ui, turn);
                        ui.add_space(6.0);
                    }
                    if !self.streaming_reply.is_empty() {
                        render_turn(
                            ui,
                            &Turn::Assistant {
                                text: format!("{}▌", self.streaming_reply.trim()),
                                ts: now_str(),
                            },
                        );
                    }
                });
        });

        // Repaint frequently while streaming so the cursor blinks.
        if !self.streaming_reply.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }

        // Push pixels_per_point + persist prefs at most once per frame, only
        // when the scale actually changed.
        self.apply_scale_if_changed(ctx);
    }
}

fn render_turn(ui: &mut egui::Ui, turn: &Turn) {
    let (who, color, body, ts) = match turn {
        Turn::User { text, ts } => ("you", egui::Color32::from_rgb(140, 200, 255), text, ts),
        Turn::Assistant { text, ts } => {
            ("assistant", egui::Color32::from_rgb(180, 230, 180), text, ts)
        }
        Turn::Stub { text, ts } => {
            ("gate", egui::Color32::from_rgb(240, 200, 120), text, ts)
        }
        Turn::Error { text, ts } => ("error", egui::Color32::from_rgb(240, 120, 120), text, ts),
        Turn::System { text, ts } => {
            ("system", egui::Color32::from_rgb(200, 200, 200), text, ts)
        }
    };
    ui.horizontal(|ui| {
        ui.colored_label(color, format!("{who} ·"));
        ui.weak(ts);
    });
    ui.label(body);
}
