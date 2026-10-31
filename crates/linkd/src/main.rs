//! `rtx-linkd`: RealTimeX Link's hosting daemon (#2753).
//! `rtx-linkd [--data-dir <dir>] [--port <base port>]`. Prints one JSON line when ready ({ ready, pid, port, version }) or when it can't start
//! ({ error, code }), and stops cleanly on SIGTERM or SIGINT. Exit code 3: another daemon
//! already runs for this data folder (callers then use that one).

// The glibc 2.34 floor on Linux (no code to call).
use rtx_glibc_compat as _;

mod actions;
mod artifacts;
mod autostart;
mod backend;
#[cfg(target_os = "macos")]
mod bridges;
mod clients;
mod clipboard_share;
// rtx-control's physical key table (data only), to check a batch's codes before it runs.
#[path = "../../control/src/codes.rs"]
#[allow(dead_code)]
mod codes;
mod daemon;
mod dib;
mod displays;
mod error;
mod file_browse;
mod files;
mod helper;
mod host;
mod identity;
mod install;
mod instance;
#[cfg(any(target_os = "macos", test))]
mod launchd;
mod local_api;
mod lock;
mod owner;
mod password;
mod paths;
mod relay;
mod requests;
mod router;
mod secret_box;
mod secure_attention;
mod service;
mod shared;
mod signin;
mod store;
mod tailscale;
mod transfers;
mod unattended;
mod update;
mod util;
mod webhooks;

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;

const EXIT_ALREADY_RUNNING: i32 = 3;
/// Link's service runs this folder's daemon (realtimex-link#10): callers use that one.
const EXIT_SERVICE_RUNS_IT: i32 = 4;

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn print_line(value: serde_json::Value) {
    println!("{value}");
}

fn main() {
    // Link's Windows service (service.rs): the Service Control Manager starts it, and it makes
    // its own runtime once the service is registered.
    // The installer, elevated, before it writes in Link's ProgramData folder: the same check
    // the service makes at every start (service.rs). Exit 1: nothing may be written there.
    #[cfg(windows)]
    if std::env::args().nth(1).as_deref() == Some("secure-machine-dir") {
        let safe = service::prepare_machine_dir(&unattended::machine_dir());
        print_line(json!({ "secured": safe }));
        std::process::exit(if safe { 0 } else { 1 });
    }
    if std::env::args().skip(1).any(|arg| arg == "--service") {
        let _ = rustls::crypto::ring::default_provider().install_default();
        service::run();
        return;
    }
    // macOS unattended access (realtimex-link#11): launchd's roles, started by `rtx-link
    // --launchd <role>`, and the root steps (the administrator's prompt, or sudo).
    #[cfg(target_os = "macos")]
    if let Some(role) = std::env::args().nth(1).filter(|arg| {
        arg.starts_with("launchd-") || arg == "unattended-apply" || arg == "unattended-remove"
    }) {
        let rest: Vec<String> = std::env::args().skip(2).collect();
        launchd::run(&role, &rest);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("the daemon's runtime")
        .block_on(run());
}

async fn run() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // TLS for the relay (relay.rs) with one crypto provider, chosen here.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let version = env!("CARGO_PKG_VERSION").to_string();
    if args.iter().any(|arg| arg == "--version") {
        print_line(
            json!({ "tool": "rtx-linkd", "version": version, "protocol": local_api::PROTOCOL }),
        );
        return;
    }
    // `install --check DESTINATION`: exit 0 when installing would replace what is there (nothing
    // or older), 11 when this version is installed, 10 when a newer one is. Nothing changes.
    if args.first().map(String::as_str) == Some("install") && args.get(1).map(String::as_str) == Some("--check") {
        let Some(destination) = args.get(2) else {
            print_line(json!({ "error": "usage: rtx-linkd install --check DESTINATION" }));
            std::process::exit(2);
        };
        match install::would_replace(std::path::Path::new(destination)) {
            Ok(installed) => {
                let (replace, code) = match installed {
                    install::Installed::Older => (true, 0),
                    install::Installed::Same => (false, 11),
                    install::Installed::Newer => (false, 10),
                };
                print_line(json!({ "replace": replace, "installed": format!("{installed:?}").to_lowercase() }));
                std::process::exit(code);
            }
            Err(error) => {
                print_line(json!({ "error": format!("{error:#}") }));
                std::process::exit(1);
            }
        }
    }
    if args.first().map(String::as_str) == Some("install") {
        let Some(destination) = args.get(1) else {
            print_line(json!({ "error": "usage: rtx-linkd install DESTINATION" }));
            std::process::exit(2);
        };
        match install::run(std::path::Path::new(destination)) {
            Ok(result) => print_line(result),
            Err(error) => {
                print_line(json!({ "error": format!("{error:#}") }));
                std::process::exit(1);
            }
        }
        return;
    }
    let data_dir =
        arg_value(&args, "--data-dir").map_or_else(paths::default_data_dir, PathBuf::from);
    let port = arg_value(&args, "--port")
        .and_then(|p| p.parse().ok())
        .unwrap_or(host::DEFAULT_PORT);
    // macOS (#11 M3): the owner's daemon, run as the owner by Link's root supervisor, which
    // hands it the session bridges on this fd. The folder is checked here, after the drop to the
    // owner (C5): the standard one, not a link, with its store; else idle (C6).
    #[cfg(target_os = "macos")]
    let supervised = arg_value(&args, "--supervised-fd").and_then(|fd| fd.parse::<i32>().ok());
    #[cfg(not(target_os = "macos"))]
    let supervised: Option<i32> = None;
    #[cfg(target_os = "macos")]
    if let Some(fd) = supervised {
        let usable = data_dir == paths::default_data_dir()
            && std::fs::symlink_metadata(&data_dir).is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink())
            && data_dir.join("remote-host.db").is_file();
        if !usable {
            print_line(json!({
                "error": "the owner's RealTimeX Link folder isn't available",
                "code": unattended::PROFILE_UNAVAILABLE,
            }));
            std::process::exit(1);
        }
        service::set_supervised();
        bridges::start(fd);
    }
    // While Link's service runs this folder's daemon, a second one must not take its lock
    // (SD C4): the window and RealTimeX attach to the service's through linkd.json.
    if supervised.is_none() && unattended::supported() {
        let request = unattended::Request::Status {
            data_dir: data_dir.clone(),
        };
        if unattended::yields(unattended::ask(&request).ok().flatten().as_ref()) {
            print_line(json!({
                "error": "RealTimeX Link's service runs this folder's daemon",
                "code": "service-runs-it",
            }));
            std::process::exit(EXIT_SERVICE_RUNS_IT);
        }
    }
    let log: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(|line: &str| eprintln!("{line}"));
    let started = daemon::start(daemon::Options {
        data_dir,
        version: version.clone(),
        port,
        label: arg_value(&args, "--label"),
        log,
        backend: None,
    })
    .await;
    let daemon = match started {
        Ok(daemon) => daemon,
        Err(error) => {
            let running = error
                .downcast_ref::<lock::LockError>()
                .and_then(|e| match e {
                    lock::LockError::AlreadyRunning(pid) => Some(*pid),
                    lock::LockError::Io(_) => None,
                });
            let code = if running.is_some() {
                json!("already-running")
            } else if error.downcast_ref::<store::StoreTooNew>().is_some() {
                json!("store-too-new")
            } else {
                serde_json::Value::Null
            };
            print_line(json!({ "error": error.to_string(), "code": code }));
            std::process::exit(if running.is_some() {
                EXIT_ALREADY_RUNNING
            } else {
                1
            });
        }
    };
    print_line(
        json!({ "ready": true, "pid": std::process::id(), "port": daemon.port, "version": version }),
    );
    if supervised.is_none() {
        hand_over_to_the_service(&daemon, data_dir_of(&args));
    }
    tokio::select! {
        () = wait_for_stop() => {}
        () = daemon.shutdown.notified() => {}
    }
    daemon.stop().await;
}

fn data_dir_of(args: &[String]) -> PathBuf {
    arg_value(args, "--data-dir").map_or_else(paths::default_data_dir, PathBuf::from)
}

/// While Start with this computer is on, the owner's own daemon is only a stand-in: one started
/// while the service was stopped steps aside once the service is back and waiting for its folder
/// (QA of !12; SD C4), so the sign-in and lock screens are reachable again.
fn hand_over_to_the_service(daemon: &daemon::Daemon, data_dir: PathBuf) {
    if !unattended::supported() {
        return;
    }
    let shutdown = daemon.shutdown.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            let request = unattended::Request::Status {
                data_dir: data_dir.clone(),
            };
            let reply = tokio::task::spawn_blocking(move || unattended::ask(&request))
                .await
                .ok()
                .and_then(Result::ok)
                .flatten();
            if unattended::yields(reply.as_ref()) {
                eprintln!("[unattended] Link's service runs this folder again: stepping aside");
                shutdown.notify_one();
                return;
            }
        }
    });
}

#[cfg(unix)]
async fn wait_for_stop() {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut interrupt)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = interrupt.recv() => {}
    }
}

/// Windows: RealTimeX and the window stop the daemon with POST /v1/shutdown. At logoff Windows
/// ends it without any signal, so nothing here cleans up; the instance lock is the OS's and goes
/// with the process (lock.rs), so the next login's daemon starts.
#[cfg(not(unix))]
async fn wait_for_stop() {
    let _ = tokio::signal::ctrl_c().await;
}
