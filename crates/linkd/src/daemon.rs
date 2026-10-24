//! Start the hosting daemon: one per data folder, with its store, secrets, hosting and local API,
//! as RealTimeX expects it (server/utils/remoteHost/client.js). The discovery file is written last, once the API
//! listens, so a reader that finds it can connect.

use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use serde_json::json;
use tokio::sync::oneshot;

use crate::backend::Backend;
use crate::files;
use crate::host::Host;
use crate::local_api::{self, LocalState, PROTOCOL};
use crate::lock::InstanceLock;
use crate::secret_box::SecretBox;
use crate::store::Store;
use crate::util::{iso_now, random_bytes};

pub const TOKEN_FILE: &str = "api-token";
pub const DISCOVERY_FILE: &str = "linkd.json";

/// The local API's token, new on every start: one that leaked (or that a process listening on
/// a dead daemon's port was handed) stops working at the next start. RealTimeX and the window read
/// the file again each time they connect. Written whole (a temporary file renamed over it), so a
/// reader never sees an empty or half-written token.
fn rotate_token(data_dir: &Path) -> Result<String> {
    let token: String = random_bytes::<32>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    files::write_atomically(&data_dir.join(TOKEN_FILE), &token)?;
    Ok(token)
}

/// The data folder and the secret key stay this user's only (0700 and 0600) on every start, also
/// where an older version or a copy left them more open.
fn make_data_private(data_dir: &Path) -> Result<()> {
    files::ensure_private_dir(data_dir)?;
    let key = data_dir.join(crate::secret_box::KEY_FILE);
    if key.exists() {
        files::make_private(&key)?;
    }
    Ok(())
}

pub struct Options {
    pub data_dir: PathBuf,
    pub version: String,
    pub port: u16,
    pub log: Arc<dyn Fn(&str) + Send + Sync>,
    /// The desktop helper; tests pass one without rtx-control.
    pub backend: Option<Arc<Backend>>,
}

pub struct Daemon {
    pub port: u16,
    /// Notified when the local API is asked to stop the daemon.
    pub shutdown: Arc<tokio::sync::Notify>,
    pub host: Arc<Host>,
    discovery: PathBuf,
    stop_api: Option<oneshot::Sender<()>>,
    api: tokio::task::JoinHandle<()>,
    lock: InstanceLock,
}

pub async fn start(options: Options) -> Result<Daemon> {
    let lock = InstanceLock::acquire(&options.data_dir)?;
    make_data_private(&options.data_dir)?;
    crate::paths::remember(&options.data_dir);
    let store = Arc::new(Store::open(&options.data_dir)?);
    let secrets = SecretBox::new(&options.data_dir);
    let backend = options.backend.clone().unwrap_or_else(Backend::new);
    let host = Host::new(store, secrets, backend, options.port, options.log.clone());
    host.start().await;
    // Bound before the state is made: /v1/hello's proof names the port.
    let socket = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
    let port = socket.local_addr()?.port();
    let token = rotate_token(&options.data_dir)?;
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let started_at = iso_now();
    let state = Arc::new(LocalState {
        token,
        port,
        version: options.version.clone(),
        started_at: started_at.clone(),
        host: host.clone(),
        shutdown: shutdown.clone(),
        window: Default::default(),
        show_window: Default::default(),
    });
    let (stop_api, stopped) = oneshot::channel::<()>();
    let routes = local_api::router(state);
    let api = tokio::spawn(async move {
        let _ = axum::serve(socket, routes)
            .with_graceful_shutdown(async move {
                let _ = stopped.await;
            })
            .await;
    });
    let discovery = options.data_dir.join(DISCOVERY_FILE);
    files::write_atomically(
        &discovery,
        &json!({ "pid": std::process::id(), "port": port, "protocol": PROTOCOL, "version": options.version, "startedAt": started_at }).to_string(),
    )?;
    Ok(Daemon {
        port,
        shutdown,
        host,
        discovery,
        stop_api: Some(stop_api),
        api,
        lock,
    })
}

impl Daemon {
    pub async fn stop(mut self) {
        let ours = fs::read_to_string(&self.discovery)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|d| d.get("pid").and_then(serde_json::Value::as_u64))
            == Some(u64::from(std::process::id()));
        if ours {
            let _ = fs::remove_file(&self.discovery);
        }
        if let Some(stop) = self.stop_api.take() {
            let _ = stop.send(());
        }
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), &mut self.api).await;
        self.host.stop().await;
        self.lock.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_is_new_on_every_start_and_written_whole() {
        let dir = tempfile::tempdir().unwrap();
        // An empty file (a crash while it was written by an older version) is simply replaced.
        fs::write(dir.path().join(TOKEN_FILE), "").unwrap();
        let first = rotate_token(dir.path()).unwrap();
        assert_eq!(first.len(), 64);
        assert_eq!(fs::read_to_string(dir.path().join(TOKEN_FILE)).unwrap(), first);
        let second = rotate_token(dir.path()).unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read_to_string(dir.path().join(TOKEN_FILE)).unwrap(), second);
        // No temporary file is left behind.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir.path().join(TOKEN_FILE)), 0o600);
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
            let key = dir.path().join(crate::secret_box::KEY_FILE);
            fs::write(&key, "x").unwrap();
            fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
            make_data_private(dir.path()).unwrap();
            assert_eq!(mode(dir.path()), 0o700);
            assert_eq!(mode(&key), 0o600);
        }
    }
}
