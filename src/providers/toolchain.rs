//! `toolchain` provider — installed developer CLIs and their versions.
//!
//! Probes a known list via `<tool> --version` and collects the ones present.

use super::{EnvProvider, ProviderOutput};
use crate::context::Context;

/// Tools worth knowing about for a coding agent, in display order.
const TOOLS: &[&str] = &[
    "git", "cargo", "rustc", "node", "pnpm", "npm", "deno", "bun", "python3", "uv", "poetry", "go",
    "rg", "fd", "gh", "docker",
];

/// Surfaces installed developer toolchains.
pub struct ToolchainProvider;

impl EnvProvider for ToolchainProvider {
    fn id(&self) -> &'static str {
        "toolchain"
    }

    fn probe(&self, _ctx: &Context) -> crate::Result<Option<ProviderOutput>> {
        let found = super::probe_versions(TOOLS);
        if found.is_empty() {
            return Ok(None);
        }
        let text = found
            .iter()
            .map(|(t, v)| format!("{t} {v}"))
            .collect::<Vec<_>>()
            .join(", ");
        let data = serde_json::Value::Object(
            found
                .into_iter()
                .map(|(t, v)| (t, serde_json::Value::String(v)))
                .collect(),
        );
        Ok(Some(ProviderOutput {
            text: format!("installed: {text}"),
            data,
            redacted: 0,
        }))
    }
}
