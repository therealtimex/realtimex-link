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
mod clients;
mod clipboard_share;
// rtx-control's physical key table (data only), to check a batch's codes before it runs.
#[path = "../../control/src/codes.rs"]
#[allow(dead_code)]
mod codes;
mod daemon;
mod dib;
mod error;
mod file_browse;
mod files;
mod host;
mod identity;
mod install;
mod instance;
mod local_api;
mod lock;
mod owner;
mod password;
mod paths;
mod relay;
mod requests;
mod router;
mod secret_box;
mod shared;
mod signin;
mod store;
mod tailscale;
mod transfers;
mod update;
mod util;
mod webhooks;

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;

const EXIT_ALREADY_RUNNING: i32 = 3;

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn print_line(value: serde_json::Value) {
    println!("{value}");
}

#[tokio::main]
async fn main() {
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
    tokio::select! {
        () = wait_for_stop() => {}
        () = daemon.shutdown.notified() => {}
    }
    daemon.stop().await;
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
