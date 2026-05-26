# Personal AI Assistant — Requirements & Architecture Spec

**Document type:** Build specification for Claude Code (Opus)
**Status:** v1 prototype scope, written to be extensible
**Audience:** Claude Code in a fresh session. This document is the source of truth — read it fully before generating any code.

---

## 0. How to use this document

This is a requirements document, not a design that has been validated. Treat the **Functional Requirements**, **Security Model**, and **Message Contract** sections as fixed intent. Treat the **Tech Stack** and **Build Phases** as strong recommendations that you may refine, but flag any deviation explicitly and explain why before changing it.

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

**Naming note (open decision):** working names used in this doc — *the Gate* (sanitizer), *the Assistant Core* (the main "heavy lifter" LLM layer), *the Curator* (the memory-decay worker), *the Scout* (background news worker). Rename freely; pick something the user likes.

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
  }
}
```
Notes:
- The client is responsible for obtaining geolocation and current datetime. Metadata is intentionally free-form so it can grow without a schema change.
- A message with attachments and no `content` question = "here is more data to remember."

### 5.2 Backend → client
```json
{
  "type": "reply" | "stub_notice" | "error",
  "text": "the assistant's response, or a redaction stub notice",
  "meta": { "tier_summary": "...", "sources": [ ] }   // optional
}
```
Support **streaming** replies (token/chunk frames) so the conversation feels live; define a simple `reply_chunk` / `reply_done` framing if you implement it in Phase 1, otherwise stub it for later.

---

## 6. Memory store & tiered data lifecycle

The store must not keep everything forever. Ingestion volume may be large — potentially gigabytes per month once photos and documents are included. Data **decays by importance, not by a fixed calendar**, and decay is driven by the Assistant/Curator's judgment.

### 6.1 Storage substrate
- File-based on the EC2 instance (the user prefers a file structure over a heavy database for the prototype). A reasonable layout: per-item files (sanitized text + small JSON sidecar of metadata/importance), grouped by date or kind, plus a lightweight index file the Assistant Core can scan or query. SQLite is acceptable *only* if you justify it as the index layer; primary content stays as readable text files.
- Everything in the store is **already sanitized.** Raw input never lands here.

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

- **Backend:** Rust on an EC2 instance. Async runtime: **Tokio**. WebSocket server via `tokio-tungstenite` or an `axum` WebSocket handler. HTTP client (`reqwest`) for the Claude API and web search.
- **Client:** Rust, **native Mac application**, v1 typed input.
  - Options to evaluate and pick: **`egui`/`eframe`** (pure-Rust native window, simplest path for a typed chat UI) vs **Tauri** (web-tech UI shell with a Rust core; richer UI, more moving parts). For a fast v1 typed prototype, `egui` is the lower-friction choice; Tauri is the better long-term bet if the UI gets rich. State the trade-off and choose.
- **Transport:** WebSocket (JSON frames per §5; optional streaming).
- **LLM:** Use Claude in a way that uses the user's Claude Max token budget rather than the Claude API. That may change in the future.
- **Config:** a single config file/env for intervals, decay thresholds, model names, and the sanitizer rule set.

---

## 10. Build phases

[ Deleted section as I will try to have this built all at once. ]

---

## 11. Non-negotiable invariants (restate at top of generated code)
1. **No outbound actions, ever.** Read-in / respond-out only.
2. **Raw input is ephemeral.** It exists only inside a Sanitizer pass, is never logged or persisted, and the pass context is destroyed afterward.
3. **The Sanitizer sees everything, including the user's own queries.** Nothing reaches the Assistant Core or the store un-gated.
4. **Tier-1 content is never stored or forwarded** — only a content-free stub.
5. **The store contains sanitized data only.**

---

## 12. Final notes

- Retention windows / decay thresholds should be determined by AI. This means that the application needs to have some ability to curate all of the data from time to time. This needs to be designed during implementation.
- Make the Claude model modular. Pick the smartest one to start but the user may change it over time.
