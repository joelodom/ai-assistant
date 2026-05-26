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
    Attachment, AttachmentKind, ClientMessage, Geolocation, MessagePayload, Metadata,
    ServerMessage,
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

    input_buf: String,
    /// Optional pasted/dropped attachment as text.
    pending_attachment_text: String,
    pending_attachment_kind: AttachmentKind,
    pending_attachment_name: String,

    show_settings: bool,

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
            input_buf: String::new(),
            pending_attachment_text: String::new(),
            pending_attachment_kind: AttachmentKind::Document,
            pending_attachment_name: String::new(),
            show_settings: false,
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
                }
                NetToUi::Frame(f) => match f {
                    ServerMessage::ReplyChunk { text } => {
                        self.streaming_reply.push_str(&text);
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
                    }
                    ServerMessage::StubNotice { text } => {
                        self.transcript.push(Turn::Stub {
                            text,
                            ts: now_str(),
                        });
                    }
                    ServerMessage::Error { text } => {
                        self.transcript.push(Turn::Error {
                            text,
                            ts: now_str(),
                        });
                    }
                    ServerMessage::Pong => {}
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
        if text.trim().is_empty() && self.pending_attachment_text.is_empty() {
            return;
        }
        let mut attachments = Vec::new();
        if !self.pending_attachment_text.is_empty() {
            attachments.push(Attachment {
                kind: self.pending_attachment_kind,
                data: std::mem::take(&mut self.pending_attachment_text),
                mime: "text/plain".into(),
                name: if self.pending_attachment_name.is_empty() {
                    None
                } else {
                    Some(std::mem::take(&mut self.pending_attachment_name))
                },
            });
        }
        self.transcript.push(Turn::User {
            text: render_user_outgoing(&text, &attachments),
            ts: now_str(),
        });
        let msg = ClientMessage::Message {
            payload: MessagePayload {
                content: text,
                attachments,
            },
            metadata: self.current_metadata(),
        };
        let _ = self.ui_tx.send(UiToNet::Send(msg));
    }
}

fn render_user_outgoing(text: &str, attachments: &[Attachment]) -> String {
    if attachments.is_empty() {
        text.to_string()
    } else {
        let mut s = text.to_string();
        for a in attachments {
            s.push_str(&format!(
                "\n[attached {:?}{}]",
                a.kind,
                a.name.as_deref().map(|n| format!(" {n}")).unwrap_or_default()
            ));
        }
        s
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
                    ui.separator();
                    ui.heading("Attachment");
                    ui.label("Type a text attachment (e.g. paste email body):");
                    egui::ComboBox::from_label("kind")
                        .selected_text(format!("{:?}", self.pending_attachment_kind))
                        .show_ui(ui, |ui| {
                            for k in [
                                AttachmentKind::Email,
                                AttachmentKind::Document,
                                AttachmentKind::Calendar,
                                AttachmentKind::Photo,
                            ] {
                                ui.selectable_value(
                                    &mut self.pending_attachment_kind,
                                    k,
                                    format!("{:?}", k),
                                );
                            }
                        });
                    ui.text_edit_singleline(&mut self.pending_attachment_name);
                    ui.add(
                        egui::TextEdit::multiline(&mut self.pending_attachment_text)
                            .desired_rows(6),
                    );
                });
        }

        egui::TopBottomPanel::bottom("input")
            .resizable(true)
            .min_height(120.0)
            .default_height(160.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.add_sized(
                    [ui.available_width(), ui.available_height() - 36.0],
                    egui::TextEdit::multiline(&mut self.input_buf)
                        .hint_text(
                            "Type a message, paste an email, drop a note. \
                             Enter = newline · ⌘+Enter or Send button = send.",
                        ),
                );
                // ⌘+Enter shortcut works whether or not the text area has focus.
                let cmd_enter = ui.input(|i| {
                    i.key_pressed(egui::Key::Enter)
                        && (i.modifiers.command || i.modifiers.mac_cmd || i.modifiers.ctrl)
                });
                ui.horizontal(|ui| {
                    ui.weak(format!("{} char(s)", self.input_buf.chars().count()));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let send_clicked = ui
                            .add_enabled(self.connected, egui::Button::new("Send  ⌘↵"))
                            .clicked();
                        if !self.pending_attachment_text.is_empty() {
                            ui.weak(format!(
                                "attachment: {:?} ({} chars)",
                                self.pending_attachment_kind,
                                self.pending_attachment_text.chars().count()
                            ));
                        }
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
