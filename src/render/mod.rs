//! Template rendering: context + composed capabilities + template → an overlay.
//!
//! The low-level [`TemplateRenderer`] trait abstracts the engine (here
//! minijinja). [`render`] is the high-level entry the adapters call: it resolves
//! the base template, renders each composed capability into the body, prepends
//! the generated header, and returns the content plus the context hash and
//! provenance.

pub mod header;

use std::path::Path;

use minijinja::{Environment, UndefinedBehavior, Value};
use serde::Serialize;

use crate::capability::Capability;
use crate::config::{self, Config};
use crate::context::Context;
use crate::profile::Composition;
use crate::templates;

/// Abstraction over a template engine.
pub trait TemplateRenderer {
    /// Render `source` against `model`, returning the output string.
    fn render_str(&self, source: &str, model: &Value) -> crate::Result<String>;
}

/// minijinja-backed renderer with lenient undefined handling (so optional
/// context fields render as empty rather than erroring).
pub struct MinijinjaRenderer {
    env: Environment<'static>,
}

impl Default for MinijinjaRenderer {
    fn default() -> Self {
        let mut env = Environment::new();
        env.set_undefined_behavior(UndefinedBehavior::Lenient);
        MinijinjaRenderer { env }
    }
}

impl TemplateRenderer for MinijinjaRenderer {
    fn render_str(&self, source: &str, model: &Value) -> crate::Result<String> {
        self.env
            .render_str(source, model)
            .map_err(|e| anyhow::anyhow!("template render error: {e:#}"))
    }
}

/// Inputs for a render.
pub struct RenderRequest<'a> {
    /// Agent id shown in the header (`claude`/`codex`/`generic`).
    pub agent: &'a str,
    /// Base template name (`claude`/`agents`/`generic`).
    pub template_name: &'a str,
    /// Detected context.
    pub context: &'a Context,
    /// Composed capabilities + matching profiles.
    pub composition: &'a Composition,
    /// Loaded config (template overrides, source provenance).
    pub config: &'a Config,
    /// Injected generation timestamp (RFC3339) — passed in for testability.
    pub generated_at: String,
}

/// Result of a render.
pub struct RenderOutput {
    /// Header + body, ready to write.
    pub content: String,
    /// `sha256:…` of the context that produced it.
    pub context_hash: String,
    /// Where the base template came from.
    pub template_source: String,
    /// Concatenated capability guidance (the `profile_guidance` body; may be
    /// empty, e.g. when every capability is restricted to other agents).
    pub profile_guidance: String,
}

/// The serializable model exposed to the base overlay template.
#[derive(Serialize)]
struct RenderModel<'a> {
    agent: &'a str,
    profile: &'a str,
    profile_guidance: &'a str,
    context: &'a Context,
}

/// The serializable model exposed to each capability's guidance template.
#[derive(Serialize)]
struct CapabilityModel<'a> {
    agent: &'a str,
    /// The profile that pulled this capability in.
    profile: &'a str,
    context: &'a Context,
    capability: &'a Capability,
    /// Convenience alias for `capability.params`.
    params: &'a toml::Value,
}

/// Render an overlay for `req`.
pub fn render(req: &RenderRequest) -> crate::Result<RenderOutput> {
    let renderer = MinijinjaRenderer::default();
    let profile_label = req.composition.label();

    // 1. Resolve the base template. The primary (highest-priority) matching
    //    profile may override the template name.
    let template_override = req
        .composition
        .primary_profile()
        .and_then(|name| req.config.profiles.iter().find(|p| p.name == name))
        .and_then(|p| p.template.as_deref());
    let template_name = template_override.unwrap_or(req.template_name);
    let base = templates::resolve(&req.context.repo_base, template_name)?;

    // 2. Render the composed capabilities into the guidance body.
    let profile_guidance = render_capabilities(&renderer, req.context, req.composition, req.agent)?;

    // 3. Context hash.
    let context_hash = req.context.compute_hash();

    // 4. Header.
    let sources: Vec<String> = req
        .config
        .sources
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    let header = header::build(&header::HeaderMeta {
        generated_at: &req.generated_at,
        host: &req.context.system.hostname,
        agent: req.agent,
        profile: profile_label,
        context_hash: &context_hash,
        template_source: &base.source,
        sources: &sources,
    });

    // 5. Body.
    let model = RenderModel {
        agent: req.agent,
        profile: profile_label,
        profile_guidance: &profile_guidance,
        context: req.context,
    };
    let model_value = Value::from_serialize(&model);
    let body = renderer.render_str(&base.content, &model_value)?;

    Ok(RenderOutput {
        content: format!("{header}{body}"),
        context_hash,
        template_source: base.source,
        profile_guidance,
    })
}

/// Render each composed capability (in order) into a single guidance body.
///
/// Capabilities restricted to other agents are skipped (the active agent varies
/// per render). Each capability becomes a `### <title>` section, annotated with
/// its risk when not `Info`. A synthetic `<profile>:inline` capability can still
/// be overridden by a `profiles/<name>.md.j2` template file (repo, then global).
fn render_capabilities(
    renderer: &MinijinjaRenderer,
    ctx: &Context,
    composition: &Composition,
    agent: &str,
) -> crate::Result<String> {
    let mut sections: Vec<String> = Vec::new();

    for rc in &composition.capabilities {
        let cap = &rc.capability;
        if !cap.applies_to_agent(agent) {
            continue;
        }

        // Guidance source: an inline capability may be overridden by a
        // `profiles/<name>.md.j2` file; otherwise the capability's own text.
        let template_src = if rc.inline {
            read_profile_template(&ctx.repo_base, &rc.via_profile)
                .unwrap_or_else(|| cap.guidance.clone())
        } else {
            cap.guidance.clone()
        };
        if template_src.trim().is_empty() {
            continue;
        }

        let model = CapabilityModel {
            agent,
            profile: &rc.via_profile,
            context: ctx,
            capability: cap,
            params: &cap.params,
        };
        let rendered = renderer
            .render_str(&template_src, &Value::from_serialize(&model))?
            .trim()
            .to_string();
        if rendered.is_empty() {
            continue;
        }

        // Inline capabilities are titled by their profile (their description is
        // synthetic); named capabilities use their title.
        let title = if rc.inline {
            rc.via_profile.clone()
        } else {
            cap.title().to_string()
        };
        let heading = match cap.risk.annotation() {
            Some(ann) => format!("### {title} — {ann}"),
            None => format!("### {title}"),
        };
        sections.push(format!("{heading}\n\n{rendered}"));
    }

    Ok(sections.join("\n\n"))
}

fn read_profile_template(repo_base: &Path, profile: &str) -> Option<String> {
    let file = format!("profiles/{profile}.md.j2");
    let repo = config::repo_templates_dir(repo_base).join(&file);
    if let Ok(s) = std::fs::read_to_string(&repo) {
        return Some(s);
    }
    if let Some(global) = config::global_templates_dir() {
        if let Ok(s) = std::fs::read_to_string(global.join(&file)) {
            return Some(s);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{Capability, Risk};
    use crate::context::test_support::sample_context;
    use crate::profile::ResolvedCapability;

    fn named_cap(id: &str, guidance: &str) -> Capability {
        Capability {
            id: id.into(),
            description: Some(id.into()),
            tags: vec![],
            risk: Risk::Info,
            when: vec![],
            requires: vec![],
            params: toml::Value::Table(Default::default()),
            guidance: guidance.into(),
            agents: vec![],
        }
    }

    fn resolved(cap: Capability, via: &str, inline: bool) -> ResolvedCapability {
        ResolvedCapability {
            capability: cap,
            via_profile: via.into(),
            reason: "test".into(),
            inline,
        }
    }

    fn composition(profile: &str, caps: Vec<ResolvedCapability>) -> Composition {
        Composition {
            profiles: vec![profile.into()],
            capabilities: caps,
            reasons: vec![],
        }
    }

    #[test]
    fn renders_header_and_body() {
        let mut ctx = sample_context();
        ctx.stacks = vec!["rust".into()];
        ctx.languages = vec!["Rust".into()];
        ctx.package_managers = vec!["cargo".into()];
        ctx.commands.test = vec!["cargo test".into()];
        let cfg = Config::defaults();
        let comp = composition(
            "rust",
            vec![resolved(
                named_cap(
                    "rust-conventions",
                    "Use cargo for **{{ context.stacks | join(\",\") }}**.",
                ),
                "rust",
                false,
            )],
        );

        let out = render(&RenderRequest {
            agent: "claude",
            template_name: "claude",
            context: &ctx,
            composition: &comp,
            config: &cfg,
            generated_at: "2026-05-29T00:00:00Z".into(),
        })
        .unwrap();

        assert!(out.content.starts_with(header::GENERATED_MARKER));
        assert!(out.content.contains("profile   : rust"));
        assert!(out.content.contains("Stack:** rust"));
        assert!(out.content.contains("`cargo test`"));
        // The capability appears under its own heading...
        assert!(out.content.contains("### rust-conventions"));
        // ...with its guidance template rendered against the context.
        assert!(out.content.contains("Use cargo for **rust**."));
        assert!(out.context_hash.starts_with("sha256:"));
    }

    #[test]
    fn concatenates_capabilities_in_order_with_risk_annotation() {
        let ctx = sample_context();
        let cfg = Config::defaults();
        let mut risky = named_cap("infra-caution", "Be careful.");
        risky.risk = Risk::Caution;
        let comp = composition(
            "infra",
            vec![
                resolved(risky, "infra", false),
                resolved(named_cap("baseline", "Keep it minimal."), "default", false),
            ],
        );
        let out = render(&RenderRequest {
            agent: "claude",
            template_name: "claude",
            context: &ctx,
            composition: &comp,
            config: &cfg,
            generated_at: "2026-05-29T00:00:00Z".into(),
        })
        .unwrap();

        // Risk is annotated on the caution capability only.
        assert!(out.content.contains("### infra-caution — ⚠️ caution"));
        assert!(out.content.contains("### baseline"));
        // Order is preserved: infra before baseline.
        assert!(out.content.find("infra-caution").unwrap() < out.content.find("baseline").unwrap());
    }

    #[test]
    fn agent_restricted_capability_is_skipped() {
        let ctx = sample_context();
        let cfg = Config::defaults();
        let mut only_codex = named_cap("codex-only", "Codex specifics.");
        only_codex.agents = vec!["codex".into()];
        let comp = composition("default", vec![resolved(only_codex, "default", false)]);

        let out = render(&RenderRequest {
            agent: "claude",
            template_name: "claude",
            context: &ctx,
            composition: &comp,
            config: &cfg,
            generated_at: "2026-05-29T00:00:00Z".into(),
        })
        .unwrap();
        // Restricted to codex → absent from a claude render's guidance.
        assert!(!out.content.contains("Codex specifics."));
        assert!(out.profile_guidance.is_empty());
    }

    #[test]
    fn profile_template_file_overrides_inline_guidance() {
        let d = tempfile::tempdir().unwrap();
        let pdir = config::repo_templates_dir(d.path()).join("profiles");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(pdir.join("rust.md.j2"), "FILE GUIDANCE for {{ profile }}").unwrap();

        let mut ctx = sample_context();
        ctx.repo_base = d.path().to_path_buf();
        ctx.cwd = d.path().to_path_buf();
        let cfg = Config::defaults();
        // An inline capability whose guidance must be overridden by the file.
        let inline = resolved(
            Capability::inline("rust", "INLINE GUIDANCE".into()),
            "rust",
            true,
        );
        let comp = composition("rust", vec![inline]);

        let out = render(&RenderRequest {
            agent: "claude",
            template_name: "claude",
            context: &ctx,
            composition: &comp,
            config: &cfg,
            generated_at: "2026-05-29T00:00:00Z".into(),
        })
        .unwrap();

        assert!(out.content.contains("FILE GUIDANCE for rust"));
        assert!(!out.content.contains("INLINE GUIDANCE"));
    }

    #[test]
    fn empty_composition_renders_no_guidance_section() {
        let ctx = sample_context();
        let cfg = Config::defaults();
        let comp = composition("default", vec![]);
        let out = render(&RenderRequest {
            agent: "generic",
            template_name: "generic",
            context: &ctx,
            composition: &comp,
            config: &cfg,
            generated_at: "2026-05-29T00:00:00Z".into(),
        })
        .unwrap();
        assert!(!out.content.contains("Profile guidance —"));
    }

    #[test]
    fn missing_optional_git_does_not_error() {
        let mut ctx = sample_context();
        ctx.git = None; // exercise lenient undefined handling
        let cfg = Config::defaults();
        let comp = composition("default", vec![]);
        let out = render(&RenderRequest {
            agent: "claude",
            template_name: "claude",
            context: &ctx,
            composition: &comp,
            config: &cfg,
            generated_at: "2026-05-29T00:00:00Z".into(),
        })
        .unwrap();
        assert!(out.content.contains("agent context"));
    }
}
