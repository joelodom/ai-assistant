# iOS Client Roadmap

**Status:** planning / draft. This document is a *plan only* — no code has been
written. It describes a strategy for building an iOS client and keeping it in
parallel with the existing desktop client, with a deliberately fast path to
"running on my iPhone."

Audience note: written assuming you are **new to iOS development**. Where a step
is iOS-specific plumbing rather than a design decision, it's called out so you
can follow it mechanically the first time.

---

## 1. The strategic decision

You floated two framework options. The recommendation is the one you leaned
toward, and it's the right call given the Windows-someday goal:

> **Keep desktop on egui (Mac + future Windows + Linux, one codebase). Build iOS
> as a native SwiftUI app. Let the `shared/` wire protocol be the contract that
> keeps them honest.**

Why not "egui everywhere, iOS included":

- egui *can* run on iOS (via `cargo-mobile2` + a winit/UIKit shim), but the
  mobile UX is rough — text input/IME, scroll physics, safe-area insets, the
  share sheet, file/photo pickers, and the OAuth flow all need native shims
  anyway. You'd inherit the worst of both worlds.
- You're new to iOS. **SwiftUI is the more beginner-friendly path on iOS**, not
  the harder one: Xcode Previews, the entire Apple tutorial corpus, and Stack
  Overflow are all SwiftUI-first. Fighting egui-on-iOS would mean learning iOS
  *and* an off-the-beaten-path toolchain at the same time.
- egui already gives you Windows + Linux nearly for free from the same desktop
  codebase. The "reuse" win you'd get from egui-on-iOS is mostly the *rendering*
  code (`app.rs`, `markdown.rs`, `theme.rs` — ~700 LOC), and that's exactly the
  part that benefits most from being native per-platform.

What actually gets shared is the thing worth sharing: **the protocol**. See §3.

### The architecture in one picture

```
                 ┌─────────────────────────────────────────┐
                 │              Backend (the hub)            │
                 │  Preprocessor · Assistant · Memory ·      │
                 │  Embedder · Workers — runs on a Mac/box   │
                 └──────────────────┬────────────────────────┘
                                    │  WebSocket, shared wire protocol
              ┌─────────────────────┼───────────────────────────┐
              │                     │                           │
      ┌───────┴────────┐    ┌───────┴────────┐         ┌────────┴────────┐
      │ Desktop client │    │  iOS client    │   ...   │ future Windows  │
      │ egui/eframe    │    │  SwiftUI       │         │ (same egui code)│
      │ (Mac/Win/Linux)│    │  (iPhone/iPad) │         │                 │
      └────────────────┘    └────────────────┘         └─────────────────┘
```

The backend **cannot run on iOS** (it spawns `claude` subprocesses, runs the
local bge-base-en-v1.5 embedder, and owns a filesystem memory store). So the iOS
client is, by nature, a **thin remote client**. That is a feature, not a
limitation: it keeps all security machinery server-side and means the iOS app
has a small, well-defined job.

---

## 2. Guiding principles (read once, refer back often)

1. **The protocol is the only thing two languages must agree on.** Everything
   else can diverge per platform. Keep `shared/src/lib.rs` the single source of
   truth and make drift *detectable* (§3).
2. **iOS is a thin client.** It sends `ClientMessage`, renders `ServerMessage`.
   That's the whole contract.
3. **Defer the hard connector setup to desktop.** The desktop client already
   owns the OAuth loopback dance (`net.rs::run_oauth_listener`). The iOS client
   does **not** need to set up Gmail/Drive connectors — those are configured
   once from desktop and the backend remembers them. This deletes the single
   hardest iOS porting problem from scope, possibly forever.
4. **The security diode still applies to iOS, unchanged.** An iOS
   `ClientMessage::Message` flows through the backend Preprocessor exactly like
   a desktop one. iOS gets no special bypass. HAZMAT remains an explicit,
   user-driven, session-scoped toggle (same semantics as desktop). No raw input
   is ever logged on device.
5. **Secrets live in the Keychain, nowhere else.** The device/session token from
   pairing (§Phase 2) goes in the iOS Keychain — never `UserDefaults`, never a
   plist, never a log line. This mirrors the backend's logging discipline.
6. **Follow the existing `config_protocol` pattern for any new sensitive input.**
   Per Invariant #8, the auth handshake should be a new `ConfigPayloadKind` /
   `config_protocol` handler — not a new bypass bolted onto the message pipeline.

---

## 3. The shared contract — how Swift and Rust stay in sync

The wire protocol (`shared/src/lib.rs`, ~240 LOC) is small, stable, and already
*forward-compatible by design* (`#[serde(default)]` everywhere, the
`bypass_sanitizer` alias). That's what makes a second client cheap to maintain.

**Phase 1 approach (recommended start): hand-mirrored Swift `Codable` structs.**
No Rust-for-iOS toolchain, no FFI, no build complexity. You write a single
`Protocol.swift` that mirrors the Rust enums/structs. For a beginner this is the
fastest path and the JSON is simple.

Key facts a Swift mirror must get right (these are the only sharp edges):

| Rust type | Tagging | Discriminator field | Variant naming |
|---|---|---|---|
| `ClientMessage` | internally tagged | `type` | snake_case (`message`, `ping`, `config_payload`) |
| `ServerMessage` | internally tagged | `type` | snake_case (`reply_chunk`, `reply_done`, `status`, …) |
| `ConfigPayloadKind` | internally tagged | `kind` | snake_case |
| `ConfigRequestKind` | internally tagged | `kind` | snake_case |
| `AttachmentKind`, `Tier` | plain enums | — | snake_case |

Swift doesn't decode internally-tagged enums for free, so each enum gets a small
custom `init(from:)` that switches on the discriminator. Illustrative shape:

```swift
enum ServerMessage: Decodable {
    case replyChunk(text: String)
    case replyDone(text: String?, meta: ReplyMeta?)
    case status(phase: String, detail: String?, slot: String?)
    case stubNotice(text: String)
    case error(text: String)
    case configRequest(ConfigRequestKind)
    case configStatus(connector: String, ok: Bool, message: String)
    case pong

    private enum K: String, CodingKey { case type }

    init(from d: Decoder) throws {
        let c = try d.container(keyedBy: K.self)
        switch try c.decode(String.self, forKey: .type) {
        case "reply_chunk": /* decode text */ ...
        case "status":      /* decode phase/detail/slot */ ...
        default:            /* forward-compat: ignore unknown, don't crash */ ...
        }
    }
}
```

**Make drift detectable — the testable bit.** Add a tiny `protocol-fixtures/`
directory of golden JSON samples (one per message variant). Then:

- A **Rust test** asserts each fixture round-trips through `serde` to the
  expected `ClientMessage`/`ServerMessage`.
- A **Swift test** decodes the *same* fixture files and asserts the same shape.

If someone changes the protocol and only updates one side, a fixture test fails.
This is the entire maintenance contract in ~30 lines of test per side. (Wire it
into CI in Phase 3.)

**Phase 3 upgrade path (optional, only if drift ever bites):** compile the
`shared` crate to an `XCFramework` via [UniFFI](https://mozilla.github.io/uniffi-rs/)
so Swift consumes the *actual* Rust types. More setup; single source of truth.
Don't reach for this on day one — the fixture tests cover you until it's worth
the toolchain cost.

> **Decision to make at Phase 1 kickoff:** hand-mirror + fixtures (recommended)
> vs. UniFFI from the start. The roadmap assumes hand-mirror; switching later is
> a contained change.

---

## 4. Transport: how the iPhone actually reaches the backend

Today the desktop client talks plaintext `ws://127.0.0.1:8765/ws` because it's
on the same machine. The iPhone is not, so you need a network path *and*
transport security. For a personal setup, the pragmatic answer:

> **Put the Mac and the iPhone on the same [Tailscale](https://tailscale.com)
> tailnet.** The backend becomes reachable at the Mac's MagicDNS name; all
> traffic is WireGuard-encrypted; no port-forwarding, no public exposure, no
> certificate wrangling. The app-level pairing token (Phase 2) is then
> *defense-in-depth* on top of an already-encrypted channel.

This is the lowest-friction way to get a real iPhone talking to your real
backend, which directly serves your "quick start to testing" goal. Alternatives
(reverse proxy + Let's Encrypt for a public `wss://`, or self-signed cert with
pinning) are heavier and can wait until/if you want off-tailnet access.

The backend currently binds `127.0.0.1`. To accept tailnet connections it must
bind the tailnet interface (or `0.0.0.0`, *only* when fronted by Tailscale's
access controls). **That's a backend change — noted here as a Phase 2
requirement, not done in this doc.** Treat binding beyond loopback as a security
decision: it must be gated behind the pairing token from day one.

---

## Phase 1 — UI mock on a mocked backend (get it on the iPhone fast)

**Goal:** a SwiftUI app you can run on your physical iPhone that looks and feels
like the real client, driven entirely by canned data. No network, no auth, no
backend. This de-risks "can I even ship to my device" and lets you build the
entire UI before any plumbing exists.

### Why mock-first
You learn Xcode, provisioning, on-device deploy, and SwiftUI layout without any
moving network parts. When Phase 2 adds real transport, the UI is already done
and you swap one object (the transport) behind a protocol boundary.

### The seam that makes this work

Define a transport protocol up front so the mock and the real client are
interchangeable:

```swift
protocol AssistantTransport {
    var inbound: AsyncStream<ServerMessage> { get }   // frames from "backend"
    func send(_ msg: ClientMessage) async
    func connect() async
}

final class MockTransport: AssistantTransport { /* canned ServerMessages */ }
final class WebSocketTransport: AssistantTransport { /* Phase 2+ */ }
```

`MockTransport` replays scripted frames: a `status` sequence (`preprocessing` →
`retrieving` → `thinking` → `replying`), then `reply_chunk` chunks, then
`reply_done`. This mirrors the real stream so the status bar and streaming
bubble logic are exercised for real. (The existing backend even has a
canned-response smoke-test mode you can later point `WebSocketTransport` at.)

### Build the UI to match the desktop surface
Reproduce the desktop client's surface (`client/src/app.rs`) natively:

- Scrollable transcript of message cards (user / assistant / stub / error /
  system), sender label + timestamp, "stick to bottom" on new content.
- Streaming assistant bubble that grows as `reply_chunk`s arrive.
- Live **status bar** driven by `status` frames (single-slot line, plus the
  multi-`slot` case for concurrent connector progress).
- Input bar: multiline text field + Send.
- The **HAZMAT** toggle and **Force Opus** toggle (wire the flags into the
  outgoing `ClientMessage::Message`; they do nothing against the mock but the UI
  is real).
- Markdown rendering of assistant replies. iOS has `AttributedString(markdown:)`
  built in — use it; you don't need to port `markdown.rs`.
- Settings sheet (UI scale is desktop-specific; on iOS use Dynamic Type instead
  — note it and move on).

### Beginner setup checklist (first sitting)
1. Install **Xcode** from the Mac App Store. Open it once to install components.
2. **File → New → Project → iOS → App**, SwiftUI lifecycle, Swift language.
3. Sign in with your Apple ID under *Settings → Accounts*. The **free** tier is
   enough for on-device testing — builds are signed with a *personal team* and
   run on your own device for **7 days** before needing a re-deploy. (A paid
   $99/yr account is only needed later for TestFlight/App Store — Phase 3.)
4. Plug in the iPhone, trust the Mac, select it as the run destination, press ▶.
   First run walks you through enabling Developer Mode on the phone.
5. Build the UI against `MockTransport`. Iterate with Xcode Previews for layout,
   deploy to the device when you want the real feel.

### Definition of done for Phase 1
- [ ] App installs and runs on your physical iPhone via free provisioning.
- [ ] Full transcript UI renders all five card types from mock data.
- [ ] Streaming + status-bar animation works off scripted `MockTransport` frames.
- [ ] `Protocol.swift` mirrors `shared/src/lib.rs`; golden-fixture decode test
      passes in Swift (and the matching Rust fixture test exists).
- [ ] HAZMAT / Force-Opus toggles set the right fields on the outgoing message
      (verified by encoding to JSON and checking the bytes).

**Effort:** ~1–1.5 weeks for someone new to iOS, most of it learning Xcode and
SwiftUI layout. No backend dependency, so it can proceed fully in parallel with
other work on the repo.

---

## Phase 2 — The auth framework (and a real connection)

**Goal:** the iOS app connects to the *real* backend over a secured channel,
authenticating with a credential it obtained by **scanning a QR code shown by
the desktop client**. Built test-first so you can trust it.

### Why auth is needed *now* (and wasn't before)
The moment the backend accepts a connection from anything other than localhost,
"whoever can reach the port can talk to your assistant and read your memory."
The pairing token is what makes the remote door require a key. (Tailscale from §4
already encrypts and network-gates the channel; the token is the second factor
and the thing that lets you *revoke* a lost phone.)

### Auth options considered

| Option | How it works | Pros | Cons | Verdict |
|---|---|---|---|---|
| **QR pairing** (your idea) | Desktop shows a QR; iOS scans it; gets a device token | No typing, no IdP, feels magical, easy to re-pair, revocable | Need a QR scanner + a pairing handshake in the backend | **Recommended** |
| Manual token paste | Backend prints a token; you type it into iOS | Trivial to build; good *fallback* | Long secrets are painful to type; error-prone | Build as the QR fallback |
| Client TLS cert (mTLS) | iOS holds a client certificate | Strong, stand;ard | Cert provisioning on iOS is fiddly; overkill for personal | Skip |
| OAuth/OIDC vs an IdP | Real identity provider | "Proper" auth | Massive overkill; you'd run/trust an IdP | Skip |

### Recommended design: QR pairing with a short-lived code → long-lived device token

Two-layer so a glance at the QR isn't a permanent compromise:

1. **Pairing code (short-lived, one-time).** From the desktop client you trigger
   "Pair a device." The backend mints a pairing code with a short TTL (e.g. 60 s)
   and the desktop client renders a QR encoding:
   ```json
   { "url": "wss://your-mac.tailnet.ts.net:8765/ws",
     "pairing_code": "<one-time, 60s TTL>",
     "v": 1 }
   ```
2. **Device token (long-lived, revocable).** The iOS app scans the QR, connects,
   and exchanges the pairing code for a **device token** which it stores in the
   **Keychain**. Every subsequent connection presents the device token (e.g. as
   a bearer credential on the WebSocket upgrade). The backend keeps a list of
   issued device tokens with labels ("Joel's iPhone") and can **revoke** any of
   them from desktop.

This maps cleanly onto the existing `config_protocol` pattern (Invariant #8):

- New `ConfigPayloadKind::DevicePairingRedeem { pairing_code }` → backend
  verifies the code, mints + returns a device token.
- New `ConfigRequestKind` / `ConfigStatus` plumbing as needed for the desktop
  "show pairing QR" step.
- The desktop side mints/show; the iOS side scans/redeems. **No new bypass** —
  it rides the config dispatcher, which already bypasses the Preprocessor for
  mechanical handshakes by design.

> **Backend work required in this phase (flagged, not done here):** mint/verify
> pairing codes; issue/verify/revoke device tokens; verify the token on the WS
> upgrade; bind beyond loopback (§4). This is the security-sensitive heart of the
> phase — give it the same care as the rest of the diode, and keep token values
> out of logs (lengths/IDs only, per the logging discipline).

### Making auth testable (the part you asked to be testable)

The auth logic is almost entirely *pure* and therefore unit-testable without a
device or a network:

**Backend (Rust) unit tests:**
- Pairing-code lifecycle: mint → redeem within TTL succeeds; redeem after TTL
  fails; redeem twice fails (one-time); unknown code fails.
- Device-token verify: valid token accepted; revoked token rejected; unknown
  token rejected; malformed rejected.
- WS upgrade gate: connection without a valid token is refused *before* any
  message processing.

**Swift unit tests:**
- QR payload codec: encode/decode the pairing JSON; reject malformed/`v`-mismatch
  payloads (forward-compat: refuse unknown major versions gracefully).
- Keychain layer behind a `TokenStore` protocol with a `MockTokenStore`, so
  storage logic is tested without touching the real Keychain.
- State machine: `unpaired → pairing → paired → revoked/expired` transitions.

**Integration test:**
- A test backend that *requires* a token; a Swift (or Rust) client that pairs via
  a freshly minted code, connects, sends one `ping`, and asserts `pong`. Run it
  against a backend bound to loopback so it's CI-friendly. This proves the whole
  handshake end-to-end without your phone.

**Contract:** add the pairing payload + token-presentation format to the
`protocol-fixtures/` golden set from §3, so both sides stay aligned.

### Definition of done for Phase 2
- [ ] Desktop client can display a pairing QR (triggered by the user).
- [ ] iOS app scans it (Camera + `AVFoundation` QR detection), redeems the code,
      stores the device token in the Keychain.
- [ ] `WebSocketTransport` connects to the real backend over Tailscale `wss://`,
      authenticating with the stored token; `MockTransport` still selectable for
      offline UI work.
- [ ] Backend refuses unauthenticated/revoked connections (covered by tests).
- [ ] Revoke-a-device works from desktop and the iPhone is locked out on next
      connect.
- [ ] All auth unit tests + the end-to-end pairing integration test pass.

**Effort:** ~1.5–2.5 weeks. The Swift QR/Keychain work is small; the backend
pairing/token/binding work and getting the security right is the bulk. Camera
usage needs an `NSCameraUsageDescription` Info.plist string (easy to forget).

---

## Phase 3 — Finish & be ready to maintain

By here you have a real, authenticated, streaming iOS client. What's left is
"make it a keeper." Several items may legitimately be **nothing** for your usage —
each is marked with whether it's likely needed.

### Feature parity (pull from desktop only if you want it on mobile)
- **Attachments** *(optional)* — desktop uses `rfd`; iOS uses `PHPickerViewController`
  (photos) and `UIDocumentPickerViewController` (files). The classify/base64
  logic from `read_and_classify`/`classify_extension` is portable as plain Swift.
  Skip if you don't attach from your phone.
- **Geolocation** *(optional)* — desktop fetches IP-geo; iOS can do better with
  `CoreLocation` (needs `NSLocationWhenInUseUsageDescription`). The metadata
  shape (`Geolocation { lat, lon, label }`) is already in the protocol. Skip if
  you don't want location-aware answers on mobile.
- **Connector OAuth setup on iOS** *(likely NOT needed — see Principle #3)* — the
  desktop loopback-listener flow doesn't translate to iOS. If you ever want it,
  it's `ASWebAuthenticationSession` + a custom URL scheme, and it's a real chunk
  of work. Recommendation: **leave connector setup a desktop-only job** and let
  iOS enjoy the connectors the backend already has. This is probably your
  "maybe nothing."

### Hardening & polish
- **Reconnect/backoff** matching `net.rs` (reconnect loop, "connecting…/connected/
  disconnected" states) and clean handling of backgrounding (iOS suspends sockets;
  reconnect on foreground).
- **Off-tailnet access** *(optional)* — if you want to use it on cellular without
  Tailscale, that's where a public `wss://` + proper TLS (Caddy/Let's Encrypt, or
  pinned self-signed) comes in. Tailscale-only is fine for most personal use.
- **Accessibility & Dynamic Type**, dark mode, app icon, launch screen.

### Distribution
- **TestFlight** for no-cable installs and not-re-signing-every-7-days requires
  the **paid Apple Developer Program ($99/yr)**. If you're the only user, free
  provisioning + occasional re-deploy may be enough — decide based on annoyance.

### The maintenance model (the actual long-term cost)
This is small *by design*, and the work above protects it:

1. **Protocol drift is the only real risk, and it's caught by tests.** Wire the
   §3 golden-fixture tests into CI on both Rust and Swift. A one-sided protocol
   change fails the build. This is the single most important maintenance
   investment — do it here if not earlier.
2. **Version field.** The pairing payload (and optionally a hello frame) carries
   a `v`. Bump it on breaking changes; old clients refuse gracefully rather than
   misbehave — consistent with the backend's forward-compatible-reads invariant.
3. **One changelog section for "wire protocol changes."** Any PR touching
   `shared/src/lib.rs` notes it; that's the trigger to update `Protocol.swift`
   and fixtures.
4. **Release cadence:** desktop and iOS release independently. Because the
   protocol is forward-compatible, a newer backend won't break an older iOS app —
   you are not forced into lockstep releases.

### Definition of done for Phase 3
- [ ] Reconnect/backgrounding behaves well on a real phone over a real day.
- [ ] Golden-fixture protocol tests run in CI on both languages.
- [ ] You've consciously decided yes/no on attachments, geolocation, off-tailnet
      access, and paid distribution — and either built or explicitly deferred each.
- [ ] Docs updated: a short "iOS client" section in `docs/USER_GUIDE.md` and the
      pairing flow noted where the OAuth flow is documented. (Per the repo's
      documentation-freshness rule — do this when the behavior actually ships,
      not before.)

**Effort:** highly variable — could be a few days (Tailscale-only, no
attachments, free provisioning) or 2–3 weeks (attachments + location + public
TLS + TestFlight). Most of it is *optional* and demand-driven.

---

## 5. Effort summary

| Phase | Outcome | Rough effort (new to iOS) | Backend changes? |
|---|---|---|---|
| 1 — UI mock | Real app on your iPhone, canned data, full UI | ~1–1.5 wks | None |
| 2 — Auth | Authenticated real connection via QR pairing | ~1.5–2.5 wks | Yes (pairing/token/bind) |
| 3 — Finish + maintain | Parity choices, hardening, CI contract tests | days–~3 wks | Maybe (revoke UI, optional) |

The phases are sequenced so you get the dopamine (app on phone) in Phase 1, the
substance (real, secure connection) in Phase 2, and the durability (tests +
parity decisions) in Phase 3.

---

## 6. Risks & mitigations

| Risk | Likelihood | Mitigation |
|---|---|---|
| Protocol drift between Rust and Swift | Medium | Golden-fixture tests both sides (§3); CI gate (Phase 3) |
| Binding the backend beyond loopback weakens the model | High if careless | Token-gate the WS upgrade *before* binding wider; Tailscale-only by default (§4) |
| QR token leakage (someone photographs the screen) | Low | Short-TTL one-time pairing code, not a raw long-lived token; revocation from desktop |
| iOS dev learning curve stalls momentum | Medium | Phase 1 is pure UI + mock — no network/auth to fight while learning Xcode |
| Free-provisioning 7-day expiry annoyance | Low | Re-deploy via cable, or buy the paid account once it bites (Phase 3) |
| Scope creep into connector OAuth on iOS | Medium | Principle #3: connector setup stays desktop-only unless proven necessary |

---

## 7. First-day checklist (when you start Phase 1)

1. Install Xcode; sign in with your Apple ID (free team).
2. New SwiftUI iOS app; run the empty template on your physical iPhone end-to-end
   (this validates the whole signing/deploy path before you've written anything).
3. Create `Protocol.swift` mirroring `shared/src/lib.rs`; add one golden JSON
   fixture and a decode test.
4. Define `AssistantTransport` + `MockTransport`.
5. Build the transcript list + one message card type. Iterate from there.

When in doubt, ask: *"does this belong on the iPhone, or can it stay a desktop
job?"* The answer is usually "desktop," and that keeps the iOS client small and
maintainable — which is the whole point.
