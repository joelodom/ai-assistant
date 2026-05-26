# ai-assistant — Agent Guidelines

<!-- ct-code-intelligence-start -->
## Code Intelligence — ct

This project is indexed by `ct`. **PREFER ct over Read/Grep/Glob** — one
`ct lookup` returns signature + body + callers + callees in a single call.
For structural edits prefer `ct splice` / `ct delete-function` / `ct move-symbol`
over Edit + sed.

Fall back to built-in tools only for binary files, files outside the indexed
project, or when Edit requires a prior built-in Read.
<!-- ct-code-intelligence-end -->

## What this is

A personal AI assistant built around a strict one-way data flow ("the diode").
Source of truth is [SPEC.md](SPEC.md) — read it before changing architecture.

```
client (egui, Mac native) ──WS──> backend ──> Sanitizer ──> Assistant ──> Memory
                                              ↑                              ↑
                                         (ephemeral)               Scout / Curator
```

## Non-negotiable invariants

These are restated at the top of `backend/src/main.rs`. If you find yourself
relaxing one, stop and ask.

1. **No outbound actions, ever.** Backend reads in, responds out. No sending,
   no booking, no transactions, no calls to write-capable APIs.
2. **Raw input is ephemeral.** The Sanitizer is the only thing that ever sees
   raw input. Each Sanitizer call is a fresh `claude` subprocess (no
   `--continue`, no shared session) and dies after the call. Raw input is
   never logged, never written to disk, never reaches the Assistant Core or
   the memory store.
3. **The Sanitizer sees everything**, including the user's own queries.
4. **Tier-1 (drop) content is never stored or forwarded** — only a content-free
   stub note.
5. **The memory store contains sanitized data only.**
6. **The backend is restart-safe at any time.** `kill`, `Ctrl-C`, panic, OS
   reboot, power loss — any of these must be safe. The data directory is the
   only persistent state; there is no in-memory-only cache, no lockfile, no
   sequence number, no "graceful shutdown" required. Every write goes through
   `memory::atomic_write` (temp file → fsync → rename). A user can stop the
   backend mid-conversation, restart it, reconnect, and lose nothing except
   the in-flight request itself.

   **When extending**: if you find yourself wanting per-process state (a
   counter, a cache, an in-memory index, a "current session") — first ask
   whether it must survive restart. If yes, persist it via `MemoryStore` or
   a sidecar file under the data directory using `atomic_write`. If no, fine —
   but document that it's deliberately ephemeral. Never introduce a
   shutdown-required path (e.g. "flush this buffer on exit") without an
   atomic equivalent that works when the process is killed.

## Layout

- `shared/` — wire protocol types (ClientMessage, ServerMessage, Tier).
- `backend/src/sanitizer.rs` — the Gate. Ephemeral, isolated, three-tier.
- `backend/src/assistant.rs` — Assistant Core. Memory-aware response pipeline.
- `backend/src/memory.rs` — file-based store with atomic writes + decay metadata.
- `backend/src/scout.rs` — periodic web/news worker.
- `backend/src/curator.rs` — periodic decay/summarization worker.
- `backend/src/claude.rs` — `LlmClient` trait + `ClaudeCliClient` (production)
  + `MockLlmClient` + `FailingLlmClient` (testing).
- `backend/src/ws.rs` — axum WebSocket handler, error→memory→client wiring.
- `client/src/app.rs` — egui chat surface.
- `client/src/net.rs` — WebSocket worker on its own tokio runtime.

## Testability

Set `AI_ASSISTANT_MOCK_CLAUDE=1` to swap the real CLI for a deterministic
mock. The full pipeline (sanitizer → assistant → memory) runs without
spending any Claude tokens. `cargo test -p backend` exercises this.

Add new failure-path tests via `backend::claude::FailingLlmClient`. Add new
canned LLM responses via `MockLlmClient::respond_when(matcher, response)`.

## Data store

The memory directory is the only persistent state. Override its location with
`--memory-dir <path>` or `AI_ASSISTANT_MEMORY_DIR=<path>`. Backups are just
`tar czf data.tgz <dir>`. The on-disk format is human-readable text + JSON;
all writes are atomic (temp file + rename) so crashes mid-write cannot
corrupt items.

## Error policy

When the Sanitizer fails (out of tokens, malformed JSON, etc.) the input is
**dropped** without inspection, an audit record (kind=`sanitizer_error`)
goes to memory, and the client gets a `StubNotice` explaining what happened.
When the Assistant fails, the user message is already in memory; an
`assistant_error` record is added and the client gets an `Error` frame.

Never silently swallow LLM errors. Always persist + surface.

## Conventions

- Rust 2021. `cargo fmt` (default).
- Atomic on-disk writes via `memory::atomic_write`.
- No `unwrap`/`panic!` in backend request paths; bubble with `anyhow::Result`.
- Tests run with `cargo test --workspace`. CI-friendly: no network deps
  except the optional client geolocation (which has a 4s timeout).
- Prefer `ct` for code intelligence (see banner above).
