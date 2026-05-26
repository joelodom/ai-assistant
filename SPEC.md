# Personal AI Assistant — Requirements & Architecture Spec

**Document type:** Requirements + as-built record for the v1 prototype
**Status:** v1 prototype built and running locally
**Audience:** Claude Code in a fresh session. This document is the source of truth — read it fully before generating any code.

---

## 0. How to use this document

This is both a requirements document and a record of what was actually built in v1. Treat the **Functional Requirements**, **Security Model**, **Threat Model**, and **Message Contract** sections as fixed intent. Sections marked **(as built)** record concrete implementation choices made during the v1 build — change them deliberately, not casually.

If you're extending the prototype: re-read §11 (Non-negotiable invariants) before touching the Sanitizer, memory store, or any path that could open an outbound side channel.

---

## 1. Concept

A personal assistant backed by an LLM, designed around a strict **one-way data flow** — a "diode."

- Data flows **in**: emails, calendar entries, scanned documents, photos, news, and free-text notes.
- The assistant **accumulates** knowledge about the user and their world over time.
- The assistant only ever produces **outputs**: reminders, to-do prompts, summaries, news the user cares about, and answers to questions.
- The assistant **cannot take actions in the outside world.** It cannot send email, change a thermostat, reset a password, move money, or touch any external account or device. There is no write path out. This is a deliberate, load-bearing security property — do not add outbound action capability "for convenience." Don't worry about side effects of fetching public URLs or similar.

The mental model is a trusted human personal assistant: you hand them your mail, they read and remember it, and later you ask "what do I need to be thinking about right now?" and they tell you. They never act on your behalf in the world but they remind you of things and tell you things you may be interested and prompt you and learn from your feedback.

---

## 2. Threat model (read before the security section)

The user is protecting against **sophisticated, financially motivated attackers** whose goal is account takeover, account hijacking, or theft of banking/monetary information.

**In scope to protect against (must never reach long-term storage or the main LLM):**
- Two-factor / multi-factor authentication codes (OTPs).
- Password reset links and password reset tokens.
- API keys, access tokens, session tokens, recovery codes.
- Full bank account numbers, full card numbers, routing numbers, wire/ACH/ETF transfer identifiers, and similar directly-actionable financial identifiers.

**Explicitly OUT of scope (acceptable to store and reason over):**
- Birthdays, family members' names, kids' schedules.
- Vacation dates, travel plans, "house is empty" implications.
- Job interviews, employment transitions, calendar events.
- Anything that would only help a *sophisticated social engineer* rather than enable a *direct* takeover.

The user may tighten this scope later. The sanitizer's rules must therefore be easy to read and adjust in one place.

---

## 3. Security model — the Sanitizer ("the Gate")

The Sanitizer is the **first layer everything passes through** — including the user's own questions and conversational messages, not just ingested data. Most messages pass through untouched; the point is that nothing bypasses the gate.

### 3.1 Ephemeral, isolated context (hard requirement)
- Each sanitization pass runs in its own **isolated, ephemeral context** that is **destroyed immediately after** the pass completes.
- The raw, un-sanitized input must exist **only** for the duration of that pass. It must never be written to disk, never be logged, and never be passed to the main LLM or the memory store.
- The Sanitizer must not share conversation state or memory with the main assistant. It is a pure function: raw input in, sanitized output (+ a redaction report) out, then forget everything.
- Implementation note: the Sanitizer LLM call must use a fresh request with no shared history. Do not reuse the main assistant's context for it.

### 3.4 HAZMAT bypass (as built)

The default rule is: every message goes through the Sanitizer, no exceptions. There is one **explicit, user-controlled, opt-in exception**: HAZMAT mode. The client exposes a checkbox (`☢ Hazmat (bypass sanitizer)`); when ticked, the next message's `bypass_sanitizer` flag is true and the backend skips the Sanitizer for that message, passing the raw content directly to the Assistant.

Constraints on the bypass:
- **User-initiated only.** No background worker, ingestor, or programmatic path may set the bypass flag. The flag only flips via the UI affordance.
- **Per-message.** Each message decides independently. The flag does not persist on the backend.
- **Session-scoped at the UI.** The checkbox resets to off on every client restart so a forgotten toggle doesn't carry between sessions.
- **Audited.** The backend logs `WARN` for every bypass with the message length. The resulting memory item is tagged `hazmat` and carries elevated importance (0.8) so the user can audit later with "show me everything I bypassed the sanitizer for."
- **Visible.** The client renders the user's outgoing message with a `☢ HAZMAT (sanitizer bypassed) ☢` banner in the local transcript so the user never wonders whether a message went through the Gate.

Intended use: pasting a document the user knows is safe and wants reasoned over verbatim, testing assistant behavior on edge-case inputs, or asking the assistant to discuss content where redaction would defeat the purpose. The user is taking on the risk explicitly.

### 3.2 Three-tier handling
For each piece of input, the Sanitizer classifies and acts:

**Tier 1 — Drop entirely.** Input that is *obviously* and *only* security-relevant (a 2FA code email, a password reset link, an MFA prompt). Drop the content completely. Pass through only a short stub note, e.g.:
> *"Received and dropped an email that appeared to be only a security message (likely a login/reset code)."*
The stub records that something arrived and why it was dropped — never the content.

**Tier 2 — Redact, then pass.** Input that is sensitive but contextually useful (a deposit confirmation with a dollar amount, an IRS letter, a transfer confirmation with an account/ETF number). Pass the message through with the dangerous identifiers redacted, plus a short description of what was redacted. Example output:
> *"Email from [bank]: a deposit was confirmed. [Dollar amount redacted] [Account number redacted]. Rest of message: …"*
Keep the context (who, what kind of event, when); strip the directly-actionable number.

**Tier 3 — Pass through.** The vast majority of input. No security relevance. Passes to the main assistant unchanged. If a message contains no question, assume it is more data to remember rather than a request.

---

## 4. End-to-end architecture

Note that the architecture depicted runs on a cloud server, but initial prototyping may all be local.

```
┌─────────────────────────────┐         WebSocket        ┌──────────────────────────────────────┐
│   Native Mac client (Rust)  │ <══════════════════════> │        Backend server (Rust, EC2)      │
│                             │                          │                                        │
│  - typed input (v1)         │   message + metadata     │  1. SANITIZER  (ephemeral, destroyed)  │
│  - attaches metadata        │ ───────────────────────> │       │                                │
│    (geolocation, datetime)  │                          │       ▼                                │
│  - renders responses        │ <─────────────────────── │  2. ASSISTANT CORE  ("heavy lifter")   │
│                             │      assistant reply     │       - reads memory store             │
└─────────────────────────────┘                          │       - calls AI
                                                          │       - returns answer / reminders     │
                                                          │       ▲           │                    │
                                                          │       │           ▼                    │
                                                          │  3. MEMORY STORE (files, tiered decay) │
                                                          │       ▲                                │
                                                          │  4. BACKGROUND WORKER (news/web, ~10m) │
                                                          └──────────────────────────────────────┘
```

**Naming (as built):** The working names stuck. The codebase uses *Sanitizer* (a.k.a. the Gate), *Assistant* (the Core / "heavy lifter"), *Curator*, and *Scout*. User-visible UI labels for turn types are: `you` / `assistant` / `gate` (for Sanitizer stub notices) / `system` (for the introduction) / `error`.

Regardig the background worker, it should probably call the assistant core just to wake it up and let the assistant core do any fetching of news or events that the user is interested in. Fetches of PUBLIC URLs should still go through the sanitizer but the sanitizer should only sanitize the user's personal data, so it should be tagged as probably public. The point of this is that just in case there is an attack where the user's personal data is put on a "public URL" there is still a sanitization on it to hopefully stop the attack.

### 4.1 Components
1. **Mac client (Rust, native).** Single unified surface for *both* data ingestion and conversation. Sends every message — data drops and questions alike — through the same channel. Attaches metadata on every message.
2. **Sanitizer / the Gate (backend, first layer).** As specified in §3. Ephemeral and isolated.
3. **Assistant Core (backend).** Receives only sanitized content. Pulls relevant memory, combines with the message and metadata, calls the AI, returns the reply. This is the only component that "talks back" to the user.
4. **Memory store (backend).** File-based backing store on the EC2 instance (see §6). Holds sanitized text, summaries, and learned preferences.
5. **Background worker / the Scout (backend).** Periodically (≈ every 10 minutes) does routine web/news queries and folds results into memory so they're ready when the user asks.
6. **Curator (backend, periodic).** Runs the tiered data-decay / summarization process (see §6).

---

## 5. Message contract (client ↔ backend)

All traffic over a single WebSocket connection. JSON messages.

### 5.1 Client → backend
```json
{
  "type": "message",
  "payload": {
    "content": "free text, or a question, or pasted email/doc text",
    "attachments": [
      { "kind": "photo" | "document" | "email" | "calendar", "data": "...", "mime": "..." }
    ]
  },
  "metadata": {
    "datetime_iso": "2026-05-25T14:03:00-05:00",
    "geolocation": { "lat": 30.53, "lon": -92.08, "label": "Opelousas, LA" },
    "freeform": { }   // open-ended; AI-friendly; NOTHING SECRET goes here by design
  },
  "bypass_sanitizer": false   // optional; true = HAZMAT mode (§3.4). Default false.
}
```
Notes:
- The client is responsible for obtaining geolocation and current datetime. Metadata is intentionally free-form so it can grow without a schema change.
- A message with attachments and no `content` question = "here is more data to remember."
- `bypass_sanitizer` defaults to false (omit the field for normal traffic). Only the explicit user-driven UI checkbox sets it; see §3.4.

### 5.2 Backend → client (as built)

Five frame types, all JSON, snake_case `type` discriminator. Defined in `shared/src/lib.rs::ServerMessage`:

```json
// Streaming reply chunk — zero or more per turn.
{ "type": "reply_chunk", "text": "..." }

// End of a streamed reply. Optionally carries the full reply text (used for
// the introduction and for cases where the backend chose to send one shot).
{ "type": "reply_done", "text": null | "...", "meta": { "tier_summary": "pass|redact|introduction", "sources": [] } }

// Sanitizer Tier-1 drop, or sanitizer-failure notice. Content-free.
{ "type": "stub_notice", "text": "Received and dropped an email that ..." }

// Backend-side error surfaced to the user.
{ "type": "error", "text": "..." }

// Keepalive reply.
{ "type": "pong" }
```

On connect, the backend immediately sends one `reply_done` frame containing an **introduction** so a brand-new user knows who/what this is and that they can start sending data. The intro text branches on whether memory is empty (bootstrap) vs. populated (welcome back).

---

## 6. Memory store & tiered data lifecycle

The store must not keep everything forever. Ingestion volume may be large — potentially gigabytes per month once photos and documents are included. Data **decays by importance, not by a fixed calendar**, and decay is driven by the Assistant/Curator's judgment.

### 6.1 Storage substrate (as built)

- File-based, **no database**. Layout under the configured memory root:

  ```
  <root>/
    items/
      YYYY-MM-DD/
        YYYYMMDDTHHMMSSZ-<uuid>.txt   # sanitized body, plain text
        YYYYMMDDTHHMMSSZ-<uuid>.json  # sidecar: kind, importance, decay_stage,
                                      #          tags, redaction_report, state,
                                      #          metadata (datetime + geo)
    stubs/
      <id>.json                       # Tier-1 drop records (content-free)
    preferences.json                  # learned user preferences (with timestamps)
  ```

- **No index file.** The volume justified by v1 is small enough that the Assistant scans sidecars per turn (`MemoryStore::scan_all`). When this stops being trivial, the right move is a SQLite index over sidecars; the bodies stay as text on disk.

- **Atomic writes.** Every body, sidecar, stub, and preference write goes through `memory::atomic_write` (write temp file → fsync → rename). A crash mid-write cannot leave a partially-written item.

- **Override the root** with `--memory-dir <path>` or `AI_ASSISTANT_MEMORY_DIR=<path>`. The directory **is** the database — point the backend at different folders to run against different datasets.

- **Backup = `tar czf data.tgz <root>`.** The format is human-readable text + JSON; no binary state lives outside the directory.

- Everything in the store is **already sanitized.** Raw input never lands here. (See §3.1 and §13.)

### 6.2 Decay tiers (progressive summarization, not hard delete)
- **Fresh (recent):** kept in full (sanitized). Highest fidelity.
- **Aging:** as importance drops, items are **summarized down** rather than deleted. The summary preserves the durable, useful nuggets — names of people met, what an event was, key facts — and drops bulk.
  - *Photos:* the binary is eventually discarded and replaced with a text summary: "Photo from [date]; appears to show [people/scene]; text parsed from it: […]." Keep the OCR'd/described text, drop the pixels.
  - *Emails / scanned documents:* these are text. Sanitized text is cheap to keep, so retain it far longer than photos.
  - *Low-value bulk* (e.g., a company's privacy-policy email): collapse quickly (even within ~a month) to a stub: "Email re: privacy policy from [company], dated [date] — search your source if you need the full text."
- **Stale / ridiculous-old:** collapsed to a one-line pointer or dropped when there is genuinely no reason to keep it.

### 6.3 Importance scoring
- The Curator (LLM-assisted) assigns/updates an importance score per item and decides when to summarize or collapse. There is no required schedule; it runs periodically and uses judgment. Make the thresholds configurable.

### 6.4 Learned preferences
- The store also holds **preferences** the user expresses over time ("stop telling me about this kind of news," "I don't care about X"). These persist and shape both the Scout's filtering and the Assistant Core's responses. Acknowledgements like "I finished that, take it off the list" update item state too.

---

## 7. Background behavior (the Scout)

- Wakes on an interval (≈ 10 min; configurable). Issues routine queries — news, the user's favorite sites/topics — with the current time as context.
- Folds findings into memory, filtered through learned preferences, so they are ready when the user next asks.
- When the user asks "what's going on / what do I need to know," the Assistant Core returns **both** time/location-relevant reminders **and** genuinely interesting news the user is likely to care about (e.g., "you'll be in South Louisiana — there's a convention you might like").
- Web search runs server-side on the backend.

---

## 8. User experience

- v1: user opens the Mac app and **types**. (Voice / text-to-speech / full conversational mode comes later — design so it can be added without rework.)
- Primary interaction: "What do I need to be thinking about right now?" → the assistant uses metadata (now, here) + memory to answer.
- The user can acknowledge/dismiss: "Yeah, I finished that" / "Forget about that" → updates item state or drops it.
- The user can ask open questions about their own life and the ingested data, and about the world/news.
- Tone: like talking to a personal assistant who knows your life.
- The **same surface** handles ingestion and conversation — there is no separate "upload" screen. Hand it data, ask it things, all in one thread.

---

## 9. Tech stack

- **Backend (as built):** Rust, async via **Tokio**, WebSocket via **axum**'s `ws` extractor. Workspace layout: `shared/` (wire protocol), `backend/` (server), `client/` (UI). Runs locally for the prototype; the same binary is intended for EC2.
- **Client (as built):** Rust, native via **`egui`/`eframe`**. Chosen over Tauri for v1 because the typed-chat surface is simple, the build is fast, and the dependency footprint is smaller. Revisit if the UI grows rich (image previews, file pickers, settings tabs).
- **Transport (as built):** WebSocket, JSON frames per §5.2. Streaming is implemented as `reply_chunk` (zero or more) followed by `reply_done`.
- **LLM (as built):** The backend shells out to the `claude` CLI (one `tokio::process::Command` per call) so the user's Claude Max subscription is the token budget. There is **no shared session** between calls — each invocation is a fresh `claude -p`. This is load-bearing for the Sanitizer's ephemeral-context invariant (§3.1).
  - Tools per call:
    - Sanitizer + Curator: `--tools ""` (all tools disabled).
    - Assistant + Scout: `--allowedTools WebSearch,WebFetch --permission-mode dontAsk` (the `dontAsk` is required for tools to actually fire in `-p` mode, where there's no human to approve prompts).
  - The model name is configurable; default `claude-opus-4-7`.
  - Tests swap in a `MockLlmClient` via `AI_ASSISTANT_MOCK_CLAUDE=1`. See §14.
- **Config (as built):** `config.toml` at the working directory (optional — all keys have built-in defaults). CLI flags `--config`, `--memory-dir`, `--addr` override the file. Env vars `AI_ASSISTANT_MEMORY_DIR` and `AI_ASSISTANT_ADDR` also override.

---

## 10. Build phases (as built)

V1 was built in a single pass rather than phased. The system is small enough that the phase boundaries didn't earn their keep. Future work — voice input, photo OCR, EC2 deployment, richer search — should land as its own discrete step with the existing invariants preserved.

---

## 11. Non-negotiable invariants (restate at top of generated code)
1. **No outbound actions, ever.** Read-in / respond-out only.
2. **Raw input is ephemeral.** It exists only inside a Sanitizer pass, is never logged or persisted, and the pass context is destroyed afterward.
3. **The Sanitizer sees everything, including the user's own queries.** Nothing reaches the Assistant Core or the store un-gated — *except* via the explicit, audited, user-driven HAZMAT bypass (§3.4). No code path may set the bypass flag programmatically.
4. **Tier-1 content is never stored or forwarded** — only a content-free stub.
5. **The store contains sanitized data only.**

---

## 12. Operations (as built)

- **Run locally:**
  ```bash
  ./target/release/ai-assistant-backend                            # ./memory, ws://127.0.0.1:8765/ws
  ./target/release/ai-assistant-backend --memory-dir ~/data/work   # different dataset
  AI_ASSISTANT_MEMORY_DIR=/tmp/scratch ./target/release/ai-assistant-backend
  ./target/release/ai-assistant-client --url ws://127.0.0.1:8765/ws
  ```
- **Backup:** `tar czf data-$(date +%F).tgz -C <parent> <dirname>`. There is no other persistent state.
- **Logs:** stderr, controlled by `RUST_LOG` (default `info`).
- **Client prefs:** UI scale (and any future client-only preferences) persist at `~/.ai-assistant-client.json`. The client supports ⌘+ / ⌘− / ⌘0 to zoom and a slider in the Settings panel.

---

## 13. Error policy (as built)

LLM calls fail sometimes — out of tokens, CLI not found, network blip, malformed JSON. The system never silently swallows these; every failure is both **surfaced to the user** and **recorded in memory** so the user can ask about it later.

- **Sanitizer failure.** Input is **dropped without inspection** (the ephemerality invariant is preserved: the raw text never moves out of the per-request stack). An audit record of kind `sanitizer_error` is written to memory containing the timestamp, the *length* of the dropped input (not the content), and the underlying error message. The client receives a `stub_notice` explaining what happened and that a note was saved.
- **Assistant failure.** The user's sanitized message was already persisted before the LLM call (so it isn't lost). A paired `assistant_error` record is written with the timestamp and underlying error. The client receives an `error` frame with a user-friendly explanation and a hint about likely causes (token exhaustion, rate limiting).
- **Curator/Scout failure.** Logged at warn level; loop continues at the next interval. No user-visible surface.
- The Sanitizer's prompt is hardened against prompt injection: input is enclosed in `<<<BEGIN_INPUT>>>` / `<<<END_INPUT>>>` markers with explicit instructions to treat everything inside as data, not instructions.

---

## 14. Testability (as built)

The backend is designed so the whole pipeline can run without spending a single Claude token.

- `LlmClient` trait in `backend/src/claude.rs` with three implementations:
  - `ClaudeCliClient` — production. Spawns `claude -p ...`.
  - `MockLlmClient` — deterministic, in-process, no network. Default responses keyed off prompt markers (`SANITIZER_TASK`, `SCOUT_TASK`, `CURATOR_TASK`); per-test overrides via `respond_when(matcher, response)`.
  - `FailingLlmClient` — always errors. Used to exercise the failure paths in §13.
- Set `AI_ASSISTANT_MOCK_CLAUDE=1` to swap the production client for the mock at process start.
- `cargo test --workspace` runs 19 unit tests + 3 integration tests covering: Sanitizer JSON parsing (drop/redact/pass + nested braces), memory roundtrip + search + atomic-write durability, Curator stage promotion + summarization, preference detection, the full WebSocket roundtrip with intro and reply frames, the Tier-1 OTP-never-on-disk invariant, and the sanitizer-failure audit path.

---

## 15. Final notes

- Retention windows / decay thresholds are AI-driven via the **Curator** (see §6, §7). Thresholds are configurable in `config.toml` (`fresh_age_hours`, `aging_age_days`, `stale_age_days`). The Curator advances items through Fresh → Aging → Summarized → Stale and uses Claude to collapse aging items into short summaries; low-importance items decay sooner.
- The Claude model name is modular — configurable via `[claude].model` in `config.toml`. Default is the latest available Opus.
