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

**No unix text tools.** Do not use `wc`, `sed`, `awk`, `grep`, or `echo` — at
all. `ct` covers every one of these: counts and line/symbol info via `ct`
lookups, search via `ct grep` / `ct search` / `ct vsearch`, and edits via
`ct splice` / `ct move-lines` / `ct extract-function` / `ct delete-function`.

## Documentation freshness

When you change behavior — a marker, the wire protocol, a setup flow, an
invariant, a default, a model — **update the docs in the same change**; never
leave them describing the old reality. A doc claim must be true of the code
**as written, not as intended** (the embedding path that shipped returning
zero vectors while the docs promised "real semantic recall" is the cautionary
tale). Code wins when they disagree — fix the doc. Where each is canonical:

- `backend/src/DEFAULT_MANUAL.md` — the assistant's operating manual:
  *procedural* truth (markers, setup, troubleshooting). Embedded in the binary
  and seeded to the user's editable `SYSTEM_MANUAL.md`.
- `docs/ARCHITECTURE.md` — how it's built + how to contribute.
- `docs/SECURITY.md` — threat model + invariant rationale.
- `docs/USER_GUIDE.md` — install / run / use.
- `README.md` — the pitch + the documentation table of contents.
- `ROADMAP.md` — known gaps + planned work.
- `CLAUDE.md` (this file) — the security-relevant working agreement.

## Security model

A personal AI assistant built around a strict one-way data flow ("the diode").
Raw input enters only through the **Security Preprocessor** (`backend/src/preprocessor.rs`) —
an ephemeral, isolated worker that classifies, redacts, and scores. From there
sanitized data flows to the **Assistant** (the Core, the only component the user
talks to) and the **Memory** store. Workers (`backend/src/workers/`) fetch
external data and push every result through the Preprocessor before it reaches
memory. **This document keeps only the security-relevant rules; architecture,
layout, retrieval, and dev workflow live in `backend/src/DEFAULT_MANUAL.md`.**

## Non-negotiable invariants

These are restated at the top of `backend/src/main.rs` (which also carries the
reliability invariants — restart-safety, forward-compatible reads — omitted
here as non-security). If you find yourself relaxing one, stop and ask.

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
new background ingestor, a future Worker), it MUST default to going
through the Preprocessor. Only the explicit, user-driven UI affordance gets
to set the bypass flag — never as a side effect, never automatically.

## Workers — defense in depth

Workers fetch external data (`backend/src/workers/`). Each worker drives its
own results through the Preprocessor and into memory; no worker output reaches
the Assistant or memory unsanitized.

Each worker that talks to an authenticated API is bound to the narrowest
possible OAuth scope (Gmail uses `gmail.readonly`). The Worker trait
deliberately exposes only `search` and `tick` — there is no `.send()` or
`.delete()` method to bug-call into existence. And even if a worker tried,
Google's authorization server would 403 because the token is scope-bound at
issuance.

Worker setup is **client-driven**: the assistant emits `CONFIG_REQUEST_FILE` /
`CONFIG_BEGIN_OAUTH` markers, the client hosts the OAuth loopback listener and
launches the browser locally, and the backend exchanges the code and writes
`token.json` atomically. Any new configuration capability must follow this
pattern (assistant marker → `ConfigPayload` variant → `config_protocol`
handler), per Invariant #8 — do not add a new bypass or a CLI subcommand.

## Logging discipline (invariant-adjacent)

Never log raw user input, sanitized message bodies, memory item contents,
OAuth secrets, or search queries verbatim. Lengths, counts, structured
metadata (tier, importance, model_used, durations, marker dispatch counts),
and item IDs are fine. Enforced by convention, not code — when adding tracing
calls, log `foo_len = foo.chars().count()` and `kind = ?some_enum` patterns,
never `foo` directly.

## Error policy

When the Preprocessor fails (out of tokens, malformed JSON, etc.) the input
is **dropped** without inspection, an audit record (kind=`preprocessor_error`,
or legacy `sanitizer_error`) goes to memory, and the client gets a
`StubNotice`. When the Assistant fails, the user message is already in memory;
an `assistant_error` record is added and the client gets an `Error` frame.

Never silently swallow LLM errors. Always persist + surface.

## Explicit forget

The system never silently forgets things. When the user asks ("forget that"),
the Assistant emits a `FORGET:` marker line; the WS handler tombstones the
named item (`MemoryStore::forget(id)` → body replaced with `[forgotten <ts>]`,
sidecar kind becomes `ForgottenStub`, `.vec` and vector-index entry removed).
Tombstones are intentional and durable: the metadata stays as forensic audit.
Background workers do NOT delete or rewrite item bodies.
