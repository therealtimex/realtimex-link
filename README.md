# RealTimeX Link

RealTimeX Link is this computer's always-on companion for [RealTimeX](https://realtimex.ai).
Its first feature is **Remote control hosting**: it lets your other computers, and people you
choose, see and control this one, the way a VNC server does. RealTimeX itself is the app you
control other computers from.

- **`rtx-linkd`** (`crates/linkd`): the daemon. This computer's App ID, password and connected
  computers, connection requests, the listener other computers reach over Tailscale, and its own
  Public Exposure relay (`relay.rs`), so computers outside the tailnet reach it through
  `https://x.realtimex.ai/d/<App ID>`. It keeps running when the window is closed.
- **`rtx-link`** (`crates/link`): the window and the menu bar / tray icon. Closing the window keeps
  hosting; Quit stops it. `rtx-link --background` (at login, or when RealTimeX starts Link) shows
  only the icon. On Linux the icon is a process of its own and each opened window another, since a
  window hidden on Wayland cannot be shown again.
- **`rtx-control`** (`crates/control`): the desktop helper `rtx-linkd` starts, installed next to
  it: screenshots, the WebRTC live view, mouse and keyboard input and the clipboard, as JSON lines
  on stdin/stdout (`crates/control/src/control.rs`). `rtx-control --check` prints one JSON line,
  `{"tool":"rtx-control","version":"0.1.0","protocol":2,"platform":"macos","input":true}`, and
  exits 0 when this platform has input. It posts input but never reads the keyboard.
  **Physical keys** (0.2.4, realtimex-link#7, realtimex-ai-app#2909): a live view sends
  `{type:"key", code, down, repeat?}` with a `KeyboardEvent.code`, pressed by position (macOS key
  codes, Windows scancodes, evdev on X11 and Wayland), so this computer's layout and input source
  decide what it types; `info` lists the `physical-keys` feature. Codes Link has no key for are
  skipped. A view's held keys are released when it closes, loses control, sends a batch that fails
  or is refused, or sends nothing for 30 s; macOS and Windows post the viewer's repeats, X11 and
  Wayland repeat on their own. The HTTP `actions` endpoint takes `keys` combos only.
- **`rtx-screens`** (`crates/screens`): window and display screenshots, capture only. RealTimeX's
  recorder uses it too, pinned to a commit of this repo.
- **`rtx-glibc-compat`** (`crates/glibc-compat`): the glibc 2.34 floor on Linux, linked into each
  executable (`use rtx_glibc_compat as _;`).

The keyboard boundary (the recorder never reads the keyboard or posts input; `rtx-control` never
reads the keyboard) is checked by RealTimeX against this repo's sources at the pinned commit
(`native/rtx-capture/scripts/no-keyboard.json` there): a new process start, run-time code load,
build script or input API in `rtx-control` or `rtx-screens` needs review there before RealTimeX
moves its pin.

RealTimeX ships RealTimeX Link and manages it from Settings → Sharing → Remote control. Link
also works on its own, for a computer that should be controlled from RealTimeX elsewhere without
RealTimeX installed here: download it from
[Releases](https://github.com/therealtimex/realtimex-link/releases), sign in to your RealTimeX
account in its window and turn on **Allow remote control**.

| Platform | Download | Installs to |
| --- | --- | --- |
| macOS | `RealTimeX-Link-<version>-macos-<arch>.dmg` (signed, notarized): drag it to Applications | `/Applications/RealTimeX Link.app` |
| Windows | `RealTimeX-Link-Setup-<version>.exe` (signed) | `C:\Program Files\RealTimeX Link` |
| Linux | `realtimex-link_<version>_<arch>.deb` / `.rpm` | `/opt/RealTimeX Link` |

RealTimeX installs Link in the same places, so either one installs or updates it, never a
downgrade. Link also updates itself (`crates/linkd/src/update.rs`): it looks for a newer release at
start and every six hours, and the window and tray offer Update. On macOS it installs the signed
archive once codesign confirms the same Team ID, then reopens; on Windows it starts the signed
installer once Authenticode confirms the same publisher; on Linux it opens the release page. **Releases** (`.gitlab-ci.yml`): Link's code lives on
`rtgit.rta.vn/rtlab/rtwebteam/realtimex-link`, and this GitHub repository is its public push
mirror; send changes there. Each push runs the GitLab pipeline, which builds on GitHub Actions
here (`ci/github_build.py`). A `vX.Y.Z` tag (protected, and equal to Cargo.toml's version) builds
every platform into a draft release (unsigned archives, Linux packages). GitLab then checks that the
files come from the build run of the tag's own commit and takes them from that run's artifacts
(`ci/release.py fetch`), RealTimeX's release runners sign and notarize the macOS builds and make
the `.dmg` (`ci/sign-mac.sh`) and sign the Windows executables and installer (`ci/sign-win.ps1`),
and the draft is published, as Link's latest unless a newer release is out (`ci/release.py`).

- **Installing:** `rtx-linkd install DESTINATION` installs the Link it belongs to as its own app:
  `/Applications/RealTimeX Link.app`, `C:\Program Files\RealTimeX Link`, `/opt/RealTimeX Link`
  (with an applications-menu entry on Linux). It never downgrades, and stops an older Link first.
  RealTimeX runs it from its installers (Windows, Linux) or at start (macOS).
- **At login:** `GET/POST /v1/autostart` on the local API adds or removes a per-user login item
  (LaunchAgent, Run key, XDG autostart) that starts `rtx-link --background`. RealTimeX turns it on
  with its Remote Control plugin and off without it.
- **Account:** Link signs in to the RealTimeX account itself: Sign in opens the browser at
  accounts.realtimex.ai (Keycloak client `realtimex-link`, Authorization Code with PKCE, answered on
  a one-time `127.0.0.1` port), and Link keeps the offline refresh token sealed and renews its own
  access tokens (`GET /v1/account`, `POST /v1/account/sign-in`, `/sign-out`). That is for a
  computer with Link only. **When RealTimeX is installed, Link always uses RealTimeX's account**
  (realtimex-ai-app#2836 §6.2–6.3; see `account` below): its handover replaces Link's own sign-in,
  and Link's window shows the account read-only, with no Sign out or other account.
  `RTX_LINK_AUTH_ISSUER` points at another realm for development.
- **Public Exposure** (realtimex-ai-app#2836): Link is this computer's only connector to
  RealTimeX's relay (`relay.rs`). It enrolls every surface: published artifacts (`a`), the embed
  widget (`e`), webhooks (`h`), the mobile app (`m`), desktop messaging (`p`) and Remote Control
  (`d`, served in-process). It runs while **Keep RealTimeX Link running** is on, whatever "Allow
  remote control" says, and stays up while RealTimeX is closed. Frames to the relay, request
  bodies and WebSockets wait in bounded queues, and each surface has a request body limit
  (below).

### Local API for RealTimeX's Public Exposure

The contract agreed with RealTimeX (realtimex-ai-app#2836, `loop-briefs/issue-2836-design.md` §6).
All of it is on the local API (127.0.0.1, the data folder's bearer token, `POST /v1/hello` first)
with JSON bodies; each block is a flag in `GET /v1/version`'s `features`. Error codes are
kebab-case.

- **`account`** (the owner RealTimeX, `owner.rs`):
  - `POST /v1/account/token { token, apiBase?, installId }`: the first RealTimeX install to hand
    over with its `installId` owns Link; its handover always replaces Link's own sign-in. Another
    install gets 409 `adopted-other` and changes nothing. Without an `installId` (an older
    RealTimeX) the handover is ignored once there is an owner; with none it signs Link in only
    while Link has no sign-in of its own.
  - When the handed-over account isn't the one the current exposure belongs to (Link keeps that
    apart from the sign-in, so a sign-out in between doesn't hide a switch), Link drops that
    exposure and everything shared on it: its credential, install ID and adoption, the artifact,
    webhook and Shared lists and the origin. It keeps the exposure's ID as a former one, and the
    window says once that the previous account's links are offline. That account's queued webhook
    deliveries stay, readable by it alone. The exposure itself stays its owner's; Link retires
    nothing without its token.
  - `POST /v1/account/sign-out { installId }` (the owner only): Link is signed out and its relay
    stops until the next handover. Without an owner it is Link's own sign-out.
  - `POST /v1/account/release { installId }` (RealTimeX's uninstallers): Link manages its own
    account again. Link also stops counting RealTimeX as present by itself, checked at its start,
    when its window opens (`GET /v1/account?check=1`) and before a sign-out: present while a
    handover came in the last 24 h; otherwise not when RealTimeX isn't installed, or after 30 days
    without a handover. `GET /v1/account` says `managedByRealtimex`.
- **`running`:** `GET /v1/running` → `{ on }`; `POST /v1/running { on }`. On by default. Off stops
  the relay and remote control and removes the login item; the local API keeps answering, so
  RealTimeX can offer Turn on. Turning it on clears an opt-out made through `POST /v1/autostart`.
  RealTimeX turns it on only when the person asks.
- **`exposure`:**
  - `GET /v1/exposure` → `{ state, appId, installId, account, surfaces: { <surface>:
    publicBaseUrl }, formerAppIds, error? }`. `state`: `off`, `signed-out`, `connecting`, `online`
    or `error` (then `error: { code, message }`: `relay-error`, `recovery-capped`, or
    `waiting-for-realtimex` when enrolling or recovering needs a token only RealTimeX hands over). `online` means connected
    with every surface enrolled; until then Link keeps enrolling in the background. `p` is enrolled
    only where the Marketplace allows it.
  - `POST /v1/exposure/adopt { exposureId?, installId, account }` (the owner install; `account`:
    the Keycloak subject of its handover) → `{ appId, formerAppIds }`. With `exposureId` Link
    recovers that exposure with the account's token (the credential rotates); without it Link
    creates one under RealTimeX's install ID (a retry replays the same creation). Either way it is
    enrolled with every surface and the exposure's app ID, kept, and connected; RealTimeX counts the
    adoption done once `GET /v1/exposure` is `online` with that `appId`. Link's previous App ID
    joins `formerAppIds`, and its own exposure is retired once the adopted one has been online.
    Adopting another exposure replaces the adopted one and what was shared on it. An exposure of
    another account is never reused. 409 `signed-out` (no handover of that account yet) or
    `adopted-other` (another install); `adopt-failed` or `exposure-not-found` when the Marketplace
    refuses, and then nothing changed in Link. Once recovered or created the exposure is Link's: if
    enrolling it fails then, the answer is 503 `adopt-pending` and Link keeps trying. Adopting the
    same exposure again gives the same answer. Link recovers an exposure by itself at most 3 times
    an hour; past that `GET /v1/exposure` is `error` with `recovery-capped` (another app, such as
    an older RealTimeX, keeps taking it) instead of a fight.
- **`origin`:** `PUT /v1/origin { exposureId, targets: { a, e, h, m, p: "http://127.0.0.1:<port>" },
  token }`, `DELETE /v1/origin`, `GET /v1/origin` → `{ registered, targets }`. Link passes each
  surface's requests to its target, HTTP and WebSockets (not `p`, GET and POST only), with the
  headers RealTimeX's own connector set: `x-forwarded-host`, `x-forwarded-prefix`
  (`/<surface>/<id>`), `x-forwarded-proto: https`, `x-forwarded-for` (the relay's client address),
  `x-realtimex-relay-surface`, `x-realtimex-relay-source: public-relay`, and
  `x-realtimex-link-origin: <token>` (RealTimeX accepts relayed requests only with it). The client's own `x-forwarded-*` and `x-realtimex-*` headers
  are dropped, except the phone's own on `m` (0.2.5, realtimex-link#8): `x-realtimex-mobile-device-token`,
  `x-realtimex-device-signature`, `-timestamp`, `-nonce` and `x-realtimex-operator-capabilities`. Paths follow the enrollment's `pathAllowlist`, else relayConnector.cjs's defaults
  (`p` never leaves `/api/peer/v1`). The origin is kept in memory: RealTimeX sends it at each start
  and when Link's `startedAt` changes. With no target, or when nothing answers on it (it is then
  forgotten), the answer is 503 `{ error: "desktop offline", code: "desktop-offline" }` with
  `Retry-After: 30`, or a page for browsers.
- **`shared`:** `PUT /v1/shared { exposureId, items: [{ kind, count, names? }] }` (kinds:
  `artifacts`, `webhooks`, `widget`, `mobile`, `messaging`; `names`: the widget's sites, the
  messaging desktops) replaces the window's read-only "Shared from this computer" card; a kind left
  out hides its row, and Link writes the text. `GET /v1/shared` → `{ items, origin }`. Kept across
  restarts. Up to 1 MiB.
- **`artifacts`** (M2): `PUT /v1/artifacts { exposureId, items: [{ id, dir, entry, status,
  expiresAt? }], embedAssetsDir?, restrictedRoots? }` (up to 1 MiB). Link serves surface `a`'s `/ar/<id>/…` from
  those folders itself, whether RealTimeX runs or not, as RealTimeX's route answered (the entry
  file; `index.html` for a folder; `Cache-Control: no-store`, `X-Robots-Tag: noindex`; types by
  extension; ETag, Last-Modified, 304 and one Range; paused, revoked, expired or missing: 404
  `{"error":"Artifact not found."}`), stricter on decoding, dot segments, symlinks out of the
  folder and `nosniff`. Surface `e`'s `/embed/<file>` comes from `embedAssetsDir`.
  `restrictedRoots?` (0.2.3, realtimex-ai-app#2838; at most 1000 absolute paths) are the folders
  nothing may be served from, kept with the list so they hold while RealTimeX is closed. Each
  request opens its file once and checks the path of that open file: inside the artifact's folder
  as it is now, outside every restricted folder (as pushed, canonical, or its own path now), and
  the folder itself neither is, holds nor lies in one; the body, a range and a 304 come from that
  file only. A push that changes the list ends the bodies still streaming from the last one.
- **`webhooks`** (M3): `PUT /v1/webhooks { exposureId, items: [{ tokens, enabled, maxBodyBytes,
  perMinute, ackStatus, handshake, verifyTokenSha256? }] }` (up to 1 MiB). While RealTimeX isn't
  there (no origin, or one that refuses), `/wh/<token>` deliveries are checked for token, enabled,
  size and rate, kept byte-exact for 7 days (per endpoint at most 1000 or 100 MiB) and answered
  with `ackStatus`; Meta's hub challenge and Pancake's handshake are answered and kept too.
  `GET /v1/inbox?after=&limit=` (arrival order) → `{ items: [{ id, endpoint, receivedAt, method, path, headers,
  body (base64) }], total }`; `POST /v1/inbox/ack { exposureId, ids }` → `{ acked }`. Only the
  current account's deliveries are listed or acknowledged.
- `PUT /v1/origin`, `/v1/shared`, `/v1/artifacts`, `/v1/webhooks` and `POST /v1/inbox/ack` name the
  adopted exposure; any other is 409 `exposure-mismatch`.
- **Public request bodies** (413 beyond): `a` and `e` 1 MiB, `h` 15 MiB, `m` 50 MiB, `p` 10 MiB.

## Build

Rust is pinned in `rust-toolchain.toml` (rustup installs it).

```
scripts/build.sh [arm64|x86_64] dist          # macOS (RealTimeX Link.app), Linux
scripts\build.ps1 [x86_64|arm64] dist         # Windows (Visual Studio C++ tools)
cargo test
scripts/test-macos-control.py path/to/rtx-control   # live macOS check of rtx-control
```

`test-macos-control.py` drives the protocol, then posts input into its own test window only
(`test-control-window.swift`, after a minute without anyone using the Mac, and only if
Accessibility is already granted), including that held keys and buttons are released when a
session ends.

`rtx-linkd [--data-dir DIR] [--port 7420]` prints one JSON line when ready and stops on SIGTERM
(or `POST /v1/shutdown` on its local API). Its data folder is per user: Application
Support/RealTimeX Link (macOS), `%APPDATA%\RealTimeX Link` (Windows), `~/.config/realtimex-link`
(Linux), or `RTX_LINK_DATA_DIR`.

## License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
