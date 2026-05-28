# Workers Refactor — review notes

> **STATUS: UNREVIEWED, UNTESTED BY A HUMAN.** The Rust test suite passes
> (`cargo test --workspace` → 106 tests green) and `cargo fmt` is clean,
> but nothing here has been exercised against a live backend, a real
> Gmail account, or the egui client. Treat every claim below as "intended
> behavior," not "verified behavior." Read this, then test, then resume
> the session.

This file documents the change that unified the **Scout** and **Connectors**
subsystems into a single **Workers** abstraction, plus a streaming
search interface and a bug fix. It exists so the work can be reviewed
asynchronously.

## Commits in this change

- `182b87b` — Workers module: unified Worker trait, GmailWorker, WwwWorker
- `0428352` — Workers refactor: unify Scout + Connectors; streaming SEARCH

(The immediately prior `Parallel connector preprocessing + slot-keyed
status bar` commit is a separate feature — parallel per-result
preprocessing + a multi-row status bar — and was reviewed/committed
before this refactor began.)

## Motivation (the idea that started this)

The Scout was, structurally, just a "www connector." Both the Scout and
the Gmail connector:

- pull from the outside world,
- produce items that must pass through the Preprocessor before reaching
  memory,
- ought to be addressable by the assistant.

The split between "Scout" (autonomous) and "Connector" (search-only) was
bookkeeping, not architecture. So both collapse into one notion: a
**Worker** — "a thing that produces external data." Whether it runs
autonomously, responds on demand, or both, is a property of the
implementation, not the abstraction.

## The new shape

### `Worker` trait (`backend/src/workers/mod.rs`)

```
trait Worker {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;   // rendered into assistant prompt
    fn is_available(&self) -> bool;
    fn tick_interval(&self) -> Option<Duration> { None }   // None = no autonomous mode
    async fn tick(&self, ctx) -> Result<()> { Ok(()) }     // autonomous background work
    async fn search(&self, query, limit, ctx, metadata, tx) -> Result<()>;  // on-demand
}
```

A worker can implement **either or both** of the two flows:

1. **Autonomous tick** — the harness spawns one tokio task per worker
   that declares a `tick_interval()`, calling `tick()` on cadence.
2. **On-demand search** — the assistant emits `SEARCH: <worker> <query>`;
   the worker fetches, runs each result through the Preprocessor + memory
   itself, and emits `SearchEvent`s on a channel so the core can observe
   progress.

### `WorkerContext` + `ingest_one`

Shared services (Preprocessor, MemoryStore, Embedder, VectorIndex,
`preprocess_concurrency`) are bundled in `WorkerContext`.
`WorkerContext::ingest_one` is the single ingestion pipeline every worker
uses: Preprocessor → (drop-stub OR memory write) → embed → HNSW upsert,
emitting `Ingested` / `Dropped` / `Failed` events. Workers never write to
`MemoryStore::add` directly — this keeps the "everything passes through
the Preprocessor" invariant (Invariant #3) structurally enforced.

### `SearchEvent` stream

```
enum SearchEvent {
    Started   { worker, expected_total, detail }
    Progress  { worker, completed, total, detail }
    Ingested  { worker, item_id, importance }
    Dropped   { worker, reason }
    Failed    { worker, error }
    Finished  { worker, kept, dropped, failed, duration_ms }
}
```

The Assistant consumes this stream in `execute_search`, re-emits each
event as a slot-keyed `ServerMessage::Status` frame (so the client status
bar shows live per-worker progress), and uses `Finished` to know when to
re-prompt. A **60-second stall watchdog** (`tokio::time::timeout` on each
`recv`) abandons a worker that goes silent, logs `worker_stalled`, and
returns whatever was collected.

## File-by-file

| File | What |
|---|---|
| `backend/src/workers/mod.rs` | Worker trait, WorkerRegistry, WorkerContext, SearchEvent, tick driver, MockWorker (tests). **Start here.** |
| `backend/src/workers/gmail.rs` | GmailWorker. Old connector's search + new `tick()` that polls every minute. |
| `backend/src/workers/www.rs` | WwwWorker. Old Scout's autonomous scan + new on-demand `search()`. |
| `backend/src/workers/oauth.rs` | Google OAuth runtime (moved verbatim from `connectors/oauth.rs`). |
| `backend/src/assistant.rs` | `execute_search` rewritten around the event stream; SEARCH-leak fix; `connectors`→`workers` field; tests ported to MockWorker. |
| `backend/src/lib.rs` | Builds `WorkerContext` + `WorkerRegistry` (gmail if configured, www always), passes to Assistant. |
| `backend/src/main.rs` | `built.workers.spawn_tick_drivers()` replaces the old `Scout::spawn()`. |
| `backend/src/config_protocol.rs` | Registers a live `GmailWorker` after OAuth (was `GmailConnector`). |
| `backend/src/memory.rs` | `ItemKind::WorkerFinding` added; `ConnectorFinding`/`ScoutFinding` retained for back-compat reads. |
| `backend/src/DEFAULT_MANUAL.md` | New `workers` section; `connector-setup-*` → `worker-setup-*`; AVAILABLE CONNECTORS → AVAILABLE WORKERS. |
| `CLAUDE.md` | Component glossary + Layout + Workers section rewritten. |
| **deleted** | `backend/src/scout.rs`, `backend/src/connectors/` |

## Gmail tick() — the part to scrutinize

- Polls with `after:<unix-seconds>` against a persisted cursor at
  `<memory-dir>/connectors/gmail/last_seen.json` (atomic write, Invariant #6).
- **First tick after setup seeds the cursor to "now" and ingests
  nothing** — deliberately avoids a full-mailbox backfill on first run.
- New mail uses `InputProvenance::Personal` (it's your own data, even
  though it arrives via an API) so the Preprocessor applies the
  personal-data ruleset rather than the stricter PublicWeb one.
- `TICK_FETCH_CAP = 25` bounds a single tick.
- Cursor is always advanced to "now" at the end of a tick, even on
  partial failure — losing the cursor (→ full re-ingest) is worse than a
  rare duplicate.

**Open question for review:** the cursor advances to wall-clock "now"
rather than to the max `internalDate` of fetched messages. If Gmail
indexing lags, a message could in theory be missed. Max-internalDate
would be safer but adds complexity. Flagged, not decided.

## The bug this also fixes

Previously, when the LLM emitted a `SEARCH:` marker as its final reply
*after* the `max_search_rounds` cap was hit, the marker leaked to the UI
verbatim (you saw `SEARCH: gmail after:2026/05/24` as a "reply"). Now,
when that happens, the Assistant re-prompts **once** with an explicit
"you've used your search budget; answer with memory as-is, no more
SEARCH markers" instruction, and strips any leftover markers from the
recovery reply as a safety net. Costs one extra LLM call instead of
showing a raw marker.

## Back-compat (Invariant #7)

- Old items written as `ConnectorFinding` / `ScoutFinding` still
  deserialize.
- New items write `WorkerFinding` and carry BOTH `worker:<name>` and
  `connector:<name>` tags, so any saved query keying on the old tag form
  keeps matching.
- Wire-protocol variant names (`ConfigPayloadKind::ConnectorClientSecret`,
  etc.) were **deliberately NOT renamed** — older clients keep working.
- `cfg.scout` config section is **unchanged** — the WWW worker's
  autonomous tick still reads `cfg.scout.enabled` /
  `cfg.scout.interval_minutes`. Renaming the section to `[workers.www]`
  is a future schema bump, intentionally deferred so existing
  `joel.toml` keeps working.

## Known unfinished / deferred

- Config section still named `[scout]` (see above).
- Stall-watchdog timeout (60s) is a hardcoded const in `execute_search`;
  could be a `WorkerContext` field.
- "Stalled worker → check on it / redirect it" is currently just
  "abandon after 60s with a logged warning." Richer interaction (the
  core nudging or re-tasking a stuck worker) is not implemented.
- The cap-recovery prompt is an inline string literal; might belong with
  the other prompt-building code.
- WWW `search()` re-runs the Preprocessor inside `ingest_one` even though
  `ingest_body` already sanitized once — a minor double-preprocess on the
  on-demand path. Noted in a code comment; harmless but wasteful.

## How to test when you resume

1. **Build + unit tests:** `cargo test --workspace` (should be 106 green).
2. **Mock end-to-end (no tokens):**
   `AI_ASSISTANT_MOCK_CLAUDE=1 AI_ASSISTANT_MOCK_EMBEDDER=1 cargo run -p backend`
   then drive the egui client; confirm a `SEARCH:`-triggering message
   shows per-worker status rows and doesn't leak markers.
3. **Gmail tick (real account):** configure Gmail via the client, send a
   yourself a test email, wait ~1 min, confirm a `WorkerFinding` appears
   in `<memory-dir>/items/...` and `last_seen.json` advances. Watch logs
   for `gmail_tick_ingest_done`.
4. **SEARCH-leak regression:** force the model to keep emitting SEARCH
   markers (or set `max_search_rounds` low) and confirm the final reply
   is prose, never a raw `SEARCH:` line.
5. **Stall path:** harder to trigger naturally; could add a test worker
   that emits `Started` then never `Finished` and confirm the 60s
   watchdog fires (no such test exists yet).
