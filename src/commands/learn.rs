//! `load learn on|off|status|reset` — the ambient-learning lifecycle.
//!
//! `on` prints an honest consent block (exactly what will run, when, the
//! per-machine call ceiling, which agent dotfiles get edited, the claim-sync
//! disclosure, and a concrete per-run cost estimate), asks for confirmation,
//! then flips `[learn] enabled = true` in the synced global config, mints this
//! machine's learning id, writes the activation ack, and registers the
//! session-end learning hooks. `off` reverses all of that (and, when the config
//! dir is synced, pushes the disable so learning goes dormant on other machines
//! at their next launch). `status` reports what's on and what's staged; `reset`
//! re-baselines the harvest watermarks after corruption.

use std::io::{IsTerminal, Write as _};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context as _, Result};

use crate::adapters;
use crate::cli::{LearnAction, LearnArgs};
use crate::config::{self, Config};
use crate::context;
use crate::learn::journal::{self, CandidateStatus};
use crate::learn::watermarks::Watermarks;
use crate::learn::{agent_cli, state, trigger, worker};
use crate::style::Painter;
use crate::sync;

/// A bounded timeout for the one network op `off` makes (the disable push).
/// Warn-don't-fail: a slow/offline remote must never leave the local machine
/// half-disabled.
const OFF_PUSH_TIMEOUT: Duration = Duration::from_secs(30);

/// Dispatch. Bare `load learn` (no action) is `status`.
pub fn run(rt: &super::Runtime, args: &LearnArgs) -> Result<()> {
    match args.action.as_ref() {
        None | Some(LearnAction::Status) => status(rt),
        Some(LearnAction::On { yes }) => on(rt, *yes),
        Some(LearnAction::Off) => off(rt),
        Some(LearnAction::Reset) => reset(rt),
    }
}

// --- on ---------------------------------------------------------------------

fn on(rt: &super::Runtime, yes: bool) -> Result<()> {
    let p = Painter::auto();

    // The consent block is a product commitment: it always prints first, before
    // any prompt, side effect, or early return, so the user sees exactly what
    // they're agreeing to even in the non-TTY abort path.
    print_consent(&p);

    if rt.dry_run {
        println!(
            "\n{} dry run: would enable ambient learning on this machine; re-run without --dry-run.",
            p.dim("~")
        );
        return Ok(());
    }

    // Confirm. `--yes` skips the prompt; a non-TTY without `--yes` can't prompt,
    // so it aborts with a hint (nothing is enabled) rather than blocking.
    if !yes {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            println!(
                "\n{} not a terminal — re-run {} to enable ambient learning without the prompt.",
                p.dim("·"),
                p.bold("load learn on --yes")
            );
            return Ok(());
        }
        if !prompt_yes_no("Enable ambient learning on this machine?", false)? {
            println!("{} left learning off — nothing changed.", p.dim("·"));
            return Ok(());
        }
    }

    // 1. Flip the synced intent flag (comment-preserving).
    set_enabled(true).context("enabling learning in the global config")?;

    // 2. Mint this machine's id + write the activation ack (per-machine, never
    //    synced). The ack is the gate the passive hook bootstrap consults.
    let learn_dir = state::learn_dir()
        .ok_or_else(|| anyhow!("cannot resolve the learning state dir (no home)"))?;
    std::fs::create_dir_all(&learn_dir).ok();
    let machine_id =
        state::machine_id_at(&learn_dir).context("minting this machine's learning id")?;
    let ack = state::Activation {
        machine_id: machine_id.clone(),
        hostname: hostname(),
        activated_at: super::now_rfc3339(),
    };
    state::write_activation_at(&learn_dir, &ack).context("writing the activation ack")?;
    // A fresh opt-in clears any prior consecutive-failure pause.
    state::reset_failures_at(&learn_dir);

    // 3. Register the learning hooks (bootstrap with learn_active = true). Notes
    //    name each file touched; the claude `disableAllHooks` note surfaces here
    //    too (the caller just opted in — they deserve to know the hook is inert).
    let repo_base = context::repo_base_for(&rt.cwd);
    let config = Config::load(&repo_base).context("loading configuration")?;
    let notes = adapters::bootstrap_hook_registrations(&config, true, rt.dry_run);

    println!(
        "\n{} ambient learning is on for {} ({}).",
        p.green("✓"),
        p.bold(&hostname()),
        p.dim(&format!(
            "machine {}",
            &machine_id[..machine_id.len().min(12)]
        ))
    );
    for note in &notes {
        println!("  {} {}", p.dim("·"), note);
    }
    if !notes.iter().any(|n| n.contains("learning hook")) {
        println!(
            "  {} no learning hooks registered here yet — entry-point triggers still fire \
             ({} at loadout's own commands).",
            p.dim("·"),
            p.dim("load run/refresh/hook/studio")
        );
    }

    // 4. Offer an immediate first run (foreground, outcome printed). Only in an
    //    interactive TTY: a metered extraction call is exactly the surprise
    //    spend the whole feature guards against, so it never fires unprompted.
    if !yes
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && prompt_yes_no("Run a first harvest now?", false)?
    {
        let outcome = worker::run_harvest(&config, true).context("running the first harvest")?;
        super::harvest::print_summary(&outcome);
        return Ok(());
    }
    println!(
        "  {} run {} for a first pass now, or it runs after your next agent session.",
        p.dim("·"),
        p.bold("load harvest")
    );
    Ok(())
}

/// The honest consent block. Every commitment named here is a binding
/// acceptance criterion — the ceiling, both dotfile paths, the sync disclosure,
/// and the cost figure are load-bearing and asserted by tests.
fn print_consent(p: &Painter) {
    println!("{}", p.bold("Ambient learning — before you turn it on:"));
    println!();
    println!(
        "  {} loadout mines your recent agent sessions for durable, cross-project",
        p.dim("what:")
    );
    println!("        preferences and stages them as candidates you review in the studio.");
    println!(
        "        It runs {} — a normal process you can see in `ps`, never a daemon.",
        p.bold("load harvest --ambient")
    );
    println!(
        "  {} after your agent sessions end and at loadout's own commands, at most",
        p.dim("when:")
    );
    println!("        once per 6h tick per machine.");
    // The honest ceiling — stated in full, numbers included.
    println!(
        "  {}  at most 1 extraction call per 6h tick — ≤4/day at defaults, per machine",
        p.dim("cap:")
    );
    println!(
        "        you enable; plus any {} you run yourself.",
        p.bold("load harvest")
    );
    // Cost, computed from the per-run caps (≤400KB ≈ ~100k input tokens worst
    // case) against current haiku-class API pricing ($1/MTok in, $5/MTok out).
    println!(
        "  {} typically ~1-3¢, up to ~10-15¢ per run on a metered API key; $0 marginal",
        p.dim("cost:")
    );
    println!(
        "        on a subscription-backed CLI. Pin {} if that matters.",
        p.bold("learn.cli")
    );
    // Files this edits, with the one-time-backup promise.
    println!(
        "  {} it registers a session-end hook in ~/.claude/settings.json and",
        p.dim("edits:")
    );
    println!("        ~/.cursor/hooks.json (a one-time .loadout-bak backup precedes each edit).");
    // The sync disclosure — verbatim phrasing.
    println!(
        "  {}  claim text distilled from your sessions is stored in your synced loadout",
        p.dim("sync:")
    );
    println!("        config; verbatim quotes never leave this machine.");
}

// --- off --------------------------------------------------------------------

fn off(rt: &super::Runtime) -> Result<()> {
    let p = Painter::auto();

    if rt.dry_run {
        println!(
            "{} dry run: would disable ambient learning (synced) and clean up this machine.",
            p.dim("~")
        );
        return Ok(());
    }

    // 1. Flip the synced intent flag off (comment-preserving).
    set_enabled(false).context("disabling learning in the global config")?;
    println!("{} disabled ambient learning (synced).", p.green("✓"));

    // 2. Push the disable so learning goes dormant on other machines at their
    //    next launch. Bounded and warn-don't-fail: never leave this machine
    //    half-off because a remote was slow.
    if let Some(dir) = config::global_config_dir() {
        if sync::is_synced(&dir) {
            match sync::commit_push(&dir, "loadout: disable ambient learning", OFF_PUSH_TIMEOUT) {
                Ok(sync::PushOutcome::Pushed) => println!(
                    "  {} pushed the disable — learning goes dormant on other machines at their next launch.",
                    p.dim("·")
                ),
                Ok(sync::PushOutcome::NothingToPush) => {}
                Ok(sync::PushOutcome::Diverged) | Err(_) => crate::warn_user!(
                    "couldn't push the disable now; it syncs to other machines at their next \
                     launch (run `load sync` to push it sooner)."
                ),
            }
        }
    }

    // 3. Remove the activation ack locally (deactivates this machine).
    if let Some(dir) = state::learn_dir() {
        if let Err(e) = state::remove_activation_at(&dir) {
            crate::warn_user!("could not remove the activation ack: {e}");
        }
    }

    // 4. Deregister the learning hooks (learn-owned entries only; foreign
    //    content and Cursor's freshness hook survive). Prints each file cleaned.
    let repo_base = context::repo_base_for(&rt.cwd);
    let config = Config::load(&repo_base).context("loading configuration")?;
    let notes = adapters::remove_learn_hooks(&config, rt.dry_run);
    for note in &notes {
        println!("  {} {}", p.dim("·"), note);
    }
    if notes.is_empty() {
        println!("  {} no learning hooks were registered here.", p.dim("·"));
    }
    Ok(())
}

// --- status -----------------------------------------------------------------

fn status(rt: &super::Runtime) -> Result<()> {
    let p = Painter::auto();
    let repo_base = context::repo_base_for(&rt.cwd);
    let config = Config::load(&repo_base).context("loading configuration")?;

    let learn_dir = state::learn_dir();
    let activation = learn_dir.as_deref().and_then(state::read_activation_at);

    // Enabled (synced intent) + activated (this machine ran `load learn on`).
    println!("{}", p.bold("Ambient learning"));
    let flag = if config.learn.enabled {
        p.green("on")
    } else {
        p.dim("off")
    };
    println!("  intent (synced): {flag}");
    match &activation {
        Some(a) => println!(
            "  this machine:    {} ({}, machine {})",
            p.green("activated"),
            a.hostname,
            &a.machine_id[..a.machine_id.len().min(12)]
        ),
        None => println!(
            "  this machine:    {} — run `load learn on` here to activate",
            p.dim("not activated")
        ),
    }

    // The CLI + model a run would use.
    let selection = agent_cli::select(&config.learn);
    match &selection {
        agent_cli::Selection::Chosen(c) => {
            let model = if c.model.is_empty() {
                "cli default".to_string()
            } else {
                c.model.clone()
            };
            println!("  extraction:      {} ({})", c.cli_id, p.dim(&model));
        }
        agent_cli::Selection::Unsupported(u) => {
            let reason = match (u.reason, u.installed_version.as_deref()) {
                (agent_cli::UnsupportedReason::TooOld, Some(found)) => {
                    format!("{found} is too old")
                }
                (agent_cli::UnsupportedReason::ProbeTimedOut, _) => {
                    "version probe timed out".to_string()
                }
                _ => "version is unrecognized".to_string(),
            };
            println!(
                "  extraction:      {} ({}; requires >= {})",
                p.yellow(u.cli_id),
                reason,
                u.minimum_version
            );
            let diagnostic = worker::HarvestDiagnostic::UnsupportedCli(u.clone());
            println!(
                "  {} current issue:   {}",
                p.yellow("!"),
                super::harvest::format_diagnostic(&diagnostic)
            );
        }
        agent_cli::Selection::None => println!(
            "  extraction:      {} (install claude/codex/gemini or set `learn.cli`)",
            p.dim("no CLI available")
        ),
    }

    if let Some(dir) = &learn_dir {
        // Last run (from the machine-local run log).
        let log = worker::read_log(&dir.join("log.jsonl"));
        match log.last() {
            Some(r) => {
                let cli = r.cli.as_deref().unwrap_or("?");
                println!(
                    "  last run:        {} — {} ({}), {} session{}, {} candidate{}",
                    r.ts,
                    r.outcome,
                    cli,
                    r.sessions,
                    plural(r.sessions),
                    r.candidates,
                    plural(r.candidates),
                );
            }
            None => println!("  last run:        {}", p.dim("none yet")),
        }

        // Next eligibility — BOTH throttle stamps, computed by the trigger
        // module's own guard math (one source of truth, exactly as a real
        // trigger would decide). A waiting session-end hint is surfaced but
        // does NOT make a fresh spend stamp eligible: it never buys an extra
        // extraction call (design Decision #3).
        let e = trigger::eligibility_at(dir, config.learn.interval, SystemTime::now());
        println!("  next harvest:    {}", eligibility_line(&p, &e));

        // The breaker counter, not `log.last()`, decides whether an older
        // failure is still actionable. A later empty/no-op run must not hide
        // the reason; `load learn on` resets the counter and makes old log
        // entries historical.
        if let Some(failure) = worker::latest_unresolved_failure(dir) {
            println!(
                "  {} unresolved failure: {}",
                p.yellow("!"),
                super::harvest::format_logged_diagnostic(&failure)
            );
        }

        // Paused after repeated failures + the clearing action.
        if state::paused_at(dir) {
            println!(
                "  {} paused after repeated failures — a successful {} or {} clears it.",
                p.yellow("!"),
                p.bold("load harvest"),
                p.bold("load learn on")
            );
        }
    }

    // Hooks registered per agent (learn-owned entries only).
    print_hook_status(&p, &config);

    // Review backlog (folded from the synced journals).
    if let Some(inbox) = config::global_config_dir().map(|d| d.join("inbox")) {
        let fold = journal::fold_at(&inbox);
        let quarantined = fold
            .candidates
            .values()
            .filter(|c| c.status == CandidateStatus::Quarantined)
            .count();
        println!(
            "  inbox:           {} pending, {} suppressed, {} held by the injection lint",
            fold.pending_count(),
            fold.suppressed.len(),
            quarantined,
        );
    }
    Ok(())
}

/// Per-agent learn-hook registration state, read straight from each dotfile.
fn print_hook_status(p: &Painter, config: &Config) {
    let Some(home) = config::home_dir() else {
        return;
    };
    let mut any = false;
    for d in &config.agents {
        for hr in &d.learn_hooks {
            any = true;
            let path = home.join(&hr.hooks_file);
            let registered = std::fs::read_to_string(&path)
                .map(|c| c.contains(&format!(" {}", hr.subcommand)))
                .unwrap_or(false);
            let mark = if registered {
                p.green("registered")
            } else {
                p.dim("not registered")
            };
            println!("  hook ({}): {}", d.id, mark);
        }
    }
    if !any {
        println!("  hooks:           {}", p.dim("none configured"));
    }
}

// --- reset ------------------------------------------------------------------

fn reset(rt: &super::Runtime) -> Result<()> {
    let p = Painter::auto();
    if rt.dry_run {
        println!(
            "{} dry run: would delete the harvest watermarks (re-baselining forward).",
            p.dim("~")
        );
        return Ok(());
    }
    let learn_dir = state::learn_dir()
        .ok_or_else(|| anyhow!("cannot resolve the learning state dir (no home)"))?;
    Watermarks::reset(&learn_dir.join("watermarks.json"))
        .context("resetting the harvest watermarks")?;
    // A corrupt store is often paired with a failure pause; clear it too so the
    // next tick can actually run.
    state::reset_failures_at(&learn_dir);
    println!("{} reset the harvest watermarks.", p.green("✓"));
    println!(
        "  {} the next run re-baselines and harvests only sessions from the last 14 days",
        p.dim("·")
    );
    println!("        forward. Your reviewed candidates and evidence are untouched.");
    Ok(())
}

// --- shared helpers ---------------------------------------------------------

/// Flip `[learn] enabled` in the global `config.toml` via `toml_edit`, so the
/// user's comments and every other key survive (the binding-writer precedent,
/// not a plain `toml` re-serialize). Creates the file/dir and a real `[learn]`
/// table if absent.
fn set_enabled(value: bool) -> Result<PathBuf> {
    let path = config::global_config_path()
        .ok_or_else(|| anyhow!("cannot resolve the global config path (no home)"))?;
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .with_context(|| format!("parsing {} before setting [learn] enabled", path.display()))?;
    if !doc.contains_key("learn") {
        doc["learn"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    doc["learn"]["enabled"] = toml_edit::value(value);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    crate::writer::atomic_write(&path, &doc.to_string())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// This machine's hostname (display metadata for the activation ack + status).
fn hostname() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

/// A y/N prompt with a default (matches `commands::sync`'s prompt shape).
fn prompt_yes_no(question: &str, default_yes: bool) -> Result<bool> {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("{question} {hint} ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        return Ok(default_yes); // EOF — take the default.
    }
    let t = line.trim().to_ascii_lowercase();
    Ok(if t.is_empty() {
        default_yes
    } else {
        t.starts_with('y')
    })
}

/// A plain-language "next eligibility" line from the trigger module's
/// guard-6/7 view: "eligible now" when both throttle stamps are due; otherwise
/// the wait is the later of the scan-debounce and spend-interval remainders. A
/// waiting session-end hint is noted honestly — it does NOT shorten the wait
/// (it never buys an extra extraction call); it only lets the next due tick
/// harvest the just-ended session despite the quiescence window.
fn eligibility_line(p: &Painter, e: &trigger::Eligibility) -> String {
    if e.now() {
        let why = if e.hint {
            "interval elapsed; a session-end hint will be harvested this tick"
        } else {
            "interval elapsed"
        };
        format!("{} ({})", p.green("eligible now"), p.dim(why))
    } else if e.hint {
        p.dim(&format!(
            "in ~{} (session-end hint waiting; runs at the next tick, not sooner)",
            human_duration(e.wait)
        ))
    } else {
        p.dim(&format!("in ~{}", human_duration(e.wait)))
    }
}

/// Coarse human duration for the "in ~Xh" line (hours/minutes granularity).
fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}
