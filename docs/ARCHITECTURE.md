# Architecture & Development

This document explains *how the system is built* and *how to work on it*. It is
written to be useful to both human contributors and AI coding agents. For the
product pitch see the [README](../README.md); for day-to-day usage see the
[User's Guide](USER_GUIDE.md); for the threat model and guarantees see the
[Security Model](SECURITY.md).

> **Where truth lives.** This file describes the *shape* of the system and the
> principles behind it. The canonical source of *procedural* truth for the
> running assistant (marker vocabulary, setup walkthroughs, troubleshooting) is
> `backend/src/DEFAULT_MANUAL.md`. The working agreement for editing this repo
> (focused on the security invariants and the "when extending" rules) is
> `CLAUDE.md`. When code and
> this document disagree, the code wins — please fix the doc.

## Contents

- [The big picture](#the-big-picture)
- [Data flow, end to end](#data-flow-end-to-end)
- [Components](#components)
- [The eight invariants](#the-eight-invariants)
- [Workers](#workers)
- [Memory & the on-disk format](#memory--the-on-disk-format)
- [Embedding & the vector index](#embedding--the-vector-index)
- [Retrieval](#retrieval)
- [The marker vocabulary](#the-marker-vocabulary)
- [Wire protocol](#wire-protocol)
- [Client-driven configuration](#client-driven-configuration)
- [Logging & observability](#logging--observability)
- [Error policy](#error-policy)
- [Testing](#testing)
- [Repository layout](#repository-layout)
- [Development principles](#development-principles)
- [How to contribute a change](#how-to-contribute-a-change)

---

## The big picture

A Rust workspace with three crates:

- **`shared/`** — the wire protocol (`ClientMessage`, `ServerMessage`, `Tier`,
  config payload/request types). Re-used by both other crates so they can't
  drift.
- **`backend/`** — everything with state and intelligence: the security gate,
  the assistant core, the file-based memory store, the local embedder, the
  vector index, the maintenance indexer, and the workers.
- **`client/`** — a native [egui] desktop app talking to the backend over a
  WebSocket on its own tokio runtime.

The backend is the trust boundary. The client is a thin window onto it.

[egui]: https://github.com/emilk/egui

## Data flow, end to end

```
client (egui, native) ──WS──> backend ──> Preprocessor ──> Assistant ──> Memory
                                            ↑                  │           ↑
                                       (ephemeral)         retrieve()    Embedder
                                                               │           │
                                                               ▼           ▼
                                                        vector index       .vec sidecars
                                                               ▲           ▲
                                                               └─ Indexer ─┘
                                                                (rebuilds /
                                                                 backfills)
```

Every byte from the outside world enters through the **Preprocessor**, which is
the *only* component that sees raw input. What it passes (sanitized) flows to
the **Assistant**, which persists it to **Memory**, embeds it, and indexes it.
On each turn the Assistant **retrieves** a relevant slice of memory to build its
prompt. Background **Workers** pull in external data (Gmail, the web), and they
too route everything through the Preprocessor before it reaches Memory. The
**Indexer** is a mechanical janitor that keeps derived caches in sync.

## Components

| Component | File(s) | Responsibility |
|-----------|---------|----------------|
| **Security Preprocessor** | `backend/src/preprocessor.rs` | The gate. A fresh, isolated `claude` subprocess per call that classifies input into a `Tier` (Drop / Redact / Pass), redacts dangerous identifiers, and assigns an importance score. Stateless across calls. |
| **Assistant ("the Core")** | `backend/src/assistant.rs` | The only component the user talks to. Builds each prompt from persona + metadata + retrieved memory + preferences + worker descriptions, calls the LLM, interprets markers, persists the turn. |
| **Memory store** | `backend/src/memory.rs` | File-based, atomic-write store. Bodies (`.txt`), metadata (`.json`), vectors (`.vec`), stubs, preferences. Explicit-forget tombstones. SHA-256 integrity field. |
| **Embedder** | `backend/src/embedder.rs` | `Embedder` trait + `FastembedEmbedder` (bge-base-en-v1.5, an English model; the default, behind the on-by-default `fastembed-real` feature) + a deterministic `MockEmbedder` (tests and `--no-default-features`). Inference is local; weights download once. |
| **VectorIndex** | `backend/src/vector_index.rs` | In-memory cosine index over all vectors (brute-force scan; fine at personal scale, despite the legacy `hnsw/` directory name). A *derived cache* — rebuildable from `.vec` sidecars. |
| **Indexer** | `backend/src/indexer.rs` | Mechanical maintenance worker, **no LLM**. Backfills missing embeddings, detects embedder-model changes and re-embeds, checkpoints the index manifest. (Replaced the old destructive "Curator.") |
| **Workers** | `backend/src/workers/` | Subsystems that fetch external data: `gmail.rs`, `gdrive.rs`, `www.rs`, plus the `Worker` trait, `WorkerRegistry`, `WorkerContext`, `SearchEvent`, and `oauth.rs`. See [Workers](#workers). |
| **LLM client** | `backend/src/claude.rs` | `LlmClient` trait + `ClaudeCliClient` (production) + `MockLlmClient` + `FailingLlmClient` (tests). |
| **WebSocket handler** | `backend/src/ws.rs` | axum WS endpoint; wires the turn pipeline, status frames, and error→memory→client handling. |
| **Config protocol** | `backend/src/config_protocol.rs` | Handles sensitive `ConfigPayload` traffic (OAuth secrets/codes). The one path that bypasses both the Preprocessor and long-term memory (Invariant #8). |
| **Manual** | `backend/src/manual.rs` + `DEFAULT_MANUAL.md` | The assistant's on-demand operating manual. |
| **Client app** | `client/src/app.rs`, `client/src/net.rs` | egui chat surface; WebSocket worker on its own runtime. |

## The eight invariants

These are the load-bearing rules. The first seven head `backend/src/main.rs`;
the security-relevant ones — plus the ConfigPayload bypass (#8, a later
addition) — are restated in `CLAUDE.md`. This list and the
[Security Model](SECURITY.md), which explains the *why* behind each, carry the
full set. In brief:

1. **No outbound actions, ever.** Read in, respond out. No write-capable APIs.
   The embedder runs locally.
2. **Raw input is ephemeral.** Only the Preprocessor sees it; each call is a
   fresh subprocess that dies after. Raw input is never logged or persisted.
3. **The Preprocessor sees everything** — except the one explicit,
   user-controlled HAZMAT bypass.
4. **Tier-1 (drop) content is never stored or forwarded** — only a
   content-free stub.
5. **The memory store contains sanitized data only.** Embeddings derive from
   sanitized bodies.
6. **The backend is restart-safe at any time.** The data directory is the only
   persistent state; every write is atomic (temp → fsync → rename). No
   shutdown-required paths.
7. **Forward-compatible reads.** Any version reads any earlier version's
   memory. Derived caches rebuild; source-of-truth files are preserved; unknown
   fields tolerated; orphans quarantined, never deleted.
8. **ConfigPayload traffic bypasses the Preprocessor AND never reaches
   long-term memory.** It's handled only by `config_protocol.rs`.

If you find yourself wanting to relax one, **stop and ask** — that's the signal
the design is being bent.

## Workers

A **Worker** is the single abstraction for "a thing that produces external data
for the assistant." It lives in `backend/src/workers/` and replaces what used
to be two separate notions — *Connectors* (synchronous search adapters) and the
*Scout* (an autonomous web poller). They did the same architectural job under
two names, so they were unified.

A worker can participate in **either or both** of two flows:

1. **On-demand search.** The assistant emits a `SEARCH: <worker> <query>`
   marker. `Assistant::execute_search` dispatches to the worker, which fetches,
   drives each result through the Preprocessor + memory itself, and emits a
   stream of `SearchEvent`s back. The assistant re-emits those as slot-keyed
   status frames (so the UI shows live progress) and uses the terminal
   `Finished` event to know when to re-prompt with the now-updated memory.
2. **Autonomous tick.** A worker that returns `Some(interval)` from
   `tick_interval()` gets a background task spawned at startup that calls
   `tick()` on cadence. Items just appear in memory; no marker involved.

### The trait

```rust
trait Worker {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;     // rendered into the assistant prompt
    fn is_available(&self) -> bool;
    fn tick_interval(&self) -> Option<Duration> { None }   // None = no autonomous mode
    async fn tick(&self, ctx) -> Result<()> { Ok(()) }     // autonomous background work
    async fn search(&self, query, limit, ctx, metadata, tx) -> Result<()>;  // on-demand
}
```

### Shared plumbing: `WorkerContext` and `ingest_one`

`WorkerContext` bundles the Preprocessor, memory store, embedder, vector index,
and a `preprocess_concurrency` knob. `WorkerContext::ingest_one` is the **single
ingestion pipeline** every worker uses: Preprocessor → (drop-stub *or* memory
write) → embed → vector-index upsert, emitting `Ingested` / `Dropped` / `Failed`
events. Workers never call `MemoryStore::add` directly — routing through
`ingest_one` is what keeps Invariant #3 structurally enforced rather than
remembered.

### The `SearchEvent` stream

```rust
enum SearchEvent {
    Started   { worker, expected_total, detail }
    Progress  { worker, completed, total, detail }
    Ingested  { worker, item_id, importance }
    Dropped   { worker, reason }
    Failed    { worker, error }
    Finished  { worker, kept, dropped, failed, duration_ms }
}
```

The assistant consumes this with a **60-second stall watchdog**: if a worker
goes silent (no event within the timeout), the assistant logs it, abandons the
worker, and answers with what it has. A worker that closes its channel without
a `Finished` is treated as done.

### The shipping workers

- **`gmail` (`workers/gmail.rs`)** — read-only Gmail.
  - *search*: lists matching messages, fetches each in full, fans the bodies
    out through the Preprocessor (`preprocess_concurrency`, default 4).
  - *tick* (every 60s): lists mail newer than a persisted cursor at
    `<memory-dir>/connectors/gmail/last_seen.json` (atomic write per Invariant
    #6), ingests via the same pipeline. **The first tick after setup seeds the
    cursor to "now" and ingests nothing** — no full-mailbox backfill. Tick
    ingestion uses `Personal` provenance (it's your own mail); the Preprocessor
    decides keep/redact/drop.
  - Scope is hardcoded to `gmail.readonly`. The trait exposes no write verbs,
    and Google enforces the scope server-side.
- **`gdrive` (`workers/gdrive.rs`)** — read-only Google Drive. **Search-only**
  (no autonomous tick — we don't silently ingest an entire Drive).
  - *search*: a `files.list` full-text query, then for each hit it pulls the
    file's text — Google Docs/Sheets/Slides exported to text, PDFs and text
    files downloaded + extracted (`pdf-extract`), images/video/binaries skipped
    — fanning out through the Preprocessor (`Personal` provenance). Bounded by a
    25-file cap and a 5 MB per-file size guard; extracted text clipped at 50k chars.
  - Scope is hardcoded to `drive.readonly`. The trait exposes no write verbs and
    Google enforces the scope server-side, so it cannot modify Drive.
- **`www` (`workers/www.rs`)** — the open web via the LLM's `WebSearch` /
  `WebFetch` tools.
  - *search*: dispatched by `SEARCH: www <query>` for fresh-news questions
    whose answer isn't in memory.
  - *tick* (opt-in): the old Scout's behavior — infers your interests from
    memory and surfaces relevant news. Gated on `[scout].enabled`. (The config
    section keeps the legacy `scout` name; renaming it is a deferred schema
    bump — see [Known deferrals](#known-deferrals-and-rough-edges).)
  - Web content uses `PublicWeb` provenance (the stricter sanitization pass).
- **`briefing` (`workers/briefing.rs`)** — the one worker that fetches nothing
  external. On a tick (`[briefing].enabled`, default on) it reads memory and has
  the LLM (with NO tools) synthesize a short "what's important now" briefing.
  - It does NOT use the Preprocessor: its input is already-sanitized memory and
    its call has no tools, so the output is a pure function of sanitized data
    (like an `AssistantNote`). No new raw-input path, so the diode holds. Stored
    directly as a low-importance `Briefing` item tagged `auto-briefing`.
  - `Briefing` items are excluded from contextual retrieval; the startup greeting
    (`Assistant::introduction`) summarizes the latest *fresh* one with the cheap
    `briefing_summary_model`. `SEARCH: briefing` forces one on demand.

### Parallelism

Two layers of fan-out, both bounded:

- **Across markers:** multiple `SEARCH:` markers in one reply run concurrently
  (`Assistant.connector_concurrency`, default 4).
- **Within a worker:** per-result Preprocessor calls fan out via
  `futures::stream::buffer_unordered` (`WorkerContext.preprocess_concurrency`,
  default 4). Each Preprocessor call is a fresh subprocess (Invariant #2), so
  they're independent. Memory writes stay serial — they're sub-millisecond and
  serializing avoids vector-index upsert contention.

The search loop is bounded by `max_search_rounds` (default 2). If the model
emits *another* `SEARCH:` marker after the cap, the assistant re-prompts once
more with an explicit "no more SEARCH; answer with what you have" instruction
and strips any leftover markers — so a marker can never leak to the UI as a
"reply."

## Memory & the on-disk format

The memory directory is the **only** persistent state. It's human-readable text
+ JSON plus small binary vector sidecars, and every write is atomic.

```
<memory-dir>/
  items/YYYY-MM-DD/<id>.txt        # sanitized body — source of truth
  items/YYYY-MM-DD/<id>.json       # metadata (kind, importance, tags, sha256)
  items/YYYY-MM-DD/<id>.vec        # N × f32 packed little-endian — source of truth
  stubs/<id>.json                  # content-free drop records
  preferences.json                 # standing user preferences
  embedding_model.json             # active model + dimension (for invalidation)
  connectors/<name>/               # per-worker credentials + cursors (e.g. gmail token, last_seen)
  hnsw/manifest.json               # derived cache: which item ids are indexed
                                   #   (dir name is historical; the index is an
                                   #   in-memory cosine scan, not an ANN graph —
                                   #   no graph file is written)
  logs/<prefix>.YYYY-MM-DD         # daily-rotated structured logs
  SYSTEM_MANUAL.md                 # the manual, seeded on first run; user-editable
```

`items/` is the source of truth. `hnsw/` is **cache** — delete it and the
Indexer rebuilds from the `.vec` sidecars. Item *kinds* include user messages,
worker findings, assistant notes, preprocessor stubs/errors, assistant errors,
and forget tombstones. Legacy kinds (`ConnectorFinding`, `ScoutFinding`,
`Sanitizer*`) still deserialize for back-compat (Invariant #7); new
worker-produced items are written as `WorkerFinding` and tagged both
`worker:<name>` and `connector:<name>` so old queries keep matching.

**Explicit forget** tombstones an item: body becomes `[forgotten <ts>]`, kind
becomes a forgotten stub, the `.vec` and vector-index entry are removed, and the
metadata remains as forensic audit. Nothing background ever rewrites a body.

## Embedding & the vector index

Embeddings are computed **locally** — there is no remote embeddings API call
(Invariant #1). The model weights download once on first use; after that
everything runs in-process. The `Embedder` trait has two implementations:

- **`FastembedEmbedder`** — **bge-base-en-v1.5**, an English-first retrieval
  model (768-dim) via fastembed-rs, running in-process on CPU. Behind the
  `fastembed-real` Cargo feature, which is **on by default**, so `cargo build`,
  `cargo build --release`, and `cargo run` all use it. (Heavier 1024-dim
  options like `BGELargeENV15` / `MxbaiEmbedLargeV1` exist for more quality at
  higher RAM/latency — change the enum and `dim` in `embedder.rs`.)
- **`MockEmbedder`** — deterministic hash-based "bag-of-words" vectors (384-dim).
  Used when built with `--no-default-features`, and forced in tests via
  `AI_ASSISTANT_MOCK_EMBEDDER=1` so the suite stays offline and deterministic.
  Architecturally correct but **not semantically meaningful**.

`embedding_model.json` records the active model + dimension. When it differs
from the live embedder (you switch from the mock to bge, or bump the model),
the Indexer wipes every `.vec` sidecar and re-embeds from the stored bodies on
its next tick — so changing models is safe and automatic. (Note: `build_app`
only writes this record when it's absent, so it never masks a change from the
Indexer.)

The vector index itself is a **brute-force cosine scan** over an in-memory
`HashMap` of all vectors (`vector_index.rs::search`), not an approximate-
nearest-neighbor graph — despite the `hnsw/` directory name and `graph.bin`
filename, no ANN graph is built or persisted. That is simple and entirely
adequate at personal scale (thousands to tens of thousands of items); a real
ANN index is a future change tracked in the [Roadmap](../ROADMAP.md). Vectors
live in `.vec` sidecars (source of truth); the index is a rebuildable cache.

## Retrieval

The assistant does not stuff all of memory into the prompt. Each turn it
hybrid-retrieves a top-K and uses only that. The score per candidate is:

```
final = α · relevance + β · recency + γ · importance
```

where `relevance = max(vector_cosine, keyword_rank)`,
`recency = exp(-age_days / half_life)`, and `importance` is what the
Preprocessor assigned at ingest. Defaults (`config.toml [retrieval]`):
`α = 0.6, β = 0.25, γ = 0.15, half_life = 30d`. Each leg over-fetches a
candidate pool before re-ranking. Recency is folded into the single score — there
is no separate "recent N" pull.

The effect: a strong semantic match from a year ago still surfaces; a weak
match from yesterday still surfaces; a weak match from a year ago is filtered
out; important items float up regardless of age.

## The marker vocabulary

The assistant communicates intent to the backend by emitting **marker lines**
in its replies. The WS handler / assistant loop intercept these and strip them
before anything reaches the user. The full set:

| Marker | Meaning |
|--------|---------|
| `SEARCH: <worker> <query>` | Dispatch a worker search. Bounded by `max_search_rounds`. |
| `ESCALATE_TO_OPUS: <reason>` | Hand this turn to the heavier model. |
| `FORGET: <item-id>` | Tombstone a memory item (only when the user asked). |
| `READ_MANUAL: <section>` | Pull a section of the manual (bare `READ_MANUAL` = table of contents). Bounded at 4 reads/turn. |
| `CONFIG_REQUEST_FILE: <worker> <filename>` | Ask the client to provide a file (e.g. OAuth `client_secret.json`). |
| `CONFIG_BEGIN_OAUTH: <worker>` | Begin the browser OAuth handshake. |

When you add or change a marker, update `DEFAULT_MANUAL.md` in the same change —
it's the assistant's source of truth for its own vocabulary.

## Wire protocol

Defined once in `shared/src/lib.rs`. Highlights:

- **`ClientMessage`** — `Message { payload, metadata, bypass_preprocessor,
  force_opus }`, `Ping`, and `ConfigPayload { … }`. The `bypass_preprocessor`
  flag is the HAZMAT switch (older alias `bypass_sanitizer` still
  deserializes).
- **`ServerMessage`** — `ReplyChunk`, `ReplyDone`, `StubNotice`, `Error`,
  `Pong`, `ConfigRequest`, `ConfigStatus`, and `Status { phase, detail, slot }`.
  The `slot` field lets concurrent activities each own a row in the client's
  status bar.
- **`Tier`** — `Drop` / `Redact` / `Pass`, the Preprocessor's verdict.

Forward/back-compat: new fields are added with `#[serde(default)]`; renamed
fields keep deserialization aliases; removed enum variants are retained so old
data still loads. Wire-type renames are avoided so older clients keep working.

## Client-driven configuration

The backend's CLI has exactly one flag (`--config`). Everything *tunable* is in
the TOML; everything *runtime* (connecting Gmail, OAuth) flows from the client
over the WebSocket. The pattern, which any new configuration capability should
follow:

```
user: "set up gmail"
  → assistant emits CONFIG_REQUEST_FILE / CONFIG_BEGIN_OAUTH markers
  → backend turns them into ServerMessage::ConfigRequest frames
  → client performs the UI action (file picker, browser launch, loopback)
  → client replies with ClientMessage::ConfigPayload (bypasses Preprocessor + memory, Invariant #8)
  → config_protocol.rs does the mechanical work (validate, exchange code, write token.json atomically)
  → ServerMessage::ConfigStatus + a synthesized continuation turn keep the conversation moving
```

Do **not** add CLI subcommands for runtime config. Add a `ConfigPayloadKind`
variant and a `config_protocol` handler instead.

## Logging & observability

`tracing` with structured **JSON** output by default, to stdout and/or a
daily-rotated file under `<memory-dir>/logs/`. Configured in `[logging]`;
`RUST_LOG` overrides the level.

Per-turn spans carry a UUID `turn_id`, so every event from one user message
groups together — `grep`/`jq` by `turn_id` to reconstruct a turn. INFO covers
the lifecycle; DEBUG adds intermediate stages; TRACE adds per-event detail.

**Logging discipline (invariant-adjacent):** never log raw user input,
sanitized bodies, memory contents, OAuth secrets, or search queries verbatim.
Log lengths, counts, enum values, durations, and item IDs. A bare `debug`/`trace`
level turns on dependency logging — including tungstenite's per-frame payloads,
which would contain reply text — so scope verbosity to our crate:
`level = "info,backend=trace"`.

## Error policy

Failures are **always persisted and surfaced**, never silently swallowed:

- **Preprocessor fails** → the input is dropped *without inspection*, an audit
  record (`preprocessor_error`) is written, and the client gets a `StubNotice`.
- **Assistant fails** → the user message is already in memory; an
  `assistant_error` record is added and the client gets an `Error` frame.
- **Empty LLM reply** → logged at WARN; the user gets a polite substitute
  message rather than a blank turn.

## Testing

```bash
cargo test --workspace
```

The suite is **offline and free**: tests construct `MockLlmClient` /
`MockEmbedder` (and `FailingLlmClient` for failure paths) directly, and the
integration tests set `AI_ASSISTANT_MOCK_EMBEDDER=1`, so no `claude` tokens are
spent and no real embedding model loads — even though `fastembed-real` is
compiled in by default. (`cargo test --no-default-features` skips ONNX Runtime
for a lighter build.) Two env switches let you run the *binaries* against mocks
for manual smoke tests:

- `AI_ASSISTANT_MOCK_CLAUDE=1` — deterministic canned LLM.
- `AI_ASSISTANT_MOCK_EMBEDDER=1` — deterministic hash embedder.

Add failure-path coverage via `FailingLlmClient`; add canned responses via
`MockLlmClient::respond_when(matcher, response)`. Integration tests in
`backend/tests/ws_roundtrip.rs` stand up a real backend + WS client and assert
end-to-end behavior, including the Tier-1 drop path and the
preprocessor-failure audit path.

## Repository layout

```
shared/src/lib.rs            wire protocol (ClientMessage, ServerMessage, Tier, config types)
backend/src/
  main.rs                    entry point; invariants restated; logging init; tick drivers
  lib.rs                     build_app(): wires the whole graph together
  preprocessor.rs            the Security Preprocessor
  assistant.rs               the Core: retrieval, prompt build, marker loop, execute_search
  memory.rs                  file-based store, atomic writes, forget, item kinds
  embedder.rs                Embedder trait + fastembed + MockEmbedder
  vector_index.rs            in-memory cosine vector index (brute-force)
  indexer.rs                 mechanical maintenance worker
  workers/
    mod.rs                   Worker trait, WorkerRegistry, WorkerContext, SearchEvent, tick driver
    gmail.rs                 read-only Gmail: search + tick
    gdrive.rs                read-only Google Drive: search (download + extract)
    www.rs                   open web: search + autonomous interest scan
    oauth.rs                 Google OAuth runtime
  claude.rs                  LlmClient trait + real/mock/failing clients
  config.rs                  TOML config types + defaults
  config_protocol.rs         sensitive ConfigPayload handling (Invariant #8)
  manual.rs + DEFAULT_MANUAL.md   the assistant's operating manual
  ws.rs                      axum WebSocket handler
client/src/
  app.rs                     egui chat surface
  net.rs                     WebSocket worker on its own tokio runtime
config.toml                  annotated configuration template
```

## Development principles

- **Rust 2021, `cargo fmt` (default).** Keep new code in the idiom of the
  surrounding code.
- **No `unwrap`/`panic!` in backend request paths.** Bubble errors with
  `anyhow::Result`; persist and surface them (see [Error policy](#error-policy)).
- **All on-disk writes go through `memory::atomic_write`.** No exceptions, so
  Invariant #6 holds.
- **Don't break the on-disk format.** Add fields as `#[serde(default)]`; keep
  removed enum variants deserializable; migrate lazily on write, never via batch
  rewrites (Invariant #7).
- **New ingestion paths default to the Preprocessor.** Only the explicit human
  HAZMAT affordance may set the bypass flag — never as a side effect.
- **Prefer the structural tools.** This repo is indexed by `ct`; one
  `ct lookup` returns signature + body + callers + callees. Prefer it (and
  `ct splice` / `ct move-symbol`) over ad-hoc reads and sed.
- **Keep the docs honest.** When you change behavior, the marker vocabulary,
  the wire protocol, a setup flow, or an invariant, update
  `DEFAULT_MANUAL.md` (procedural truth) and, where relevant, this file and
  `CLAUDE.md` in the *same* change.

## How to contribute a change

1. **Read `CLAUDE.md`.** It's the working agreement for this repo and restates
   the security invariants with "when extending" guidance.
2. **Find the seam.** Use `ct` to locate the function and its callers/callees
   before editing.
3. **Respect the invariants.** If your change seems to need relaxing one,
   that's a design discussion, not a patch.
4. **Add or update tests.** Keep the suite offline (mocks). Cover the failure
   path, not just the happy path.
5. **Update the docs in the same change** (see the principle above).
6. **Run** `cargo fmt --all` and `cargo test --workspace` before you commit.

## Known deferrals and rough edges

These are intentional, documented debts — good first contributions:

- **`[scout]` config section name.** The autonomous web scan is still toggled
  under `[scout]` for back-compat; renaming it to something like
  `[workers.www]` is a deferred config-schema bump.
- **Stall watchdog timeout** (60s) is a hardcoded constant in
  `execute_search`; it could move to `WorkerContext`.
- **Stalled-worker handling** is "abandon after 60s with a logged warning."
  Richer interaction (the core nudging or re-tasking a stuck worker) is not yet
  implemented.
- **Gmail tick cursor** advances to wall-clock "now" rather than the maximum
  message timestamp seen; if Gmail indexing lags, a message could in principle
  be missed. Tracked as a correctness nuance, not yet changed.
- **WWW on-demand path** re-runs the Preprocessor inside `ingest_one` even
  though the body was already sanitized once — a minor double-preprocess, noted
  in a code comment.
