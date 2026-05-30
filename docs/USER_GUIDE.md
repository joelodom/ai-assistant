# User's Guide

How to install, run, and live with the assistant day to day. For the *why*
and the high-level pitch, see the [README](../README.md). For the internals,
see the [Architecture doc](ARCHITECTURE.md). For the security reasoning, see
the [Security Model](SECURITY.md).

## Contents

- [Install & build](#install--build)
- [Running the backend](#running-the-backend)
- [Running the client](#running-the-client)
- [Your first conversation](#your-first-conversation)
- [Handing it data](#handing-it-data)
- [Asking it things](#asking-it-things)
- [Telling it to forget](#telling-it-to-forget)
- [HAZMAT: bypassing the security gate](#hazmat-bypassing-the-security-gate)
- [Forcing the heavier model](#forcing-the-heavier-model)
- [Connecting Gmail](#connecting-gmail)
- [Connecting Google Drive](#connecting-google-drive)
- [Curated news (the web worker)](#curated-news-the-web-worker)
- [The status bar](#the-status-bar)
- [Multiple datasets & backup](#multiple-datasets--backup)
- [Configuration you actually care about](#configuration-you-actually-care-about)
- [Troubleshooting](#troubleshooting)

---

## Install & build

You need **Rust** (1.75+) and the authenticated **`claude` CLI**.

```bash
cargo build --release
```

This builds `ai-assistant-backend` and `ai-assistant-client` into
`target/release/`. Real local semantic embeddings (**bge-base-en-v1.5**, an
English model) are on by default; the model (~400 MB) downloads once on first
use and then runs entirely on your machine.

> For a fast, dependency-light build that skips ONNX Runtime, add
> `--no-default-features`. It substitutes a deterministic *mock* embedder —
> fine for kicking the tires or CI, but recall is **not** semantically
> meaningful. Daily use should be the plain default build.

## Running the backend

```bash
./target/release/ai-assistant-backend                  # uses ./config.toml if present, else built-in defaults
./target/release/ai-assistant-backend --config my.toml # explicit config
```

The backend has **exactly one flag**: `--config <path>`. Everything tunable
lives in the TOML (see [Configuration](#configuration-you-actually-care-about)).
It listens on `127.0.0.1:8765` by default and keeps all state in the memory
directory — nothing else on your system is touched.

You can stop it (`Ctrl-C`), restart it, reboot the machine — it's safe at any
moment. The only thing you can lose is a request that was in flight at the
instant you killed it.

## Running the client

```bash
./target/release/ai-assistant-client                              # connects to the default URL
./target/release/ai-assistant-client --url ws://127.0.0.1:8765/ws # explicit
AI_ASSISTANT_URL=ws://10.0.0.5:8765/ws ./target/release/ai-assistant-client
```

The client is a native desktop window with a single chat surface. It also
attempts a coarse IP-based geolocation (with a short timeout) so the assistant
knows roughly *where* you are when that's relevant; you can edit or clear the
location in settings.

**Keyboard:** `Enter` inserts a newline; `⌘+Enter` (or `Ctrl+Enter`) sends.
You can also click **Send**.

## Your first conversation

On first connect the assistant introduces itself and explains what it can and
can't do. There's nothing to configure to start — just type. Early on, tell it
things about yourself and your projects; that's what makes later recall useful.

Useful opening moves:

- *"Remember that I'm renovating the kitchen and the contractor is Dana."*
- *"I prefer terse answers."* (it learns standing preferences)
- Paste an email or a note and say *"hang on to this."*

## Handing it data

Anything you type or paste becomes memory (after passing the security gate).
You can also attach files:

- Click the **📎** button, or **drag files onto the window**.
- Supported: images (photos), PDFs and text documents (text is extracted),
  and pasted email/calendar text.

Every piece of incoming data — typed, pasted, or attached — passes through the
**Security Preprocessor** first. Most content passes straight through. Content
that is *only* security-relevant (a one-time passcode, a password-reset link)
is dropped and replaced with a content-free note. Sensitive-but-useful content
(a bank confirmation) is redacted — the actionable identifiers are stripped,
the who/what/when is kept. See [Security Model](SECURITY.md) for the full
picture.

## Asking it things

Just ask in plain English. The assistant retrieves the most relevant slice of
your memory each turn (semantic + keyword + recency + importance) and answers
from it. Examples:

- *"What am I supposed to be following up on?"*
- *"When did I last hear from my accountant, and about what?"*
- *"Summarize what I know about the roof situation."*

If the answer is likely in a connected source (like Gmail) rather than in
memory, the assistant will **search** it for you and fold the results in before
answering — you'll see that happen live in the status bar.

## Telling it to forget

The system **never silently forgets**. If you want something gone, say so:

- *"Forget that last note about the password."*
- *"Delete what I told you about the surprise party."*

The assistant tombstones the item: the body is zeroed, its vector and search
index entry are removed, and an audit record remains (so you can tell *that*
something was forgotten, just not *what*). This is deliberate and durable.

## HAZMAT: bypassing the security gate

The client has a **`☢ HAZMAT`** checkbox. When ticked, your next message
**skips the Security Preprocessor** and goes straight to the assistant, verbatim.

Use it when you've *consciously* decided some content is safe and you want it
reasoned over exactly as written (the Preprocessor occasionally redacts
something you actually wanted kept).

Guarantees around it:

- It is **opt-in and manual only** — no code path can flip it for you.
- Every bypass is **logged** and the resulting memory item is **tagged
  `hazmat`** with elevated importance, so the audit trail is intact.
- Your local transcript shows a **`☢ HAZMAT`** banner on the message, so you
  never wonder later whether something skipped the gate.
- It is **session-scoped** and resets to off every time you restart the client.

## Forcing the heavier model

By default the assistant answers with a fast mid-tier model and *escalates
itself* to the heavier model when a question warrants it. If you already know a
question deserves the big model, tick **🧠 OPUS** to route straight there.
Like HAZMAT, the choice is shown on your message in the transcript.

## Connecting Gmail

The assistant can search your Gmail **read-only**, and (once connected) will
also quietly pull in new mail as it arrives so it can reason over it later.

Setup is a **conversation**, not a wizard. Start the backend and client, then
type:

> set up gmail

The assistant reads its own manual and walks you through the whole thing:
creating OAuth credentials in the Google Cloud Console, uploading the
`client_secret.json`, the browser-based authorization, the *"Google hasn't
verified this app"* warning (expected for a personal app in testing mode), and
a suggested test query at the end.

What's worth knowing up front:

- The browser authorization happens **on your machine** — the client launches
  your browser and hosts the local loopback that catches the redirect. The
  auth code reaches the backend over the existing trusted connection. This
  works the same whether the backend is local or on a headless server.
- The scope is hardcoded to **`gmail.readonly`**. Google enforces it
  server-side; the assistant has no ability to send, delete, or modify mail.
- Every fetched email passes through the Security Preprocessor before anything
  reaches memory.
- The token refreshes silently. Revoke anytime at
  <https://myaccount.google.com/permissions>.

Once connected, just ask naturally — *"what did Dana say about the cabinets?"*
— and the assistant will search Gmail if the answer isn't already in memory.

## Connecting Google Drive

The assistant can also search your **Google Drive, read-only** — and pull the
*contents* of matching files into memory, not just their names. It cannot
create, edit, move, or delete anything in your Drive.

Setup is the same conversation as Gmail. Type:

> set up Google Drive

The assistant walks you through it: enabling the **Google Drive API** in the
Google Cloud Console (you can reuse the same project and OAuth client you made
for Gmail — just enable the extra API), uploading `client_secret.json`, and the
browser authorization. On the consent screen you'll see *"See and download all
your Google Drive files"* — that is the read-only Drive permission.

What's worth knowing up front:

- The scope is hardcoded to **`drive.readonly`**. Google enforces it
  server-side, so the assistant can read and download your files but has no
  ability to change them. It is read-*everything*, write-*nothing* — the
  permission covers all your Drive files, not a subset.
- When you search, each matching file's **text is downloaded and remembered**:
  Google Docs/Sheets/Slides are exported to text, PDFs and text files are
  extracted. Images, video, and other binaries are skipped. Everything passes
  through the Security Preprocessor before it reaches memory.
- It is **search-only** — unlike Gmail it does *not* run in the background, so
  it won't pull your whole Drive into memory on its own. You ask, it fetches.
- The token refreshes silently. Revoke anytime at
  <https://myaccount.google.com/permissions>.

Then just ask — *"find my notes about the kitchen remodel"* or *"what's in my
budget spreadsheet?"* — and the assistant searches Drive and folds in what it
finds.

## Curated news (the web worker)

There's a built-in **web worker** that can do two things:

1. **On demand** — when you ask something time-sensitive whose answer isn't in
   memory, the assistant searches the open web and folds in what it finds.
2. **Autonomously** (opt-in) — on a timer, it infers what you care about from
   your memory and surfaces relevant news without you asking.

The autonomous scan is **off by default** because it spends tokens in the
background. Turn it on once you've used the system enough that it has a sense
of you, by setting `enabled = true` under `[scout]` in your config (the config
section is still named `scout` for historical reasons — see
[Configuration](#configuration-you-actually-care-about)).

## The status bar

While the backend works, a live status bar shows what's happening — *reviewing
your message*, *looking through memory*, *thinking*, *searching gmail…*, and so
on, with an elapsed timer. When several things run at once (e.g. Gmail and the
web worker in parallel), each gets **its own row** so you can watch real
progress instead of a single flickering line. This is also your cue that a slow
turn is actually doing work, not hung.

## Multiple datasets & backup

The memory directory **is** the database. To run against different datasets,
point different config files at different folders:

```bash
./target/release/ai-assistant-backend --config personal.toml
./target/release/ai-assistant-backend --config work.toml
```

Backup is a tarball:

```bash
tar czf data-$(date +%F).tgz -C ~/data personal
```

Restore by untarring. Backups can even be partial — if you restore without the
`hnsw/` index or the `.vec` sidecars, the backend rebuilds them in the
background.

## Configuration you actually care about

Full annotated reference: **`config.toml`** at the repo root. The knobs most
users touch:

```toml
[server]
addr = "127.0.0.1:8765"   # bind elsewhere only behind your own auth (SSH tunnel, mTLS proxy)

[memory]
dir = "./memory"          # where everything lives

[claude]
model = "claude-opus-4-7"                 # default for any role not overridden below
preprocessor_model = "claude-haiku-4-5"   # runs on every message; latency matters
assistant_model    = "claude-sonnet-4-6"  # chat; escalates itself when needed
assistant_escalation_model = "claude-opus-4-7"

[scout]
enabled = false           # the autonomous web scan; opt-in
interval_minutes = 10
pinned_topics = []        # empty → infer from your memory

[briefing]
enabled = true            # background "what's important" worker → the startup greeting
interval_minutes = 10
staleness_minutes = 30    # ignore a briefing older than this when you connect

[indexer]
enabled = true            # mechanical maintenance; leave on
```

There is no CLI for any of this — the backend's only flag is `--config`.
Runtime setup (like connecting Gmail) happens through conversation with the
assistant, not the config file.

## Troubleshooting

**"The assistant's reply came back empty."** Occasionally the model returns
nothing (often when it attempted a tool call without follow-up text). The
client shows a polite note; just ask again. Your message is still in memory.

**"It can't search my Gmail."** Gmail probably isn't connected yet (or the
credentials expired). Ask the assistant to *"set up gmail"* again — it will
check what's missing and walk you through it.

**"A query is taking a long time."** Open the status bar — if it shows a
worker searching (e.g. Gmail across many messages), it's doing real work. Each
fetched item is security-screened before it lands. The status rows update as it
progresses.

**"Did my message go through the security gate?"** Check the transcript. A
message that bypassed it carries a `☢ HAZMAT` banner. No banner = it went
through the Preprocessor.

**Logs.** The backend writes structured logs to stdout and to a daily-rotated
file under `<memory-dir>/logs/`. Raise verbosity for your own code with
`level = "info,backend=debug"` in `[logging]` (avoid a bare `debug`/`trace`,
which turns on noisy dependency logging — see the note in `config.toml`).
