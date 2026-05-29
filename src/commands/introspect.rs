//! `rosita capabilities` / `profiles` / `agents` — introspect the resolved
//! configuration and what's active for the current context.
//!
//! These are read-only debugging aids: they run the same config load + context
//! detection + composition as a render (via [`super::prepare`]) and print the
//! library plus which capabilities/profiles are active here.

use anyhow::bail;
use serde::Serialize;

use super::{prepare, Runtime};
use crate::adapters::AgentDescriptor;
use crate::capability::{Capability, Layer};
use crate::cli::{AgentsArgs, CapabilitiesAction, CapabilitiesArgs, ProfilesArgs};
use crate::profile::ProfileConfig;

// --- capabilities ------------------------------------------------------------

/// Entry point for `rosita capabilities`.
pub fn capabilities(rt: &Runtime, args: &CapabilitiesArgs) -> crate::Result<()> {
    let prep = prepare(rt)?;
    let active: Vec<&str> = prep
        .composition
        .capabilities
        .iter()
        .map(|rc| rc.capability.id.as_str())
        .collect();

    match &args.action {
        Some(CapabilitiesAction::Show { id }) => {
            let Some(cap) = prep.config.capabilities.iter().find(|c| &c.id == id) else {
                bail!("unknown capability '{id}'");
            };
            let via = prep
                .composition
                .capabilities
                .iter()
                .find(|rc| &rc.capability.id == id)
                .map(|rc| rc.via_profile.clone());
            if args.json {
                println!("{}", serde_json::to_string_pretty(&cap_detail(cap, via))?);
            } else {
                print_capability_show(cap, via.as_deref());
            }
        }
        _ => {
            if args.json {
                let rows: Vec<_> = prep
                    .config
                    .capabilities
                    .iter()
                    .map(|c| cap_row(c, active.contains(&c.id.as_str())))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                print_capabilities_list(&prep.config.capabilities, &active);
            }
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct CapRow {
    id: String,
    description: Option<String>,
    risk: crate::capability::Risk,
    tags: Vec<String>,
    kind: &'static str,
    active: bool,
}

fn kind_of(c: &Capability) -> &'static str {
    if c.command.is_some() {
        "command"
    } else if c.provider.is_some() {
        "provider"
    } else {
        "static"
    }
}

fn cap_row(c: &Capability, active: bool) -> CapRow {
    CapRow {
        id: c.id.clone(),
        description: c.description.clone(),
        risk: c.risk,
        tags: c.tags.clone(),
        kind: kind_of(c),
        active,
    }
}

#[derive(Serialize)]
struct CapDetail<'a> {
    #[serde(flatten)]
    capability: &'a Capability,
    origin: String,
    kind: &'static str,
    active_via_profile: Option<String>,
}

fn cap_detail(c: &Capability, via: Option<String>) -> CapDetail<'_> {
    CapDetail {
        capability: c,
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

fn print_capabilities_list(caps: &[Capability], active: &[&str]) {
    println!(
        "Capabilities ({} in library, {} active for this context)",
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
        if let Some(r) = c.risk.annotation() {
            meta.push(r.to_string());
        }
        if !c.tags.is_empty() {
            meta.push(format!("tags: {}", c.tags.join(", ")));
        }
        let suffix = if meta.is_empty() {
            String::new()
        } else {
            format!("  ({})", meta.join("; "))
        };
        println!("  {mark} {} — {}{suffix}", c.id, c.title());
    }
    println!("\nShow one with `rosita capabilities show <id>`.");
}

fn dynamic_target(c: &Capability) -> String {
    c.command
        .clone()
        .or_else(|| c.provider.clone())
        .unwrap_or_default()
}

fn print_capability_show(c: &Capability, via: Option<&str>) {
    println!("Capability: {}", c.id);
    println!("  description : {}", c.title());
    println!("  kind        : {}", kind_of(c));
    println!("  risk        : {:?}", c.risk);
    println!("  origin      : {}", origin_label(c.origin));
    match via {
        Some(p) => println!("  active      : yes (via profile '{p}')"),
        None => println!("  active      : no (not composed for this context)"),
    }
    println!(
        "  tags        : {}",
        if c.tags.is_empty() {
            "-".into()
        } else {
            c.tags.join(", ")
        }
    );
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

/// Entry point for `rosita profiles`.
pub fn profiles(rt: &Runtime, args: &ProfilesArgs) -> crate::Result<()> {
    let prep = prepare(rt)?;
    let matching = &prep.composition.profiles;

    if args.json {
        let rows: Vec<_> = prep
            .config
            .profiles
            .iter()
            .map(|p| profile_row(p, matching.contains(&p.name)))
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    println!(
        "Profiles ({} configured; matching: {})",
        prep.config.profiles.len(),
        if matching.is_empty() {
            "none".into()
        } else {
            matching.join(", ")
        }
    );
    // Show in priority order (desc) for readability.
    let mut profs: Vec<&ProfileConfig> = prep.config.profiles.iter().collect();
    profs.sort_by(|a, b| b.priority.cmp(&a.priority));
    for p in profs {
        let mark = if matching.contains(&p.name) {
            "→"
        } else {
            " "
        };
        let caps: Vec<&str> = p.capabilities.iter().map(|r| r.id()).collect();
        let rules = if p.when.is_empty() {
            "(always)".to_string()
        } else {
            format!("{} rule(s)", p.when.len())
        };
        let excl = if p.exclusive { " [exclusive]" } else { "" };
        println!(
            "  {mark} {:<14} (priority {:>3}){excl}  {rules}",
            p.name, p.priority
        );
        if !caps.is_empty() {
            println!("        capabilities: {}", caps.join(", "));
        }
        if !p.exclude.is_empty() {
            println!("        excludes: {}", p.exclude.join(", "));
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct ProfileRow {
    name: String,
    priority: i32,
    matched: bool,
    exclusive: bool,
    capabilities: Vec<String>,
    exclude: Vec<String>,
    rule_count: usize,
}

fn profile_row(p: &ProfileConfig, matched: bool) -> ProfileRow {
    ProfileRow {
        name: p.name.clone(),
        priority: p.priority,
        matched,
        exclusive: p.exclusive,
        capabilities: p.capabilities.iter().map(|r| r.id().to_string()).collect(),
        exclude: p.exclude.clone(),
        rule_count: p.when.len(),
    }
}

// --- agents ------------------------------------------------------------------

/// Entry point for `rosita agents`.
pub fn agents(rt: &Runtime, args: &AgentsArgs) -> crate::Result<()> {
    let prep = prepare(rt)?;

    if args.json {
        let rows: Vec<_> = prep.config.agents.iter().map(agent_row).collect();
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
            delivery_of(a)
        );
    }
    Ok(())
}

/// How an agent receives the overlay (mirrors `adapters::apply`).
fn delivery_of(a: &AgentDescriptor) -> String {
    if let Some(importer) = &a.importer {
        format!("import → {importer}")
    } else if let Some(ovr) = &a.override_target {
        format!("override → {ovr} (opt-in)")
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

fn agent_row(a: &AgentDescriptor) -> AgentRow {
    AgentRow {
        id: a.id.clone(),
        display_name: a.display().to_string(),
        launch: a.launch.clone(),
        delivery: delivery_of(a),
    }
}
