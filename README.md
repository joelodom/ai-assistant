# ai-assistant

A personal AI assistant with long-term memory that lives on **your** machine,
not in someone else's cloud. You hand it your inbox, your notes, your
documents. It remembers. Later you ask *"what am I supposed to be thinking
about right now?"* and it answers — using everything you've ever told it,
weighted by recency and importance.

It is built around one **non-negotiable property**: a strict one-way data
flow we call **the diode**.

```
  INPUTS                      THE GATE                 THE CORE           OUT
  ──────                      ────────                 ────────          ─────
  you · emails ·        ┌──────────────────┐      ┌────────────────┐
  notes · docs ·  ──▶   │     Security      │ ──▶  │   Assistant    │ ──▶ answers,
  photos · web          │   Preprocessor    │      │       ↕        │     reminders,
                        │  redact · score   │      │     Memory     │     summaries
  workers (Gmail, ──▶   └──────────────────┘ only  │  (your files)  │     to you
  the web) fetch in        sanitized text          └────────────────┘

  ◀──── the diode ────▶  data flows IN and answers flow OUT. The assistant
  cannot send, book, pay, or call any write-capable API. The architecture forbids it.
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
| **[Roadmap](ROADMAP.md)** | See what's planned and what's known-imperfect — prioritized improvements across security, scalability, and UX, with file/line evidence. |

---

## Why this exists

Mainstream assistants make you choose between **memory** and **privacy**.
Cloud chat tools either forget you between sessions or remember you on
someone else's servers. Agentic tools that *do* remember also tend to *act* —
and an assistant that can both read your email and take actions is a single
prompt-injection away from being a liability.

This project takes a different stance:

- **Your substrate is yours.** Memory is plain text + JSON files in a folder
  you choose.
- **Safe to feed.** Because the backend has no outbound-action machinery, the
  worst a malicious email can do is corrupt an *answer* — never trigger an
  action. That makes it safe to hand the system your real, messy, sensitive
  data.
- **Screened before it's stored.** Every input — what you type, every email,
  every web page a worker pulls in — first passes through a **Security
  Preprocessor** that strips live secrets (2FA codes, password-reset links,
  account and card numbers) and scores what's worth remembering. Raw input is
  ephemeral; only sanitized text is ever persisted. That's what makes it safe
  to hand the system your real, messy inbox.
- **It actually remembers.** A local English embedding model plus hybrid
  retrieval (semantic + keyword + recency + importance) means a strong match
  from a year ago still surfaces, and important things float up on their own.

## What it's good for

- **A persistent memory layer for an LLM you trust with personal data.**
- **Replacing the scatter** of Notes + email screenshots + a half-used
  calendar + sticky notes with one queryable surface that actually remembers.
- **"What's on my plate?"** — answered from *now* + *here* + accumulated
  memory + learned preferences.
- **Cross-domain recall**: *"When did I last hear from my accountant?"*,
  *"What did the inspector say about the roof?"*, *"Did I ever follow up on
  that interview?"* — across email, your Google Drive, notes, and documents.
- **A second brain for personal projects**: drop in research, paste
  conversations, attach PDFs, ask questions later in plain English.
- **A proactive watch** (opt-in): a background web worker that infers what you
  care about from your memory and surfaces things without you asking — not just
  news, but a house that just listed in a town you're eyeing, an obituary for
  someone you know, a price drop on something you've been tracking, or a
  development in a topic you follow.

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
cargo build --release
```

This produces two binaries:

- `target/release/ai-assistant-backend` — the WebSocket server.
- `target/release/ai-assistant-client` — the native chat UI.

> **Embeddings:** real local semantic embeddings (**bge-base-en-v1.5**, an
> English retrieval model) are **on by default**. The model (~400 MB) downloads
> once on first use, then runs entirely on your machine — no embedding API, no
> tokens, nothing leaves the box. For a fast, dependency-light build that skips
> ONNX Runtime and substitutes a deterministic mock embedder (no semantic
> recall — handy for a quick look or CI), add `--no-default-features`. See the
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
| Embedder | backend | Local English embedding model (bge-base-en-v1.5). Text → vector, on-device. Weights download once. |
| VectorIndex | backend | In-memory cosine index over the `.vec` sidecars (brute-force; fine at personal scale). Derived cache, rebuildable. |
| Indexer | backend | Periodic mechanical worker (no LLM): backfill, re-embed, stats. |
| Workers | backend | External-data subsystems (Gmail, Google Drive, WWW). On-demand search and/or autonomous tick. |
| Client | client | egui chat surface, attachments, geolocation, metadata. |
| Protocol | shared | Wire types shared by both crates. |

License: MIT.
