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
Source of procedural / architectural truth for the running system is
`backend/src/DEFAULT_MANUAL.md` (which gets seeded into
`<memory-dir>/SYSTEM_MANUAL.md` on first run, where the user can edit it).
The assistant reads sections of this manual on demand via the `READ_MANUAL`
marker — see "System manual" below.

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
- **Connectors** — search-only adapters to external personal-data sources
  (Gmail today; Drive/Calendar later). Triggered by the assistant emitting
  `SEARCH: <name> <query>` markers. Results always pass through the
  Preprocessor before reaching memory.
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
8. **ConfigPayload traffic bypasses the Preprocessor AND never reaches
   long-term memory.** Configuration payloads (OAuth credentials, callback
   codes, etc., sent via `ClientMessage::ConfigPayload`) are mechanical
   handshakes and secrets, not personal data. They are handled by
   `backend/src/config_protocol.rs`, which only writes to the connector
   directory and holds pending OAuth state in process memory with a TTL.
   This is the only path that bypasses both the Preprocessor and memory.

   **When extending**: if you add a new sensitive runtime input
   (credentials, tokens, mechanical handshakes), add a new
   `ConfigPayloadKind` variant and a config_protocol handler — NOT a new
   bypass somewhere in the main message pipeline.

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
- `backend/src/connectors/` — search-only adapters (Connector trait,
  ConnectorRegistry, OAuth machinery, Gmail implementation).
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

The memory directory is the only persistent state. Set its location in
`[memory] dir` of the TOML config (pass with `--config <path>`). Backups are just
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

## System manual (single source of procedural truth)

The assistant has a markdown file it can consult on demand:

- **Bundled default:** `backend/src/DEFAULT_MANUAL.md` (embedded into the
  binary via `include_str!`).
- **On disk:** `<memory-dir>/SYSTEM_MANUAL.md`, written from the default
  on first run. User-editable; we never overwrite it after that.
- **Loaded by:** `backend/src/manual.rs`. Re-reads from disk on every
  section lookup so user edits take effect without a restart.

The assistant reads sections via the `READ_MANUAL: <section>` marker
(see `backend/src/assistant.rs::READ_MANUAL_MARKER`). Bare `READ_MANUAL`
returns the TOC. Bound at 4 reads per turn. The manual covers:
architecture, all 8 invariants, marker vocabulary, hybrid retrieval,
memory store layout, HAZMAT, forget, connector setup (Gmail walk-through
including Cloud Console), client-driven config, error handling,
troubleshooting, self-knowledge.

**This is the rule:** when you add or change a feature that affects the
system's behavior, the marker vocabulary, the wire protocol, a setup
flow, or the invariants — **update `backend/src/DEFAULT_MANUAL.md` in
the same change.** It is the canonical source for "how does X work"
content; everything else (this CLAUDE.md, the README, the assistant's
own prose) should pull from it rather than duplicate it.

Static procedural content used to live in `SelfKnowledge` memory items
seeded at startup; that's removed. The `ItemKind::SelfKnowledge` enum
variant stays for back-compat with items written by earlier versions
(Invariant #7).

## Logging

The backend uses `tracing` with structured JSON output by default. Two
destinations available simultaneously: stdout and a daily-rotated file at
`<memory-dir>/logs/<file_prefix>.YYYY-MM-DD`. New file opens at midnight
UTC; old files are never auto-deleted. Configured in `[logging]` of the
TOML; `RUST_LOG` env var overrides the level if set.

**Discipline (invariant-adjacent):** never log raw user input, sanitized
message bodies, memory item contents, OAuth secrets, search queries
verbatim. Lengths, counts, structured metadata (tier, importance,
model_used, durations, marker dispatch counts), and item IDs are fine.
This is enforced by convention, not code — when adding new tracing
calls, log `foo_len = foo.chars().count()` and `kind = ?some_enum`
patterns, never `foo` directly.

Per-turn spans are tagged with a UUID `turn_id` so all events from one
user message group together (just `grep` or `jq` by turn_id). The
default level INFO covers the lifecycle; DEBUG adds intermediate stages;
TRACE adds per-event detail. For product-analysis work, run at TRACE
(see joel.toml as an example) and the manual's `logging-and-analysis`
section.

## Connectors (search-only)

Connectors are search-only adapters to external personal-data sources. The
assistant emits `SEARCH: <name> <query>` markers when it judges the answer
is likely in one. The WS-driven `Assistant::respond` loop executes each
search via the registered connector, runs every result through the
Preprocessor (Invariant #3 — connector data is "outside world" data), and
ingests non-drop results as `ItemKind::ConnectorFinding` items.

After ingestion the assistant is re-prompted with the now-updated memory.
Bounded at `max_search_rounds` (default 2) so the loop can't recurse forever.

**Defense in depth.** Each connector is bound to the narrowest possible
OAuth scope (Gmail uses `gmail.readonly`). The connector trait deliberately
exposes only `search` — there is no `.send()` or `.delete()` method for a
bug to call into existence. And even if the connector tried, Google's
authorization server would 403 because the token is scope-bound at
issuance.

Setup is **client-driven** (see "Client-driven configuration" below). The
user tells the assistant they want to set up a connector; the assistant
walks them through it conversationally, emitting `CONFIG_REQUEST_FILE` and
`CONFIG_BEGIN_OAUTH` markers that the backend translates into structured
`ServerMessage::ConfigRequest` frames. The client hosts the OAuth loopback
listener (so the browser dance works even when the backend is on a
headless EC2 instance) and launches the browser locally. The backend
exchanges the resulting code with Google, writes `token.json` atomically,
and registers the live connector instance.

## Client-driven configuration (Invariant #8)

The backend's CLI has exactly one flag: `--config <path>`. Everything else
(memory dir, listen address, model choices, retrieval weights, scout
toggle) lives in the TOML config. Runtime configuration (connector setup,
OAuth flows, etc.) flows from the client over the WebSocket — see the
manual section on `client-driven-config`.

Two new wire variants drive this:

- `ClientMessage::ConfigPayload { payload: ConfigPayloadKind }` —
  sensitive payloads (OAuth client_secret.json contents, callback codes,
  loopback port handshake). **Invariant #8: these bypass the Preprocessor
  AND never reach long-term memory.** They are handled by
  `backend/src/config_protocol.rs`, which writes only to the connector
  directory and holds pending OAuth state in memory with a 10-min TTL.
- `ServerMessage::ConfigRequest { request: ConfigRequestKind }` —
  structured ask for the client to perform a UI action (file picker,
  browser launch). Driven by the assistant's `CONFIG_REQUEST_FILE` /
  `CONFIG_BEGIN_OAUTH` markers, which the WS handler intercepts.
- `ServerMessage::ConfigStatus { connector, ok, message }` — rendered in
  the client transcript as a system note.

The flow: user types "set up gmail" → assistant emits markers → backend
sends structured requests → client UI responds → backend handles the
mechanical bits → continuation turn synthesized via the assistant so the
conversation moves forward conversationally.

**When extending**: any new configuration capability (enabling Scout, etc.)
should follow this pattern — assistant marker → ConfigPayload variant →
config_protocol handler → ConfigStatus + continuation. Do not add CLI
subcommands.

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
