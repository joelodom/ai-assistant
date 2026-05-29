//! Visual theme: real font faces, a refined dark palette, and global spacing.
//!
//! Why real font files: egui's bundled font set ships a single weight, so its
//! `RichText::strong()` only *brightens* text — it cannot render a heavier
//! stroke. To get genuine boldface (and italics) we load actual faces from the
//! OS at runtime and register them as named font families ("bold", "italic",
//! "bolditalic"). The inline-markdown renderer in `markdown.rs` then selects
//! those families per span.
//!
//! Nothing is vendored into the repo: faces are read from the system font
//! directories on first launch. If a face is missing (e.g. on a non-mac host),
//! we fall back to egui's bundled fonts — the UI still runs, bold simply
//! renders at normal weight rather than crashing.

use eframe::egui::{self, Color32, FontFamily, FontId, Rounding, Stroke};

// ── Palette ─────────────────────────────────────────────────────────────
// A calm, low-chroma dark scheme. Card fills sit just a hair above the panel
// so messages read as distinct surfaces without garish color blocks.

pub const BG_PANEL: Color32 = Color32::from_rgb(0x15, 0x17, 0x1c);
pub const BG_RAISED: Color32 = Color32::from_rgb(0x1b, 0x1e, 0x25);
pub const BG_INPUT: Color32 = Color32::from_rgb(0x20, 0x24, 0x2c);

pub const CARD_USER: Color32 = Color32::from_rgb(0x18, 0x21, 0x2f);
pub const CARD_ASSISTANT: Color32 = Color32::from_rgb(0x1b, 0x1f, 0x27);
pub const CARD_GATE: Color32 = Color32::from_rgb(0x26, 0x20, 0x16);
pub const CARD_ERROR: Color32 = Color32::from_rgb(0x2a, 0x1b, 0x1c);
pub const CARD_SYSTEM: Color32 = Color32::from_rgb(0x1c, 0x1e, 0x24);

pub const ACCENT_USER: Color32 = Color32::from_rgb(0x6f, 0xb3, 0xff);
pub const ACCENT_ASSISTANT: Color32 = Color32::from_rgb(0x7c, 0xd6, 0xa0);
pub const ACCENT_GATE: Color32 = Color32::from_rgb(0xe6, 0xb4, 0x50);
pub const ACCENT_ERROR: Color32 = Color32::from_rgb(0xff, 0x7a, 0x7a);
pub const ACCENT_SYSTEM: Color32 = Color32::from_rgb(0x9a, 0xa2, 0xad);

pub const TEXT_BODY: Color32 = Color32::from_rgb(0xd7, 0xdb, 0xe1);
pub const TEXT_STRONG: Color32 = Color32::from_rgb(0xf3, 0xf5, 0xf8);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x7c, 0x84, 0x90);
pub const CODE_FG: Color32 = Color32::from_rgb(0x8f, 0xd6, 0xc9);
pub const CODE_BG: Color32 = Color32::from_rgb(0x10, 0x14, 0x19);

// Base text sizes (logical points; the global UI scale multiplies these).
pub const SIZE_BODY: f32 = 15.0;
pub const SIZE_SMALL: f32 = 12.0;

/// Named font family for bold spans. Registered in [`install_fonts`].
pub fn bold_family() -> FontFamily {
    FontFamily::Name("bold".into())
}
pub fn italic_family() -> FontFamily {
    FontFamily::Name("italic".into())
}
pub fn bold_italic_family() -> FontFamily {
    FontFamily::Name("bolditalic".into())
}

/// Install fonts, text styles, palette, and spacing. Call once at startup.
pub fn install(ctx: &egui::Context) {
    install_fonts(ctx);

    let mut style = (*ctx.style()).clone();

    use egui::TextStyle::*;
    style.text_styles = [
        (Heading, FontId::new(21.0, bold_family())),
        (Body, FontId::new(SIZE_BODY, FontFamily::Proportional)),
        (Monospace, FontId::new(13.5, FontFamily::Monospace)),
        (Button, FontId::new(14.0, FontFamily::Proportional)),
        (Small, FontId::new(SIZE_SMALL, FontFamily::Proportional)),
    ]
    .into();

    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(11.0, 6.0);
    style.spacing.window_margin = egui::Margin::same(12.0);
    style.spacing.menu_margin = egui::Margin::same(8.0);
    style.spacing.interact_size.y = 28.0;

    style.visuals = dark_visuals();
    ctx.set_style(style);
}

/// Load real faces from the OS and register the named bold/italic families.
///
/// We prefer a single consistent family (Arial regular/bold/italic), so weight
/// changes look intentional rather than like a font swap. SF Mono backs code
/// spans. All loads are best-effort; whatever is missing falls back to egui's
/// bundled fonts, which remain in each family list so emoji and any glyph the
/// primary face lacks still render.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // Fallback chains already present (egui defaults: Ubuntu-Light + emoji).
    let prop_fallback = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let mono_fallback = fonts
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();

    // (key, candidate paths). First existing file wins; index 0 only, so we
    // stick to single-face .ttf and avoid .ttc face-index guesswork.
    let have_regular = load_face(
        &mut fonts,
        "ui-regular",
        &[
            "/System/Library/Fonts/Supplemental/Arial.ttf",
            "/Library/Fonts/Arial.ttf",
        ],
    );
    let have_bold = load_face(
        &mut fonts,
        "ui-bold",
        &["/System/Library/Fonts/Supplemental/Arial Bold.ttf"],
    );
    let have_italic = load_face(
        &mut fonts,
        "ui-italic",
        &["/System/Library/Fonts/Supplemental/Arial Italic.ttf"],
    );
    let have_bolditalic = load_face(
        &mut fonts,
        "ui-bolditalic",
        &["/System/Library/Fonts/Supplemental/Arial Bold Italic.ttf"],
    );
    let have_mono = load_face(
        &mut fonts,
        "ui-mono",
        &[
            "/System/Library/Fonts/SFNSMono.ttf",
            "/System/Library/Fonts/Supplemental/Andale Mono.ttf",
        ],
    );

    // Proportional: our regular face first, then egui's defaults as fallback.
    let mut proportional = Vec::new();
    if have_regular {
        proportional.push("ui-regular".to_owned());
    }
    proportional.extend(prop_fallback.clone());
    fonts
        .families
        .insert(FontFamily::Proportional, proportional);

    // Monospace: SF Mono / Andale first, then egui's Hack.
    let mut monospace = Vec::new();
    if have_mono {
        monospace.push("ui-mono".to_owned());
    }
    monospace.extend(mono_fallback);
    fonts.families.insert(FontFamily::Monospace, monospace);

    // Named families for emphasis. Each prefers its real face, degrades to the
    // regular face, then to bundled fallbacks — always a valid, present key.
    insert_named(
        &mut fonts,
        "bold",
        have_bold,
        "ui-bold",
        have_regular,
        &prop_fallback,
    );
    insert_named(
        &mut fonts,
        "italic",
        have_italic,
        "ui-italic",
        have_regular,
        &prop_fallback,
    );
    // Bold-italic prefers its own face, then bold, then regular.
    {
        let mut fam = Vec::new();
        if have_bolditalic {
            fam.push("ui-bolditalic".to_owned());
        } else if have_bold {
            fam.push("ui-bold".to_owned());
        } else if have_regular {
            fam.push("ui-regular".to_owned());
        }
        fam.extend(prop_fallback.clone());
        fonts
            .families
            .insert(FontFamily::Name("bolditalic".into()), fam);
    }

    ctx.set_fonts(fonts);
}

/// Register a named family that prefers `primary_key` (if loaded), then the
/// regular face (if loaded), then the bundled proportional fallbacks.
fn insert_named(
    fonts: &mut egui::FontDefinitions,
    name: &str,
    have_primary: bool,
    primary_key: &str,
    have_regular: bool,
    prop_fallback: &[String],
) {
    let mut fam = Vec::new();
    if have_primary {
        fam.push(primary_key.to_owned());
    } else if have_regular {
        fam.push("ui-regular".to_owned());
    }
    fam.extend(prop_fallback.iter().cloned());
    fonts.families.insert(FontFamily::Name(name.into()), fam);
}

/// Read the first existing path into a font_data entry. Returns whether one
/// loaded.
fn load_face(fonts: &mut egui::FontDefinitions, key: &str, candidates: &[&str]) -> bool {
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert(key.to_owned(), egui::FontData::from_owned(bytes));
            return true;
        }
    }
    false
}

fn dark_visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    let round = Rounding::same(7.0);

    v.panel_fill = BG_PANEL;
    v.window_fill = BG_RAISED;
    v.window_rounding = Rounding::same(10.0);
    v.window_stroke = Stroke::new(1.0, Color32::from_rgb(0x2c, 0x31, 0x3a));
    v.extreme_bg_color = BG_INPUT;
    v.faint_bg_color = Color32::from_rgb(0x1f, 0x23, 0x2a);
    v.override_text_color = None;
    v.hyperlink_color = ACCENT_USER;

    v.selection.bg_fill = Color32::from_rgba_unmultiplied(0x6f, 0xb3, 0xff, 0x40);
    v.selection.stroke = Stroke::new(1.0, ACCENT_USER);

    // Buttons / interactive widgets: soft raised chips with subtle hover.
    let w = &mut v.widgets;
    w.noninteractive.rounding = round;
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_BODY);
    w.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x25, 0x29, 0x31));

    w.inactive.rounding = round;
    w.inactive.weak_bg_fill = Color32::from_rgb(0x25, 0x2a, 0x33);
    w.inactive.bg_fill = Color32::from_rgb(0x25, 0x2a, 0x33);
    w.inactive.fg_stroke = Stroke::new(1.0, Color32::from_rgb(0xc4, 0xca, 0xd2));
    w.inactive.bg_stroke = Stroke::NONE;

    w.hovered.rounding = round;
    w.hovered.weak_bg_fill = Color32::from_rgb(0x30, 0x37, 0x42);
    w.hovered.bg_fill = Color32::from_rgb(0x30, 0x37, 0x42);
    w.hovered.fg_stroke = Stroke::new(1.0, TEXT_STRONG);
    w.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x3b, 0x44, 0x52));

    w.active.rounding = round;
    w.active.weak_bg_fill = Color32::from_rgb(0x3a, 0x44, 0x54);
    w.active.bg_fill = Color32::from_rgb(0x3a, 0x44, 0x54);
    w.active.fg_stroke = Stroke::new(1.0, TEXT_STRONG);

    w.open.rounding = round;

    v
}
