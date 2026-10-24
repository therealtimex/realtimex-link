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

/// The local API's token: made once, then kept, so RealTimeX and the window keep their access.
fn ensure_token(data_dir: &Path) -> Result<String> {
    let file = data_dir.join(TOKEN_FILE);
    let fresh: String = random_bytes::<32>()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    match files::create_new(&file, &fresh) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    files::make_private(&file)?;
    Ok(fs::read_to_string(&file)?.trim().to_string())
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
    crate::paths::remember(&options.data_dir);
    let store = Arc::new(Store::open(&options.data_dir)?);
    let secrets = SecretBox::new(&options.data_dir);
    let backend = options.backend.clone().unwrap_or_else(Backend::new);
    let host = Host::new(store, secrets, backend, options.port, options.log.clone());
    host.start().await;
    let token = ensure_token(&options.data_dir)?;
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let started_at = iso_now();
    let state = Arc::new(LocalState {
        token: token.clone(),
        version: options.version.clone(),
        started_at: started_at.clone(),
        host: host.clone(),
        shutdown: shutdown.clone(),
        window: Default::default(),
        show_window: Default::default(),
    });
    let socket = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
    let port = socket.local_addr()?.port();
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
