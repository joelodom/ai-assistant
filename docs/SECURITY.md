# Security Model

Security is the reason this system is shaped the way it is. This document lays
out the threat model, the guarantees, what is deliberately *not* defended, and
the residual risks. For how these guarantees are implemented in code, see the
[Architecture doc](ARCHITECTURE.md); for the user-facing controls, see the
[User's Guide](USER_GUIDE.md).

## Contents

- [The core guarantee: the diode](#the-core-guarantee-the-diode)
- [Threat model](#threat-model)
- [What's defended](#whats-defended)
- [What's deliberately not defended](#whats-deliberately-not-defended)
- [The Security Preprocessor](#the-security-preprocessor)
- [The eight invariants and why each matters](#the-eight-invariants-and-why-each-matters)
- [The HAZMAT bypass](#the-hazmat-bypass)
- [Workers and defense in depth](#workers-and-defense-in-depth)
- [Data at rest](#data-at-rest)
- [Logging discipline](#logging-discipline)
- [Residual risks & non-goals](#residual-risks--non-goals)
- [Reporting a concern](#reporting-a-concern)

---

## The core guarantee: the diode

```
   you ──data──▶ assistant ──answers──▶ you
                     │
                     └── cannot reach the outside world
```

The backend is **read-in / respond-out only**. No code path writes to an
external system: no SMTP, no outbound write APIs, no state-mutating SDKs, no
"just one webhook." The only outbound traffic is **read-only** web search /
fetch, read-only authenticated reads (e.g. `gmail.readonly`), and a **one-time
download of the local embedding model's weights** on first run — after which
embedding is fully on-device. None of these can mutate external state.

This is not a policy you toggle for convenience — it's an architectural
property. Its security payoff is the deepest defense against **prompt injection
through your own data**: even if a malicious email convinces the model to "send
a password-reset link to attacker@evil.com," there is no machinery in the
backend that *can* send anything. The worst an attacker who controls ingested
content can do is **corrupt the answers you get back**. They cannot make the
assistant act in the world.

## Threat model

The system is designed to be **safe to feed your real, sensitive data** while
defending primarily against:

- **Prompt injection via ingested content** — a hostile email, web page, or
  document trying to steer the assistant into harmful behavior.
- **Financially-motivated account-takeover attackers** — someone who gets a
  glimpse of your data (or injects content) and tries to turn it into direct
  theft or account compromise.

The defenses are arranged so that the *most damaging* outcomes (taking actions,
leaking the keys to your accounts) are structurally impossible or actively
suppressed, even if you don't trust the model on any given turn.

## What's defended

The Preprocessor actively suppresses content that would **directly enable
account takeover or theft**:

- 2FA / MFA / one-time passcodes
- Password-reset links and tokens
- API keys, access tokens, session tokens, recovery codes
- Full bank account numbers, full card numbers, routing numbers, wire/ACH
  identifiers

And the architecture itself defends against:

- **Outbound action** of any kind (the diode).
- **Raw-input leakage** — raw input never touches disk, logs, the assistant,
  the embedder, or memory (Invariant #2).
- **Corruption on crash** — atomic writes mean a kill mid-write can't damage
  stored data (Invariant #6).
- **Credential over-reach** — workers are bound to the narrowest OAuth scope
  and expose no write verbs (see [Workers](#workers-and-defense-in-depth)).

## What's deliberately *not* defended

This is a deliberate scope boundary, not an oversight. The Preprocessor does
**not** try to suppress every fact a social engineer might find useful.
Birthdays, vacation dates ("house empty next Tuesday"), kids' school names,
employer details, calendar events — these are **remembered and reasoned over**,
because hobbling the assistant's usefulness to defend against general social
engineering would defeat the purpose of having it.

The threshold is sharp: *"would this single fact directly enable account
takeover?"* If yes → drop or redact. If no → keep. The system is not built to
resist a nation-state adversary, a local attacker with disk access to your
unlocked machine, or a compromised host OS.

## The Security Preprocessor

Every byte from the outside world — your typing, an ingested email, text
extracted from a PDF, a web page a worker fetched — passes through the
**Security Preprocessor** before anything else sees it.

Two properties make it trustworthy:

1. **It is ephemeral and isolated.** Each call is a *fresh `claude` subprocess*
   — no shared session, no `--continue`, no history. The raw input lives only
   on one function's stack and inside that one short-lived subprocess. When the
   subprocess exits, the raw input is gone. It is never logged, never written to
   disk, never forwarded.
2. **It classifies into three tiers** (`Tier::Drop` / `Redact` / `Pass`):
   - **Drop** — content that is *only* security-relevant (an OTP email, a
     reset link). The content is destroyed; only a content-free stub note is
     recorded ("received and dropped something that looked like a security
     code").
   - **Redact** — sensitive but contextually useful (a deposit confirmation).
     Directly-actionable identifiers are replaced with placeholders; the
     who/what/when is preserved.
   - **Pass** — the vast majority of input. Goes through unchanged.

The Preprocessor also assigns an **importance score** used later in retrieval.
If the Preprocessor itself fails, the input is **dropped without inspection**
and an audit record is written — failure is fail-closed, never fail-open.

## The eight invariants and why each matters

The first seven head `backend/src/main.rs`; `CLAUDE.md` keeps the
security-relevant subset (and the ConfigPayload bypass, #8). Each one is
load-bearing for a specific reason:

1. **No outbound actions, ever.** The diode. Caps the blast radius of any
   successful prompt injection at "wrong answer," never "wrong action."
2. **Raw input is ephemeral.** Shrinks the window in which unsanitized data
   exists to a single subprocess lifetime — nothing to leak, log, or steal
   later.
3. **The Preprocessor sees everything** (except HAZMAT). No ingestion path gets
   to skip the gate by accident.
4. **Tier-1 (drop) content is never stored or forwarded.** The most dangerous
   content (live credentials) leaves no trace beyond a content-free stub.
5. **The memory store contains sanitized data only.** Embeddings derive from
   sanitized bodies, so even the vector representation can't smuggle raw
   secrets.
6. **Restart-safe at any time.** Atomic writes + no shutdown-required paths mean
   a crash, kill, or power loss can't corrupt or partially-commit your data.
7. **Forward-compatible reads.** Your data outlives any single version of this
   software; upgrades never strand or silently rewrite it.
8. **ConfigPayload bypasses the Preprocessor *and* memory.** OAuth secrets and
   callback codes are mechanical handshakes, not personal data — they're
   handled only by `config_protocol.rs`, which writes to the connector
   directory and holds pending state in memory with a short TTL. This is the
   *only* sanctioned bypass, and it is narrow by construction.

## The HAZMAT bypass

There is exactly **one** user-controlled exception to "the Preprocessor sees
everything": the **`☢ HAZMAT`** checkbox in the client. When ticked, the next
message's `bypass_preprocessor` flag is set and the raw content goes straight to
the assistant.

It is safe *as a feature* because it is hedged on every side:

- **Manual only.** No code path may set the bypass programmatically — only the
  human-pressed checkbox flips it. Any new ingestion path must default to the
  Preprocessor.
- **Audited.** Every bypass is logged at WARN; the resulting memory item is
  tagged `hazmat` with elevated importance.
- **Visible.** The client shows a `☢ HAZMAT` banner on the bypassed message in
  the transcript, so you can never wonder later whether something skipped the
  gate.
- **Session-scoped.** It resets to off on every client restart — it can't be
  left on by accident across sessions.

## Workers and defense in depth

Workers (Gmail, Google Drive, the web) pull external data, and that data is "outside world"
data — so it gets the same treatment as anything else: **every worker result
passes through the Preprocessor before reaching memory** (enforced structurally
because workers ingest via `WorkerContext::ingest_one`, never directly).

For workers that talk to authenticated APIs, three independent layers prevent
abuse — illustrated by Gmail:

1. **The scope is minimal and enforced upstream.** Gmail uses
   `gmail.readonly`. Any attempt to send/delete/modify returns 403 from
   Google's authorization server, regardless of what our code does.
2. **The trait has no write verbs.** `Worker` exposes only `search` and `tick`.
   There is no `.send()` or `.delete()` for a bug to call into existence.
3. **Every result is screened.** OTPs, reset links, and injected content are
   dropped or redacted before they reach the assistant or storage.

The Google Drive worker follows the same model: scope `drive.readonly` (it can
read and download all your files, but Google rejects any create/edit/delete
server-side), no write verbs on the trait, and every downloaded file screened
by the Preprocessor. It is read-everything / write-nothing.

OAuth credentials and tokens live under `<memory-dir>/connectors/<name>/`,
written atomically. Tokens are scope-bound at issuance and refreshed silently;
revoke any time at <https://myaccount.google.com/permissions>.

## Data at rest

There is **no database and no cloud storage**. Memory is plain text bodies plus
small JSON sidecars (and binary `.vec` vectors) under a directory you choose.
You can `cat`, `grep`, and `tar` it, and read it with `less` years from now.

The flip side: **at-rest encryption is your responsibility.** The files are
plaintext on disk. If that matters for your threat model, put the memory
directory on an encrypted volume (e.g. FileVault, LUKS) and protect the machine
accordingly. The system assumes the host and the local user account are
trusted.

## Logging discipline

Logs are structured and deliberately content-free. The rule (enforced by
convention in every `tracing` call): **never log raw user input, sanitized
bodies, memory contents, OAuth secrets, or search queries verbatim.** Lengths,
counts, enum values, durations, model names, and item IDs are fine.

One sharp edge worth knowing: a bare `debug`/`trace` log level turns on
*dependency* logging, including the WebSocket library's per-frame payloads —
which would contain reply text derived from your memory. Always scope verbose
logging to our own crate (`level = "info,backend=trace"`), as the `config.toml`
comments warn.

## Residual risks & non-goals

Being explicit about the edges:

- **The model can still be wrong or be misled.** Prompt injection can corrupt
  an *answer*. The diode ensures it can't corrupt an *action*, but you should
  treat answers with normal skepticism.
- **Plaintext at rest.** See [Data at rest](#data-at-rest). Disk encryption is
  out of scope and left to the host.
- **A compromised host or unlocked machine** defeats the model — local file
  access reads everything. Not defended; out of scope.
- **The `claude` CLI and its account** are trusted dependencies. Preprocessor
  calls are subprocesses of that CLI.
- **Social-engineering-useful facts are kept by design** (see [What's
  deliberately not defended](#whats-deliberately-not-defended)).
- **HAZMAT** intentionally disables the gate for one message at a time. That's
  the point; use it deliberately.

## Reporting a concern

If you believe an invariant can be violated — especially Invariant #1 (the
diode) or #2 (raw-input ephemerality) — that's a serious finding. Capture the
path (which function, which input) and raise it. The invariants are meant to be
*structural*; a way around one is a bug in the architecture, not a
configuration mistake.
