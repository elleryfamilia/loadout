//! `ai-tools` provider — installed AI coding-agent CLIs and their versions.

use super::{EnvProvider, ProviderOutput};
use crate::context::Context;

/// Agent CLIs to probe via `<tool> --version`.
const TOOLS: &[&str] = &[
    "claude",
    "codex",
    "gemini",
    "cursor-agent",
    "opencode",
    "aider",
];

/// Surfaces installed agent CLIs.
pub struct AiToolsProvider;

impl EnvProvider for AiToolsProvider {
    fn id(&self) -> &'static str {
        "ai-tools"
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
            text: format!("agent CLIs: {text}"),
            data,
            redacted: 0,
        }))
    }
}
