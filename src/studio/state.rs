//! Studio session state + the read-only "model" computations (selection,
//! ReadOnly overlay preview, the library snapshot) that the HTTP handlers and
//! views render. Kept free of `tiny_http` so it's unit-testable without a socket.
//!
//! Concurrency rule (design §2): handlers take a cheap [`Snapshot`] under the
//! session mutex, release it, then assemble/render **outside** the lock — never
//! hold the mutex across rendering, disk I/O, or probe execution.

use std::path::PathBuf;

use crate::adapters;
use crate::config::Config;
use crate::context::{Context, GitContext, Scope};
use crate::dynamic::DynamicMode;
use crate::fragment::{palette, Fragment, Layer};
use crate::pack::{self, Pack};
use crate::profile::{self, FragmentRef, ProfileConfig, Selection};
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
    /// Armed whenever the first-launch welcome is shown (a fresh config, or the
    /// "?" tour button). While armed, applying a starter pack routes through the
    /// guided "review → you're set" beats instead of dropping straight into the
    /// Profiles tab; the first Apply disarms it.
    pub onboarding_active: bool,
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
    /// How many fragments actually render for `agent` (after agent gating) —
    /// the provenance breadcrumb's count, truthful to what's in the overlay.
    pub fragment_count: usize,
    /// The rendered overlay markdown (header + body). Drives the profile
    /// editor's live preview.
    pub overlay: String,
    /// Per-fragment rendered guidance — the Profiles-tab detail's expandable
    /// cards. One entry per fragment that contributes a section to the overlay.
    pub caps: Vec<PreviewCap>,
    /// A human note when there's no single profile (empty / ambiguous).
    pub note: Option<String>,
}

/// One fragment's rendered guidance for the Profiles-tab detail cards.
pub struct PreviewCap {
    pub id: String,
    pub title: String,
    /// Glyph derived from the fragment's content type (markdown/script/provider).
    pub glyph: &'static str,
    /// Rendered guidance markdown (or the skip note).
    pub markdown: String,
    /// Resolved a dynamic provider/command.
    pub dynamic: bool,
    /// A dynamic command was skipped (e.g. `allow_exec = false`; markdown is the note).
    pub skipped: bool,
    /// True when this id is an editable library fragment (not a synthetic
    /// inline section) — gates the card's "Edit fragment" affordance.
    pub editable: bool,
    /// A dynamic cap that hasn't produced output in this (read-only) preview —
    /// the body is a "runs at render" placeholder, and a "Run" affordance shows.
    pub pending: bool,
}

/// One fragment row for the library view.
pub struct FragmentView {
    pub id: String,
    pub title: String,
    /// A one-line plain-text summary of what the fragment says (the first
    /// meaningful line of its guidance, or a kind-based phrase for dynamic caps).
    pub summary: Option<String>,
    pub kind: &'static str,
    /// Primary category for grouping the library.
    pub category: Option<String>,
    /// Interpreter for a script cap (`bash`/`sh`/`python`); drives the badge.
    pub script_lang: Option<String>,
    /// True when authored in a `*local.toml` layer (private / gitignored).
    pub private: bool,
    /// Composed into the current preview overlay.
    pub active: bool,
}

/// Whether a profile's referenced fragment id resolves to something that
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

/// One "atom dot" on a profile card: a referenced fragment and how it resolves.
pub struct AtomDot {
    pub id: String,
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
    pub fragments: Vec<String>,
    /// Resolved composition atoms, in declared order (drives the card's dots).
    pub atoms: Vec<AtomDot>,
}

/// The whole left-pane library snapshot for a context.
pub struct LibraryView {
    pub yours: Vec<FragmentView>,
    pub palette: Vec<FragmentView>,
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

/// Render a specific profile (by name) composed for `agent`. `mode` is
/// [`DynamicMode::ReadOnly`] for the normal preview (dynamic caps don't execute)
/// or [`DynamicMode::Live`] to run scripts/providers now (the "Run" affordance).
pub fn render_profile(
    snap: &Snapshot,
    profile_name: &str,
    agent: &str,
    mode: DynamicMode,
) -> crate::Result<PreviewOutcome> {
    let cfg = staged_config(snap)?;
    let profile = cfg
        .profiles
        .iter()
        .find(|p| p.name == profile_name)
        .ok_or_else(|| anyhow::anyhow!("unknown profile '{profile_name}'"))?
        .clone();
    render_profile_config(snap, &profile, agent, mode)
}

/// Render an arbitrary (possibly unsaved/draft) profile composed for `agent`.
/// The context is synthesized from the profile's own targets so its fragments
/// gate as intended. Used by the Profiles-tab preview and the editor's live draft.
/// `mode` gates dynamic execution (ReadOnly = placeholder cards; Live = run now).
pub fn render_profile_config(
    snap: &Snapshot,
    profile: &ProfileConfig,
    agent: &str,
    mode: DynamicMode,
) -> crate::Result<PreviewOutcome> {
    let cfg = staged_config(snap)?;
    render_profile_in_config(&cfg, &snap.base_context, profile, agent, mode)
}

/// Render a starter pack's profile as a preview document, **before** applying.
/// The pack's palette fragments aren't owned yet, so the library is augmented
/// with the palette versions of the pack's ids that aren't already in the config
/// (exactly what `apply_pack` would duplicate in) so each section renders.
pub fn render_pack(
    snap: &Snapshot,
    pack: &Pack,
    agent: &str,
    mode: DynamicMode,
) -> crate::Result<PreviewOutcome> {
    let mut cfg = staged_config(snap)?;
    let owned: std::collections::HashSet<String> =
        cfg.fragments.iter().map(|c| c.id.clone()).collect();
    let extra: Vec<Fragment> = palette()
        .into_iter()
        .filter(|p| pack.fragments.contains(&p.id.as_str()) && !owned.contains(&p.id))
        .collect();
    cfg.fragments.extend(extra);
    render_profile_in_config(&cfg, &snap.base_context, &pack.profile(), agent, mode)
}

/// Compose + render `profile` against an explicit `cfg` (its fragment library +
/// agent/template config). Shared by the live profile preview and the pack
/// preview (which augments `cfg` with the pack's not-yet-owned palette caps).
fn render_profile_in_config(
    cfg: &Config,
    base_context: &Context,
    profile: &ProfileConfig,
    agent: &str,
    mode: DynamicMode,
) -> crate::Result<PreviewOutcome> {
    let ctx = context_for_profile(base_context, profile);
    let composition = profile::compose_profile(&ctx, profile, &cfg.fragments, &cfg.fragment_params);

    let agent_id = if agent.is_empty() {
        cfg.default_agent.clone()
    } else {
        agent.to_string()
    };
    let descriptor = adapters::descriptor(cfg, &agent_id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("unknown agent '{agent_id}'"))?;
    let out = render::render(&RenderRequest {
        agent: &descriptor.id,
        template_name: &descriptor.template,
        context: &ctx,
        composition: &composition,
        config: cfg,
        generated_at: now_rfc3339(),
        dynamic: mode,
    })?;
    let fragment_count = composition
        .fragments
        .iter()
        .filter(|rc| rc.fragment.applies_to_agent(&agent_id))
        .count();
    // Per-fragment cards. Built from the *composition* (every cap the profile
    // pulls in, agent-filtered) rather than only the rendered sections: a dynamic
    // cap that resolves to nothing in ReadOnly (its provider/command doesn't run
    // and there's no cache) is dropped from the overlay, which would hide its
    // card. Such caps get a "runs at render" placeholder so the preview still
    // lists — and can open/edit — them. Each cap's glyph is derived from its
    // content type; editability is looked up from the staged library.
    let rendered: std::collections::HashMap<&str, _> =
        out.fragments.iter().map(|c| (c.id.as_str(), c)).collect();
    let caps: Vec<PreviewCap> = composition
        .fragments
        .iter()
        .filter(|rc| rc.fragment.applies_to_agent(&agent_id))
        .filter_map(|rc| {
            let cap = &rc.fragment;
            let owned = cfg.fragments.iter().find(|x| x.id == cap.id);
            let editable = owned.is_some();
            let glyph = type_glyph(
                kind_of(cap.command.is_some(), cap.provider.is_some()),
                cap.script_lang.as_deref(),
            );
            if let Some(c) = rendered.get(cap.id.as_str()) {
                Some(PreviewCap {
                    glyph,
                    editable,
                    id: c.id.clone(),
                    title: c.title.clone(),
                    markdown: c.body.clone(),
                    dynamic: c.dynamic,
                    skipped: c.skipped,
                    pending: false,
                })
            } else if cap.is_dynamic() {
                Some(PreviewCap {
                    glyph,
                    editable,
                    id: cap.id.clone(),
                    title: cap.title().to_string(),
                    markdown: "_Dynamic — runs at render; no preview output yet._".to_string(),
                    dynamic: true,
                    skipped: false,
                    pending: true,
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
        fragment_count,
        overlay: out.content,
        caps,
        note: profile
            .disabled
            .then(|| "This profile is disabled — it won't be selected in real runs.".to_string()),
    })
}

/// Execute one dynamic fragment **now** (Live), in `profile_name`'s context, so
/// its (redacted) output lands in the on-disk cache. A subsequent ReadOnly
/// preview then surfaces that cached output. This is the per-card "Run" action;
/// execution is gated by `allow_exec` (a disabled command leaves the cache empty
/// and the re-render shows the skip note). Resolving a non-dynamic id is a
/// harmless no-op.
pub fn run_fragment(snap: &Snapshot, profile_name: &str, fragment_id: &str) -> crate::Result<()> {
    let cfg = staged_config(snap)?;
    let cap = cfg
        .fragments
        .iter()
        .find(|c| c.id == fragment_id)
        .ok_or_else(|| anyhow::anyhow!("unknown fragment '{fragment_id}'"))?;
    let ctx = match cfg.profiles.iter().find(|p| p.name == profile_name) {
        Some(p) => context_for_profile(&snap.base_context, p),
        None => snap.base_context.clone(),
    };
    // Live resolve runs the provider/command and writes the cache as a side
    // effect; the returned value (or skip note) is surfaced by the re-render.
    let _ = crate::dynamic::resolve(
        cap,
        &ctx,
        &ctx.repo_base,
        DynamicMode::Live,
        chrono::Utc::now(),
    );
    Ok(())
}

/// Synthesize a context from a profile's targets so its fragments gate as
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
            profile::compose_profile(&ctx, p, &cfg.fragments, &cfg.fragment_params)
                .fragments
                .iter()
                .map(|rc| rc.fragment.id.clone())
                .collect()
        }
        _ => vec![],
    };

    let tags = ctx.selection_targets();
    let yours = cfg
        .fragments
        .iter()
        .map(|c| FragmentView {
            kind: kind_of(c.command.is_some(), c.provider.is_some()),
            category: c.category.clone(),
            active: active_ids.contains(&c.id),
            title: c.title().to_string(),
            summary: fragment_summary(c),
            script_lang: c.script_lang.clone(),
            private: matches!(c.origin, Layer::RepoLocal | Layer::GlobalLocal),
            id: c.id.clone(),
        })
        .collect();
    // The palette (called once; the local `palette` below would shadow the fn).
    let palette_items = palette();
    // Resolve each profile's fragment refs to provenance for the atom dots:
    // owned (contributes) vs palette-only (named but not duplicated, so it
    // renders nothing) vs unknown. No extra requests — all from the snapshot.
    let owned_ids: std::collections::HashSet<&str> =
        cfg.fragments.iter().map(|c| c.id.as_str()).collect();
    let palette_ids: std::collections::HashSet<&str> =
        palette_items.iter().map(|c| c.id.as_str()).collect();
    let resolve_atom = |id: String| -> AtomDot {
        let state = if owned_ids.contains(id.as_str()) {
            AtomState::Owned
        } else if palette_ids.contains(id.as_str()) {
            AtomState::Palette
        } else {
            AtomState::Unknown
        };
        AtomDot { id, state }
    };
    // Palette items not already owned (by id) in your library.
    let palette: Vec<FragmentView> = palette_items
        .iter()
        .filter(|c| !owned_ids.contains(c.id.as_str()))
        .map(|c| FragmentView {
            kind: kind_of(c.command.is_some(), c.provider.is_some()),
            category: c.category.clone(),
            active: false,
            title: c.title().to_string(),
            summary: fragment_summary(c),
            script_lang: c.script_lang.clone(),
            private: false,
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
            fragments: p.fragments.iter().map(|r| r.id().to_string()).collect(),
            atoms: p
                .fragments
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

/// First-launch onboarding readout for a fresh config (no profiles **and** no
/// own fragments yet): what rosita detected here, plus a "quick start"
/// suggestion — a starter profile pre-filled from the detected target and a few
/// palette fragments. The palette items are only *suggested*; quick-start
/// duplicates them into your library (staged) so you own and can edit them.
pub struct Onboarding {
    /// The detected primary stack (`rust`, `node`, …), or `None` when none was
    /// recognized / outside a repo.
    pub stack: Option<String>,
    /// Repo vs machine, derived from the detected context.
    pub scope: Scope,
    /// The current branch, when in a repo.
    pub branch: Option<String>,
    /// Suggested profile name (the detected target, `machine`, or `project`).
    pub name: String,
    /// Suggested `targets` (the detected target, or empty when none was found).
    pub targets: Vec<String>,
    /// Palette fragment ids the quick-start composes (each duplicated into the
    /// library on use). Already filtered to real palette ids and de-duplicated.
    pub caps: Vec<String>,
}

/// Compute the [`Onboarding`] suggestion for a detected context. The mapping is
/// deliberately coarse: the matching `<stack>-conventions` palette cap (or
/// `infra-caution` for the machine context), plus the universal `terse-comms`
/// and `conventional-commits` starters.
pub fn onboarding(base: &Context) -> Onboarding {
    let scope = base.scope();
    let stack = base.stacks.first().cloned();
    let branch = base.git.as_ref().and_then(|g| g.branch.clone());
    let (name, targets) = match (&stack, scope) {
        (Some(s), _) => (s.clone(), vec![s.clone()]),
        (None, Scope::Machine) => ("machine".to_string(), vec!["machine".to_string()]),
        (None, Scope::Repo) => ("project".to_string(), Vec::new()),
    };
    let stack_cap = match stack.as_deref() {
        Some("rust") => Some("rust-conventions"),
        Some("nextjs") => Some("nextjs-conventions"),
        Some("node") => Some("node-conventions"),
        Some("go") => Some("go-conventions"),
        Some("python") => Some("python-conventions"),
        _ => None,
    };
    let mut caps: Vec<String> = Vec::new();
    if let Some(c) = stack_cap {
        caps.push(c.to_string());
    } else if scope == Scope::Machine {
        caps.push("infra-caution".to_string());
    }
    caps.push("terse-comms".to_string());
    caps.push("conventional-commits".to_string());
    // Keep only ids that really exist in the palette, de-duplicated, order-stable.
    let pal: std::collections::HashSet<String> = palette().into_iter().map(|c| c.id).collect();
    let mut seen = std::collections::HashSet::new();
    caps.retain(|id| pal.contains(id) && seen.insert(id.clone()));
    Onboarding {
        stack,
        scope,
        branch,
        name,
        targets,
        caps,
    }
}

/// One starter-pack card for the gallery: the pack's metadata plus resolved
/// atom dots and whether it's already been applied in this context.
pub struct PackView {
    pub id: String,
    pub name: String,
    pub description: String,
    pub icon: String,
    /// True when this pack matches the detected context (badged + ordered first).
    pub recommended: bool,
    /// True when a profile with this pack's name already exists in the staged
    /// config (the card shows an "applied" state instead of an "Apply" button).
    pub applied: bool,
    /// One atom dot per composed fragment (owned vs palette-only vs unknown).
    pub atoms: Vec<AtomDot>,
}

/// The pack recommended for the snapshot's detected context, if any (the first
/// pack whose `recommended_for` matches a selection target).
pub fn recommended_pack(snap: &Snapshot) -> Option<Pack> {
    let ctx = simulated_context(&snap.base_context, &snap.sim);
    let targets = ctx.selection_targets();
    pack::packs()
        .into_iter()
        .find(|p| targets.iter().any(|t| p.is_recommended_for(t)))
}

/// Build the starter-pack gallery for the snapshot's (simulated) context:
/// recommended packs first, each with its fragments' atom dots and an
/// already-applied flag. No probes — purely from the staged snapshot.
pub fn pack_views(snap: &Snapshot) -> crate::Result<Vec<PackView>> {
    let cfg = staged_config(snap)?;
    let ctx = simulated_context(&snap.base_context, &snap.sim);
    let targets = ctx.selection_targets();

    let owned_ids: std::collections::HashSet<&str> =
        cfg.fragments.iter().map(|c| c.id.as_str()).collect();
    let palette_items = palette();
    let palette_ids: std::collections::HashSet<&str> =
        palette_items.iter().map(|c| c.id.as_str()).collect();
    let resolve_atom = |id: &str| -> AtomDot {
        let state = if owned_ids.contains(id) {
            AtomState::Owned
        } else if palette_ids.contains(id) {
            AtomState::Palette
        } else {
            AtomState::Unknown
        };
        AtomDot {
            id: id.to_string(),
            state,
        }
    };
    let existing: std::collections::HashSet<&str> =
        cfg.profiles.iter().map(|p| p.name.as_str()).collect();

    let mut views: Vec<PackView> = pack::packs()
        .into_iter()
        .map(|p| PackView {
            recommended: targets.iter().any(|t| p.is_recommended_for(t)),
            applied: existing.contains(p.profile_name),
            atoms: p.fragments.iter().map(|c| resolve_atom(c)).collect(),
            id: p.id.to_string(),
            name: p.name.to_string(),
            description: p.description.to_string(),
            icon: p.icon.to_string(),
        })
        .collect();
    // Recommended packs first; catalog order is otherwise preserved (stable sort).
    views.sort_by_key(|v| !v.recommended);
    Ok(views)
}

/// One profile a staged op will create, summarized for the onboarding review.
pub struct ProfileBrief {
    pub name: String,
    pub targets: Vec<String>,
}

/// A friendly, human-readable rollup of what the current staged ops will add —
/// fed to the guided onboarding "review what will change" beat. Counts only
/// additive fragment ops (so "will add N fragments" reads true) and lists the
/// profiles being created/replaced with their targets.
pub struct StagedSummary {
    pub fragments_added: usize,
    pub profiles: Vec<ProfileBrief>,
}

pub fn staged_summary(session: &Session) -> StagedSummary {
    let mut fragments_added = 0usize;
    let mut profiles = Vec::new();
    for op in session.ops() {
        match op {
            StagedOp::CreateFragment { .. } | StagedOp::DuplicatePaletteItem { .. } => {
                fragments_added += 1;
            }
            StagedOp::CreateProfile { profile, .. } | StagedOp::EditProfile { profile, .. } => {
                profiles.push(ProfileBrief {
                    name: profile.name.clone(),
                    targets: profile.targets.clone(),
                });
            }
            _ => {}
        }
    }
    StagedSummary {
        fragments_added,
        profiles,
    }
}

/// Apply a starter pack into the staged session: duplicate each of its palette
/// fragments that isn't already owned, then create (or replace) its profile.
/// Everything is staged — the user reviews the diff and Applies like any edit.
/// Used by both the gallery's per-pack Apply and the recommended-pack quick start.
pub fn apply_pack(session: &mut Session, pack: &Pack) -> crate::Result<()> {
    let owned: std::collections::HashSet<String> = session
        .staged_config()
        .map(|cfg| cfg.fragments.iter().map(|c| c.id.clone()).collect())
        .unwrap_or_default();
    for id in pack.fragments {
        if owned.contains(*id) {
            continue;
        }
        session.stage(StagedOp::DuplicatePaletteItem {
            id: id.to_string(),
            to_layer: Layer::Global,
        })?;
    }
    let exists = session
        .staged_config()
        .map(|cfg| cfg.profiles.iter().any(|p| p.name == pack.profile_name))
        .unwrap_or(false);
    let profile = Box::new(pack.profile());
    if exists {
        session.stage(StagedOp::EditProfile {
            layer: Layer::Global,
            name: pack.profile_name.to_string(),
            profile,
        })?;
    } else {
        session.stage(StagedOp::CreateProfile {
            layer: Layer::Global,
            profile,
        })?;
    }
    Ok(())
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

/// The studio glyph for a fragment, derived from its content type rather than a
/// user-chosen icon: a document for static markdown, a terminal for shell
/// scripts, `code` for Python, and a bolt for a live provider. (Bash vs POSIX
/// `sh` is distinguished by the separate script-lang badge.)
pub(crate) fn type_glyph(kind: &str, script_lang: Option<&str>) -> &'static str {
    match kind {
        "provider" => "bolt",
        "command" => match script_lang {
            Some("python") => "code",
            _ => "terminal",
        },
        _ => "file",
    }
}

/// A one-line, plain-text summary for a fragment card — the "what it says" the
/// title alone can't carry. For a dynamic cap (whose guidance is often a
/// `{{ provider.output }}` template) we describe the source instead of dumping
/// the template; otherwise it's the first meaningful line of the guidance.
fn fragment_summary(c: &Fragment) -> Option<String> {
    if c.command.is_some() {
        return Some("Runs a script; its output is embedded.".to_string());
    }
    if let Some(p) = &c.provider {
        return Some(format!("Embeds live {p} output."));
    }
    first_meaningful_line(&c.guidance)
}

/// The first non-empty line of markdown as plain-ish text: skip leading HTML
/// comments and blanks, drop a leading heading/bullet marker, collapse runs of
/// whitespace to single spaces. `None` when there's nothing to show.
fn first_meaningful_line(md: &str) -> Option<String> {
    let mut text = md.trim_start();
    // Skip leading HTML comments (e.g. a generated header).
    while let Some(rest) = text.strip_prefix("<!--") {
        match rest.find("-->") {
            Some(end) => text = rest[end + 3..].trim_start(),
            None => break,
        }
    }
    for raw in text.lines() {
        let line = raw
            .trim()
            .trim_start_matches('#')
            .trim_start_matches(['-', '*', '>'])
            .trim();
        if line.is_empty() {
            continue;
        }
        return Some(line.split_whitespace().collect::<Vec<_>>().join(" "));
    }
    None
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

// --- form → typed model (the write engine) -----------------------------------

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

/// Which writable layer a form's `visibility` control selects. Fragments and
/// profiles are global-only, so studio always authors into a global layer:
/// `config.toml` (shared) or, when marked private, `local.toml` (real hostnames
/// and other machine-specific values). A repo layer is never a write target.
pub fn layer_from_form(pairs: &[(String, String)]) -> Layer {
    if value_of(pairs, "visibility") == Some("private") {
        Layer::GlobalLocal
    } else {
        Layer::Global
    }
}

/// The fragment id an editor submission targets: the readonly `id` field when
/// editing, otherwise the slug of the `name` field (a new fragment).
pub fn editor_fragment_id(pairs: &[(String, String)]) -> Option<String> {
    if let Some(id) = opt(value_of(pairs, "id")) {
        return Some(id);
    }
    opt(value_of(pairs, "name")).map(|n| slug(&n))
}

/// Slugify a display name into a stable fragment id (lowercase, alphanumeric
/// runs joined by single hyphens). Used to derive a new fragment's id.
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

/// Whether a fragment is too rich for studio's content-first editor and must
/// be hand-edited as TOML: a built-in `provider`, or a `command` *and* a custom
/// guidance template (the simple "markdown OR script" form can't represent it
/// without clobbering one side).
pub fn is_advanced_fragment(c: &Fragment) -> bool {
    c.provider.is_some() || (c.command.is_some() && !c.guidance.trim().is_empty())
}

/// Build a [`Fragment`] from the content-first editor form. `base` is the
/// existing fragment when editing — fields the simple form doesn't expose
/// (requires, agents, cache, provider, when, params) are preserved from it, so
/// editing never silently drops them. `origin` is left default; it
/// is re-tagged by layer when the staged config is assembled.
pub fn fragment_from_form(
    base: Option<&Fragment>,
    pairs: &[(String, String)],
) -> crate::Result<Fragment> {
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
    // `category` is editable in the form. When the field is present in the post
    // we honor it (even cleared); when absent we preserve the base fragment's
    // value, so the simple editor never drops advanced metadata.
    let present = |key: &str| pairs.iter().any(|(k, _)| k == key);
    let category = if present("category") {
        opt(value_of(pairs, "category"))
    } else {
        base.and_then(|c| c.category.clone())
    };
    Ok(Fragment {
        id,
        description: name.or_else(|| base.and_then(|c| c.description.clone())),
        category,
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
/// fragment rule (§3) — a profile with no fragments can't be saved.
pub fn profile_from_form(pairs: &[(String, String)]) -> crate::Result<ProfileConfig> {
    let name =
        opt(value_of(pairs, "name")).ok_or_else(|| anyhow::anyhow!("profile name is required"))?;
    let fragments: Vec<FragmentRef> = values_for(pairs, "fragments")
        .into_iter()
        .map(FragmentRef::Id)
        .collect();
    if fragments.is_empty() {
        anyhow::bail!("a profile needs at least one fragment");
    }
    Ok(ProfileConfig {
        name,
        targets: values_for(pairs, "targets"),
        fragments,
        template: opt(value_of(pairs, "template")),
        disabled: value_of(pairs, "disabled").is_some(),
    })
}

/// Build a fragment + its target layer from the profile editor's inline
/// quick-create fields (`fragment_name`/`fragment_kind`/`fragment_content`/`fragment_private`).
/// Returns `None` when no name was typed (nothing to add).
pub fn inline_fragment_from_form(pairs: &[(String, String)]) -> Option<(Fragment, Layer)> {
    let name = opt(value_of(pairs, "fragment_name"))?;
    let id = slug(&name);
    if id.is_empty() {
        return None;
    }
    let content = value_of(pairs, "fragment_content")
        .unwrap_or("")
        .to_string();
    let (guidance, command, script_lang) =
        if value_of(pairs, "fragment_kind") == Some("script") && !content.trim().is_empty() {
            (String::new(), Some(content), Some("bash".to_string()))
        } else {
            (content, None, None)
        };
    let layer = if value_of(pairs, "fragment_private").is_some() {
        Layer::GlobalLocal
    } else {
        Layer::Global
    };
    let cap = Fragment {
        id,
        description: Some(name),
        category: None,
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

/// A lenient profile built from an in-progress editor form — no ≥1-fragment
/// rule — used only to render the editor's live preview (never staged).
pub fn draft_profile_from_form(pairs: &[(String, String)]) -> ProfileConfig {
    ProfileConfig {
        name: opt(value_of(pairs, "name")).unwrap_or_else(|| "(unnamed)".to_string()),
        targets: values_for(pairs, "targets"),
        fragments: values_for(pairs, "fragments")
            .into_iter()
            .map(FragmentRef::Id)
            .collect(),
        template: None,
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
    fn summary_takes_first_meaningful_guidance_line() {
        let mut c = crate::fragment::palette()
            .into_iter()
            .find(|c| c.id == "terse-comms")
            .unwrap();
        let s = fragment_summary(&c).expect("static cap has a summary");
        assert!(s.starts_with("Be terse"), "got: {s}");
        assert!(!s.contains('\n'), "summary is a single line");
        // A leading HTML comment + heading marker are stripped, whitespace collapsed.
        c.guidance = "<!-- gen -->\n# Heading\n\nbody".into();
        assert_eq!(fragment_summary(&c).as_deref(), Some("Heading"));
    }

    #[test]
    fn summary_describes_dynamic_caps_by_source() {
        let mut c = crate::fragment::palette().into_iter().next().unwrap();
        // A provider cap's guidance is often a template — describe the source.
        c.guidance = "Running on {{ provider.output }}".into();
        c.provider = Some("host".into());
        assert_eq!(
            fragment_summary(&c).as_deref(),
            Some("Embeds live host output.")
        );
        c.provider = None;
        c.command = Some("uname -sm".into());
        assert_eq!(
            fragment_summary(&c).as_deref(),
            Some("Runs a script; its output is embedded.")
        );
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
    fn fragment_from_form_markdown_new() {
        let cap = fragment_from_form(
            None,
            &parse_pairs("name=Rust+conventions&kind=markdown&guidance=Use+clippy"),
        )
        .unwrap();
        assert_eq!(cap.id, "rust-conventions"); // id derived from the name
        assert_eq!(cap.description.as_deref(), Some("Rust conventions"));
        assert_eq!(cap.guidance, "Use clippy");
        assert!(cap.command.is_none());
        assert!(cap.allow_exec); // moot for static, defaults on
                                 // A new fragment needs a name.
        assert!(fragment_from_form(None, &parse_pairs("kind=markdown&guidance=x")).is_err());
    }

    #[test]
    fn fragment_from_form_script_exec_toggle() {
        // Checkbox present → execution allowed.
        let on = fragment_from_form(
            None,
            &parse_pairs("name=Deploy&kind=script&command=echo+hi&allow_exec=on"),
        )
        .unwrap();
        assert_eq!(on.command.as_deref(), Some("echo hi"));
        assert!(on.guidance.is_empty());
        assert!(on.allow_exec);
        // Checkbox absent → execution disabled (the off-switch).
        let off = fragment_from_form(
            None,
            &parse_pairs("name=Deploy&kind=script&command=echo+hi"),
        )
        .unwrap();
        assert!(!off.allow_exec);
        assert_eq!(on.script_lang.as_deref(), Some("bash"));
        // An empty script falls back to a (markdown) fragment, not an error.
        let empty = fragment_from_form(None, &parse_pairs("name=X&kind=script")).unwrap();
        assert!(empty.command.is_none());
        assert!(empty.script_lang.is_none());
    }

    #[test]
    fn fragment_from_form_parses_category() {
        let cap = fragment_from_form(
            None,
            &parse_pairs("name=Guardrails&kind=markdown&guidance=g&category=Operating+Style"),
        )
        .unwrap();
        assert_eq!(cap.category.as_deref(), Some("Operating Style"));
    }

    #[test]
    fn fragment_from_form_present_field_clears_absent_field_preserves() {
        // Start from a fragment that has category set.
        let base = fragment_from_form(
            None,
            &parse_pairs("name=X&kind=markdown&guidance=g&category=Safety"),
        )
        .unwrap();
        // A post that *includes* an empty category clears it.
        let cleared = fragment_from_form(
            Some(&base),
            &parse_pairs("id=x&kind=markdown&guidance=g&category="),
        )
        .unwrap();
        assert_eq!(cleared.category, None);
        // A post that *omits* the field entirely preserves the base value
        // (the simple editor never silently drops metadata).
        let preserved =
            fragment_from_form(Some(&base), &parse_pairs("id=x&kind=markdown&guidance=g")).unwrap();
        assert_eq!(preserved.category.as_deref(), Some("Safety"));
    }

    #[test]
    fn fragment_from_form_edit_preserves_hidden_fields() {
        // A base fragment carrying fields the simple editor never shows.
        let mut base = crate::fragment::palette()
            .into_iter()
            .find(|c| c.id == "rust-conventions")
            .unwrap();
        base.requires = vec!["baseline".into()];
        base.agents = vec!["claude".into()];
        // Editing just the content must not drop requires/agents.
        let edited = fragment_from_form(
            Some(&base),
            &parse_pairs("name=Rust+conventions&kind=markdown&guidance=Updated+body"),
        )
        .unwrap();
        assert_eq!(edited.id, "rust-conventions"); // id stays fixed on edit
        assert_eq!(edited.guidance, "Updated body");
        assert_eq!(edited.requires, vec!["baseline".to_string()]);
        assert_eq!(edited.agents, vec!["claude".to_string()]);
    }

    #[test]
    fn profile_from_form_requires_a_fragment() {
        let p = profile_from_form(&parse_pairs(
            "name=rust&targets=rust&fragments=rc&fragments=terse",
        ))
        .unwrap();
        assert_eq!(p.name, "rust");
        assert_eq!(p.targets, vec!["rust".to_string()]);
        assert_eq!(p.fragments.len(), 2);
        // Zero fragments is rejected (§3).
        assert!(profile_from_form(&parse_pairs("name=rust&targets=rust")).is_err());
    }

    #[test]
    fn layer_from_form_is_global_only() {
        // Authoring always targets a global layer; visibility picks public vs
        // private. The `scope` field is ignored (a repo is never a write target).
        assert_eq!(layer_from_form(&parse_pairs("")), Layer::Global);
        assert_eq!(layer_from_form(&parse_pairs("scope=repo")), Layer::Global);
        assert_eq!(
            layer_from_form(&parse_pairs("visibility=private")),
            Layer::GlobalLocal
        );
        assert_eq!(
            layer_from_form(&parse_pairs("scope=repo&visibility=private")),
            Layer::GlobalLocal
        );
    }
}
