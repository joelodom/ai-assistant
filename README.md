# ai-assistant

A personal AI assistant built around a strict one-way data flow ("the diode").
Data flows in (emails, notes, calendar, photos); the assistant accumulates
knowledge over time; the assistant only ever produces outputs — reminders,
summaries, answers. **It cannot take actions in the outside world.**

See [SPEC.md](SPEC.md) for the full architecture, threat model, and rationale.
See [CLAUDE.md](CLAUDE.md) for invariants and contribution guidelines.

---

## Run it locally

### Prerequisites

- Rust 2021 (1.75+). `rustc --version`.
- The `claude` CLI, authenticated against your Claude Max account: `claude --version`.

### Build

```bash
cargo build --release
```

This builds two binaries:

- `target/release/ai-assistant-backend` — the WebSocket server.
- `target/release/ai-assistant-client` — the native Mac chat UI.

### Start the backend

```bash
./target/release/ai-assistant-backend                           # uses ./memory
./target/release/ai-assistant-backend --memory-dir ~/data/work  # different dataset
AI_ASSISTANT_MEMORY_DIR=/tmp/scratch ./target/release/ai-assistant-backend
```

Default listen address is `127.0.0.1:8765`. Override with `--addr` or
`AI_ASSISTANT_ADDR`. By default the **Scout** (web/news worker) is disabled
and the **Curator** (memory decay worker) is enabled. Edit `config.toml` to
change either.

Logs go to stderr; tune with `RUST_LOG=debug`.

### Start the client

```bash
./target/release/ai-assistant-client                            # connects to default
./target/release/ai-assistant-client --url ws://127.0.0.1:8765/ws
AI_ASSISTANT_URL=ws://10.0.0.5:8765/ws ./target/release/ai-assistant-client
```

The client opens an 800×720 window with a single chat surface. ⌘+Enter sends.
On first connect, the assistant introduces itself and tells you what it can
and can't do. After that, just talk to it — hand it data (paste an email,
jot a note), ask it questions, or tell it what to remember or forget.

---

## Different datasets / backup

The memory directory **is** the database. Point the backend at different
folders to run against different datasets:

```bash
./target/release/ai-assistant-backend --memory-dir ~/data/personal
./target/release/ai-assistant-backend --memory-dir ~/data/work
```

Backup is just a tarball of that folder:

```bash
tar czf data-$(date +%F).tgz -C ~/data personal
```

All writes are atomic (temp file + rename), so a crash mid-write cannot
corrupt items. Stopping and restarting the backend is safe — there is no
process-resident state beyond what's on disk.

---

## Test it without spending tokens

```bash
AI_ASSISTANT_MOCK_CLAUDE=1 cargo test --workspace
```

This swaps the real `claude` CLI for a deterministic in-process mock. Unit
tests cover the sanitizer JSON parser, memory store, decay logic, and
preference detection. Integration tests in `backend/tests/` spin up a real
backend + WebSocket client and assert end-to-end behavior including the
sanitizer Tier-1 drop path and the sanitizer-failure audit path.

You can also run the backend itself against the mock for a UI smoke test:

```bash
AI_ASSISTANT_MOCK_CLAUDE=1 ./target/release/ai-assistant-backend
./target/release/ai-assistant-client
```

Messages will get canned responses, but every code path runs and nothing is
sent to Claude.

---

## Config

`config.toml` (optional — built-in defaults match the example):

```toml
[server]
addr = "127.0.0.1:8765"

[memory]
dir = "./memory"
recent_window = 20

[claude]
binary = "claude"
model = "claude-opus-4-7"
timeout_secs = 180
scout_allowed_tools = ["WebSearch", "WebFetch"]

[scout]
enabled = false           # opt-in; enable once you've validated the basics
interval_minutes = 10
topics = ["world news headlines", "technology news"]

[curator]
enabled = true
interval_minutes = 60
fresh_age_hours = 48
aging_age_days = 14
stale_age_days = 90
```

CLI overrides win over the file; the file wins over built-in defaults.

---

## What does what

| Component  | Crate     | Role                                                |
|-----------:|-----------|-----------------------------------------------------|
| The Gate   | backend   | Sanitizer. Three-tier classification, ephemeral.    |
| The Core   | backend   | Assistant. Reads memory, replies, persists turns.   |
| Memory     | backend   | File-based store, atomic writes, decay metadata.    |
| The Scout  | backend   | Periodic web/news worker (opt-in).                  |
| Curator    | backend   | Periodic memory decay/summarization worker.         |
| Client     | client    | egui chat surface, IP-based geolocation, metadata.  |
| Protocol   | shared    | Wire types — re-used by both crates.                |
