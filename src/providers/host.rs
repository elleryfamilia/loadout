//! `host` provider — machine identity, reusing detected [`SystemContext`].
//!
//! No new process execution: it surfaces what context detection already found
//! (os/arch/hostname/user, and the derived host class).

use super::{EnvProvider, ProviderOutput};
use crate::context::Context;

/// Surfaces the current machine's identity.
pub struct HostProvider;

impl EnvProvider for HostProvider {
    fn id(&self) -> &'static str {
        "host"
    }

    fn probe(&self, ctx: &Context) -> crate::Result<Option<ProviderOutput>> {
        let s = &ctx.system;
        let host = if s.hostname.is_empty() {
            "(unknown host)"
        } else {
            &s.hostname
        };
        let user = if s.user.is_empty() {
            "(unknown)"
        } else {
            &s.user
        };
        let mut text = format!("{host} — {}/{}, user {user}", s.os, s.arch);
        if let Some(class) = &s.host_class {
            text.push_str(&format!(" (host class: {class})"));
        }
        let data = serde_json::json!({
            "hostname": s.hostname,
            "os": s.os,
            "arch": s.arch,
            "user": s.user,
            "host_class": s.host_class,
        });
        Ok(Some(ProviderOutput {
            text,
            data,
            redacted: 0,
        }))
    }
}
