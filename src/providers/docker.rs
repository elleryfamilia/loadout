//! `docker` provider — running containers, parsed from `docker ps`.

use super::{EnvProvider, ProviderOutput};
use crate::context::Context;

/// A running container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerContainer {
    /// Container name.
    pub name: String,
    /// Image reference.
    pub image: String,
    /// Status string (e.g. `Up 3 hours`).
    pub status: String,
}

/// Surfaces running containers.
pub struct DockerProvider;

impl EnvProvider for DockerProvider {
    fn id(&self) -> &'static str {
        "docker"
    }

    fn probe(&self, _ctx: &Context) -> crate::Result<Option<ProviderOutput>> {
        // Tab-delimited so the parser is unambiguous regardless of names/images.
        let Some(raw) = super::run_ok(
            "docker",
            &["ps", "--format", "{{.Names}}\t{{.Image}}\t{{.Status}}"],
        ) else {
            return Ok(None); // not installed, or daemon down (non-zero exit)
        };
        let containers = parse_docker_ps(&raw);
        if containers.is_empty() {
            return Ok(None); // daemon up but nothing running
        }
        let lines: Vec<String> = containers
            .iter()
            .map(|c| format!("- {} ({}) — {}", c.name, c.image, c.status))
            .collect();
        let text = format!(
            "{} running container(s):\n{}",
            containers.len(),
            lines.join("\n")
        );
        let data = serde_json::Value::Array(
            containers
                .iter()
                .map(|c| serde_json::json!({"name": c.name, "image": c.image, "status": c.status}))
                .collect(),
        );
        Ok(Some(ProviderOutput {
            text,
            data,
            redacted: 0,
        }))
    }
}

/// Parse tab-delimited `docker ps` output (`Names\tImage\tStatus`).
pub fn parse_docker_ps(s: &str) -> Vec<DockerContainer> {
    s.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let mut parts = line.splitn(3, '\t');
            let name = parts.next()?.trim().to_string();
            if name.is_empty() {
                return None;
            }
            Some(DockerContainer {
                name,
                image: parts.next().unwrap_or("").trim().to_string(),
                status: parts.next().unwrap_or("").trim().to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tab_delimited_rows() {
        let raw = "web\tnginx:latest\tUp 3 hours\ndb\tpostgres:16\tUp 2 days (healthy)\n";
        let cs = parse_docker_ps(raw);
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].name, "web");
        assert_eq!(cs[0].image, "nginx:latest");
        assert_eq!(cs[0].status, "Up 3 hours");
        assert_eq!(cs[1].name, "db");
        assert_eq!(cs[1].status, "Up 2 days (healthy)");
    }

    #[test]
    fn empty_output_yields_nothing() {
        assert!(parse_docker_ps("").is_empty());
        assert!(parse_docker_ps("\n  \n").is_empty());
    }
}
