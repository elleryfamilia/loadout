//! Template rendering: context + composed fragments + template → an overlay.
//!
//! The low-level [`TemplateRenderer`] trait abstracts the engine (here
//! minijinja). [`render`] is the high-level entry the adapters call: it resolves
//! the base template, renders each composed fragment into the body, prepends
//! the generated header, and returns the content plus the context hash and
//! provenance.

pub mod header;

use chrono::{DateTime, Utc};
use minijinja::{Environment, UndefinedBehavior, Value};
use serde::Serialize;

use crate::config::Config;
use crate::context::Context;
use crate::dynamic::{self, DynamicMode};
use crate::fragment::Fragment;
use crate::profile::Composition;
use crate::providers::ProviderOutput;
use crate::templates;
use crate::workflow::Workflow;

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
    /// Composed fragments + matching profiles.
    pub composition: &'a Composition,
    /// The workflow bound by the selected profile, already resolved by the
    /// caller (a built-in or user `[[workflows]]`), or `None` when the profile
    /// binds none / the binding dangles. Rendered as the `## Workflow` context
    /// section and folded into the freshness fingerprint.
    pub workflow: Option<&'a Workflow>,
    /// Loaded config (template overrides, source provenance).
    pub config: &'a Config,
    /// Injected generation timestamp (RFC3339) — passed in for testability.
    pub generated_at: String,
    /// Whether dynamic fragments may execute (Live) or are cache-only
    /// (ReadOnly, for explain/dry-run).
    pub dynamic: DynamicMode,
}

/// Result of a render.
pub struct RenderOutput {
    /// Header + body, ready to write.
    pub content: String,
    /// `sha256:…` of the context that produced it.
    pub context_hash: String,
    /// Concatenated fragment guidance (the `profile_guidance` body; may be
    /// empty, e.g. when every fragment is restricted to other agents).
    pub profile_guidance: String,
    /// Whether any rendered fragment was dynamic. Dynamic overlays bypass the
    /// hash-skip so live output always lands (their volatile output is excluded
    /// from the context hash, so the cache TTL — not the hash — governs churn).
    pub has_dynamic: bool,
    /// The composed fragments, each rendered to its own markdown section.
    /// `profile_guidance` is exactly these joined; exposed structured so callers
    /// (studio) can show per-fragment preview cards without re-rendering.
    pub fragments: Vec<RenderedFragment>,
}

/// One composed fragment rendered to markdown — the structured form of a
/// `### <title>` overlay section. Only the fragments that actually produce a
/// section appear here (agent-gated and empty ones are omitted, exactly as in
/// the overlay body).
#[derive(Debug, Clone)]
pub struct RenderedFragment {
    /// Fragment id.
    pub id: String,
    /// Section title (the fragment's title).
    pub title: String,
    /// Rendered guidance markdown, or the skip note.
    pub body: String,
    /// True when this fragment resolved a dynamic provider/command.
    pub dynamic: bool,
    /// True when a dynamic command was skipped (`allow_exec = false`); `body` is
    /// the `> [loadout] …` skip note rather than rendered guidance.
    pub skipped: bool,
}

/// The serializable model exposed to the base overlay template.
#[derive(Serialize)]
struct RenderModel<'a> {
    agent: &'a str,
    profile: &'a str,
    profile_guidance: &'a str,
    context: &'a Context,
}

/// The serializable model exposed to each fragment's guidance template.
#[derive(Serialize)]
struct FragmentModel<'a> {
    agent: &'a str,
    /// The profile that pulled this fragment in.
    profile: &'a str,
    context: &'a Context,
    fragment: &'a Fragment,
    /// Convenience alias for `fragment.params`.
    params: &'a toml::Value,
    /// Live provider/command output for a dynamic fragment (`{{ provider.output }}`,
    /// `{{ provider.data }}`); absent for static fragments.
    provider: Option<ProviderRef<'a>>,
}

/// The dynamic-output view exposed to a fragment's template as `provider`.
#[derive(Serialize)]
struct ProviderRef<'a> {
    output: &'a str,
    data: &'a serde_json::Value,
}

/// The freshness fingerprint stamped into a generated overlay (and compared on
/// the next render / by `doctor`): the detected context, the composition that
/// produced the overlay, **and** the bound workflow. A change to any — including
/// a global-config edit that alters the resolved fragments/profile/workflow for
/// an unchanged context — moves the fingerprint, so a cached overlay is never
/// silently stale.
///
/// When no workflow is bound the fingerprint reduces to the
/// `(HEADER_VERSION, context, composition)` hash — binding a workflow is the
/// only thing that folds more in, so a profile without one doesn't churn when
/// the workflow feature evolves.
pub fn overlay_fingerprint(
    context: &Context,
    composition: &Composition,
    workflow: Option<&Workflow>,
) -> String {
    let base = (
        HEADER_VERSION,
        context.compute_hash(),
        composition.fingerprint(),
    );
    match workflow {
        None => crate::hash::context_hash(&base),
        Some(w) => crate::hash::context_hash(&(base.0, base.1, base.2, w.content_hash())),
    }
}

/// Bumped when the generated header's wording changes in a way agents should
/// actually see (e.g. the self-heal instruction). The header text isn't part
/// of the context, so without this a wording change would never reach
/// already-rendered overlays — the hash-skip would keep them forever.
/// Bumping re-renders every overlay exactly once.
const HEADER_VERSION: u32 = 2;

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

    // 2. Render the composed fragments into the guidance body. Dynamic
    //    fragments resolve against `now` (parsed from the render timestamp).
    let now = DateTime::parse_from_rfc3339(&req.generated_at)
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());
    let rendered_caps = render_fragment_list(
        &renderer,
        req.context,
        req.composition,
        req.agent,
        req.dynamic,
        now,
    )?;
    let has_dynamic = rendered_caps.iter().any(|c| c.dynamic);
    let mut profile_guidance = join_fragment_sections(&rendered_caps);

    // 2b. The always-on workflow map. Appended *into* `profile_guidance` so it
    //     travels with whatever base template renders the overlay (no template
    //     needs to know about workflows), after the fragment conventions.
    if let Some(wf) = req.workflow {
        let section = render_workflow_section(wf);
        if profile_guidance.is_empty() {
            profile_guidance = section;
        } else {
            profile_guidance = format!("{profile_guidance}\n\n{section}");
        }
    }

    // 3. Freshness fingerprint: the detected context, the composition, AND the
    //    bound workflow. Folding in the composition is what makes a
    //    *global-config* change (a new/edited/removed fragment, profile, or
    //    workflow) re-render a repo whose detected context is unchanged.
    let context_hash = overlay_fingerprint(req.context, req.composition, req.workflow);

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
        profile_guidance,
        has_dynamic,
        fragments: rendered_caps,
    })
}

/// Render each composed fragment (in order) into a structured
/// [`RenderedFragment`]. The overlay body is [`join_fragment_sections`] of
/// this list; studio also consumes it for per-fragment preview cards.
///
/// Fragments restricted to other agents are skipped (the active agent varies
/// per render), as are ones that render empty. A **dynamic** fragment resolves its provider/command
/// output (cache-backed) with `provider.output`/`provider.data` in scope; a
/// command with `allow_exec = false` renders a skip note instead.
fn render_fragment_list(
    renderer: &MinijinjaRenderer,
    ctx: &Context,
    composition: &Composition,
    agent: &str,
    mode: DynamicMode,
    now: DateTime<Utc>,
) -> crate::Result<Vec<RenderedFragment>> {
    let mut out: Vec<RenderedFragment> = Vec::new();

    for rc in &composition.fragments {
        let cap = &rc.fragment;
        if !cap.applies_to_agent(agent) {
            continue;
        }

        // Render a template with the per-fragment model, optionally exposing
        // dynamic `provider` output.
        let render_tmpl = |src: &str, provider: Option<&ProviderOutput>| -> crate::Result<String> {
            let model = FragmentModel {
                agent,
                profile: &rc.via_profile,
                context: ctx,
                fragment: cap,
                params: &cap.params,
                provider: provider.map(|o| ProviderRef {
                    output: &o.text,
                    data: &o.data,
                }),
            };
            Ok(renderer
                .render_str(src, &Value::from_serialize(&model))?
                .trim()
                .to_string())
        };

        let dyn_res = dynamic::resolve(cap, ctx, &ctx.repo_base, mode, now);
        let dynamic = dyn_res.is_some();
        let mut skipped = false;

        let body: String = match &dyn_res {
            // Dynamic, but the command was skipped (e.g. allow_exec = false).
            Some(res) if res.skipped.is_some() => {
                skipped = true;
                format!("> [loadout] {}", res.skipped.as_ref().unwrap())
            }
            // Dynamic with resolved (or absent) output.
            Some(res) => {
                let out = res.output.as_ref();
                if cap.guidance.trim().is_empty() {
                    // No template → embed the raw output, or omit if none.
                    match out {
                        Some(o) => o.text.clone(),
                        None => continue,
                    }
                } else {
                    let rendered = render_tmpl(&cap.guidance, out)?;
                    if rendered.is_empty() {
                        continue;
                    }
                    rendered
                }
            }
            // Static fragment.
            None => {
                if cap.guidance.trim().is_empty() {
                    continue;
                }
                let rendered = render_tmpl(&cap.guidance, None)?;
                if rendered.is_empty() {
                    continue;
                }
                rendered
            }
        };

        let title = cap.title().to_string();
        out.push(RenderedFragment {
            id: cap.id.clone(),
            title,
            body,
            dynamic,
            skipped,
        });
    }

    Ok(out)
}

/// Join rendered fragments into the overlay's guidance body: each becomes a
/// `### <title>` section, separated by a blank line. This is exactly the
/// `profile_guidance` the base template embeds.
fn join_fragment_sections(caps: &[RenderedFragment]) -> String {
    caps.iter()
        .map(|c| format!("### {}\n\n{}", c.title, c.body))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Render a bound workflow as the always-on `## Workflow` context section: a
/// short framing plus the stage spine in order, each stage's purpose, its
/// handoff artifacts, gate marker, and exit checklist. This is the "context
/// channel" — the map an agent reads in every render; the per-stage commands
/// (the "command channel") are generated separately by the adapters.
pub fn render_workflow_section(wf: &Workflow) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "## Workflow: {}", wf.title());
    let _ = writeln!(
        s,
        "\nThe house workflow for this profile — work the stages below in order. \
         Files passed between stages (the _reads_/_writes_ notes) live under \
         `.loadout/{}/`; loadout creates that directory and exposes each file as a \
         `LOADOUT_<NAME>_PATH` environment variable. A _(gate)_ stage is a \
         checkpoint — pause for review before moving on. This is guidance, not \
         enforced.\n",
        crate::workflow::ARTIFACT_SUBDIR
    );
    // Walk the canonical spine, not the raw stages, so the prose stage names
    // (the bold headings) match the `/loadout:<command>` slash commands exactly.
    let steps = wf.canonical_layout().steps();
    for (i, &(command, stage)) in steps.iter().enumerate() {
        let _ = write!(s, "{}. **{}**", i + 1, command);
        if let Some(purpose) = &stage.purpose {
            let _ = write!(s, " — {purpose}");
        }
        // Trailing annotations (handoff + gate) collapsed into one parenthetical.
        let mut notes: Vec<String> = Vec::new();
        match (&stage.reads, &stage.writes) {
            (Some(r), Some(w)) if r == w => notes.push(format!("updates `{r}`")),
            (Some(r), Some(w)) => {
                notes.push(format!("reads `{r}`"));
                notes.push(format!("writes `{w}`"));
            }
            (Some(r), None) => notes.push(format!("reads `{r}`")),
            (None, Some(w)) => notes.push(format!("writes `{w}`")),
            (None, None) => {}
        }
        if stage.gate {
            notes.push("gate".to_string());
        }
        if !notes.is_empty() {
            let _ = write!(s, " _({})_", notes.join("; "));
        }
        s.push('\n');
        for item in &stage.exit {
            let _ = writeln!(s, "   - done when: {item}");
        }
    }
    s.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::test_support::sample_context;
    use crate::fragment::Fragment;
    use crate::profile::ResolvedFragment;

    fn named_cap(id: &str, guidance: &str) -> Fragment {
        Fragment {
            id: id.into(),
            description: Some(id.into()),
            category: None,
            when: vec![],
            requires: vec![],
            params: toml::Value::Table(Default::default()),
            guidance: guidance.into(),
            agents: vec![],
            provider: None,
            command: None,
            script_lang: None,
            allow_exec: true,
            cache: None,
            origin: crate::fragment::Layer::default(),
        }
    }

    fn resolved(cap: Fragment, via: &str) -> ResolvedFragment {
        ResolvedFragment {
            fragment: cap,
            via_profile: via.into(),
            reason: "test".into(),
        }
    }

    fn composition(profile: &str, caps: Vec<ResolvedFragment>) -> Composition {
        Composition {
            profile: Some(profile.into()),
            fragments: caps,
            reasons: vec![],
            missing: vec![],
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
            )],
        );

        let out = render(&RenderRequest {
            agent: "claude",
            template_name: "claude",
            context: &ctx,
            composition: &comp,
            workflow: None,
            config: &cfg,
            generated_at: "2026-05-29T00:00:00Z".into(),
            dynamic: DynamicMode::ReadOnly,
        })
        .unwrap();

        assert!(out.content.starts_with(header::GENERATED_MARKER));
        assert!(out.content.contains("profile   : rust"));
        assert!(out.content.contains("Stack:** rust"));
        assert!(out.content.contains("`cargo test`"));
        // The fragment appears under its own heading...
        assert!(out.content.contains("### rust-conventions"));
        // ...with its guidance template rendered against the context.
        assert!(out.content.contains("Use cargo for **rust**."));
        assert!(out.context_hash.starts_with("sha256:"));
    }

    #[test]
    fn exposes_structured_per_fragment_output() {
        let ctx = sample_context();
        let cfg = Config::defaults();
        let comp = composition(
            "infra",
            vec![
                resolved(named_cap("infra-caution", "Be careful."), "infra"),
                resolved(named_cap("baseline", "Keep it minimal."), "default"),
            ],
        );
        let out = render(&RenderRequest {
            agent: "claude",
            template_name: "claude",
            context: &ctx,
            composition: &comp,
            workflow: None,
            config: &cfg,
            generated_at: "2026-05-29T00:00:00Z".into(),
            dynamic: DynamicMode::ReadOnly,
        })
        .unwrap();

        // One structured entry per rendered fragment, in composition order.
        let ids: Vec<&str> = out.fragments.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["infra-caution", "baseline"]);
        assert_eq!(out.fragments[0].body, "Be careful.");
        assert!(!out.fragments[0].dynamic && !out.fragments[0].skipped);
        // The structured list joins back to the overlay's guidance body exactly.
        assert_eq!(out.profile_guidance, join_fragment_sections(&out.fragments));
        assert!(out.profile_guidance.contains("### infra-caution"));
    }

    #[test]
    fn concatenates_fragments_in_order() {
        let ctx = sample_context();
        let cfg = Config::defaults();
        let comp = composition(
            "infra",
            vec![
                resolved(named_cap("infra-caution", "Be careful."), "infra"),
                resolved(named_cap("baseline", "Keep it minimal."), "default"),
            ],
        );
        let out = render(&RenderRequest {
            agent: "claude",
            template_name: "claude",
            context: &ctx,
            composition: &comp,
            workflow: None,
            config: &cfg,
            generated_at: "2026-05-29T00:00:00Z".into(),
            dynamic: DynamicMode::ReadOnly,
        })
        .unwrap();

        // Each fragment renders as its own `###` section.
        assert!(out.content.contains("### infra-caution"));
        assert!(out.content.contains("### baseline"));
        // Order is preserved: infra before baseline.
        assert!(out.content.find("infra-caution").unwrap() < out.content.find("baseline").unwrap());
    }

    #[test]
    fn agent_restricted_fragment_is_skipped() {
        let ctx = sample_context();
        let cfg = Config::defaults();
        let mut only_codex = named_cap("codex-only", "Codex specifics.");
        only_codex.agents = vec!["codex".into()];
        let comp = composition("default", vec![resolved(only_codex, "default")]);

        let out = render(&RenderRequest {
            agent: "claude",
            template_name: "claude",
            context: &ctx,
            composition: &comp,
            workflow: None,
            config: &cfg,
            generated_at: "2026-05-29T00:00:00Z".into(),
            dynamic: DynamicMode::ReadOnly,
        })
        .unwrap();
        // Restricted to codex → absent from a claude render's guidance.
        assert!(!out.content.contains("Codex specifics."));
        assert!(out.profile_guidance.is_empty());
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
            workflow: None,
            config: &cfg,
            generated_at: "2026-05-29T00:00:00Z".into(),
            dynamic: DynamicMode::ReadOnly,
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
            workflow: None,
            config: &cfg,
            generated_at: "2026-05-29T00:00:00Z".into(),
            dynamic: DynamicMode::ReadOnly,
        })
        .unwrap();
        assert!(out.content.contains("agent context"));
    }

    #[test]
    fn renders_bound_workflow_section() {
        let ctx = sample_context();
        let cfg = Config::defaults();
        let comp = composition(
            "rust",
            vec![resolved(named_cap("baseline", "Be minimal."), "rust")],
        );
        let wf = crate::workflow::builtin_workflows()
            .into_iter()
            .find(|w| w.id == "spec-driven")
            .unwrap();
        let out = render(&RenderRequest {
            agent: "claude",
            template_name: "claude",
            context: &ctx,
            composition: &comp,
            workflow: Some(&wf),
            config: &cfg,
            generated_at: "2026-05-29T00:00:00Z".into(),
            dynamic: DynamicMode::ReadOnly,
        })
        .unwrap();
        // The section renders with its title, the ordered stages, and the handoff.
        assert!(out.content.contains("## Workflow: Spec-driven"));
        assert!(out.content.contains("**plan**"));
        assert!(out.content.contains("writes `plan.md`"));
        assert!(out.content.contains("reads `plan.md`"));
        // It rides inside profile_guidance, after the fragment conventions.
        assert!(out.profile_guidance.contains("### baseline"));
        let frag_at = out.profile_guidance.find("### baseline").unwrap();
        let wf_at = out.profile_guidance.find("## Workflow:").unwrap();
        assert!(frag_at < wf_at, "workflow section follows the fragments");
    }

    #[test]
    fn workflow_is_folded_into_the_fingerprint_without_churn() {
        let ctx = sample_context();
        let comp = composition("rust", vec![resolved(named_cap("a", "A"), "rust")]);
        let wf = crate::workflow::builtin_workflows()
            .into_iter()
            .find(|w| w.id == "spec-driven")
            .unwrap();

        // No workflow → the base (header-version, context, composition) hash;
        // the workflow arm folds more in only when one is actually bound.
        let base = crate::hash::context_hash(&(
            super::HEADER_VERSION,
            ctx.compute_hash(),
            comp.fingerprint(),
        ));
        assert_eq!(overlay_fingerprint(&ctx, &comp, None), base);

        // Binding a workflow moves the fingerprint...
        let with = overlay_fingerprint(&ctx, &comp, Some(&wf));
        assert_ne!(with, base);

        // ...and editing the bound workflow moves it again.
        let mut edited = wf.clone();
        edited.stages[0].purpose = Some("changed".into());
        assert_ne!(with, overlay_fingerprint(&ctx, &comp, Some(&edited)));
    }

    #[test]
    fn workflow_section_uses_purpose_not_the_elaborate_instructions() {
        // Channel 1 (the always-on context map) must stay terse: it shows the
        // one-line purpose but never the elaborate `instructions` body, which is
        // reserved for the per-step command file (channel 2).
        use crate::workflow::{Workflow, WorkflowStage};
        let wf = Workflow {
            id: "x".into(),
            name: Some("X".into()),
            description: None,
            icon: None,
            stages: vec![WorkflowStage {
                name: "plan".into(),
                purpose: Some("Plan the work".into()),
                instructions: Some("INSTRUCTIONS-MARKER: the long prescriptive body.".into()),
                reads: None,
                writes: None,
                gate: false,
                exit: vec![],
            }],
            modeled_on: None,
            researched: None,
            source: None,
            disabled: false,
            origin: crate::fragment::Layer::default(),
        };
        let section = render_workflow_section(&wf);
        assert!(section.contains("Plan the work"), "purpose is shown");
        assert!(
            !section.contains("INSTRUCTIONS-MARKER"),
            "elaborate instructions stay out of the always-on section"
        );
    }
}
