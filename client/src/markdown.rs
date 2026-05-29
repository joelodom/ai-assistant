//! Lightweight Markdown rendering for the transcript.
//!
//! The assistant speaks in Markdown (`**bold**`, `*italic*`, `# headings`,
//! `- bullets`, `` `code` ``). Showing those characters literally is what made
//! the UI feel like a prototype. This module turns them into *real* formatting:
//! emphasis becomes an actual heavier/slanted font face (see `theme.rs`), code
//! becomes monospace on a faint slab, headings and bullets get real structure.
//!
//! It is deliberately small — a line-oriented block pass plus a flat inline
//! tokenizer. It is not a CommonMark implementation and does not try to be:
//! it covers what the assistant actually emits, fails safe (an unterminated
//! `**` just renders literally), and never panics on odd input.
//!
//! One intentional non-rule: underscores are *not* emphasis. Personal data is
//! full of `IMG_4708.png` and `N271SD_notes`; treating `_` as italics would
//! mangle them. Only `*` and backticks are markers.

use crate::theme::{
    bold_family, bold_italic_family, italic_family, CODE_BG, CODE_FG, SIZE_BODY, TEXT_MUTED,
    TEXT_STRONG,
};
use eframe::egui::{
    self,
    text::{LayoutJob, TextFormat},
    Color32, FontFamily, FontId, Margin, Rounding,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Span {
    Normal,
    Bold,
    Italic,
    BoldItalic,
    Code,
}

/// Render `text` as Markdown into `ui`, using `body_color` for ordinary prose.
pub fn render_markdown(ui: &mut egui::Ui, text: &str, body_color: Color32) {
    let size = egui::TextStyle::Body.resolve(ui.style()).size;
    let size = if size > 0.0 { size } else { SIZE_BODY };

    let mut in_fence = false;
    let mut code_buf: Vec<String> = Vec::new();

    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let trimmed = line.trim_start();

        // ``` fenced code block (language tag, if any, is ignored).
        if trimmed.starts_with("```") {
            if in_fence {
                render_code_block(ui, &code_buf);
                code_buf.clear();
            }
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            code_buf.push(line.to_string());
            continue;
        }

        if line.trim().is_empty() {
            ui.add_space(5.0);
            continue;
        }

        let indent = leading_indent(line);

        // Headings: render the whole line in the heading weight (inline markers
        // inside a heading are rare and not worth the ambiguity).
        if let Some((level, rest)) = heading(trimmed) {
            let hsize = match level {
                1 => size + 6.0,
                2 => size + 3.0,
                _ => size + 1.0,
            };
            ui.add_space(2.0);
            indented(ui, indent, |ui| {
                ui.label(
                    egui::RichText::new(rest)
                        .font(FontId::new(hsize, bold_family()))
                        .color(TEXT_STRONG),
                );
            });
            return_space(ui);
            continue;
        }

        // Bullets: "- ", "* ", "+ ", "• " (the space is required, so "*x*" at
        // line start is still italics, not a bullet).
        if let Some(rest) = bullet(trimmed) {
            indented(ui, indent, |ui| {
                ui.horizontal_top(|ui| {
                    ui.label(
                        egui::RichText::new("•")
                            .color(TEXT_MUTED)
                            .font(FontId::new(size, FontFamily::Proportional)),
                    );
                    ui.add_space(7.0);
                    label_job(ui, inline_job(rest, size, body_color));
                });
            });
            continue;
        }

        // Numbered list: "1. ", "2) " …
        if let Some((marker, rest)) = numbered(trimmed) {
            indented(ui, indent, |ui| {
                ui.horizontal_top(|ui| {
                    ui.label(
                        egui::RichText::new(marker)
                            .color(TEXT_MUTED)
                            .font(FontId::new(size, FontFamily::Proportional)),
                    );
                    ui.add_space(7.0);
                    label_job(ui, inline_job(rest, size, body_color));
                });
            });
            continue;
        }

        // Blockquote: "> "
        if let Some(rest) = trimmed
            .strip_prefix("> ")
            .or_else(|| trimmed.strip_prefix(">"))
        {
            indented(ui, indent + 6.0, |ui| {
                label_job(ui, inline_job(rest, size, TEXT_MUTED));
            });
            continue;
        }

        // Ordinary paragraph line.
        indented(ui, indent, |ui| {
            label_job(ui, inline_job(trimmed, size, body_color));
        });
    }

    if in_fence && !code_buf.is_empty() {
        render_code_block(ui, &code_buf);
    }
}

/// A little breathing room after a heading.
fn return_space(ui: &mut egui::Ui) {
    ui.add_space(1.0);
}

/// Run `add` inside an optional left indent. Keeps wrapping correct because the
/// inner ui owns the remaining width.
fn indented(ui: &mut egui::Ui, indent: f32, add: impl FnOnce(&mut egui::Ui)) {
    if indent <= 0.5 {
        add(ui);
    } else {
        ui.horizontal_top(|ui| {
            ui.add_space(indent);
            ui.vertical(|ui| add(ui));
        });
    }
}

/// Set the job's wrap width to the available width and emit it.
fn label_job(ui: &mut egui::Ui, mut job: LayoutJob) {
    job.wrap.max_width = ui.available_width();
    ui.label(job);
}

fn render_code_block(ui: &mut egui::Ui, lines: &[String]) {
    let text = lines.join("\n");
    let size = egui::TextStyle::Monospace.resolve(ui.style()).size;
    ui.add_space(2.0);
    egui::Frame::none()
        .fill(CODE_BG)
        .rounding(Rounding::same(6.0))
        .inner_margin(Margin::symmetric(10.0, 8.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                egui::RichText::new(text)
                    .font(FontId::new(size, FontFamily::Monospace))
                    .color(CODE_FG),
            );
        });
    ui.add_space(2.0);
}

fn heading(trimmed: &str) -> Option<(u8, &str)> {
    for (hashes, level) in [("### ", 3u8), ("## ", 2), ("# ", 1)] {
        if let Some(rest) = trimmed.strip_prefix(hashes) {
            return Some((level, rest));
        }
    }
    None
}

fn bullet(trimmed: &str) -> Option<&str> {
    for p in ["- ", "* ", "+ ", "• "] {
        if let Some(rest) = trimmed.strip_prefix(p) {
            return Some(rest);
        }
    }
    None
}

/// Match "12. rest" or "12) rest", returning the rendered marker and the rest.
fn numbered(trimmed: &str) -> Option<(String, &str)> {
    let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 3 {
        return None;
    }
    let after = &trimmed[digits.len()..];
    let rest = after
        .strip_prefix(". ")
        .or_else(|| after.strip_prefix(") "))?;
    Some((format!("{digits}."), rest))
}

fn leading_indent(line: &str) -> f32 {
    let mut px: f32 = 0.0;
    for c in line.chars() {
        match c {
            ' ' => px += 3.5,
            '\t' => px += 14.0,
            _ => break,
        }
    }
    px.min(48.0)
}

/// Flat inline tokenizer → a `LayoutJob` with one section per styled span.
fn inline_job(text: &str, size: f32, body_color: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut normal = String::new();

    while i < n {
        // `code`
        if chars[i] == '`' {
            if let Some(j) = find_char(&chars, i + 1, '`') {
                flush(&mut job, &mut normal, size, body_color);
                let inner: String = chars[i + 1..j].iter().collect();
                append(&mut job, &inner, Span::Code, size, body_color);
                i = j + 1;
                continue;
            }
        }

        // *emph* / **strong** / ***both***
        if chars[i] == '*' {
            let run = run_len(&chars, i, '*').min(3);
            let open_end = i + run;
            // Opening delimiter must hug a non-space.
            if open_end < n && !chars[open_end].is_whitespace() {
                if let Some(j) = find_run(&chars, open_end, '*', run) {
                    if j > open_end && !chars[j - 1].is_whitespace() {
                        flush(&mut job, &mut normal, size, body_color);
                        let inner: String = chars[open_end..j].iter().collect();
                        let span = match run {
                            3 => Span::BoldItalic,
                            2 => Span::Bold,
                            _ => Span::Italic,
                        };
                        append(&mut job, &inner, span, size, body_color);
                        i = j + run;
                        continue;
                    }
                }
            }
        }

        normal.push(chars[i]);
        i += 1;
    }

    flush(&mut job, &mut normal, size, body_color);
    job
}

fn flush(job: &mut LayoutJob, normal: &mut String, size: f32, body_color: Color32) {
    if !normal.is_empty() {
        append(job, normal, Span::Normal, size, body_color);
        normal.clear();
    }
}

fn append(job: &mut LayoutJob, text: &str, span: Span, size: f32, body_color: Color32) {
    if text.is_empty() {
        return;
    }
    let (font_id, color, bg) = match span {
        Span::Normal => (
            FontId::new(size, FontFamily::Proportional),
            body_color,
            Color32::TRANSPARENT,
        ),
        Span::Bold => (
            FontId::new(size, bold_family()),
            brighten(body_color, 0.45),
            Color32::TRANSPARENT,
        ),
        Span::Italic => (
            FontId::new(size, italic_family()),
            body_color,
            Color32::TRANSPARENT,
        ),
        Span::BoldItalic => (
            FontId::new(size, bold_italic_family()),
            brighten(body_color, 0.45),
            Color32::TRANSPARENT,
        ),
        Span::Code => (
            FontId::new(size * 0.92, FontFamily::Monospace),
            CODE_FG,
            CODE_BG,
        ),
    };
    let mut fmt = TextFormat {
        font_id,
        color,
        ..Default::default()
    };
    fmt.background = bg;
    job.append(text, 0.0, fmt);
}

/// Lerp a color toward white by `t` (0..=1) so emphasis pops a little beyond
/// the weight change, whatever the base role color is.
fn brighten(c: Color32, t: f32) -> Color32 {
    let mix = |v: u8| {
        (v as f32 + (255.0 - v as f32) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color32::from_rgb(mix(c.r()), mix(c.g()), mix(c.b()))
}

fn find_char(chars: &[char], from: usize, target: char) -> Option<usize> {
    (from..chars.len()).find(|&k| chars[k] == target)
}

fn run_len(chars: &[char], from: usize, target: char) -> usize {
    let mut k = from;
    while k < chars.len() && chars[k] == target {
        k += 1;
    }
    k - from
}

/// Find the next run of `>= len` copies of `target` at/after `from`, returning
/// the run's start index.
fn find_run(chars: &[char], from: usize, target: char, len: usize) -> Option<usize> {
    let mut k = from;
    while k < chars.len() {
        if chars[k] == target && run_len(chars, k, target) >= len {
            return Some(k);
        }
        k += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn families(job: &LayoutJob) -> Vec<(String, FontFamily)> {
        job.sections
            .iter()
            .map(|s| {
                let txt = job.text[s.byte_range.clone()].to_string();
                (txt, s.format.font_id.family.clone())
            })
            .collect()
    }

    #[test]
    fn bold_becomes_bold_family() {
        let job = inline_job("**Today**", 15.0, Color32::WHITE);
        let segs = families(&job);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].0, "Today");
        assert_eq!(segs[0].1, bold_family());
    }

    #[test]
    fn mixed_inline_splits_into_sections() {
        let job = inline_job("see *Blake* and `IMG.png` now", 15.0, Color32::WHITE);
        let segs = families(&job);
        let texts: Vec<&str> = segs.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(texts, vec!["see ", "Blake", " and ", "IMG.png", " now"]);
        assert_eq!(segs[1].1, italic_family());
        assert_eq!(segs[3].1, FontFamily::Monospace);
    }

    #[test]
    fn underscores_are_literal_not_italics() {
        let job = inline_job("file IMG_4708.png and N271SD_notes", 15.0, Color32::WHITE);
        assert_eq!(job.text, "file IMG_4708.png and N271SD_notes");
        assert_eq!(job.sections.len(), 1);
        assert_eq!(
            job.sections[0].format.font_id.family,
            FontFamily::Proportional
        );
    }

    #[test]
    fn unterminated_marker_is_literal() {
        let job = inline_job("a **bold start with no close", 15.0, Color32::WHITE);
        assert_eq!(job.text, "a **bold start with no close");
        assert_eq!(job.sections.len(), 1);
    }

    #[test]
    fn emphasis_needs_nonspace_neighbors() {
        // "2 * 3 * 4" should not be read as italics around " 3 ".
        let job = inline_job("2 * 3 * 4", 15.0, Color32::WHITE);
        assert_eq!(job.sections.len(), 1);
        assert_eq!(
            job.sections[0].format.font_id.family,
            FontFamily::Proportional
        );
    }

    #[test]
    fn block_helpers() {
        assert_eq!(heading("## Soon"), Some((2, "Soon")));
        assert_eq!(bullet("- item"), Some("item"));
        assert_eq!(bullet("*italic*"), None); // no space ⇒ not a bullet
        assert_eq!(numbered("3. third"), Some(("3.".to_string(), "third")));
    }
}
