//! This machine on its tailnet: its Tailscale IPv4 and
//! MagicDNS name, and who owns another tailnet address.

use std::net::Ipv4Addr;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;

const BINARIES: &[&str] = &[
    "tailscale",
    "/usr/bin/tailscale",
    "/usr/local/bin/tailscale",
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    "C:\\Program Files\\Tailscale\\tailscale.exe",
];
const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq)]
pub struct Tailnet {
    pub address: String,
    pub dns_name: Option<String>,
}

/// Tailscale hands out IPv4 addresses from the CGNAT range 100.64.0.0/10.
pub fn is_tailscale_ipv4(address: &str) -> bool {
    address.parse::<Ipv4Addr>().is_ok_and(|ip| {
        let [a, b, ..] = ip.octets();
        a == 100 && (64..=127).contains(&b)
    })
}

async fn run(binary: &str, args: &[&str]) -> Option<String> {
    // util::tokio_command: no console window flashing up every minute on Windows.
    let mut command = crate::util::tokio_command(binary);
    command
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The address from `tailscale status --json`, or None when it is not running there.
pub fn parse_status(output: &str) -> Option<Option<Tailnet>> {
    let status: Value = serde_json::from_str(output).ok()?;
    let me = status.get("Self");
    let address = me
        .and_then(|s| s.get("TailscaleIPs"))
        .and_then(Value::as_array)
        .and_then(|ips| {
            ips.iter()
                .filter_map(Value::as_str)
                .find(|ip| is_tailscale_ipv4(ip))
        });
    if status.get("BackendState").and_then(Value::as_str) != Some("Running") || address.is_none() {
        return Some(None);
    }
    let dns_name = me
        .and_then(|s| s.get("DNSName"))
        .and_then(Value::as_str)
        .map(|name| name.trim_end_matches('.').to_string())
        .filter(|name| !name.is_empty());
    Some(Some(Tailnet {
        address: address.unwrap_or_default().to_string(),
        dns_name,
    }))
}

/// The CLI is asked first because on macOS the address does not show among the interfaces.
///
/// Without the CLI, only an interface Tailscale itself made counts: an address in 100.64.0.0/10
/// alone is not enough, since mobile carriers and ISPs hand out the same range (CGNAT), and their
/// other customers must not reach this computer's sign-in.
pub async fn detect() -> Option<Tailnet> {
    for binary in BINARIES {
        let Some(output) = run(
            binary,
            &["status", "--json", "--self=true", "--peers=false"],
        )
        .await
        else {
            continue;
        };
        if let Some(found) = parse_status(&output) {
            return found;
        }
    }
    if_addrs::get_if_addrs()
        .ok()?
        .into_iter()
        .find_map(|interface| {
            let ip = interface.ip().to_string();
            (!interface.is_loopback()
                && is_tailscale_ipv4(&ip)
                && is_tailscale_interface(std::env::consts::OS, &interface.name))
            .then_some(Tailnet {
                address: ip,
                dns_name: None,
            })
        })
}

/// An interface Tailscale makes: `tailscale0` (and other `tailscale*`) on Linux, the adapter
/// named "Tailscale" on Windows (if-addrs gives its friendly name). None on macOS, where the
/// address isn't on an interface of its own name: there the CLI decides, or Link doesn't listen.
pub fn is_tailscale_interface(os: &str, name: &str) -> bool {
    match os {
        "linux" => name.starts_with("tailscale"),
        "windows" => name == "Tailscale",
        _ => false,
    }
}

/// The Tailscale login of whoever owns `address` (e.g. "linh@example.com"), or None.
pub async fn login_of(address: &str) -> Option<String> {
    if !is_tailscale_ipv4(address) {
        return None;
    }
    for binary in BINARIES {
        let Some(output) = run(binary, &["whois", "--json", address]).await else {
            continue;
        };
        let Ok(whois) = serde_json::from_str::<Value>(&output) else {
            continue;
        };
        let login = whois
            .pointer("/UserProfile/LoginName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        return (!login.is_empty() && login != "tagged-devices").then(|| login.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_cli_status() {
        assert!(is_tailscale_ipv4("100.73.10.20"));
        assert!(!is_tailscale_ipv4("100.128.0.1"));
        assert!(!is_tailscale_ipv4("fd7a::1"));
        let running = r#"{"BackendState":"Running","Self":{"TailscaleIPs":["fd7a:115c::1","100.64.0.5"],"DNSName":"mac.tail.ts.net."}}"#;
        assert_eq!(
            parse_status(running),
            Some(Some(Tailnet {
                address: "100.64.0.5".into(),
                dns_name: Some("mac.tail.ts.net".into())
            }))
        );
        assert_eq!(
            parse_status(r#"{"BackendState":"Stopped","Self":{}}"#),
            Some(None)
        );
        assert_eq!(parse_status("not json"), None);
    }

    #[test]
    fn only_tailscales_own_interface_without_the_cli() {
        assert!(is_tailscale_interface("linux", "tailscale0"));
        assert!(!is_tailscale_interface("linux", "wwan0"));
        assert!(!is_tailscale_interface("linux", "eth0"));
        assert!(is_tailscale_interface("windows", "Tailscale"));
        assert!(!is_tailscale_interface("windows", "Cellular"));
        assert!(!is_tailscale_interface("windows", "Ethernet 2"));
        assert!(!is_tailscale_interface("macos", "utun4"));
        assert!(!is_tailscale_interface("macos", "tailscale0"));
    }
}
