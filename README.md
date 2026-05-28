# ai-assistant

A personal AI assistant with long-term memory that lives on **your** machine,
not in someone else's cloud. You hand it your inbox, your notes, your
documents. It remembers. Later you ask *"what am I supposed to be thinking
about right now?"* and it answers — using everything you've ever told it,
weighted by recency and importance.

It is built around one **non-negotiable property**: a strict one-way data
flow we call **the diode**.

```
   you ──data──▶ assistant ──answers──▶ you
                     │
                     └── cannot reach the outside world
```

Data flows **in**: emails, notes, calendar entries, scanned documents,
photos, free text. The assistant **accumulates** knowledge over time. It only
ever produces **outputs back to you**: reminders, summaries, answers, news you
might care about. **It cannot take actions in the outside world** — it cannot
send an email on your behalf, change a thermostat, reset a password, move
money, or call any write-capable API. There is no "let me just integrate this
one webhook." The architecture forbids it.

Think of it as a trusted human personal assistant: you hand them your mail,
they read and remember it, and later you ask them things. They never act on
your behalf in the world.

---

## Documentation

| Document | Read it if you want to… |
|----------|--------------------------|
| **[User's Guide](docs/USER_GUIDE.md)** | Install, run, and *use* the assistant day to day — the chat UI, attachments, HAZMAT, connecting Gmail, telling it to forget things. |
| **[Architecture & Development](docs/ARCHITECTURE.md)** | Understand the implementation, the design principles, and how to contribute. Written for humans and AI agents alike. |
| **[Security Model](docs/SECURITY.md)** | Understand the threat model, what's defended, what's deliberately *not*, and the eight invariants that make the guarantee real. |

Two further references that live with the code:

- **`config.toml`** — the annotated configuration template (every knob the
  backend has).
- **`backend/src/DEFAULT_MANUAL.md`** — the assistant's own operating manual,
  embedded in the binary and seeded to `<memory-dir>/SYSTEM_MANUAL.md` on
  first run. It is the canonical source of *procedural* truth and is
  user-editable.

*(Contributors and AI agents: `CLAUDE.md` at the repo root holds the working
agreement for editing this codebase. It is intentionally not part of the
documentation set above.)*

---

## Why this exists

Mainstream assistants make you choose between **memory** and **privacy**.
Cloud chat tools either forget you between sessions or remember you on
someone else's servers. Agentic tools that *do* remember also tend to *act* —
and an assistant that can both read your email and take actions is a single
prompt-injection away from being a liability.

This project takes a different stance:

- **Your substrate is yours.** Memory is plain text + JSON files in a folder
  you choose. No database, no cloud, no account. You can `cat`, `grep`, and
  `tar` your assistant's brain, and read it with `less` ten years from now.
- **Safe to feed.** Because the backend has no outbound-action machinery, the
  worst a malicious email can do is corrupt an *answer* — never trigger an
  action. That makes it safe to hand the system your real, messy, sensitive
  data.
- **It actually remembers.** A local embedding model plus hybrid retrieval
  (semantic + keyword + recency + importance) means a strong match from a
  year ago still surfaces, and important things float up on their own.

## What it's good for

- **A persistent memory layer for an LLM you trust with personal data.**
- **Replacing the scatter** of Notes + email screenshots + a half-used
  calendar + sticky notes with one queryable surface that actually remembers.
- **"What's on my plate?"** — answered from *now* + *here* + accumulated
  memory + learned preferences.
- **Cross-domain recall**: *"When did I last hear from my accountant?"*,
  *"What did the inspector say about the roof?"*, *"Did I ever follow up on
  that interview?"* — across email, notes, and documents you've handed it.
- **A second brain for personal projects**: drop in research, paste
  conversations, attach PDFs, ask questions later in plain English.
- **Curated news** (opt-in): a background web worker that infers what you care
  about from your memory and surfaces relevant items without you asking.

## Who it's *not* for

- People who want an agent that *does* things (books flights, sends mail,
  posts to Slack). That's a different product with a fundamentally different
  security model.
- People who want it to run on someone else's servers. The whole point is
  that the substrate is yours.

---

## Quick start

### Prerequisites

- **Rust** 2021 (1.75+) — `rustc --version`
- The **`claude` CLI**, authenticated against your Claude account —
  `claude --version`

### Build

```bash
# Default build (uses a deterministic mock embedder — fine for trying it out).
cargo build --release

# Production build (real local semantic embeddings via fastembed-rs).
cargo build --release --features fastembed-real
```

This produces two binaries:

- `target/release/ai-assistant-backend` — the WebSocket server.
- `target/release/ai-assistant-client` — the native chat UI.

> **Embeddings note:** without the `fastembed-real` feature the backend uses a
> deterministic hash-based `MockEmbedder` — great for tests and a quick look,
> but **not semantically meaningful**. Build with `--features fastembed-real`
> for real recall quality. See the
> [Architecture doc](docs/ARCHITECTURE.md#embedding--the-vector-index).

### Run

```bash
# Backend — one flag only: --config. Uses ./config.toml if present, else defaults.
./target/release/ai-assistant-backend

# Client — connects to ws://127.0.0.1:8765/ws by default.
./target/release/ai-assistant-client
```

On first connect the assistant introduces itself and tells you what it can and
can't do. After that, just talk to it — paste an email, jot a note, ask a
question, or tell it what to remember or forget.

### Try it without spending tokens

```bash
cargo test --workspace          # offline; uses in-process mocks, spends no tokens
```

For a UI smoke test against canned responses, run the backend with
`AI_ASSISTANT_MOCK_CLAUDE=1`. Full details are in the
[User's Guide](docs/USER_GUIDE.md) and
[Architecture doc](docs/ARCHITECTURE.md#testing).

---

## At a glance

| Component | Crate | Role |
|----------:|-------|------|
| Preprocessor | backend | Security gate. Ephemeral per-call subprocess. Three-tier classify + redact + importance score. |
| Assistant ("the Core") | backend | The only thing you talk to. Hybrid retrieval, replies, persists turns. |
| Memory | backend | File-based store. Atomic writes, `.vec` sidecars, explicit forget. |
| Embedder | backend | Local fastembed-rs model — text → vector. No network. |
| VectorIndex | backend | HNSW search structure. Derived cache, rebuildable. |
| Indexer | backend | Periodic mechanical worker (no LLM): backfill, re-embed, stats. |
| Workers | backend | External-data subsystems (Gmail, WWW). On-demand search and/or autonomous tick. |
| Client | client | egui chat surface, attachments, geolocation, metadata. |
| Protocol | shared | Wire types shared by both crates. |

License: MIT.
