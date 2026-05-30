//! In-memory rolling capture of recent log lines, with per-turn boundary
//! tracking, so the assistant can attach diagnostic context to a developer
//! note when the USER points out a problem (see `devnote.rs`).
//!
//! Wiring: `main::init_logging` installs `CaptureMakeWriter` as a SECOND fmt
//! layer, scoped to the backend's own targets at debug (with warn+ from
//! dependencies), independent of the user's stdout/file display filter — so
//! backend events are pushed here (a bounded ring buffer) in human-readable
//! form regardless of how the user configured their visible logs, while ONNX
//! model-load spam and dependency HTTP plumbing stay out of the report.
//! `respond_with_status` calls `mark_turn_start()` at the
//! top of each turn; when a `NOTE_TO_DEV` block is recorded, it calls
//! `logs_for_issue()` to pull the lines emitted since the *previous* turn
//! began (the turn being complained about) through now.
//!
//! This holds only already-formatted tracing lines — which, by the project's
//! logging discipline, never contain raw input, message bodies, or secrets
//! (only lengths, counts, ids, and structured metadata). Nothing here is
//! persisted except when the user explicitly asks for a developer note, at
//! which point the captured window is copied verbatim into SUGGESTIONS.md.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, OnceLock};
use tracing_subscriber::fmt::writer::MakeWriter;

/// Hard ceiling on retained lines. A rolling window; oldest lines fall off.
/// At personal scale a turn emits a few dozen debug-level lines, so this keeps
/// many turns of history while bounding memory to well under a megabyte.
const MAX_LINES: usize = 4000;

/// How many recent turn boundaries to remember.
const MAX_TURN_STARTS: usize = 64;

/// Cap on how many lines a single issue report embeds, so a noisy stretch
/// (e.g. a burst of worker ticks) can't bloat SUGGESTIONS.md without bound.
const MAX_ISSUE_LOG_LINES: usize = 600;

static GLOBAL: OnceLock<LogCapture> = OnceLock::new();

/// The process-wide capture. Lazily created on first use so the binary's
/// logging setup and the assistant share one instance without explicit
/// wiring, and tests that never install logging still get a usable handle.
pub fn global() -> &'static LogCapture {
    GLOBAL.get_or_init(LogCapture::default)
}

#[derive(Default)]
pub struct LogCapture {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// (seq, formatted_line) in arrival order. Bounded to MAX_LINES.
    lines: VecDeque<(u64, String)>,
    /// Monotonic sequence assigned to each captured line.
    next_seq: u64,
    /// `next_seq` value captured at the start of each turn. The last entry is
    /// the in-flight turn; the second-to-last is the previous (complained-of)
    /// turn. Bounded to MAX_TURN_STARTS.
    turn_starts: VecDeque<u64>,
}

impl LogCapture {
    /// Lock, recovering from poisoning rather than panicking — a panic inside
    /// the logging path would cascade into every subsequent log call.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Append one already-formatted log line (trailing newline stripped).
    pub fn push_line(&self, line: String) {
        let mut g = self.lock();
        let seq = g.next_seq;
        g.next_seq += 1;
        g.lines.push_back((seq, line));
        while g.lines.len() > MAX_LINES {
            g.lines.pop_front();
        }
    }

    /// Record that a new turn is beginning at the current sequence position.
    pub fn mark_turn_start(&self) {
        let mut g = self.lock();
        let seq = g.next_seq;
        g.turn_starts.push_back(seq);
        while g.turn_starts.len() > MAX_TURN_STARTS {
            g.turn_starts.pop_front();
        }
    }

    /// Lines emitted since the *previous* turn began — i.e. covering the turn
    /// the user is complaining about plus the current (complaint) turn. Falls
    /// back to the whole buffer if fewer than two turns have been seen. Capped
    /// to the most recent `MAX_ISSUE_LOG_LINES`.
    pub fn logs_for_issue(&self) -> Vec<String> {
        let g = self.lock();
        // turn_starts: [.., previous_turn, current_turn]. The previous turn is
        // the one whose answer the user is reacting to, so start there.
        let boundary = if g.turn_starts.len() >= 2 {
            g.turn_starts[g.turn_starts.len() - 2]
        } else {
            g.turn_starts.front().copied().unwrap_or(0)
        };
        let mut out: Vec<String> = g
            .lines
            .iter()
            .filter(|(seq, _)| *seq >= boundary)
            .map(|(_, line)| line.clone())
            .collect();
        if out.len() > MAX_ISSUE_LOG_LINES {
            let drop = out.len() - MAX_ISSUE_LOG_LINES;
            out.drain(0..drop);
        }
        out
    }
}

/// A `MakeWriter` that funnels every formatted event line into `global()`.
/// Installed as a dedicated, DEBUG-pinned fmt layer by `main::init_logging`.
#[derive(Clone, Copy, Default)]
pub struct CaptureMakeWriter;

impl<'a> MakeWriter<'a> for CaptureMakeWriter {
    type Writer = CaptureWriter;
    fn make_writer(&'a self) -> Self::Writer {
        CaptureWriter { buf: Vec::new() }
    }
}

/// Accumulates one event's bytes, then on drop pushes a single trimmed line.
/// The fmt layer builds a fresh writer per event, writes the whole formatted
/// record, and drops it — so a drop-time flush yields exactly one line each.
pub struct CaptureWriter {
    buf: Vec<u8>,
}

impl std::io::Write for CaptureWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for CaptureWriter {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let line = String::from_utf8_lossy(&self.buf).trim_end().to_string();
        if !line.is_empty() {
            global().push_line(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each test exercises a fresh LogCapture rather than the process global,
    // so they stay independent and order-free.
    fn fresh() -> LogCapture {
        LogCapture::default()
    }

    #[test]
    fn logs_for_issue_spans_previous_and_current_turn() {
        let cap = fresh();
        cap.mark_turn_start(); // turn A
        cap.push_line("a1".into());
        cap.push_line("a2".into());
        cap.mark_turn_start(); // turn B (the one with the problem)
        cap.push_line("b1".into());
        cap.mark_turn_start(); // turn C (the complaint)
        cap.push_line("c1".into());

        // Boundary is the previous turn (B), so A's lines are excluded but B's
        // and C's are included.
        let got = cap.logs_for_issue();
        assert_eq!(got, vec!["b1".to_string(), "c1".to_string()]);
    }

    #[test]
    fn logs_for_issue_with_single_turn_returns_that_turn() {
        let cap = fresh();
        cap.mark_turn_start();
        cap.push_line("only1".into());
        cap.push_line("only2".into());
        assert_eq!(cap.logs_for_issue(), vec!["only1", "only2"]);
    }

    #[test]
    fn logs_for_issue_with_no_turns_returns_everything() {
        let cap = fresh();
        cap.push_line("x".into());
        assert_eq!(cap.logs_for_issue(), vec!["x"]);
    }

    #[test]
    fn ring_buffer_drops_oldest_beyond_capacity() {
        let cap = fresh();
        cap.mark_turn_start();
        for i in 0..(MAX_LINES + 10) {
            cap.push_line(format!("line{i}"));
        }
        let g = cap.lock();
        assert_eq!(g.lines.len(), MAX_LINES);
        // Oldest survivor is line10 (0..=9 were evicted).
        assert_eq!(g.lines.front().unwrap().1, "line10");
    }

    #[test]
    fn issue_log_is_capped() {
        let cap = fresh();
        cap.mark_turn_start();
        for i in 0..(MAX_ISSUE_LOG_LINES + 50) {
            cap.push_line(format!("l{i}"));
        }
        assert_eq!(cap.logs_for_issue().len(), MAX_ISSUE_LOG_LINES);
    }

    #[test]
    fn capture_writer_pushes_one_line_on_drop() {
        // A fresh writer feeds the *global* capture; just assert it doesn't
        // panic and that a line lands. Using the global is fine here because
        // we only check membership, not exact contents.
        {
            use std::io::Write;
            let mut w = CaptureMakeWriter.make_writer();
            w.write_all(b"hello world\n").unwrap();
        } // drop flushes
        assert!(global()
            .lock()
            .lines
            .iter()
            .any(|(_, l)| l == "hello world"));
    }
}
