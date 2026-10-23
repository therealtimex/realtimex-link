# RealTimeX Link

RealTimeX Link is this computer's always-on companion for [RealTimeX](https://realtimex.ai).
Its first feature is **Remote control hosting**: it lets your other computers, and people you
choose, see and control this one, the way a VNC server does. RealTimeX itself is the app you
control other computers from.

- **`rtx-linkd`** (`crates/linkd`): the daemon. This computer's App ID, password and connected
  computers, connection requests, and the listener other computers reach over Tailscale. It keeps
  running when the window is closed.
- **`rtx-link`** (`crates/link`): the window and the menu bar / tray icon. Closing the window keeps
  hosting; Quit stops it.
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

RealTimeX ships RealTimeX Link and manages it from Settings → Sharing → Remote control. It can
also be installed on its own from [Releases](https://github.com/therealtimex/realtimex-link/releases).

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
