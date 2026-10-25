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
  `{"tool":"rtx-control","version":"0.1.0","protocol":1,"platform":"macos","input":true}`, and
  exits 0 when this platform has input. It posts input but never reads the keyboard.
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
  access tokens (`GET /v1/account`, `POST /v1/account/sign-in`, `/sign-out`). RealTimeX's handover
  (`POST /v1/account/token`) is the quick way in: it signs Link in only while Link has no sign-in of
  its own, and after Sign out it is only offered (`POST /v1/account/use-realtimex`).
  `RTX_LINK_AUTH_ISSUER` points at another realm for development.
- **Public Exposure:** Link enrolls its own exposure with the account's access token (only Remote Control's route),
  and its App ID becomes that exposure's ID. Remote Control always uses Link's own App ID;
  RealTimeX keeps its Public Exposure ID for its other features, and hands Link only the
  signed-in account. `/hello` lists the App ID Link made for itself before its exposure
  (`formerAppIds`), and RealTimeX moves a computer saved under it to the new one. The tunnel
  runs on Link's own credential, so it stays up while RealTimeX is closed.

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
