//! The system manual. A single user-editable markdown file the assistant
//! consults on demand via the `READ_MANUAL` marker.
//!
//! Per the "AI-driven self knowledge" principle: most procedural and
//! reference content lives here, NOT in the always-on assistant prompt.
//! The prompt carries only a small pointer telling the assistant the
//! manual exists and how to read it. Sections are fetched only when the
//! assistant decides it needs them.
//!
//! Storage:
//!   - `<memory-dir>/SYSTEM_MANUAL.md` — the live file. User-editable.
//!     If missing on startup, the embedded default is written here so
//!     fresh installs have something. After that, we never overwrite —
//!     user edits stick.
//!   - `backend/src/DEFAULT_MANUAL.md` — embedded into the binary via
//!     `include_str!`. The fallback when the disk file is missing or
//!     unreadable.
//!
//! Manual sections are delimited by H2 headers (`## section-name`).
//! Section names are kebab-case by convention. The TOC is the list of
//! section names.

use anyhow::Result;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Markdown bundled into the binary. The on-disk SYSTEM_MANUAL.md is
/// initialized from this on fresh installs.
pub const DEFAULT_MANUAL: &str = include_str!("DEFAULT_MANUAL.md");

pub const MANUAL_FILENAME: &str = "SYSTEM_MANUAL.md";

pub struct Manual {
    path: PathBuf,
}

impl Manual {
    /// Open the manual at `<memory_root>/SYSTEM_MANUAL.md`. If the file
    /// doesn't exist yet, write the embedded default to disk so the user
    /// has something to edit. Either way, the returned `Manual` reads
    /// fresh from disk on every call so user edits take effect without
    /// a backend restart.
    pub fn open_or_seed(memory_root: &Path) -> Result<Self> {
        let path = memory_root.join(MANUAL_FILENAME);
        if !path.exists() {
            crate::memory::atomic_write_sync(&path, DEFAULT_MANUAL.as_bytes())?;
            tracing::info!(manual = %path.display(), "manual: wrote default to disk");
        }
        Ok(Self { path })
    }

    /// Read the manual fresh from disk. Falls back to the embedded
    /// default if the on-disk file is unreadable (forward-compat /
    /// user-shot-themselves-in-the-foot defense).
    fn load_text(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap_or_else(|e| {
            tracing::warn!(
                error = %e,
                path = %self.path.display(),
                "manual: failed to read on-disk file; using embedded default"
            );
            DEFAULT_MANUAL.to_string()
        })
    }

    /// Return the list of section names (H2 headers), in document order.
    pub fn toc(&self) -> Vec<String> {
        parse_sections(&self.load_text())
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    /// Read one section by name. Case-insensitive. Returns None if no
    /// section matches.
    pub fn read_section(&self, name: &str) -> Option<String> {
        let needle = name.trim().to_lowercase();
        parse_sections(&self.load_text())
            .into_iter()
            .find(|(s, _)| s.to_lowercase() == needle)
            .map(|(_, body)| body)
    }

    /// Render a TOC for display when the assistant emits a bare
    /// `READ_MANUAL`. Plain text, easy to scan.
    pub fn render_toc(&self) -> String {
        let mut s = String::from(
            "SYSTEM MANUAL — table of contents. Fetch any section with \
             `READ_MANUAL: <name>`.\n\n",
        );
        for name in self.toc() {
            s.push_str(&format!("  - {name}\n"));
        }
        s
    }

    /// Path to the on-disk manual file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Parse a markdown string into (section_name, body) pairs, splitting on
/// lines that start with `## `. Anything before the first H2 is treated
/// as preamble and ignored — it's not addressable. Trailing newlines on
/// each body are stripped.
fn parse_sections(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_body: Vec<&str> = Vec::new();

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            // Flush previous section.
            if let Some(name) = current_name.take() {
                let body = current_body.join("\n").trim_end().to_string();
                out.push((name, body));
            }
            current_body.clear();
            current_name = Some(rest.trim().to_string());
        } else if current_name.is_some() {
            current_body.push(line);
        }
    }
    if let Some(name) = current_name {
        let body = current_body.join("\n").trim_end().to_string();
        out.push((name, body));
    }

    // De-duplicate by section name, keeping the first occurrence. (If
    // someone edits the manual to have two `## foo`, only the first is
    // addressable. Logged but not an error.)
    let mut seen: BTreeMap<String, ()> = BTreeMap::new();
    out.retain(|(name, _)| seen.insert(name.clone(), ()).is_none());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_simple_sections() {
        let md = "preamble\n## a\nbody a\nmore a\n## b\nbody b\n";
        let s = parse_sections(md);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].0, "a");
        assert!(s[0].1.contains("body a"));
        assert_eq!(s[1].0, "b");
        assert_eq!(s[1].1, "body b");
    }

    #[test]
    fn parse_dedupes_repeated_section_names() {
        let md = "## a\nfirst\n## a\nsecond\n";
        let s = parse_sections(md);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].1, "first");
    }

    #[test]
    fn open_or_seed_writes_default_on_fresh_install() {
        let td = TempDir::new().unwrap();
        let m = Manual::open_or_seed(td.path()).unwrap();
        assert!(td.path().join(MANUAL_FILENAME).exists());
        let toc = m.toc();
        assert!(
            !toc.is_empty(),
            "TOC should be non-empty from embedded default"
        );
        // Spot-check: a few sections we know are in the default.
        assert!(toc.iter().any(|s| s == "architecture"));
        assert!(toc.iter().any(|s| s == "invariants"));
        assert!(toc.iter().any(|s| s == "markers"));
    }

    #[test]
    fn open_or_seed_does_not_overwrite_existing() {
        let td = TempDir::new().unwrap();
        let path = td.path().join(MANUAL_FILENAME);
        std::fs::write(&path, "## custom\nuser's custom manual\n").unwrap();
        let m = Manual::open_or_seed(td.path()).unwrap();
        let toc = m.toc();
        assert_eq!(toc, vec!["custom"]);
    }

    #[test]
    fn read_section_is_case_insensitive() {
        let td = TempDir::new().unwrap();
        let path = td.path().join(MANUAL_FILENAME);
        std::fs::write(&path, "## Architecture\nhello\n").unwrap();
        let m = Manual::open_or_seed(td.path()).unwrap();
        assert_eq!(m.read_section("architecture").as_deref(), Some("hello"));
        assert_eq!(m.read_section("ARCHITECTURE").as_deref(), Some("hello"));
    }

    #[test]
    fn read_section_returns_none_for_unknown() {
        let td = TempDir::new().unwrap();
        let m = Manual::open_or_seed(td.path()).unwrap();
        assert!(m.read_section("totally-not-real").is_none());
    }

    #[test]
    fn render_toc_includes_every_section() {
        let td = TempDir::new().unwrap();
        let m = Manual::open_or_seed(td.path()).unwrap();
        let toc = m.render_toc();
        for s in m.toc() {
            assert!(toc.contains(&s), "TOC text should contain {s}");
        }
    }

    #[test]
    fn user_edits_are_observed_on_next_read() {
        // load_text() reads fresh on each call, so editing the file mid-
        // session takes effect immediately.
        let td = TempDir::new().unwrap();
        let path = td.path().join(MANUAL_FILENAME);
        std::fs::write(&path, "## one\nfirst\n").unwrap();
        let m = Manual::open_or_seed(td.path()).unwrap();
        assert_eq!(m.read_section("one").as_deref(), Some("first"));
        std::fs::write(&path, "## one\nsecond\n").unwrap();
        assert_eq!(m.read_section("one").as_deref(), Some("second"));
    }
}
