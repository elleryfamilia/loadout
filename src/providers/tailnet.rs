//! `tailnet` provider — Tailscale peers, parsed from `tailscale status`.
//!
//! Local-only: tailnet IPs/hostnames are machine-specific and never leave the
//! gitignored overlay/cache. The parser is pure for testing.
//!
//! The CLI is located via `PATH` and a set of known install locations — the
//! macOS App Store/standalone app keeps its CLI inside the bundle and never adds
//! it to `PATH`, so a `PATH`-only lookup would miss it.

use super::{EnvProvider, ProviderOutput};
use crate::context::Context;

/// Known `tailscale` CLI locations that aren't on `PATH` by default. The macOS
/// App Store / standalone app ships the CLI inside the bundle and never adds it
/// to `PATH`; Homebrew's paths are included for shells that don't inherit it.
const TAILSCALE_FALLBACKS: &[&str] = &[
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    "/opt/homebrew/bin/tailscale",
    "/usr/local/bin/tailscale",
];

/// `tailscale` invocations to try, in order: the bare name (resolved via `PATH`)
/// first, then any known fallback location that `exists`. Pure for testing — the
/// caller supplies the existence check.
fn tailscale_candidates(exists: impl Fn(&str) -> bool) -> Vec<String> {
    let mut candidates = vec!["tailscale".to_string()];
    candidates.extend(
        TAILSCALE_FALLBACKS
            .iter()
            .filter(|p| exists(p))
            .map(|p| p.to_string()),
    );
    candidates
}

/// Whether `path` is an executable regular file (unix). loadout is unix-targeted,
/// so the executable bit is the right gate (mirrors agent-env's `[ -x ]`).
fn is_executable_file(path: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Run `tailscale status`, locating the CLI via `PATH` then the known fallbacks.
/// Returns the first candidate that yields output (`None` if none are installed
/// or all are logged out).
fn tailscale_status() -> Option<String> {
    tailscale_candidates(is_executable_file)
        .into_iter()
        .find_map(|prog| super::run_ok(&prog, &["status"]))
}

/// A peer on the tailnet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailnetPeer {
    /// Tailscale IP (100.x.y.z).
    pub ip: String,
    /// Peer hostname.
    pub name: String,
    /// Operating system as reported by tailscale.
    pub os: String,
    /// Whether the peer is online (not reported `offline`).
    pub online: bool,
}

/// Surfaces tailnet peers.
pub struct TailnetProvider;

impl EnvProvider for TailnetProvider {
    fn id(&self) -> &'static str {
        "tailnet"
    }

    fn probe(&self, _ctx: &Context) -> crate::Result<Option<ProviderOutput>> {
        let Some(raw) = tailscale_status() else {
            return Ok(None); // not installed anywhere known, or logged out
        };
        let peers = parse_tailscale(&raw);
        if peers.is_empty() {
            return Ok(None);
        }
        let online = peers.iter().filter(|p| p.online).count();
        let lines: Vec<String> = peers
            .iter()
            .map(|p| {
                format!(
                    "- {} ({}) {} — {}",
                    p.name,
                    p.ip,
                    p.os,
                    if p.online { "online" } else { "offline" }
                )
            })
            .collect();
        let text = format!(
            "{} tailnet peer(s), {online} online:\n{}",
            peers.len(),
            lines.join("\n")
        );
        let data = serde_json::Value::Array(
            peers
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "name": p.name, "ip": p.ip, "os": p.os, "online": p.online,
                    })
                })
                .collect(),
        );
        Ok(Some(ProviderOutput {
            text,
            data,
            redacted: 0,
        }))
    }
}

/// Parse `tailscale status` text output into peers.
///
/// Lines look like: `100.x.y.z  hostname  owner@  os  active; relay "…"` (the
/// self line and idle peers show `-` as status). We treat a peer as online
/// unless its status column contains `offline`.
pub fn parse_tailscale(s: &str) -> Vec<TailnetPeer> {
    let mut peers = Vec::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 4 {
            continue;
        }
        // First field must look like an IP (starts with a digit).
        if !f[0].chars().next().is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }
        let status = f[4..].join(" ").to_lowercase();
        peers.push(TailnetPeer {
            ip: f[0].to_string(),
            name: f[1].to_string(),
            os: f[3].to_string(),
            online: !status.contains("offline"),
        });
    }
    peers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_self_and_peers_with_online_state() {
        let raw = "\
100.64.0.1   my-laptop    me@   macOS   -
100.64.0.2   build-box    me@   linux   active; relay \"sea\", tx 1 rx 2
100.64.0.3   old-pi       me@   linux   offline
# a comment line is ignored
garbage line without ip
";
        let peers = parse_tailscale(raw);
        assert_eq!(peers.len(), 3);
        assert_eq!(peers[0].name, "my-laptop");
        assert!(peers[0].online); // "-" status → online
        assert_eq!(peers[1].ip, "100.64.0.2");
        assert_eq!(peers[1].os, "linux");
        assert!(peers[1].online); // "active" → online
        assert!(!peers[2].online); // "offline" → offline
    }

    #[test]
    fn empty_or_loggedout_yields_no_peers() {
        assert!(parse_tailscale("").is_empty());
        assert!(parse_tailscale("Logged out.").is_empty());
    }

    #[test]
    fn candidates_try_path_first_then_existing_fallbacks() {
        // Nothing on disk → only the bare PATH name is tried.
        assert_eq!(
            tailscale_candidates(|_| false),
            vec!["tailscale".to_string()]
        );

        // macOS app present → appended after the PATH name (PATH still first).
        assert_eq!(
            tailscale_candidates(|p| p.contains("Tailscale.app")),
            vec![
                "tailscale".to_string(),
                "/Applications/Tailscale.app/Contents/MacOS/Tailscale".to_string(),
            ]
        );

        // All fallbacks present → PATH name first, then each in declared order.
        let all = tailscale_candidates(|_| true);
        assert_eq!(all.len(), TAILSCALE_FALLBACKS.len() + 1);
        assert_eq!(all[0], "tailscale");
        assert_eq!(&all[1..], TAILSCALE_FALLBACKS);
    }
}
