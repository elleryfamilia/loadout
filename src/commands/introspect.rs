//! `load fragments` / `profiles` / `agents` — introspect the resolved
//! configuration and what's active for the current context.
//!
//! These are read-only debugging aids: they run the same config load + context
//! detection + composition as a render (via [`super::prepare`]) and print the
//! library plus which fragments/profiles are active here.

use anyhow::bail;
use serde::Serialize;

use super::{prepare, Runtime};
use crate::adapters::AgentDescriptor;
use crate::cli::{AgentsArgs, FragmentsAction, FragmentsArgs, ListArgs, ListKind, ProfilesArgs};
use crate::fragment::{Fragment, Layer};
use crate::profile::{self, LoadoutConfig};

// --- list (consolidated front door) -----------------------------------------

/// Entry point for `load list [kind]` — one command over the introspection
/// views. `loadouts` is the default; the others map to the standalone commands.
pub fn list(rt: &Runtime, args: &ListArgs) -> crate::Result<()> {
    match args.kind {
        ListKind::Loadouts => profiles(rt, &ProfilesArgs { json: args.json }),
        ListKind::Fragments => fragments(
            rt,
            &FragmentsArgs {
                action: None,
                json: args.json,
            },
        ),
        ListKind::Agents => agents(rt, &AgentsArgs { json: args.json }),
        ListKind::Targets => targets(rt, args.json),
    }
}

// --- targets -----------------------------------------------------------------

#[derive(Serialize)]
struct TargetRow {
    id: String,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    active: bool,
}

/// `load list targets` — the built-in detection targets plus any custom ones,
/// marking which apply to the current project.
fn targets(rt: &Runtime, json: bool) -> crate::Result<()> {
    use std::collections::HashSet;
    let prep = prepare(rt)?;
    let active: HashSet<String> = prep.context.selection_targets().into_iter().collect();

    let mut rows: Vec<TargetRow> = Vec::new();
    for t in crate::target::builtin_targets() {
        let is_active = active.contains(&t.id);
        rows.push(TargetRow {
            id: t.id,
            kind: "built-in",
            description: t.description,
            active: is_active,
        });
    }
    for t in &prep.config.targets {
        rows.push(TargetRow {
            id: t.id.clone(),
            kind: "custom",
            description: t.description.clone(),
            active: active.contains(&t.id),
        });
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    let active_n = rows.iter().filter(|r| r.active).count();
    println!(
        "Targets ({} known, {active_n} active for this context)",
        rows.len()
    );
    for r in &rows {
        let mark = if r.active { "●" } else { "·" };
        let tag = if r.kind == "custom" { "  (custom)" } else { "" };
        let desc = match &r.description {
            Some(d) if !d.is_empty() => format!(" — {d}"),
            _ => String::new(),
        };
        println!("  {mark} {}{tag}{desc}", r.id);
    }
    Ok(())
}

// --- fragments ------------------------------------------------------------

/// Entry point for `load fragments`.
pub fn fragments(rt: &Runtime, args: &FragmentsArgs) -> crate::Result<()> {
    let prep = prepare(rt)?;
    let active: Vec<&str> = prep
        .composition
        .fragments
        .iter()
        .map(|rc| rc.fragment.id.as_str())
        .collect();

    match &args.action {
        Some(FragmentsAction::Show { id }) => {
            let Some(cap) = prep.config.fragments.iter().find(|c| &c.id == id) else {
                bail!("unknown fragment '{id}'");
            };
            let via = prep
                .composition
                .fragments
                .iter()
                .find(|rc| &rc.fragment.id == id)
                .map(|rc| rc.via_profile.clone());
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&fragment_detail(cap, via))?
                );
            } else {
                print_fragment_show(cap, via.as_deref());
            }
        }
        _ => {
            if args.json {
                let rows: Vec<_> = prep
                    .config
                    .fragments
                    .iter()
                    .map(|c| fragment_row(c, active.contains(&c.id.as_str())))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                print_fragments_list(&prep.config.fragments, &active);
            }
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct FragmentRow {
    id: String,
    description: Option<String>,
    kind: &'static str,
    active: bool,
}

fn kind_of(c: &Fragment) -> &'static str {
    if c.command.is_some() {
        "command"
    } else if c.provider.is_some() {
        "provider"
    } else {
        "static"
    }
}

fn fragment_row(c: &Fragment, active: bool) -> FragmentRow {
    FragmentRow {
        id: c.id.clone(),
        description: c.description.clone(),
        kind: kind_of(c),
        active,
    }
}

#[derive(Serialize)]
struct FragmentDetail<'a> {
    #[serde(flatten)]
    fragment: &'a Fragment,
    origin: String,
    kind: &'static str,
    active_via_profile: Option<String>,
}

fn fragment_detail(c: &Fragment, via: Option<String>) -> FragmentDetail<'_> {
    FragmentDetail {
        fragment: c,
        origin: origin_label(c.origin).to_string(),
        kind: kind_of(c),
        active_via_profile: via,
    }
}

fn origin_label(layer: Layer) -> &'static str {
    match layer {
        Layer::BuiltIn => "built-in",
        Layer::Global => "global config.toml",
        Layer::GlobalLocal => "global local.toml",
        Layer::Repo => "repo config.toml",
        Layer::RepoLocal => "repo local.toml",
    }
}

fn print_fragments_list(caps: &[Fragment], active: &[&str]) {
    println!(
        "Fragments ({} in library, {} active for this context)",
        caps.len(),
        active.len()
    );
    for c in caps {
        let mark = if active.contains(&c.id.as_str()) {
            "●"
        } else {
            "·"
        };
        let mut meta: Vec<String> = Vec::new();
        if kind_of(c) != "static" {
            meta.push(format!("{}: {}", kind_of(c), dynamic_target(c)));
        }
        let suffix = if meta.is_empty() {
            String::new()
        } else {
            format!("  ({})", meta.join("; "))
        };
        println!("  {mark} {} — {}{suffix}", c.id, c.title());
    }
    println!("\nShow one with `load fragments show <id>`.");
}

fn dynamic_target(c: &Fragment) -> String {
    c.command
        .clone()
        .or_else(|| c.provider.clone())
        .unwrap_or_default()
}

fn print_fragment_show(c: &Fragment, via: Option<&str>) {
    println!("Fragment: {}", c.id);
    println!("  description : {}", c.title());
    println!("  kind        : {}", kind_of(c));
    println!("  origin      : {}", origin_label(c.origin));
    match via {
        Some(p) => println!("  active      : yes (via loadout '{p}')"),
        None => println!("  active      : no (not composed for this context)"),
    }
    println!(
        "  requires    : {}",
        if c.requires.is_empty() {
            "-".into()
        } else {
            c.requires.join(", ")
        }
    );
    println!(
        "  agents      : {}",
        if c.agents.is_empty() {
            "(all)".into()
        } else {
            c.agents.join(", ")
        }
    );
    println!(
        "  when        : {}",
        if c.when.is_empty() {
            "(always)".into()
        } else {
            format!("{} rule(s)", c.when.len())
        }
    );
    if let Some(p) = &c.provider {
        println!("  provider    : {p}");
    }
    if let Some(cmd) = &c.command {
        println!("  command     : {cmd}");
    }
    if let Some(cache) = &c.cache {
        println!("  cache       : {cache}");
    }
    if !c.guidance.trim().is_empty() {
        println!("  guidance    :");
        for line in c.guidance.lines() {
            println!("    {line}");
        }
    }
}

// --- profiles ----------------------------------------------------------------

/// Entry point for `load profiles`.
pub fn profiles(rt: &Runtime, args: &ProfilesArgs) -> crate::Result<()> {
    let prep = prepare(rt)?;
    let tags = prep.context.selection_targets();
    let selected = prep.composition.profile.clone();
    let is_selected = |p: &LoadoutConfig| selected.as_deref() == Some(p.name.as_str());
    let is_candidate = |p: &LoadoutConfig| profile::profile_matches_targets(p, &tags);

    if args.json {
        let rows: Vec<_> = prep
            .config
            .profiles
            .iter()
            .map(|p| profile_row(p, is_candidate(p), is_selected(p)))
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    let candidates: Vec<&str> = prep
        .config
        .profiles
        .iter()
        .filter(|p| is_candidate(p))
        .map(|p| p.name.as_str())
        .collect();

    println!(
        "Loadouts ({} configured; selected: {})",
        prep.config.profiles.len(),
        selected.as_deref().unwrap_or("none")
    );
    if candidates.len() > 1 {
        println!(
            "  {} match here (pick one): {}",
            candidates.len(),
            candidates.join(", ")
        );
    }
    for p in &prep.config.profiles {
        let mark = if is_selected(p) {
            "→"
        } else if is_candidate(p) {
            "·"
        } else {
            " "
        };
        let caps: Vec<&str> = p.fragments.iter().map(|r| r.id()).collect();
        println!("  {mark} {:<16} targets [{}]", p.name, p.targets.join(", "));
        if !caps.is_empty() {
            println!("        fragments: {}", caps.join(", "));
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct ProfileRow {
    name: String,
    targets: Vec<String>,
    /// Whether this profile's targets match the current context.
    candidate: bool,
    /// Whether this profile is the selected one for the current context.
    selected: bool,
    fragments: Vec<String>,
}

fn profile_row(p: &LoadoutConfig, candidate: bool, selected: bool) -> ProfileRow {
    ProfileRow {
        name: p.name.clone(),
        targets: p.targets.clone(),
        candidate,
        selected,
        fragments: p.fragments.iter().map(|r| r.id().to_string()).collect(),
    }
}

// --- agents ------------------------------------------------------------------

/// Entry point for `load agents`.
pub fn agents(rt: &Runtime, args: &AgentsArgs) -> crate::Result<()> {
    let prep = prepare(rt)?;

    let write_override = prep.config.codex.write_override;
    if args.json {
        let rows: Vec<_> = prep
            .config
            .agents
            .iter()
            .map(|a| agent_row(a, write_override))
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    println!(
        "Agents ({} configured; default: {})",
        prep.config.agents.len(),
        prep.config.default_agent
    );
    for a in &prep.config.agents {
        let launch = a.launch.as_deref().unwrap_or("-");
        println!(
            "  {:<9} {:<22} launch: {:<9} delivery: {}",
            a.id,
            a.display(),
            launch,
            delivery_of(a, write_override)
        );
    }
    Ok(())
}

/// How an agent receives the overlay (mirrors `adapters::apply`). `write_override`
/// is the config default for override-style agents (it can still be flipped per
/// run via `--override` / `--no-override`).
fn delivery_of(a: &AgentDescriptor, write_override: bool) -> String {
    if let Some(importer) = &a.importer {
        format!("import → {importer}")
    } else if let Some(ovr) = &a.override_target {
        if write_override {
            format!("override → {ovr} (auto; --no-override to skip)")
        } else {
            format!("override → {ovr} (off; set [codex] write_override = true)")
        }
    } else if let Some(reg) = &a.importer_registry {
        format!("register → {}", reg.settings_file)
    } else if let Some(var) = &a.launch_context_dir_env {
        format!("run env → {var}")
    } else {
        "emit-only".to_string()
    }
}

#[derive(Serialize)]
struct AgentRow {
    id: String,
    display_name: String,
    launch: Option<String>,
    delivery: String,
}

fn agent_row(a: &AgentDescriptor, write_override: bool) -> AgentRow {
    AgentRow {
        id: a.id.clone(),
        display_name: a.display().to_string(),
        launch: a.launch.clone(),
        delivery: delivery_of(a, write_override),
    }
}
