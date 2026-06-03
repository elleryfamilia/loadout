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
use crate::profile::{self, CapabilityRef, Composition, ProfileConfig, Selection};
use crate::render::{self, RenderRequest};
use crate::studio::edit::Session;

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
    /// The rendered overlay markdown (header + body).
    pub overlay: String,
    /// A human note when there's no single profile (empty / ambiguous).
    pub note: Option<String>,
}

/// One capability row for the library view.
pub struct CapView {
    pub id: String,
    pub title: String,
    pub kind: &'static str,
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
    pub capabilities: Vec<String>,
    /// Resolved composition atoms, in declared order (drives the card's dots).
    pub atoms: Vec<AtomDot>,
}

/// Where the user is in the capability→profile→bound pipeline. Drives the
/// center welcome card and the library's empty-state copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingStage {
    /// No owned capabilities and no profiles yet.
    Empty,
    /// Owned capabilities exist, but no profile yet.
    HasCaps,
    /// Profiles exist, but none binds to the current context.
    HasProfile,
    /// A profile binds to the current simulated context.
    Bound,
}

impl OnboardingStage {
    /// Compute the stage from the library snapshot + the resolved binding.
    pub fn of(lib: &LibraryView, binding: &BindingState) -> OnboardingStage {
        if matches!(binding, BindingState::Bound(_)) {
            OnboardingStage::Bound
        } else if !lib.profiles.is_empty() {
            OnboardingStage::HasProfile
        } else if !lib.yours.is_empty() {
            OnboardingStage::HasCaps
        } else {
            OnboardingStage::Empty
        }
    }
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

/// Render the overlay for the snapshot's simulated context in **ReadOnly** mode
/// (never executes providers/commands). Selection drives which profile (if any)
/// is composed.
pub fn render_preview(snap: &Snapshot) -> crate::Result<PreviewOutcome> {
    let cfg = staged_config(snap)?;
    let ctx = simulated_context(&snap.base_context, &snap.sim);
    let selection = select_for(&cfg, &ctx);

    let (composition, note) = match &selection {
        Selection::Use(p) => (
            profile::compose_profile(&ctx, p, &cfg.capabilities, &cfg.capability_params),
            None,
        ),
        Selection::None => (
            Composition::default(),
            Some("No profile applies to this context — the overlay is empty.".to_string()),
        ),
        Selection::Ambiguous(cands) => {
            let names: Vec<&str> = cands.iter().map(|p| p.name.as_str()).collect();
            (
                Composition::default(),
                Some(format!(
                    "{} profiles match ({}). Pick one with `rosita run` to bind it.",
                    cands.len(),
                    names.join(", ")
                )),
            )
        }
    };

    let agent_id = if snap.sim.agent.is_empty() {
        cfg.default_agent.clone()
    } else {
        snap.sim.agent.clone()
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

    let binding = match &selection {
        Selection::Use(p) => BindingState::Bound(p.name.clone()),
        Selection::None => BindingState::None,
        Selection::Ambiguous(cands) => BindingState::Ambiguous(cands.len()),
    };
    // Count only the caps that actually render for this agent (matches the
    // renderer's own `applies_to_agent` gate) so the breadcrumb never overstates.
    let cap_count = composition
        .capabilities
        .iter()
        .filter(|rc| rc.capability.applies_to_agent(&agent_id))
        .count();

    Ok(PreviewOutcome {
        agent: agent_id,
        profile_label: composition.label().to_string(),
        binding,
        context_summary: context_summary(&ctx),
        cap_count,
        overlay: out.content,
        note,
    })
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
            active: active_ids.contains(&c.id),
            title: c.title().to_string(),
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
            active: false,
            title: c.title().to_string(),
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

/// Split a comma-separated field into trimmed, non-empty entries.
fn comma_list(s: Option<&str>) -> Vec<String> {
    s.unwrap_or("")
        .split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
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

/// Build a [`Capability`] from a posted editor form. `origin` is left default —
/// it is re-tagged by layer when the staged config is assembled.
pub fn capability_from_form(pairs: &[(String, String)]) -> crate::Result<Capability> {
    let id =
        opt(value_of(pairs, "id")).ok_or_else(|| anyhow::anyhow!("capability id is required"))?;
    let risk = match value_of(pairs, "risk") {
        Some("caution") => Risk::Caution,
        Some("dangerous") => Risk::Dangerous,
        _ => Risk::Info,
    };
    Ok(Capability {
        id,
        description: opt(value_of(pairs, "description")),
        tags: comma_list(value_of(pairs, "tags")),
        risk,
        when: Vec::new(), // `when` self-gates are hand-edited (not in the form yet)
        requires: comma_list(value_of(pairs, "requires")),
        params: toml::Value::Table(Default::default()),
        guidance: value_of(pairs, "guidance").unwrap_or("").to_string(),
        agents: comma_list(value_of(pairs, "agents")),
        provider: opt(value_of(pairs, "provider")),
        command: opt(value_of(pairs, "command")),
        cache: opt(value_of(pairs, "cache")),
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
    })
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
    fn capability_from_form_parses_core_fields() {
        let cap = capability_from_form(&parse_pairs(
            "id=rc&description=Rust&tags=stack%2Cfast&risk=caution&guidance=Use+clippy",
        ))
        .unwrap();
        assert_eq!(cap.id, "rc");
        assert_eq!(cap.description.as_deref(), Some("Rust"));
        assert_eq!(cap.tags, vec!["stack".to_string(), "fast".to_string()]);
        assert_eq!(cap.risk, Risk::Caution);
        assert_eq!(cap.guidance, "Use clippy");
        // Missing id is rejected.
        assert!(capability_from_form(&parse_pairs("guidance=x")).is_err());
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
