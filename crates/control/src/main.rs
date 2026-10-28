//! rtx-control: RealTimeX Remote Control's desktop side (#2611), a separate executable from the
//! recorder since #2665. It takes screenshots, streams the live view and posts mouse and keyboard
//! input for an agent on another computer, as JSON lines on stdin/stdout (see `control.rs`).
//! `rtx-capture` contains none of this and never starts it.

// The glibc 2.34 floor on Linux (no code to call).
use rtx_glibc_compat as _;

// Each platform reads its own column of the key table.
#[allow(dead_code)]
mod codes;
mod control;
#[cfg(all(windows, target_env = "msvc"))]
mod crt_shim;
// Linux posts keysyms as they are (XTest, Mutter); macOS and Windows map them to key codes.
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
mod keys;
mod peer;
mod platform;

use clap::Parser;

/// The JSON-lines protocol `control.rs` speaks. The host checks it before spawning, so an
/// executable from another release says "incompatible" instead of misreading requests. 2: the
/// physical `key` op (realtimex-link#7).
const PROTOCOL: u32 = 2;
/// The exit code when screen capture stalled for good ([`control::CaptureStalled`]). rtx-linkd
/// knows it (`CONTROL_EXIT_CAPTURE_STALLED` in its backend.rs) and starts a new rtx-control with
/// the next request. 75 is EX_TEMPFAIL: try again later.
const EXIT_CAPTURE_STALLED: i32 = 75;

#[derive(Parser)]
#[command(
    name = "rtx-control",
    version,
    about = "RealTimeX Remote Control: screenshots, live view and input as JSON lines on stdin/stdout."
)]
struct Cli {
    /// Only check: print what this build supports as one JSON line, exit 0 when it has mouse
    /// and keyboard input on this platform.
    #[arg(long)]
    check: bool,
}

fn main() {
    let cli = Cli::parse();
    if cli.check {
        println!("{}", check_report());
        std::process::exit(if platform::INPUT_AVAILABLE { 0 } else { 1 });
    }
    if let Err(e) = control::run() {
        eprintln!("error: {e:#}");
        let code = if e.is::<control::CaptureStalled>() {
            EXIT_CAPTURE_STALLED
        } else {
            1
        };
        std::process::exit(code);
    }
}

fn check_report() -> serde_json::Value {
    serde_json::json!({
        "tool": "rtx-control",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": PROTOCOL,
        "platform": platform::PLATFORM,
        "input": platform::INPUT_AVAILABLE,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_names_the_tool_and_protocol() {
        let report = check_report();
        assert_eq!(report["tool"], "rtx-control");
        assert_eq!(report["protocol"], PROTOCOL);
        assert_eq!(report["input"], platform::INPUT_AVAILABLE);
    }
}
