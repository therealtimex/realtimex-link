//! rtx-linkd's local API (crates/linkd/src/local_api.rs), as the window uses it: found through
//! the data folder's linkd.json and api-token, started when it is not running.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

const PROTOCOL: u64 = 1;
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// Where the daemon keeps its state: the same folder as rtx-linkd's paths.rs.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("RTX_LINK_DATA_DIR").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir);
    }
    let home =
        std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap_or_default();
    if cfg!(target_os = "macos") {
        PathBuf::from(home).join("Library/Application Support/RealTimeX Link")
    } else if cfg!(windows) {
        let roaming =
            std::env::var("APPDATA").unwrap_or_else(|_| format!("{home}\\AppData\\Roaming"));
        PathBuf::from(roaming).join("RealTimeX Link")
    } else {
        let config = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{home}/.config"));
        PathBuf::from(config).join("realtimex-link")
    }
}

/// rtx-linkd: RTX_LINKD_BIN, else the one installed next to this app.
fn daemon_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("RTX_LINKD_BIN").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let name = if cfg!(windows) {
        "rtx-linkd.exe"
    } else {
        "rtx-linkd"
    };
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join(name);
    candidate.is_file().then_some(candidate)
}

#[derive(Clone)]
pub struct Api {
    base: String,
    token: String,
    agent: ureq::Agent,
    pub version: String,
}

impl Api {
    fn connect(dir: &Path) -> Result<Self> {
        let discovery: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("linkd.json")).context("not running")?,
        )?;
        let token = std::fs::read_to_string(dir.join("api-token"))
            .context("not running")?
            .trim()
            .to_string();
        let port = discovery
            .get("port")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("not running"))?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(8)))
            .build()
            .into();
        let mut api = Self {
            base: format!("http://127.0.0.1:{port}"),
            token,
            agent,
            version: String::new(),
        };
        let version = api.get("/v1/version")?;
        if version.get("protocol").and_then(Value::as_u64) != Some(PROTOCOL) {
            bail!("RealTimeX Link's service is from another version; reinstall RealTimeX Link");
        }
        api.version = version
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        Ok(api)
    }

    /// The running daemon, started (on its own, so hosting outlives the window) when there is none.
    pub fn ensure() -> Result<Self> {
        let dir = data_dir();
        if let Ok(api) = Self::connect(&dir) {
            return Ok(api);
        }
        let binary = daemon_binary()
            .ok_or_else(|| anyhow!("rtx-linkd is not installed next to RealTimeX Link"))?;
        let mut command = Command::new(binary);
        command
            .arg("--data-dir")
            .arg(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Its own process group: closing the terminal or the window does not stop hosting.
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
            command.creation_flags(0x0000_0008 | 0x0000_0200 | 0x0800_0000);
        }
        command.spawn().context("could not start rtx-linkd")?;
        let started = Instant::now();
        loop {
            match Self::connect(&dir) {
                Ok(api) => return Ok(api),
                Err(error) if started.elapsed() > START_TIMEOUT => {
                    return Err(error.context("rtx-linkd did not start"))
                }
                Err(_) => std::thread::sleep(Duration::from_millis(150)),
            }
        }
    }

    fn answer(response: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<Value> {
        let mut response = response
            .map_err(|error| anyhow!("RealTimeX Link's service is not answering ({error})"))?;
        let status = response.status();
        let body: Value = response.body_mut().read_json().unwrap_or(Value::Null);
        if !status.is_success() {
            let message = body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Request failed");
            bail!("{message}");
        }
        Ok(body)
    }

    pub fn get(&self, path: &str) -> Result<Value> {
        Self::answer(
            self.agent
                .get(format!("{}{path}", self.base))
                .header("Authorization", format!("Bearer {}", self.token))
                .call(),
        )
    }

    pub fn post(&self, path: &str, body: Value) -> Result<Value> {
        Self::answer(
            self.agent
                .post(format!("{}{path}", self.base))
                .header("Authorization", format!("Bearer {}", self.token))
                .send_json(&body),
        )
    }
}
