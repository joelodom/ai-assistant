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
client (egui, Mac native) ──WS──> backend ──> Preprocessor ──> Assistant ──> Memory
                                              ↑                  │            ↑
                                         (ephemeral)         retrieve()     Embedder
                                                                 │            │
                                                                 ▼            ▼
                                                          HNSW vector index   .vec sidecars
                                                                 ▲            ▲
                                                                 └── Indexer ─┘
                                                                  (rebuilds /
                                                                   backfills)
```

**Component glossary**:

- **Security Preprocessor** (Preprocessor for short) — ephemeral
  classify+redact+score worker. Renamed from "Sanitizer" once it grew the
  importance-scoring responsibility.
- **Assistant** ("the Core") — only component the user talks to.
- **Embedder** — local fastembed-rs model turning text into vectors.
- **VectorIndex** — HNSW graph cached on disk; rebuildable from `.vec` sidecars.
- **Indexer** — mechanical, no-LLM background worker. Replaces the Curator. Backfills
  missing embeddings, compacts the HNSW graph, snapshots stats.
- **Scout** — opt-in web/news worker.

## Non-negotiable invariants

These are restated at the top of `backend/src/main.rs`. If you find yourself
relaxing one, stop and ask.

1. **No outbound actions, ever.** Backend reads in, responds out. No sending,
   no booking, no transactions, no calls to write-capable APIs. The Embedder
   runs purely locally — no calls to remote embedding APIs.
2. **Raw input is ephemeral.** The Security Preprocessor is the only thing
   that ever sees raw input. Each Preprocessor call is a fresh `claude`
   subprocess (no `--continue`, no shared session) and dies after the call.
   Raw input is never logged, never written to disk, never reaches the
   Assistant, the Embedder, or the memory store.
3. **The Preprocessor sees everything**, including the user's own queries,
   with one explicit, user-controlled exception: HAZMAT mode (see below).
4. **Tier-1 (drop) content is never stored or forwarded** — only a content-free
   stub note.
5. **The memory store contains sanitized data only.** Embeddings are derived
   from sanitized bodies, never from raw input.
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
7. **Forward-compatible reads.** Any version of the backend can read a memory
   directory written by any earlier version. Derived files (vectors, HNSW
   graph, indexes, caches) are rebuilt transparently when missing or stale.
   Source-of-truth files (`.txt` body, `.json` metadata) are preserved verbatim
   across upgrades; unknown fields are tolerated, missing optional fields
   default cleanly, removed fields are ignored on load. Orphans (a `.txt`
   without its `.json`, or vice versa) are quarantined with synthesized minimal
   metadata, never deleted. Backups may be partial — restoring without a
   `hnsw/` directory or without `.vec` sidecars is supported and triggers
   background backfill.

   **When extending**: never break the on-disk format for existing items. Add
   fields as `#[serde(default)]`. Removed enum variants must still deserialize
   (use `#[serde(other)]` or keep the variant). Migrations happen lazily, on
   write, not via batch rewrites.

## Layout

- `shared/` — wire protocol types (ClientMessage, ServerMessage, Tier).
- `backend/src/preprocessor.rs` — the Security Preprocessor. Ephemeral,
  isolated; classifies, redacts, and assigns an importance score.
- `backend/src/assistant.rs` — the Core. Memory-aware response pipeline. Uses
  hybrid retrieval (vector + keyword + recency + importance).
- `backend/src/memory.rs` — file-based store with atomic writes, vector
  sidecars, sha256 integrity field, and explicit-forget support.
- `backend/src/embedder.rs` — local `Embedder` trait + fastembed-rs
  implementation + `MockEmbedder` for tests.
- `backend/src/vector_index.rs` — HNSW wrapper. Persists `hnsw/graph.bin` +
  `hnsw/manifest.json`. Rebuilds from `.vec` sidecars on staleness.
- `backend/src/indexer.rs` — mechanical maintenance worker. No LLM calls.
  Replaces the Curator.
- `backend/src/scout.rs` — periodic web/news worker (opt-in).
- `backend/src/claude.rs` — `LlmClient` trait + `ClaudeCliClient` (production)
  + `MockLlmClient` + `FailingLlmClient` (testing).
- `backend/src/ws.rs` — axum WebSocket handler, error→memory→client wiring.
- `client/src/app.rs` — egui chat surface.
- `client/src/net.rs` — WebSocket worker on its own tokio runtime.

## Testability

Set `AI_ASSISTANT_MOCK_CLAUDE=1` to swap the real CLI for a deterministic
mock. Set `AI_ASSISTANT_MOCK_EMBEDDER=1` to swap the local fastembed model for
a deterministic hash-based mock. The full pipeline runs without spending any
Claude tokens or loading a real embedding model. `cargo test -p backend`
exercises this with both mocks on by default.

Add new failure-path tests via `backend::claude::FailingLlmClient`. Add new
canned LLM responses via `MockLlmClient::respond_when(matcher, response)`.

## Data store

The memory directory is the only persistent state. Override its location with
`--memory-dir <path>` or `AI_ASSISTANT_MEMORY_DIR=<path>`. Backups are just
`tar czf data.tgz <dir>`. The on-disk format is human-readable text + JSON
plus small binary `.vec` sidecars; all writes are atomic (temp file + rename)
so crashes mid-write cannot corrupt items.

```
<memory-dir>/
  items/YYYY-MM-DD/<id>.txt        # sanitized body — source of truth
  items/YYYY-MM-DD/<id>.json       # metadata (kind, importance, tags, sha256)
  items/YYYY-MM-DD/<id>.vec        # N × f32 packed LE — source of truth
  stubs/<id>.json                  # content-free drop records
  preferences.json                 # standing preferences
  embedding_model.json             # active model + dim (for invalidation)
  hnsw/graph.bin                   # derived cache: HNSW search graph
  hnsw/manifest.json               # derived cache: which items are indexed
```

`hnsw/` is **cache, not source of truth.** Delete it and the Indexer rebuilds
from sidecars.

## Retrieval

Hybrid scoring per turn:

```
final_score = α · vector_similarity + β · recency_decay + γ · importance
```

`recency_decay = exp(-age_days / half_life)`. Default weights (`α=0.6, β=0.25,
γ=0.15`, `half_life=30d`) are in `config.toml [retrieval]`.

There is no separate "recent N" pull anymore — recency is folded into the
score so the same retrieve call handles both axes.

## HAZMAT bypass

The client exposes a `☢ HAZMAT` checkbox. When ticked, the
`ClientMessage::Message.bypass_preprocessor` flag is set (the older name
`bypass_sanitizer` is accepted as a deserialization alias) and the backend
skips the Preprocessor entirely for that message — the raw content goes
straight to the Assistant. The backend logs `WARN` for every bypass and the
resulting memory item is tagged `hazmat` with elevated importance (0.8), so
the audit trail is intact. The checkbox is session-scoped and resets to off
on every client restart.

If you add a new path that ingests data (e.g. a new attachment kind, a
new background ingestor, a future Connector), it MUST default to going
through the Preprocessor. Only the explicit, user-driven UI affordance gets
to set the bypass flag — never as a side effect, never automatically.

## Explicit forget

The system never silently forgets things. When the user asks ("forget that"),
the Assistant emits a `FORGET:` marker line. The WS handler parses it,
tombstones the named item (`MemoryStore::forget(id)` → body replaced with
`[forgotten <ts>]`, sidecar kind becomes `ForgottenStub`, `.vec` and HNSW
entry removed). Tombstones are intentional and durable: the metadata stays
as forensic audit.

Background workers do NOT delete or rewrite item bodies. The Curator (which
used to destructively summarize) has been removed; its mechanical jobs
(embedding backfill, HNSW compaction, stats) moved to the Indexer.

## Error policy

When the Preprocessor fails (out of tokens, malformed JSON, etc.) the input
is **dropped** without inspection, an audit record (kind=`preprocessor_error`,
or legacy `sanitizer_error`) goes to memory, and the client gets a
`StubNotice` explaining what happened. When the Assistant fails, the user
message is already in memory; an `assistant_error` record is added and the
client gets an `Error` frame.

Never silently swallow LLM errors. Always persist + surface.

## Conventions

- Rust 2021. `cargo fmt` (default).
- Atomic on-disk writes via `memory::atomic_write`.
- No `unwrap`/`panic!` in backend request paths; bubble with `anyhow::Result`.
- Tests run with `cargo test --workspace`. CI-friendly: no network deps
  except the optional client geolocation (which has a 4s timeout).
- Prefer `ct` for code intelligence (see banner above).
