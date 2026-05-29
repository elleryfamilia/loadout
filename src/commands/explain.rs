//! `rosita explain` — show what would be selected and written, and why.

use serde::Serialize;

use super::{now_rfc3339, prepare, resolve_agents, Prepared, Runtime};
use crate::adapters::{self, AppContext, ApplyOptions};
use crate::cli::ExplainArgs;
use crate::profile;
use crate::writer::AtomicWriter;

/// Entry point for `rosita explain`.
pub fn run(rt: &Runtime, args: &ExplainArgs) -> crate::Result<()> {
    let prep = prepare(rt)?;
    let agents = resolve_agents(args.agent.as_deref(), &prep.config)?;
    let report = build_report(&prep, &agents)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human(&report);
    }
    Ok(())
}

#[derive(Serialize)]
struct ExplainReport {
    repo_base: String,
    repo_name: Option<String>,
    branch: Option<String>,
    stacks: Vec<String>,
    languages: Vec<String>,
    package_managers: Vec<String>,
    config_sources: Vec<String>,
    context_hash: String,
    /// Primary (highest-priority) matching profile — the display label.
    selected_profile: String,
    /// All matching profiles that contributed, priority order.
    matching_profiles: Vec<String>,
    /// Active capabilities, in render order, with provenance.
    capabilities: Vec<ActiveCapability>,
    considered: Vec<Considered>,
    plan: Vec<AgentPlan>,
}

#[derive(Serialize)]
struct ActiveCapability {
    id: String,
    via_profile: String,
    risk: crate::capability::Risk,
    reason: String,
}

#[derive(Serialize)]
struct Considered {
    name: String,
    priority: i32,
    matched: bool,
    selected: bool,
    reasons: Vec<String>,
}

#[derive(Serialize)]
struct AgentPlan {
    agent: String,
    files: Vec<PlanFile>,
}

#[derive(Serialize)]
struct PlanFile {
    path: String,
    action: String,
}

fn build_report(prep: &Prepared, agents: &[String]) -> crate::Result<ExplainReport> {
    let ctx = &prep.context;
    let matching = &prep.composition.profiles;

    let considered = prep
        .config
        .profiles
        .iter()
        .map(|p| {
            let reasons = profile::matches(ctx, p);
            Considered {
                name: p.name.clone(),
                priority: p.priority,
                matched: reasons.is_some(),
                // Additive: every contributing profile is "selected"; a matched
                // profile dropped by an exclusive winner is matched-not-selected.
                selected: matching.contains(&p.name),
                reasons: reasons.unwrap_or_default(),
            }
        })
        .collect();

    let capabilities = prep
        .composition
        .capabilities
        .iter()
        .map(|rc| ActiveCapability {
            id: rc.capability.id.clone(),
            via_profile: rc.via_profile.clone(),
            risk: rc.capability.risk,
            reason: rc.reason.clone(),
        })
        .collect();

    // Dry-run apply to compute the write plan without touching disk.
    let writer = AtomicWriter::new(true);
    let generated_at = now_rfc3339();
    let mut plan = Vec::new();
    for agent in agents {
        if let Some(descriptor) = adapters::descriptor(&prep.config, agent) {
            let app = AppContext {
                context: ctx,
                composition: &prep.composition,
                config: &prep.config,
                generated_at: generated_at.clone(),
                writer: &writer,
            };
            let result = adapters::apply(descriptor, &app, &ApplyOptions::default())?;
            plan.push(AgentPlan {
                agent: agent.clone(),
                files: result
                    .files
                    .iter()
                    .map(|f| PlanFile {
                        path: f.path.display().to_string(),
                        action: f.action.label().to_string(),
                    })
                    .collect(),
            });
        }
    }

    Ok(ExplainReport {
        repo_base: ctx.repo_base.display().to_string(),
        repo_name: ctx.repo_name.clone(),
        branch: ctx.git.as_ref().and_then(|g| g.branch.clone()),
        stacks: ctx.stacks.clone(),
        languages: ctx.languages.clone(),
        package_managers: ctx.package_managers.clone(),
        config_sources: prep
            .config
            .sources
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        context_hash: ctx.compute_hash(),
        selected_profile: prep.profile_label().to_string(),
        matching_profiles: matching.clone(),
        capabilities,
        considered,
        plan,
    })
}

fn print_human(r: &ExplainReport) {
    println!("Project");
    println!("  base   : {}", r.repo_base);
    if let Some(n) = &r.repo_name {
        println!("  name   : {n}");
    }
    if let Some(b) = &r.branch {
        println!("  branch : {b}");
    }
    println!(
        "  detected: stacks=[{}] languages=[{}] pms=[{}]",
        r.stacks.join(", "),
        r.languages.join(", "),
        r.package_managers.join(", ")
    );
    println!("  context: {}", r.context_hash);

    println!("\nConfig sources");
    if r.config_sources.is_empty() {
        println!("  (built-in defaults only)");
    } else {
        for s in &r.config_sources {
            println!("  - {s}");
        }
    }

    println!("\nProfile selection → {}", r.selected_profile);
    if r.matching_profiles.len() > 1 {
        println!("  composing: {}", r.matching_profiles.join(" + "));
    }

    println!("\nActive capabilities");
    if r.capabilities.is_empty() {
        println!("  (none)");
    } else {
        for c in &r.capabilities {
            let risk = match c.risk.annotation() {
                Some(a) => format!(" [{a}]"),
                None => String::new(),
            };
            println!("  • {}{risk}", c.id);
            println!("        {}", c.reason);
        }
    }

    println!("\nProfiles considered");
    for c in &r.considered {
        let mark = if c.selected {
            "→"
        } else if c.matched {
            "·"
        } else {
            " "
        };
        let status = if c.matched { "match" } else { "no match" };
        println!(
            "  {mark} {:<14} (priority {:>3}) {status}",
            c.name, c.priority
        );
        if c.matched {
            for reason in &c.reasons {
                println!("        {reason}");
            }
        }
    }

    println!("\nWrite plan");
    for ap in &r.plan {
        println!("  {}:", ap.agent);
        for f in &ap.files {
            println!("    {:<13} {}", f.action, f.path);
        }
    }
}
