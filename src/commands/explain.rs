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
    /// Coarse selection tags the context resolved to (stacks + `machine`).
    selection_targets: Vec<String>,
    /// The selected profile — the display label (`none` if no profile applies).
    selected_profile: String,
    /// Profiles whose `targets` match here (selection candidates).
    candidate_profiles: Vec<String>,
    /// Active fragments, in render order, with provenance.
    fragments: Vec<ActiveFragment>,
    considered: Vec<Considered>,
    plan: Vec<AgentPlan>,
}

#[derive(Serialize)]
struct ActiveFragment {
    id: String,
    via_profile: String,
    reason: String,
}

#[derive(Serialize)]
struct Considered {
    name: String,
    targets: Vec<String>,
    matched: bool,
    selected: bool,
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
    let tags = ctx.selection_targets();
    let selected = prep.composition.profile.clone();
    let candidate_profiles: Vec<String> = prep
        .config
        .profiles
        .iter()
        .filter(|p| profile::profile_matches_targets(p, &tags))
        .map(|p| p.name.clone())
        .collect();

    let considered = prep
        .config
        .profiles
        .iter()
        .map(|p| Considered {
            name: p.name.clone(),
            targets: p.targets.clone(),
            matched: profile::profile_matches_targets(p, &tags),
            selected: selected.as_deref() == Some(p.name.as_str()),
        })
        .collect();

    let fragments = prep
        .composition
        .fragments
        .iter()
        .map(|rc| ActiveFragment {
            id: rc.fragment.id.clone(),
            via_profile: rc.via_profile.clone(),
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
        selection_targets: tags,
        selected_profile: prep.profile_label().to_string(),
        candidate_profiles,
        fragments,
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

    println!("\nDetected targets: [{}]", r.selection_targets.join(", "));
    println!("Profile selection → {}", r.selected_profile);
    if r.candidate_profiles.len() > 1 {
        println!(
            "  {} profiles match (pick one): {}",
            r.candidate_profiles.len(),
            r.candidate_profiles.join(", ")
        );
    }

    println!("\nActive fragments");
    if r.fragments.is_empty() {
        println!("  (none)");
    } else {
        for c in &r.fragments {
            println!("  • {}", c.id);
            println!("        {}", c.reason);
        }
    }

    println!("\nProfiles considered");
    if r.considered.is_empty() {
        println!("  (none configured)");
    }
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
            "  {mark} {:<14} targets [{}] {status}",
            c.name,
            c.targets.join(", ")
        );
    }

    println!("\nWrite plan");
    for ap in &r.plan {
        println!("  {}:", ap.agent);
        for f in &ap.files {
            println!("    {:<13} {}", f.action, f.path);
        }
    }
}
