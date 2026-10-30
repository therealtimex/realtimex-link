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
// The login-window job's datagrams (realtimex-link#11), shared with rtx-link and rtx-linkd.
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod jobinput;
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
    /// Windows, under Link's service (realtimex-link#10): speak over these named pipes
    /// (`<name>-in`, `-out`, `-err`) instead of stdin/stdout/stderr. A helper started into
    /// another session can't inherit the service's handles.
    #[arg(long, value_name = "NAME")]
    pipes: Option<String>,
    /// macOS, under Link's supervisor (realtimex-link#11): the login-window job's input channel,
    /// inherited from the session bridge, for the requests the daemon marks `via:job`.
    #[arg(long, value_name = "FD")]
    input_fd: Option<i32>,
}

fn main() {
    let cli = Cli::parse();
    if let Some(name) = &cli.pipes {
        if let Err(error) = use_pipes(name) {
            eprintln!("error: the service's pipes: {error}");
            std::process::exit(1);
        }
    }
    #[cfg(target_os = "macos")]
    if let Some(fd) = cli.input_fd {
        platform::macos_job_channel(fd);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = cli.input_fd;
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

/// Make the service's pipes this process's standard handles: std reads and writes through
/// whatever GetStdHandle returns, so the control loop needs no change.
#[cfg(windows)]
fn use_pipes(name: &str) -> std::io::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::io::IntoRawHandle;
    #[link(name = "kernel32")]
    extern "system" {
        fn SetStdHandle(which: u32, handle: *mut c_void) -> i32;
    }
    const STD_INPUT: u32 = -10i32 as u32;
    const STD_OUTPUT: u32 = -11i32 as u32;
    const STD_ERROR: u32 = -12i32 as u32;
    let open = |suffix: &str, write: bool| {
        std::fs::OpenOptions::new()
            .read(!write)
            .write(write)
            .open(format!("{name}-{suffix}"))
    };
    for (which, suffix, write) in [
        (STD_INPUT, "in", false),
        (STD_OUTPUT, "out", true),
        (STD_ERROR, "err", true),
    ] {
        let handle = open(suffix, write)?.into_raw_handle();
        // SAFETY: an open handle this process keeps for its whole life.
        if unsafe { SetStdHandle(which, handle) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn use_pipes(_name: &str) -> std::io::Result<()> {
    Err(std::io::Error::other("only Link's Windows service uses pipes"))
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
