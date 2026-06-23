//! `load detect` — show the detected context (and optional env probes).

use std::time::Duration;

use super::{prepare, Runtime};
use crate::cli::DetectArgs;
use crate::context::Context;
use crate::providers::{self, Probes};

/// Default probe cache TTL for `detect --probes` (probes refresh after this).
const PROBE_TTL: Duration = Duration::from_secs(60);

/// Entry point for `load detect`.
pub fn run(rt: &Runtime, args: &DetectArgs) -> crate::Result<()> {
    let prep = prepare(rt)?;

    // Probes are opt-in: only `--probes` shells out to external tools.
    let probes = if args.probes {
        Some(providers::gather(
            &prep.context,
            &prep.repo_base,
            PROBE_TTL,
            super::now_utc(),
        ))
    } else {
        None
    };

    if args.json {
        match &probes {
            Some(p) => {
                let combined = serde_json::json!({
                    "context": &prep.context,
                    "probes": probes_to_json(p),
                });
                println!("{}", serde_json::to_string_pretty(&combined)?);
            }
            None => println!("{}", serde_json::to_string_pretty(&prep.context)?),
        }
    } else {
        print_human(&prep.context);
        if let Some(p) = &probes {
            print_probes(p);
        }
    }
    Ok(())
}

fn probes_to_json(p: &Probes) -> serde_json::Value {
    serde_json::Value::Object(
        p.entries
            .iter()
            .map(|(id, out)| {
                (
                    id.clone(),
                    serde_json::json!({ "text": out.text, "data": out.data }),
                )
            })
            .collect(),
    )
}

fn print_probes(p: &Probes) {
    println!("\nProbes");
    if p.is_empty() {
        println!("  (no providers available here)");
        return;
    }
    for (id, out) in &p.entries {
        // Indent each line of the (possibly multi-line) probe text.
        let body = out
            .text
            .lines()
            .map(|l| format!("               {l}"))
            .collect::<Vec<_>>()
            .join("\n");
        println!("  {id:<10} :");
        println!("{body}");
    }
}

fn print_human(ctx: &Context) {
    let dash = |o: &Option<String>| o.clone().unwrap_or_else(|| "-".into());
    let list = |v: &[String]| {
        if v.is_empty() {
            "-".to_string()
        } else {
            v.join(", ")
        }
    };

    println!("Context");
    println!("  cwd        : {}", ctx.cwd.display());
    println!("  base dir   : {}", ctx.repo_base.display());
    println!("  name       : {}", dash(&ctx.repo_name));

    match &ctx.git {
        Some(g) => {
            println!(
                "  git        : branch {} · {} remote(s){}",
                g.branch.clone().unwrap_or_else(|| "(detached)".into()),
                g.remotes.len(),
                if g.is_worktree { " · worktree" } else { "" }
            );
            for r in &g.remotes {
                println!("               {} {}", r.name, r.url);
            }
        }
        None => println!("  git        : (not a git repo — non-repo mode)"),
    }

    println!("  stacks     : {}", list(&ctx.stacks));
    println!("  languages  : {}", list(&ctx.languages));
    println!("  pkg mgrs   : {}", list(&ctx.package_managers));

    if !ctx.commands.is_empty() {
        println!("  commands   :");
        print_cmds("build", &ctx.commands.build);
        print_cmds("test", &ctx.commands.test);
        print_cmds("lint", &ctx.commands.lint);
        print_cmds("run", &ctx.commands.run);
    }

    println!(
        "  system     : {} / {} · host {} · user {}",
        ctx.system.os, ctx.system.arch, ctx.system.hostname, ctx.system.user
    );
    if let Some(c) = &ctx.system.host_class {
        println!("  host class : {c}");
    }
    if let Some(p) = &ctx.system.parent_process {
        println!("  caller     : {p}");
    }
    if !ctx.env.is_empty() {
        println!(
            "  env        : {}",
            ctx.env.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    println!("  hash       : {}", ctx.compute_hash());
}

fn print_cmds(label: &str, cmds: &[String]) {
    if !cmds.is_empty() {
        println!("                 {label:<6} {}", cmds.join(", "));
    }
}
