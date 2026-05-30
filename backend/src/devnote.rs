//! Developer notes: the `NOTE_TO_DEV` marker and the append-only
//! `SUGGESTIONS.md` log it feeds.
//!
//! The assistant emits a NOTE_TO_DEV block ONLY when the user points out a
//! problem with its behavior, or when the user suggests an improvement or fix
//! — never on its own initiative (see DEFAULT_MANUAL.md `developer-notes`).
//! The backend parses the block out of the reply (so the user never sees the
//! raw marker), attaches the diagnostic logs captured for this turn (see
//! `logcapture.rs`), and appends a timestamped entry to
//! `<memory_root>/SUGGESTIONS.md`. The user — or Claude Code, on request —
//! reads that file later to triage fixes and shape the roadmap.
//!
//! Everything written here is derived from sanitized data: the assistant only
//! ever sees Preprocessor output, and the attached logs are formatted tracing
//! lines that carry no raw input per the project's logging discipline.

use crate::assistant::{NOTE_TO_DEV_END_MARKER, NOTE_TO_DEV_MARKER};
use std::io::Write;
use std::path::Path;

/// File name, written alongside SYSTEM_MANUAL.md in the memory root. Memory
/// items live under `items/`, so a sibling `.md` is never ingested as memory.
pub const SUGGESTIONS_FILENAME: &str = "SUGGESTIONS.md";

const SUGGESTIONS_HEADER: &str = "# Developer notes & suggestions\n\
\n\
Append-only log curated by the assistant. An entry is recorded ONLY when the\n\
user points out a problem or suggests an improvement — the assistant never\n\
adds notes on its own initiative. Each entry carries the diagnostic logs from\n\
the turn in question. Read this with Claude Code to triage fixes and shape the\n\
roadmap.\n\
\n\
---\n\
\n";

/// Why the note was filed. Both are user-initiated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    /// The user pointed out something the assistant did wrong.
    Issue,
    /// The user suggested an improvement, fix, or feature.
    Idea,
}

impl NoteKind {
    fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "idea" | "suggestion" | "improvement" | "feature" | "enhancement" => NoteKind::Idea,
            _ => NoteKind::Issue,
        }
    }
    fn label(self) -> &'static str {
        match self {
            NoteKind::Issue => "issue",
            NoteKind::Idea => "idea",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DevNote {
    pub kind: NoteKind,
    /// One-line summary of what the user asked / pointed out.
    pub input: String,
    /// One-line summary of what the assistant had answered.
    pub output: String,
    /// Free-form, possibly multi-line, full explanation.
    pub details: String,
}

/// Pull a NOTE_TO_DEV block out of an assistant reply. Returns the reply with
/// the whole block removed (what the user should see) and the parsed note, if
/// any. If a start marker appears with no end marker, the block is treated as
/// running to end-of-text so a malformed block can never leak to the user.
pub fn extract(reply: &str) -> (String, Option<DevNote>) {
    let lines: Vec<&str> = reply.lines().collect();
    let Some(start) = lines
        .iter()
        .position(|l| l.trim().starts_with(NOTE_TO_DEV_MARKER))
    else {
        return (reply.to_string(), None);
    };
    // First END marker after the start, if any.
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, l)| l.trim() == NOTE_TO_DEV_END_MARKER)
        .map(|(i, _)| i);
    let block_end = end.unwrap_or(lines.len()); // exclusive end of block body

    // Visible reply = lines before the block + lines after the END marker.
    let mut visible: Vec<&str> = Vec::with_capacity(lines.len());
    visible.extend_from_slice(&lines[..start]);
    if let Some(e) = end {
        visible.extend_from_slice(&lines[e + 1..]);
    }
    let visible = visible.join("\n").trim().to_string();

    let note = parse_block(&lines[start + 1..block_end]);
    (visible, Some(note))
}

/// Parse the body lines between the start and END markers. Recognizes
/// `TYPE:`, `INPUT:`, `OUTPUT:` as single-line fields and `DETAILS:` as a
/// multi-line field running to the end of the block. If none of the fields
/// are present, the whole body is treated as details.
fn parse_block(body: &[&str]) -> DevNote {
    let mut kind = NoteKind::Issue;
    let mut input = String::new();
    let mut output = String::new();
    let mut details = String::new();
    let mut saw_field = false;

    let mut i = 0;
    while i < body.len() {
        let t = body[i].trim_start();
        if let Some(v) = t.strip_prefix("TYPE:") {
            kind = NoteKind::parse(v);
            saw_field = true;
        } else if let Some(v) = t.strip_prefix("INPUT:") {
            input = v.trim().to_string();
            saw_field = true;
        } else if let Some(v) = t.strip_prefix("OUTPUT:") {
            output = v.trim().to_string();
            saw_field = true;
        } else if let Some(v) = t.strip_prefix("DETAILS:") {
            // DETAILS captures the rest of its line plus every line after it.
            let mut acc = vec![v.trim_start().to_string()];
            for rest in &body[i + 1..] {
                acc.push((*rest).to_string());
            }
            details = acc.join("\n").trim().to_string();
            saw_field = true;
            break;
        }
        i += 1;
    }

    // Tolerate a block that's just prose with no field labels.
    if !saw_field {
        details = body.join("\n").trim().to_string();
    }

    DevNote {
        kind,
        input,
        output,
        details,
    }
}

/// Append a developer note (plus the captured diagnostic logs) to
/// `<root>/SUGGESTIONS.md`, creating the file with a header on first write.
/// Append-only: a partial write on crash can at worst truncate the last
/// entry, which is acceptable for a diagnostic log.
pub fn append(root: &Path, note: &DevNote, logs: &[String]) -> std::io::Result<()> {
    let path = root.join(SUGGESTIONS_FILENAME);
    let ts = chrono::Utc::now().format("%Y-%m-%d %H:%M:%SZ");

    let mut entry = String::new();
    if !path.exists() {
        entry.push_str(SUGGESTIONS_HEADER);
    }
    entry.push_str(&format!("## {ts} — {}\n\n", note.kind.label()));

    let input = note.input.trim();
    if !input.is_empty() {
        entry.push_str(&format!("**What the user pointed out:** {input}\n\n"));
    }
    let output = note.output.trim();
    if !output.is_empty() {
        entry.push_str(&format!("**What the assistant had answered:** {output}\n\n"));
    }
    let details = note.details.trim();
    if !details.is_empty() {
        entry.push_str("**Details**\n\n");
        entry.push_str(details);
        entry.push_str("\n\n");
    }

    entry.push_str("**Diagnostic logs** (start of the turn in question → now):\n\n");
    entry.push_str("```text\n");
    if logs.is_empty() {
        entry.push_str("(no logs captured)\n");
    } else {
        for l in logs {
            // Neutralize any stray fence so a log line can't break out of the
            // code block. Our formatted tracing lines never contain ```, but
            // be defensive.
            entry.push_str(&l.replace("```", "ʼʼʼ"));
            entry.push('\n');
        }
    }
    entry.push_str("```\n\n---\n\n");

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    f.write_all(entry.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_none_when_no_marker() {
        let (visible, note) = extract("just a normal reply\nwith two lines");
        assert_eq!(visible, "just a normal reply\nwith two lines");
        assert!(note.is_none());
    }

    #[test]
    fn extract_strips_block_and_keeps_surrounding_prose() {
        let reply = "Thanks, I've recorded that.\n\
                     NOTE_TO_DEV:\n\
                     TYPE: issue\n\
                     INPUT: what's on my plate\n\
                     OUTPUT: listed a dentist appt that doesn't exist\n\
                     DETAILS: I retrieved a stale item and stated it as fact.\n\
                     It should have been hedged.\n\
                     END_NOTE_TO_DEV\n\
                     Anything else?";
        let (visible, note) = extract(reply);
        assert_eq!(visible, "Thanks, I've recorded that.\nAnything else?");
        let note = note.unwrap();
        assert_eq!(note.kind, NoteKind::Issue);
        assert_eq!(note.input, "what's on my plate");
        assert_eq!(note.output, "listed a dentist appt that doesn't exist");
        assert_eq!(
            note.details,
            "I retrieved a stale item and stated it as fact.\nIt should have been hedged."
        );
    }

    #[test]
    fn type_idea_is_parsed() {
        let reply = "NOTE_TO_DEV:\nTYPE: idea\nDETAILS: add dark mode\nEND_NOTE_TO_DEV";
        let (_visible, note) = extract(reply);
        assert_eq!(note.unwrap().kind, NoteKind::Idea);
    }

    #[test]
    fn missing_end_marker_strips_to_eof_so_nothing_leaks() {
        let reply = "ok\nNOTE_TO_DEV:\nTYPE: issue\nDETAILS: oops the rest leaks?";
        let (visible, note) = extract(reply);
        assert_eq!(visible, "ok");
        assert_eq!(note.unwrap().details, "oops the rest leaks?");
    }

    #[test]
    fn block_without_field_labels_becomes_details() {
        let reply = "NOTE_TO_DEV:\nThe recall felt slow today.\nMaybe cache it.\nEND_NOTE_TO_DEV";
        let (_v, note) = extract(reply);
        let note = note.unwrap();
        assert_eq!(note.kind, NoteKind::Issue);
        assert_eq!(note.details, "The recall felt slow today.\nMaybe cache it.");
    }

    #[test]
    fn append_writes_header_once_then_appends() {
        let dir = tempfile::tempdir().unwrap();
        let note = DevNote {
            kind: NoteKind::Issue,
            input: "in".into(),
            output: "out".into(),
            details: "details here".into(),
        };
        append(dir.path(), &note, &["log line one".into(), "log line two".into()]).unwrap();
        append(dir.path(), &note, &[]).unwrap();

        let text = std::fs::read_to_string(dir.path().join(SUGGESTIONS_FILENAME)).unwrap();
        // Header appears exactly once.
        assert_eq!(text.matches("# Developer notes & suggestions").count(), 1);
        // Two entries.
        assert_eq!(text.matches("## ").count(), 2);
        assert!(text.contains("**What the user pointed out:** in"));
        assert!(text.contains("log line one"));
        assert!(text.contains("(no logs captured)"));
    }
}
