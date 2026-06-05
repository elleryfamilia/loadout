//! Studio session state + the read-only "model" computations (selection,
//! ReadOnly overlay preview, the library snapshot) that the HTTP handlers and
//! views render. Kept free of `tiny_http` so it's unit-testable without a socket.
//!
//! Concurrency rule (design §2): handlers take a cheap [`Snapshot`] under the
//! session mutex, release it, then assemble/render **outside** the lock — never
//! hold the mutex across rendering, disk I/O, or probe execution.

use std::path::PathBuf;

use crate::adapters;
use crate::capability::{palette, Capability, Layer, Risk};
use crate::config::Config;
use crate::context::{Context, GitContext, Scope};
use crate::dynamic::DynamicMode;
use crate::profile::{self, CapabilityRef, ProfileConfig, Selection};
use crate::render::{self, RenderRequest};
use crate::studio::edit::{Session, StagedOp};

/// The simulated context the preview is rendered for. Each field overrides the
/// real detected context; `None`/empty means "use what was detected".
#[derive(Debug, Clone)]
pub struct Simulated {
    /// Target agent id to render for.
    pub agent: String,
    /// Override the detected stack/language (empty ⇒ no stack).
    pub lang: Option<String>,
    /// Override repo-vs-machine scope.
    pub scope: Option<Scope>,
}

impl Simulated {
    /// Update the simulator from a posted urlencoded form (`lang`/`scope`/`agent`).
    /// Unrecognized/blank values reset to "use detected".
    pub fn update_from_form(&mut self, body: &str) {
        for (k, v) in parse_pairs(body) {
            match k.as_str() {
                "agent" if !v.is_empty() => self.agent = v,
                "lang" => self.lang = if v.is_empty() { None } else { Some(v) },
                "scope" => {
                    self.scope = match v.as_str() {
                        "repo" => Some(Scope::Repo),
                        "machine" => Some(Scope::Machine),
                        _ => None,
                    }
                }
                _ => {}
            }
        }
    }
}

/// A studio editing/viewing session: the edit engine + the detected context +
/// the simulator + the security token/port. Lives behind an `Arc<Mutex<…>>`.
pub struct StudioState {
    /// The comment-preserving edit engine over the writable layers.
    pub session: Session,
    /// The real detected context (the simulator overrides a clone of this).
    pub base_context: Context,
    /// Repo base (git root or cwd).
    pub repo_base: PathBuf,
    /// The simulated context the preview reflects.
    pub sim: Simulated,
    /// Per-session CSRF/session token (also the bootstrap-cookie value).
    pub token: String,
    /// Bound port (for Host/Origin checks).
    pub port: u16,
}

impl StudioState {
    /// A cheap, owned copy of everything the read-only handlers need, taken under
    /// the mutex so rendering can happen after the lock is released.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            base_context: self.base_context.clone(),
            sim: self.sim.clone(),
            layer_texts: self.session.staged_layer_texts(),
        }
    }
}

/// An owned, lock-free snapshot for rendering a view.
pub struct Snapshot {
    pub base_context: Context,
    pub sim: Simulated,
    pub layer_texts: Vec<(Layer, PathBuf, String)>,
}

/// How the simulated context resolved to a profile — the binding chip's three
/// states. Named `BindingState` (not `Binding`) to avoid colliding with the
/// on-disk [`crate::binding::Binding`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingState {
    /// Exactly one profile applies (its name).
    Bound(String),
    /// No profile applies to this context.
    None,
    /// 2+ profiles match and none is bound (how many).
    Ambiguous(usize),
}

/// The result of a ReadOnly preview render.
pub struct PreviewOutcome {
    /// Agent the overlay was rendered for.
    pub agent: String,
    /// Selected profile label (`none` when no profile applies). Retained for the
    /// `profile {label}` text in the overlay head; the chip uses `binding`.
    pub profile_label: String,
    /// Structured selection state for the binding chip.
    pub binding: BindingState,
    /// Short human summary of the simulated context, e.g. `rust · repo`.
    pub context_summary: String,
    /// How many capabilities actually render for `agent` (after agent gating) —
    /// the provenance breadcrumb's count, truthful to what's in the overlay.
    pub cap_count: usize,
    /// The rendered overlay markdown (header + body). Drives the profile
    /// editor's live preview.
    pub overlay: String,
    /// Per-capability rendered guidance — the Profiles-tab detail's expandable
    /// cards. One entry per capability that contributes a section to the overlay.
    pub caps: Vec<PreviewCap>,
    /// A human note when there's no single profile (empty / ambiguous).
    pub note: Option<String>,
}

/// One capability's rendered guidance for the Profiles-tab detail cards.
pub struct PreviewCap {
    pub id: String,
    pub title: String,
    pub risk: Risk,
    /// The capability's curated icon, if any (else the card uses a default).
    pub icon: Option<String>,
    /// Rendered guidance markdown (or the trust-skip note).
    pub markdown: String,
    /// Resolved a dynamic provider/command.
    pub dynamic: bool,
    /// A dynamic command was refused for lack of trust (markdown is the note).
    pub skipped: bool,
    /// True when this id is an editable library capability (not a synthetic
    /// inline section) — gates the card's "Edit capability" affordance.
    pub editable: bool,
}

/// One capability row for the library view.
pub struct CapView {
    pub id: String,
    pub title: String,
    pub kind: &'static str,
    /// Primary category for grouping the library (the capability's first tag).
    pub category: Option<String>,
    /// Optional curated icon name.
    pub icon: Option<String>,
    /// Interpreter for a script cap (`bash`/`sh`/`python`); drives the badge.
    pub script_lang: Option<String>,
    /// True when authored in a `*local.toml` layer (private / gitignored).
    pub private: bool,
    /// The capability's risk (drives the row's risk spine).
    pub risk: Risk,
    /// Composed into the current preview overlay.
    pub active: bool,
}

/// Whether a profile's referenced capability id resolves to something that
/// actually contributes to the overlay. Composition only pulls *owned* caps;
/// a palette ref renders nothing until duplicated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomState {
    /// Owned (in your config) — contributes to the overlay.
    Owned,
    /// Named but only in the read-only palette (not duplicated) — contributes nothing.
    Palette,
    /// Unknown id — resolves to nothing.
    Unknown,
}

/// One "atom dot" on a profile card: a referenced capability and how it resolves.
pub struct AtomDot {
    pub id: String,
    pub risk: Risk,
    pub state: AtomState,
}

/// One profile row for the library view.
pub struct ProfileView {
    pub name: String,
    pub targets: Vec<String>,
    pub selected: bool,
    pub candidate: bool,
    /// When true the profile is an off-switch off (never selected/composed).
    pub disabled: bool,
    pub capabilities: Vec<String>,
    /// Resolved composition atoms, in declared order (drives the card's dots).
    pub atoms: Vec<AtomDot>,
}

/// The whole left-pane library snapshot for a context.
pub struct LibraryView {
    pub yours: Vec<CapView>,
    pub palette: Vec<CapView>,
    pub profiles: Vec<ProfileView>,
}

/// Assemble the staged config (origin-tagged) from a snapshot.
pub fn staged_config(snap: &Snapshot) -> crate::Result<Config> {
    Config::from_layer_strs(snap.layer_texts.clone())
}

/// Apply the simulator overrides to the detected context.
pub fn simulated_context(base: &Context, sim: &Simulated) -> Context {
    let mut ctx = base.clone();
    if let Some(lang) = &sim.lang {
        ctx.stacks = if lang.is_empty() {
            vec![]
        } else {
            vec![lang.clone()]
        };
    }
    match sim.scope {
        Some(Scope::Machine) => ctx.git = None,
        Some(Scope::Repo) if ctx.git.is_none() => {
            ctx.git = Some(GitContext {
                root: ctx.repo_base.clone(),
                branch: Some("main".to_string()),
                remotes: vec![],
                is_worktree: false,
            });
        }
        _ => {}
    }
    ctx
}

/// Select the profile for `(cfg, ctx)` honoring the on-disk binding.
pub fn select_for(cfg: &Config, ctx: &Context) -> Selection {
    let binding = crate::binding::read(ctx);
    profile::select(ctx, &cfg.profiles, binding.as_ref())
}

/// Render a specific profile (by name) composed for `agent`, in ReadOnly mode.
/// Drives the Profiles-tab preview pane.
pub fn render_profile(
    snap: &Snapshot,
    profile_name: &str,
    agent: &str,
) -> crate::Result<PreviewOutcome> {
    let cfg = staged_config(snap)?;
    let profile = cfg
        .profiles
        .iter()
        .find(|p| p.name == profile_name)
        .ok_or_else(|| anyhow::anyhow!("unknown profile '{profile_name}'"))?
        .clone();
    render_profile_config(snap, &profile, agent)
}

/// Render an arbitrary (possibly unsaved/draft) profile composed for `agent`.
/// The context is synthesized from the profile's own targets so its capabilities
/// gate as intended. Used by the Profiles-tab preview and the editor's live draft.
pub fn render_profile_config(
    snap: &Snapshot,
    profile: &ProfileConfig,
    agent: &str,
) -> crate::Result<PreviewOutcome> {
    let cfg = staged_config(snap)?;
    let ctx = context_for_profile(&snap.base_context, profile);
    let composition =
        profile::compose_profile(&ctx, profile, &cfg.capabilities, &cfg.capability_params);

    let agent_id = if agent.is_empty() {
        cfg.default_agent.clone()
    } else {
        agent.to_string()
    };
    let descriptor = adapters::descriptor(&cfg, &agent_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("unknown agent '{agent_id}'"))?;
    let out = render::render(&RenderRequest {
        agent: &descriptor.id,
        template_name: &descriptor.template,
        context: &ctx,
        composition: &composition,
        config: &cfg,
        generated_at: now_rfc3339(),
        dynamic: DynamicMode::ReadOnly,
    })?;
    let cap_count = composition
        .capabilities
        .iter()
        .filter(|rc| rc.capability.applies_to_agent(&agent_id))
        .count();
    // Per-capability cards. Built from the *composition* (every cap the profile
    // pulls in, agent-filtered) rather than only the rendered sections: a dynamic
    // cap that resolves to nothing in ReadOnly (its provider/command doesn't run
    // and there's no cache) is dropped from the overlay, which would hide its
    // card. Such caps get a "runs at render" placeholder so the preview still
    // lists — and can open/edit — them. Each cap's icon/editability is looked up
    // from the staged library (inline/synthetic caps fall back to a default).
    let rendered: std::collections::HashMap<&str, _> =
        out.capabilities.iter().map(|c| (c.id.as_str(), c)).collect();
    let caps: Vec<PreviewCap> = composition
        .capabilities
        .iter()
        .filter(|rc| rc.capability.applies_to_agent(&agent_id))
        .filter_map(|rc| {
            let cap = &rc.capability;
            let owned = cfg.capabilities.iter().find(|x| x.id == cap.id);
            let icon = owned.and_then(|x| x.icon.clone());
            let editable = owned.is_some();
            if let Some(c) = rendered.get(cap.id.as_str()) {
                Some(PreviewCap {
                    icon,
                    editable,
                    id: c.id.clone(),
                    title: c.title.clone(),
                    risk: c.risk,
                    markdown: c.body.clone(),
                    dynamic: c.dynamic,
                    skipped: c.skipped,
                })
            } else if cap.is_dynamic() {
                Some(PreviewCap {
                    icon,
                    editable,
                    id: cap.id.clone(),
                    title: cap.title().to_string(),
                    risk: cap.risk,
                    markdown: "_Dynamic — runs at render; no preview output yet._".to_string(),
                    dynamic: true,
                    skipped: false,
                })
            } else {
                None // static cap that rendered nothing — omit, as the overlay does
            }
        })
        .collect();
    Ok(PreviewOutcome {
        agent: agent_id,
        profile_label: profile.name.clone(),
        binding: if profile.disabled {
            BindingState::None
        } else {
            BindingState::Bound(profile.name.clone())
        },
        context_summary: context_summary(&ctx),
        cap_count,
        overlay: out.content,
        caps,
        note: profile
            .disabled
            .then(|| "This profile is disabled — it won't be selected in real runs.".to_string()),
    })
}

/// Synthesize a context from a profile's targets so its capabilities gate as
/// intended when previewed (a `machine`-only profile previews as machine scope).
fn context_for_profile(base: &Context, profile: &ProfileConfig) -> Context {
    let mut ctx = base.clone();
    ctx.stacks = profile
        .targets
        .iter()
        .filter(|t| t.as_str() != "machine")
        .cloned()
        .collect();
    if profile.targets.len() == 1 && profile.targets.first().map(String::as_str) == Some("machine")
    {
        ctx.git = None;
    }
    ctx
}

/// A short human summary of a (simulated) context for the provenance
/// breadcrumb, e.g. `rust · repo` or `no stack · machine`.
fn context_summary(ctx: &Context) -> String {
    let stack = if ctx.stacks.is_empty() {
        "no stack".to_string()
    } else {
        ctx.stacks.join("+")
    };
    let scope = if ctx.git.is_some() { "repo" } else { "machine" };
    format!("{stack} · {scope}")
}

/// Build the left-pane library view (your caps + the palette + your profiles),
/// marking what's active/selected for the snapshot's simulated context.
pub fn library_view(snap: &Snapshot) -> crate::Result<LibraryView> {
    let cfg = staged_config(snap)?;
    let ctx = simulated_context(&snap.base_context, &snap.sim);
    let selection = select_for(&cfg, &ctx);

    let selected_name = match &selection {
        Selection::Use(p) => Some(p.name.clone()),
        _ => None,
    };
    let active_ids: Vec<String> = match &selection {
        Selection::Use(p) => {
            profile::compose_profile(&ctx, p, &cfg.capabilities, &cfg.capability_params)
                .capabilities
                .iter()
                .map(|rc| rc.capability.id.clone())
                .collect()
        }
        _ => vec![],
    };

    let tags = ctx.selection_targets();
    let yours = cfg
        .capabilities
        .iter()
        .map(|c| CapView {
            kind: kind_of(c.command.is_some(), c.provider.is_some()),
            category: c.tags.first().cloned(),
            active: active_ids.contains(&c.id),
            title: c.title().to_string(),
            icon: c.icon.clone(),
            script_lang: c.script_lang.clone(),
            private: matches!(c.origin, Layer::RepoLocal | Layer::GlobalLocal),
            risk: c.risk,
            id: c.id.clone(),
        })
        .collect();
    // The palette (called once; the local `palette` below would shadow the fn).
    let palette_items = palette();
    // Resolve each profile's capability refs to risk + provenance for the atom
    // dots: owned (contributes) vs palette-only (named but not duplicated, so it
    // renders nothing) vs unknown. No extra requests — all from the snapshot.
    let owned_risk: std::collections::HashMap<&str, Risk> = cfg
        .capabilities
        .iter()
        .map(|c| (c.id.as_str(), c.risk))
        .collect();
    let palette_risk: std::collections::HashMap<&str, Risk> = palette_items
        .iter()
        .map(|c| (c.id.as_str(), c.risk))
        .collect();
    let resolve_atom = |id: String| -> AtomDot {
        let (risk, state) = if let Some(&r) = owned_risk.get(id.as_str()) {
            (r, AtomState::Owned)
        } else if let Some(&r) = palette_risk.get(id.as_str()) {
            (r, AtomState::Palette)
        } else {
            (Risk::Info, AtomState::Unknown)
        };
        AtomDot { id, risk, state }
    };
    // Palette items not already owned (by id) in your library.
    let owned: std::collections::HashSet<&str> =
        cfg.capabilities.iter().map(|c| c.id.as_str()).collect();
    let palette: Vec<CapView> = palette_items
        .iter()
        .filter(|c| !owned.contains(c.id.as_str()))
        .map(|c| CapView {
            kind: kind_of(c.command.is_some(), c.provider.is_some()),
            category: c.tags.first().cloned(),
            active: false,
            title: c.title().to_string(),
            icon: c.icon.clone(),
            script_lang: c.script_lang.clone(),
            private: false,
            risk: c.risk,
            id: c.id.clone(),
        })
        .collect();
    let profiles = cfg
        .profiles
        .iter()
        .map(|p| ProfileView {
            name: p.name.clone(),
            targets: p.targets.clone(),
            selected: selected_name.as_deref() == Some(p.name.as_str()),
            candidate: profile::profile_matches_targets(p, &tags),
            disabled: p.disabled,
            capabilities: p.capabilities.iter().map(|r| r.id().to_string()).collect(),
            atoms: p
                .capabilities
                .iter()
                .map(|r| resolve_atom(r.id().to_string()))
                .collect(),
        })
        .collect();

    Ok(LibraryView {
        yours,
        palette,
        profiles,
    })
}

/// Studio's "first open" affordance: when a repo has no authored capabilities,
/// write the shipped starter set into its public `config.toml` as normal, owned
/// entries (stage + apply through the comment-preserving engine). From then on
/// the starters are just capabilities — fully editable and deletable — so studio
/// never needs a separate read-only palette. A no-op (returns 0) the moment any
/// capability already exists in any layer.
pub fn seed_starters_if_empty(session: &mut Session) -> crate::Result<usize> {
    if !session.staged_config()?.capabilities.is_empty() {
        return Ok(0);
    }
    let starters = palette();
    for cap in &starters {
        session.stage(StagedOp::CreateCapability {
            layer: Layer::Repo,
            cap: Box::new(cap.clone()),
        })?;
    }
    session.apply()?;
    Ok(starters.len())
}

fn kind_of(has_command: bool, has_provider: bool) -> &'static str {
    if has_command {
        "command"
    } else if has_provider {
        "provider"
    } else {
        "static"
    }
}

/// Current UTC time as an RFC3339 (`…Z`) string for the rendered header.
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Parse a urlencoded `a=b&c=d` body/query into decoded key/value pairs.
pub fn parse_pairs(s: &str) -> Vec<(String, String)> {
    s.split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

/// Minimal `application/x-www-form-urlencoded` decode (`+`→space, `%XX`). Also
/// used to decode `{id}`/`{name}` path segments (which never contain a bare `+`,
/// since the views percent-encode them).
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// --- form → typed model (Slice 2 write engine) -------------------------------

fn value_of<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// All non-empty values for a repeated field (checkboxes/multi-select).
fn values_for(pairs: &[(String, String)], key: &str) -> Vec<String> {
    pairs
        .iter()
        .filter(|(k, v)| k == key && !v.is_empty())
        .map(|(_, v)| v.clone())
        .collect()
}

/// A trimmed, non-empty optional field.
fn opt(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_string)
}

/// Which writable layer a form's `scope`/`visibility` controls select.
pub fn layer_from_form(pairs: &[(String, String)]) -> Layer {
    let global = value_of(pairs, "scope") == Some("global");
    let private = value_of(pairs, "visibility") == Some("private");
    match (global, private) {
        (true, true) => Layer::GlobalLocal,
        (true, false) => Layer::Global,
        (false, true) => Layer::RepoLocal,
        (false, false) => Layer::Repo,
    }
}

/// The capability id an editor submission targets: the readonly `id` field when
/// editing, otherwise the slug of the `name` field (a new capability).
pub fn editor_cap_id(pairs: &[(String, String)]) -> Option<String> {
    if let Some(id) = opt(value_of(pairs, "id")) {
        return Some(id);
    }
    opt(value_of(pairs, "name")).map(|n| slug(&n))
}

/// Slugify a display name into a stable capability id (lowercase, alphanumeric
/// runs joined by single hyphens). Used to derive a new capability's id.
pub fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_dash = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    out
}

/// Whether a capability is too rich for studio's content-first editor and must
/// be hand-edited as TOML: a built-in `provider`, or a `command` *and* a custom
/// guidance template (the simple "markdown OR script" form can't represent it
/// without clobbering one side).
pub fn is_advanced_capability(c: &Capability) -> bool {
    c.provider.is_some() || (c.command.is_some() && !c.guidance.trim().is_empty())
}

/// Build a [`Capability`] from the content-first editor form. `base` is the
/// existing capability when editing — fields the simple form doesn't expose
/// (tags, risk, requires, agents, cache, provider, when, params) are preserved
/// from it, so editing never silently drops them. `origin` is left default; it
/// is re-tagged by layer when the staged config is assembled.
pub fn capability_from_form(
    base: Option<&Capability>,
    pairs: &[(String, String)],
) -> crate::Result<Capability> {
    let name = opt(value_of(pairs, "name"));
    // id: fixed when editing; derived from the name when new.
    let id = match base {
        Some(c) => c.id.clone(),
        None => {
            let n = name
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("a name is required"))?;
            let s = slug(n);
            if s.is_empty() {
                anyhow::bail!("name must contain letters or digits");
            }
            s
        }
    };
    // "markdown" → static guidance; "script" → a command body run under an
    // interpreter (bash/sh/python), its output embedded. An empty script falls
    // back to markdown so the editor never errors mid-modal.
    let script_cmd = (value_of(pairs, "kind") == Some("script"))
        .then(|| opt(value_of(pairs, "command")))
        .flatten();
    let (guidance, command, script_lang, allow_exec) = if let Some(cmd) = script_cmd {
        let lang = match value_of(pairs, "script_lang") {
            Some("python") => "python",
            Some("sh") => "sh",
            _ => "bash",
        };
        (
            String::new(),
            Some(cmd),
            Some(lang.to_string()),
            value_of(pairs, "allow_exec").is_some(),
        )
    } else {
        (
            value_of(pairs, "guidance").unwrap_or("").to_string(),
            None,
            None,
            true,
        )
    };
    Ok(Capability {
        id,
        description: name.or_else(|| base.and_then(|c| c.description.clone())),
        icon: opt(value_of(pairs, "icon")),
        tags: base.map(|c| c.tags.clone()).unwrap_or_default(),
        risk: base.map(|c| c.risk).unwrap_or_default(),
        when: base.map(|c| c.when.clone()).unwrap_or_default(),
        requires: base.map(|c| c.requires.clone()).unwrap_or_default(),
        params: base
            .map(|c| c.params.clone())
            .unwrap_or_else(|| toml::Value::Table(Default::default())),
        guidance,
        agents: base.map(|c| c.agents.clone()).unwrap_or_default(),
        provider: base.and_then(|c| c.provider.clone()),
        command,
        script_lang,
        allow_exec,
        cache: base.and_then(|c| c.cache.clone()),
        origin: Layer::default(),
    })
}

/// Build a [`ProfileConfig`] from a posted composer form. Enforces the ≥1
/// capability rule (§3) — a profile with no capabilities can't be saved.
pub fn profile_from_form(pairs: &[(String, String)]) -> crate::Result<ProfileConfig> {
    let name =
        opt(value_of(pairs, "name")).ok_or_else(|| anyhow::anyhow!("profile name is required"))?;
    let capabilities: Vec<CapabilityRef> = values_for(pairs, "capabilities")
        .into_iter()
        .map(CapabilityRef::Id)
        .collect();
    if capabilities.is_empty() {
        anyhow::bail!("a profile needs at least one capability");
    }
    Ok(ProfileConfig {
        name,
        targets: values_for(pairs, "targets"),
        capabilities,
        template: opt(value_of(pairs, "template")),
        guidance: opt(value_of(pairs, "guidance")),
        disabled: value_of(pairs, "disabled").is_some(),
    })
}

/// Build a capability + its target layer from the profile editor's inline
/// quick-create fields (`cap_name`/`cap_kind`/`cap_content`/`cap_private`).
/// Returns `None` when no name was typed (nothing to add).
pub fn inline_capability_from_form(pairs: &[(String, String)]) -> Option<(Capability, Layer)> {
    let name = opt(value_of(pairs, "cap_name"))?;
    let id = slug(&name);
    if id.is_empty() {
        return None;
    }
    let content = value_of(pairs, "cap_content").unwrap_or("").to_string();
    let (guidance, command, script_lang) =
        if value_of(pairs, "cap_kind") == Some("script") && !content.trim().is_empty() {
            (String::new(), Some(content), Some("bash".to_string()))
        } else {
            (content, None, None)
        };
    let layer = if value_of(pairs, "cap_private").is_some() {
        Layer::RepoLocal
    } else {
        Layer::Repo
    };
    let cap = Capability {
        id,
        description: Some(name),
        icon: None,
        tags: Vec::new(),
        risk: Risk::Info,
        when: Vec::new(),
        requires: Vec::new(),
        params: toml::Value::Table(Default::default()),
        guidance,
        agents: Vec::new(),
        provider: None,
        command,
        script_lang,
        allow_exec: true,
        cache: None,
        origin: Layer::default(),
    };
    Some((cap, layer))
}

/// A lenient profile built from an in-progress editor form — no ≥1-capability
/// rule — used only to render the editor's live preview (never staged).
pub fn draft_profile_from_form(pairs: &[(String, String)]) -> ProfileConfig {
    ProfileConfig {
        name: opt(value_of(pairs, "name")).unwrap_or_else(|| "(unnamed)".to_string()),
        targets: values_for(pairs, "targets"),
        capabilities: values_for(pairs, "capabilities")
            .into_iter()
            .map(CapabilityRef::Id)
            .collect(),
        template: None,
        guidance: opt(value_of(pairs, "guidance")),
        disabled: value_of(pairs, "disabled").is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pairs_decodes() {
        let got = parse_pairs("lang=rust&agent=claude&q=a%20b");
        assert_eq!(got[0], ("lang".to_string(), "rust".to_string()));
        assert_eq!(got[2], ("q".to_string(), "a b".to_string()));
    }

    #[test]
    fn simulator_form_updates_and_resets() {
        let mut sim = Simulated {
            agent: "claude".into(),
            lang: None,
            scope: None,
        };
        sim.update_from_form("lang=go&scope=machine&agent=codex");
        assert_eq!(sim.lang.as_deref(), Some("go"));
        assert!(matches!(sim.scope, Some(Scope::Machine)));
        assert_eq!(sim.agent, "codex");
        // Blank lang resets to "use detected".
        sim.update_from_form("lang=&scope=");
        assert!(sim.lang.is_none());
        assert!(sim.scope.is_none());
    }

    #[test]
    fn slug_derives_a_stable_id() {
        assert_eq!(slug("Rust conventions"), "rust-conventions");
        assert_eq!(slug("  Deploy — prod!! "), "deploy-prod");
        assert_eq!(slug("CacheKeys"), "cachekeys");
        assert_eq!(slug("***"), "");
    }

    #[test]
    fn capability_from_form_markdown_new() {
        let cap = capability_from_form(
            None,
            &parse_pairs("name=Rust+conventions&kind=markdown&guidance=Use+clippy"),
        )
        .unwrap();
        assert_eq!(cap.id, "rust-conventions"); // id derived from the name
        assert_eq!(cap.description.as_deref(), Some("Rust conventions"));
        assert_eq!(cap.guidance, "Use clippy");
        assert!(cap.command.is_none());
        assert!(cap.allow_exec); // moot for static, defaults on
                                 // A new capability needs a name.
        assert!(capability_from_form(None, &parse_pairs("kind=markdown&guidance=x")).is_err());
    }

    #[test]
    fn capability_from_form_script_exec_toggle() {
        // Checkbox present → execution allowed.
        let on = capability_from_form(
            None,
            &parse_pairs("name=Deploy&kind=script&command=echo+hi&allow_exec=on"),
        )
        .unwrap();
        assert_eq!(on.command.as_deref(), Some("echo hi"));
        assert!(on.guidance.is_empty());
        assert!(on.allow_exec);
        // Checkbox absent → execution disabled (the off-switch).
        let off = capability_from_form(
            None,
            &parse_pairs("name=Deploy&kind=script&command=echo+hi"),
        )
        .unwrap();
        assert!(!off.allow_exec);
        assert_eq!(on.script_lang.as_deref(), Some("bash"));
        // An empty script falls back to a (markdown) capability, not an error.
        let empty = capability_from_form(None, &parse_pairs("name=X&kind=script")).unwrap();
        assert!(empty.command.is_none());
        assert!(empty.script_lang.is_none());
    }

    #[test]
    fn capability_from_form_edit_preserves_hidden_fields() {
        // A base capability carrying fields the simple editor never shows.
        let mut base = crate::capability::palette()
            .into_iter()
            .find(|c| c.id == "rust-conventions")
            .unwrap();
        base.tags = vec!["stack".into()];
        base.risk = Risk::Caution;
        base.requires = vec!["baseline".into()];
        base.agents = vec!["claude".into()];
        // Editing just the content must not drop tags/risk/requires/agents.
        let edited = capability_from_form(
            Some(&base),
            &parse_pairs("name=Rust+conventions&kind=markdown&guidance=Updated+body"),
        )
        .unwrap();
        assert_eq!(edited.id, "rust-conventions"); // id stays fixed on edit
        assert_eq!(edited.guidance, "Updated body");
        assert_eq!(edited.tags, vec!["stack".to_string()]);
        assert_eq!(edited.risk, Risk::Caution);
        assert_eq!(edited.requires, vec!["baseline".to_string()]);
        assert_eq!(edited.agents, vec!["claude".to_string()]);
    }

    #[test]
    fn profile_from_form_requires_a_capability() {
        let p = profile_from_form(&parse_pairs(
            "name=rust&targets=rust&capabilities=rc&capabilities=terse",
        ))
        .unwrap();
        assert_eq!(p.name, "rust");
        assert_eq!(p.targets, vec!["rust".to_string()]);
        assert_eq!(p.capabilities.len(), 2);
        // Zero capabilities is rejected (§3).
        assert!(profile_from_form(&parse_pairs("name=rust&targets=rust")).is_err());
    }

    #[test]
    fn seed_starters_populates_empty_repo_then_noops() {
        use crate::studio::edit::Session;
        let d = tempfile::tempdir().unwrap();

        // Empty repo (no .rosita dir): first open seeds the shipped palette into
        // config.toml as normal entries.
        let mut s = Session::open(d.path(), None).unwrap();
        let n = seed_starters_if_empty(&mut s).unwrap();
        assert!(n >= 5, "expected the shipped starter set to be seeded");
        let on_disk = std::fs::read_to_string(crate::config::repo_config_path(d.path())).unwrap();
        assert!(on_disk.contains("id = \"baseline\""));
        assert!(on_disk.contains("id = \"rust-conventions\""));

        // A fresh session over the now-populated repo seeds nothing more.
        let mut s2 = Session::open(d.path(), None).unwrap();
        assert_eq!(seed_starters_if_empty(&mut s2).unwrap(), 0);
    }

    #[test]
    fn seed_starters_skips_a_repo_that_already_has_one() {
        use crate::studio::edit::Session;
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(crate::config::repo_dir(d.path())).unwrap();
        std::fs::write(
            crate::config::repo_config_path(d.path()),
            "[[capabilities]]\nid = \"mine\"\nguidance = \"hand-authored\"\n",
        )
        .unwrap();
        let mut s = Session::open(d.path(), None).unwrap();
        assert_eq!(seed_starters_if_empty(&mut s).unwrap(), 0);
        // The user's hand-authored config is untouched.
        let on_disk = std::fs::read_to_string(crate::config::repo_config_path(d.path())).unwrap();
        assert!(on_disk.contains("id = \"mine\""));
        assert!(!on_disk.contains("id = \"baseline\""));
    }

    #[test]
    fn layer_from_form_maps_scope_and_visibility() {
        assert_eq!(layer_from_form(&parse_pairs("scope=repo")), Layer::Repo);
        assert_eq!(
            layer_from_form(&parse_pairs("scope=repo&visibility=private")),
            Layer::RepoLocal
        );
        assert_eq!(layer_from_form(&parse_pairs("scope=global")), Layer::Global);
        assert_eq!(
            layer_from_form(&parse_pairs("scope=global&visibility=private")),
            Layer::GlobalLocal
        );
    }
}
