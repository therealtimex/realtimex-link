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

RealTimeX ships RealTimeX Link and manages it from Settings → Sharing → Remote control. It can
also be installed on its own from [Releases](https://github.com/therealtimex/realtimex-link/releases).

## Build

Rust is pinned in `rust-toolchain.toml` (rustup installs it).

```
scripts/build.sh [arm64|x86_64] dist          # macOS, Linux
scripts\build.ps1 [x86_64|arm64] dist         # Windows (Visual Studio C++ tools)
cargo test
```

`rtx-linkd [--data-dir DIR] [--port 7420]` prints one JSON line when ready and stops on SIGTERM
(or `POST /v1/shutdown` on its local API). Its data folder is per user: Application
Support/RealTimeX Link (macOS), `%APPDATA%\RealTimeX Link` (Windows), `~/.config/realtimex-link`
(Linux), or `RTX_LINK_DATA_DIR`.

## License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
