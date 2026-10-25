//! rtx-linkd's local API (crates/linkd/src/local_api.rs), as the window uses it: found through
//! the data folder's linkd.json and api-token, started when it is not running. The port linkd.json
//! names must prove it holds the token (POST /v1/hello) before the token is sent to it: a daemon
//! that died leaves its port to whoever takes it next, another user's process too.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use crate::i18n::{t, tf};

const PROTOCOL: u64 = 1;
/// rtx-linkd's local_api::HELLO_CONTEXT.
const HELLO_CONTEXT: &str = "linkd-hello";
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
        let port = u16::try_from(port).map_err(|_| anyhow!("not running"))?;
        let base = format!("http://127.0.0.1:{port}");
        verify_daemon(&agent, &base, port, &token)?;
        let mut api = Self {
            base,
            token,
            agent,
            version: String::new(),
        };
        let version = api.get("/v1/version")?;
        if version.get("protocol").and_then(Value::as_u64) != Some(PROTOCOL) {
            bail!(t("errors.otherVersion"));
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
        let binary = daemon_binary().ok_or_else(|| anyhow!(t("errors.daemonMissing")))?;
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
        command
            .spawn()
            .with_context(|| t("errors.daemonStartFailed"))?;
        let started = Instant::now();
        loop {
            match Self::connect(&dir) {
                Ok(api) => return Ok(api),
                Err(error) if started.elapsed() > START_TIMEOUT => {
                    return Err(error.context(t("errors.daemonDidNotStart")))
                }
                Err(_) => std::thread::sleep(Duration::from_millis(150)),
            }
        }
    }

    fn answer(response: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<Value> {
        let mut response = response.map_err(|error| {
            anyhow!(tf(
                "errors.notAnswering",
                &[("error", error.to_string().as_str())]
            ))
        })?;
        let status = response.status();
        let body: Value = response.body_mut().read_json().unwrap_or(Value::Null);
        if !status.is_success() {
            let message = body
                .get("error")
                .and_then(Value::as_str)
                .map_or_else(|| t("errors.requestFailed"), str::to_string);
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

/// Ask the port to prove it is the daemon that wrote `token` (HMAC-SHA256 with the token over
/// "linkd-hello:<port>:<nonce>", local_api::hello_proof), with a fresh nonce, before anything is
/// sent with the token. An older daemon without /v1/hello can't prove it either: the window then
/// treats it as not running.
fn verify_daemon(agent: &ureq::Agent, base: &str, port: u16, token: &str) -> Result<()> {
    use ring::rand::SecureRandom;
    let mut nonce = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| anyhow!("no random numbers for the local API"))?;
    let nonce: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
    let answer = Api::answer(
        agent
            .post(format!("{base}/v1/hello"))
            .send_json(&serde_json::json!({ "nonce": nonce })),
    )
    .context("not running")?;
    let proof = answer
        .get("hmac")
        .and_then(Value::as_str)
        .and_then(unhex)
        .ok_or_else(|| anyhow!("not running"))?;
    proves(token, port, &nonce, &proof)
        .then_some(())
        .ok_or_else(|| anyhow!(t("errors.cannotProve")))
}

/// Constant-time check of a /v1/hello proof (ring::hmac::verify compares in constant time).
fn proves(token: &str, port: u16, nonce: &str, proof: &[u8]) -> bool {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, token.as_bytes());
    let message = format!("{HELLO_CONTEXT}:{port}:{nonce}");
    !token.is_empty() && ring::hmac::verify(&key, message.as_bytes(), proof).is_ok()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_tokens_holder_proves_itself() {
        // The same proof rtx-linkd's local_api tests pin.
        let proof =
            unhex("0951928c6c00bc5c4f3e9628f20424305372b20f170d80b13ee212f402181af6").unwrap();
        assert!(proves("token", 7000, "nonce-0123456789", &proof));
        assert!(!proves("other", 7000, "nonce-0123456789", &proof));
        assert!(!proves("token", 7001, "nonce-0123456789", &proof));
        assert!(!proves("token", 7000, "another-nonce-01", &proof));
        assert!(!proves("token", 7000, "nonce-0123456789", &proof[..31]));
        assert!(!proves("", 7000, "nonce-0123456789", &proof));
        assert!(unhex("abc").is_none());
        assert!(unhex("zz").is_none());
    }
}
