# ai-assistant

A personal AI assistant with long-term memory that lives on your machine, not
in someone else's cloud. You hand it your inbox, your notes, your documents.
It remembers. Later you ask "what am I supposed to be thinking about right
now?" and it answers — using everything you've ever told it, weighted by
recency and importance.

It is designed around one **non-negotiable security property**: a strict
one-way data flow we call **"the diode."**

```
   you ──data──▶ assistant ──answers──▶ you
                     │
                     └── cannot reach the outside world
```

Data flows **in**: emails, notes, calendar entries, scanned documents, photos,
free-text. The assistant **accumulates** knowledge over time. It only ever
produces **outputs back to you**: reminders, summaries, answers, news you
might care about. **It cannot take actions in the outside world.** It cannot
send an email on your behalf, change a thermostat, reset a password, move
money, or call any write-capable API. There is no "let me just integrate
this one webhook" — the architecture forbids it.

Think of it as a trusted human personal assistant: you hand them your mail,
they read and remember it, and later you ask them things. They never act on
your behalf in the world.

## What it's good for

- **A persistent memory layer for an LLM you actually trust with personal
  data.** ChatGPT, Claude.ai, and similar tools either forget between
  sessions or store everything in someone else's cloud. This stores
  everything in plain files in a folder you control.
- **Replacing the scatter** of Apple Notes + email screenshots + a half-used
  calendar + sticky notes + "I'll remember this" with one queryable surface
  that actually does remember.
- **"What's on my plate?" / "What am I supposed to be thinking about?"** —
  the assistant uses *now* + *here* + accumulated memory + learned
  preferences to answer.
- **Cross-domain reasoning**: "When did I last hear from my accountant?",
  "What did the inspector say about the roof?", "Did I ever follow up on
  that interview?" — answers across email, notes, and documents you've
  handed it.
- **A second-brain for personal projects**: drop in research notes, paste
  in conversations, attach PDFs. Ask it questions later in plain English.
- **Curated news** (opt-in): a background "Scout" worker that infers what
  you care about from your memory and surfaces relevant news without you
  asking. Off by default until you've used the system enough that it
  knows you.

## Who it's NOT for

- People who want an agent that *does* things (books flights, sends email,
  posts to Slack). That's a different product with a fundamentally
  different security model.
- People who want it to run on someone else's servers. The whole point is
  that the substrate is yours.

## Security model — the short version

Security is the whole reason this design looks the way it does. Five
properties hold whether you trust the model or not:

### 1. The diode: no outbound actions, ever

The backend is read-in / respond-out only. No code path writes to an
external system. This is the deepest defense against prompt injection in
your data: even if a malicious email convinces the model to "send a
password-reset link to attacker@evil.com," there is no machinery in the
backend that *can* send an email. The worst an attacker can do via
ingested data is corrupt the answers you get back. They cannot make the
assistant act in the world.

This is what we mean by "load-bearing." The diode isn't a policy you can
relax for convenience — it's an architectural property. The system has no
HTTP client for outbound writes, no SMTP, no SDKs that mutate state. Web
search and URL fetching are read-only and are the only outbound traffic.

### 2. The Security Preprocessor: a checkpoint that sees everything first

Every byte from the outside world — your typing, an ingested email, text
extracted from a PDF, a web page the Scout fetched — passes through the
**Security Preprocessor** (Preprocessor for short) before anything else sees
it. The Preprocessor is a **separate, ephemeral process per call**: a fresh
`claude` subprocess with no shared session state, no `--continue`, no
history. The raw input lives only on one function's stack and inside that
one short-lived subprocess. When the subprocess exits, the raw input is gone.

The Preprocessor classifies each piece of input into three tiers:

- **Drop entirely** — content that is *only* security-relevant (an OTP
  email, a password-reset link). The content is destroyed; only a
  content-free stub note is recorded ("Received and dropped a message that
  appeared to be a security code").
- **Redact, then pass** — sensitive but contextually useful content (a
  bank deposit confirmation). Account numbers and similar
  directly-actionable identifiers are replaced with placeholders; who/what/
  when is preserved.
- **Pass through** — the vast majority of input. Just goes through.

### 3. Threat model: account-takeover attackers, not nation-states

The Preprocessor is tuned to defeat **financially motivated attackers** trying for
account takeover or direct theft. It actively suppresses anything that
would directly enable that:

- 2FA / MFA / OTP codes
- Password reset links and tokens
- API keys, access tokens, session tokens, recovery codes
- Full bank account numbers, full card numbers, routing numbers, wire/ACH
  identifiers

It is **not** trying to suppress every fact a social engineer might find
useful. Birthdays, vacation dates ("house empty next Tuesday"), kids'
school names, employer info, calendar events — these get remembered and
reasoned over, because hobbling the assistant's usefulness to defend
against social engineering would defeat the point. The threshold is "would
this one fact directly enable account takeover?" — if yes, drop or redact;
if no, keep.

### 4. Your data lives on your disk in plain text

There is **no database** and **no cloud storage**. Memory is plain text
bodies plus small JSON sidecars under a directory you choose:

```
<memory-dir>/
  items/2026-05-25/<id>.txt        # the sanitized body
  items/2026-05-25/<id>.json       # who/when/how-important
  stubs/<id>.json                  # content-free drop records
  preferences.json                 # things you've told it to remember about your preferences
```

You can `cat` your assistant's memory. You can `grep` it. You can back it
up with `tar`. You can move it to another machine. You can delete a
specific item by removing two files. The whole format is designed to
survive the death of this software — you can read the data with `less`
ten years from now.

All writes are atomic (temp file + fsync + rename), so a crash mid-write
cannot corrupt items. The backend can be killed, restarted, power-cycled
at any moment without losing anything except an in-flight request.

### 5. The HAZMAT bypass is opt-in, audited, and never automatic

There is one explicit user-controlled exception to "the Preprocessor sees
everything": a `☢ HAZMAT` checkbox in the client. Tick it and the next
message skips the Preprocessor and goes straight to the assistant. Use it
when you've consciously decided the content is safe and you want it
reasoned over verbatim. Every bypass is logged at WARN, tagged `hazmat`
in the memory audit trail, and shown with a banner in your local
transcript so you can never wonder later whether a message went through
the Preprocessor. **No code path may set the bypass programmatically** —
only the human-pressed checkbox can flip it.

---

For the full threat model, architecture rationale, and as-built notes,
see [SPEC.md](SPEC.md). For contribution invariants, see
[CLAUDE.md](CLAUDE.md).

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

## How recall works (RAG, hybrid retrieval)

The assistant doesn't load all your memory into the model every turn — at
years-of-data scale that wouldn't work. Instead, each turn it hybrid-retrieves
the top-K most relevant items and only those go into the prompt.

The score per candidate item is:

```
final = α · relevance + β · recency + γ · importance
```

where `relevance = max(vector_cosine, keyword_rank)`, `recency = exp(-age_days
/ half_life)`, and `importance` is the score the Preprocessor assigned at ingest time.
Defaults: `α=0.6, β=0.25, γ=0.15, half_life=30d` — configurable in
`config.toml [retrieval]`.

This means a very strong semantic match from a year ago still surfaces, a
weak match from yesterday still surfaces, and a weak match from a year ago
gets filtered out. Important items (a wedding invitation, a deposition date)
float up regardless.

Embedding is local — a small fastembed-rs model runs in-process, no calls
to a remote embeddings API. The vectors live as `.vec` sidecars next to your
text bodies; the HNSW search index is a derived cache that gets rebuilt from
the sidecars if missing.

The system never silently forgets things. The old "Curator" that
destructively summarized aging memory has been removed; items live verbatim
until you explicitly ask the assistant to forget a specific one ("forget
that"), at which point the item is tombstoned (body zeroed, vector removed,
audit metadata kept).

---

## Config

`config.toml` (optional — built-in defaults match the example):

```toml
[server]
addr = "127.0.0.1:8765"

[memory]
dir = "./memory"

[claude]
binary = "claude"
# Default for any role that doesn't override below.
model = "claude-opus-4-7"
# Per-role models — chosen to match each component's job:
#   Preprocessor: Haiku  — pattern recognition + importance scoring on every
#                          message; latency matters. (legacy alias:
#                          `sanitizer_model`)
#   Assistant:    Sonnet — chat; self-escalates to the escalation model.
#   Escalation:   Opus   — used on self- or user-forced escalation.
#   Scout:        Sonnet — web triage.
preprocessor_model         = "claude-haiku-4-5"
assistant_model            = "claude-sonnet-4-6"
assistant_escalation_model = "claude-opus-4-7"
scout_model                = "claude-sonnet-4-6"
timeout_secs = 180
scout_allowed_tools = ["WebSearch", "WebFetch"]

[scout]
enabled = false           # opt-in; enable once memory is substantial
interval_minutes = 10
pinned_topics = []        # empty → Scout infers topics from your memory

[indexer]
# Mechanical maintenance worker (no LLM). Backfills missing `.vec` sidecars,
# detects embedder-model changes and re-embeds, checkpoints the HNSW
# manifest. Replaces the old Curator.
enabled = true
interval_minutes = 5
batch_size = 50

[retrieval]
alpha          = 0.6
beta           = 0.25
gamma          = 0.15
half_life_days = 30.0
vector_candidates  = 50
keyword_candidates = 20
recent_candidates  = 20
```

CLI overrides win over the file; the file wins over built-in defaults.
Legacy keys (`sanitizer_model`, `[curator]`) still load via serde aliases —
old `config.toml` files keep working (forward-compatible reads invariant).

---

## What does what

| Component    | Crate     | Role                                                |
|-------------:|-----------|-----------------------------------------------------|
| Preprocessor | backend   | Security Preprocessor. Three-tier classification + importance scoring. Ephemeral. |
| The Core     | backend   | Assistant. Hybrid retrieval, replies, persists turns. |
| Memory       | backend   | File-based store. Atomic writes. `.vec` sidecars. Explicit forget. |
| Embedder     | backend   | Local fastembed-rs model — text → vector. No network. |
| VectorIndex  | backend   | HNSW search structure. Derived cache, rebuildable. |
| Indexer      | backend   | Periodic mechanical worker. Backfill + re-embed + stats. NO LLM. |
| The Scout    | backend   | Periodic web/news worker (opt-in).                  |
| Client       | client    | egui chat surface, IP-based geolocation, metadata.  |
| Protocol     | shared    | Wire types — re-used by both crates.                |
