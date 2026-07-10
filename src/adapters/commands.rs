//! Per-stage slash-command generation — the workflow "command channel".
//!
//! A bound workflow renders two ways. Channel 1 (see [`crate::render`]) is the
//! always-on `## Workflow` context section. Channel 2 — this module — is for
//! agents that support project slash commands: one generated command file per
//! stage, carrying that stage's contract (its purpose, the handoff artifact to
//! read/write, the gate, the exit checklist, and an argument slot for the
//! specific task).
//!
//! Files land in a dedicated [`COMMAND_NAMESPACE`] subdirectory of the agent's
//! command dir (e.g. `.claude/commands/loadout/plan.md`) — a dir loadout owns
//! entirely, so the commands invoke as `/loadout:<stage>` and cleanup can remove
//! the whole dir without touching the user's own commands.

use serde::{Deserialize, Serialize};

use crate::workflow::{self, Workflow, WorkflowStage, ARTIFACT_SUBDIR};

/// The namespace subdir loadout owns under an agent's command directory.
pub const COMMAND_NAMESPACE: &str = "loadout";

/// Fixed intro for the native-review branch of the verify stage (tests key on it).
pub(crate) const REVIEW_COMMANDS_INTRO: &str =
    "As part of this stage, run the agent's native review commands and fold their findings into the verdict:";
/// Fixed intro for the vendored-checklist branch (tests key on it). Frames the
/// vendored prompt for agents without native review commands: it originated as
/// a Claude Code slash-command, so its git-context blocks are a template to
/// reproduce, not literal commands to run.
pub(crate) const SECURITY_CHECKLIST_INTRO: &str =
    "Also run a security review of the change. The checklist below is a vendored security-review prompt — gather the branch's git status/diff/log yourself where it references them, then work through it:";
/// Vendored security-review prompt — see vendored/sources.toml (`security-review`).
const SECURITY_CHECKLIST: &str = include_str!("../../vendored/security-review/security-review.md");
/// Appended to every plan-slot command body (channel 2), whatever the stage is
/// named — so each workflow's plan step also produces the visual plan preview.
/// Depends only on the `load` binary, deliberately NOT on the
/// `loadout-plan-preview` skill being surfaced: the observed failure mode is a
/// session where the skill never enters the agent's context, and this line is
/// what keeps the flow alive there.
pub(crate) const PLAN_PREVIEW_EPILOGUE: &str = "\
Also produce the visual plan preview: emit `.loadout/workflow/artifacts/plan.json` \
(the format is printed by `load plan schema`), run `load plan check --json` and fix \
errors until clean, then `load plan render` to open the review page in the user's \
browser. If plan.json already holds a different pending plan, don't overwrite it — \
write a sibling `plan-<topic>.json` instead and render it with `load plan render \
.loadout/workflow/artifacts/plan-<topic>.json --out .loadout/generated/plan-<topic>.html`. \
The `loadout-plan-preview` skill carries the full authoring guidance when available.";

/// On-disk format for an agent's command files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandFormat {
    /// Markdown with YAML frontmatter (Claude Code, opencode).
    Markdown,
    /// TOML with `description` + `prompt` (Gemini CLI).
    Toml,
    /// Cursor Skills: a folder per command holding a `SKILL.md` (the leaf
    /// folder names the skill, so commands land as
    /// `<commands_dir>/loadout/loadout-<stage>/SKILL.md` → `/loadout-<stage>`).
    Skill,
}

impl CommandFormat {
    /// File extension for this format.
    pub fn ext(self) -> &'static str {
        match self {
            CommandFormat::Markdown | CommandFormat::Skill => "md",
            CommandFormat::Toml => "toml",
        }
    }

    /// The placeholder this agent substitutes the user's command text into.
    /// Cursor skills have no substitution syntax — the invoking message rides
    /// along as-is, so the body just points the agent at it.
    fn arg_placeholder(self) -> &'static str {
        match self {
            CommandFormat::Markdown => "$ARGUMENTS",
            CommandFormat::Toml => "{{args}}",
            CommandFormat::Skill => "the request that accompanied this skill invocation",
        }
    }
}

/// A generated command file: its name within the namespace dir + its content.
pub struct StageCommand {
    /// Filename (e.g. `plan.md`), written under `<commands_dir>/loadout/`.
    pub filename: String,
    /// Full file content (frontmatter/TOML header + prompt body).
    pub content: String,
}

/// Render one command file per generated step of `wf` in `format`, in spine
/// order. Stages are laid onto the fixed canonical spine first (see
/// [`Workflow::canonical_layout`]), so the filenames are the stable
/// `/loadout:<command>` names (`plan`, `verify`, …) — identical to what the
/// studio shows — not each stage's free-string name. Custom stages that match no
/// canonical phase keep their own (slugged) name, appended after the spine.
pub fn stage_commands(
    wf: &Workflow,
    format: CommandFormat,
    review_commands: &[String],
) -> Vec<StageCommand> {
    let steps = wf.canonical_layout().steps();
    let total = steps.len();
    steps
        .iter()
        .enumerate()
        .map(|(i, &(command, stage))| {
            render_stage_command(wf, command, i, total, stage, format, review_commands)
        })
        .collect()
}

fn render_stage_command(
    wf: &Workflow,
    command: &str,
    idx: usize,
    total: usize,
    stage: &WorkflowStage,
    format: CommandFormat,
    review_commands: &[String],
) -> StageCommand {
    let stem = slug(command);
    let filename = match format {
        // A skill is a folder named after the skill, holding a SKILL.md.
        CommandFormat::Skill => format!("loadout-{stem}/SKILL.md"),
        _ => format!("{stem}.{}", format.ext()),
    };
    let description = stage
        .purpose
        .clone()
        .unwrap_or_else(|| format!("{} — {} stage", wf.title(), command));
    let body = stage_body(
        wf,
        command,
        idx,
        total,
        stage,
        format.arg_placeholder(),
        review_commands,
    );
    let content = match format {
        CommandFormat::Markdown => {
            format!(
                "---\ndescription: {}\n---\n\n{body}\n",
                yaml_dq(&description)
            )
        }
        CommandFormat::Skill => {
            format!(
                "---\nname: loadout-{stem}\ndescription: {}\n---\n\n{body}\n",
                yaml_dq(&description)
            )
        }
        // Build via the toml crate so escaping is always correct.
        CommandFormat::Toml => toml::to_string(&GeminiCommandFile {
            description: &description,
            prompt: &body,
        })
        .unwrap_or_default(),
    };
    StageCommand { filename, content }
}

/// Serializable shape of a Gemini CLI command file (`description` + `prompt`).
#[derive(Serialize)]
struct GeminiCommandFile<'a> {
    description: &'a str,
    prompt: &'a str,
}

/// The stage's prompt body — the contract the agent follows when the command
/// runs: where it sits in the spine, what to do, the handoff to read/write, the
/// gate, the exit checklist, and the user's per-run focus via `arg`. `command`
/// is the canonical `/loadout:<command>` name this step generates as; `idx`/`total`
/// position it within the workflow's generated steps.
fn stage_body(
    wf: &Workflow,
    command: &str,
    idx: usize,
    total: usize,
    stage: &WorkflowStage,
    arg: &str,
    review_commands: &[String],
) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "You're at the **{}** stage ({}/{}) of the _{}_ workflow.\n",
        command,
        idx + 1,
        total,
        wf.title()
    );
    if let Some(purpose) = &stage.purpose {
        let _ = writeln!(s, "{purpose}\n");
    }
    // Elaborate, on-demand guidance (channel 2 only): the full prescriptive body
    // lands in the per-step command file but never in the always-on `## Workflow`
    // context section, so depth here costs nothing until the command is invoked.
    if let Some(instructions) = &stage.instructions {
        let _ = writeln!(s, "{}\n", instructions.trim());
    }
    // `artifact_env_var` returns `None` for an unsafe name, so this both
    // validates the artifact and yields its `LOADOUT_<NAME>_PATH` env var.
    if let Some(reads) = &stage.reads {
        if let Some(env) = workflow::artifact_env_var(reads) {
            let _ = writeln!(
                s,
                "First read the handoff from `.loadout/{ARTIFACT_SUBDIR}/{reads}` \
                 (its path is also in `${env}`).\n"
            );
        }
    }
    if let Some(writes) = &stage.writes {
        if let Some(env) = workflow::artifact_env_var(writes) {
            let _ = writeln!(
                s,
                "Write your output to `.loadout/{ARTIFACT_SUBDIR}/{writes}` \
                 (its path is also in `${env}`) so the next stage can pick it up.\n"
            );
        }
    }
    if stage.gate {
        let _ = writeln!(
            s,
            "This stage is a checkpoint — pause and let me review before moving on.\n"
        );
    }
    if !stage.exit.is_empty() {
        let _ = writeln!(s, "Done when:");
        for item in &stage.exit {
            let _ = writeln!(s, "- {item}");
        }
        s.push('\n');
    }
    // Visual-plan enrichment for the plan slot: the stage also emits the
    // machine-readable plan and renders the review page. Keyed off the
    // canonical command, so a workflow's "planning"/"decompose" stage gets it
    // too (channel 2 only, by the same standing rule as verify below).
    if command == "plan" {
        let _ = writeln!(s, "{PLAN_PREVIEW_EPILOGUE}\n");
    }
    // Security enrichment for the verify slot: prefer the agent's native
    // review commands; otherwise embed the vendored checklist (channel 2
    // only — the always-on context carries summaries, by standing rule).
    if command == "verify" {
        if !review_commands.is_empty() {
            let _ = writeln!(s, "{REVIEW_COMMANDS_INTRO}");
            for c in review_commands {
                let _ = writeln!(s, "- `{c}`");
            }
            s.push('\n');
        } else {
            let _ = writeln!(s, "{SECURITY_CHECKLIST_INTRO}\n");
            let _ = writeln!(
                s,
                "{}\n",
                crate::workflow::strip_frontmatter(SECURITY_CHECKLIST).trim()
            );
        }
    }
    let _ = write!(s, "Focus for this run: {arg}");
    s.trim_end().to_string()
}

/// Slugify a free-string stage name into a safe command filename stem: lowercase
/// alphanumerics, runs of anything else collapsed to a single `-`, no leading or
/// trailing `-`. Falls back to `stage` for a name with no alphanumerics.
fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(c.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    if out.is_empty() {
        "stage".to_string()
    } else {
        out
    }
}

/// Double-quote and escape a string for a single-line YAML frontmatter value.
fn yaml_dq(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin(id: &str) -> Workflow {
        crate::workflow::builtin_workflows()
            .into_iter()
            .find(|w| w.id == id)
            .unwrap()
    }

    #[test]
    fn markdown_command_uses_canonical_names_args_and_handoff() {
        let cmds = stage_commands(&builtin("superpowers"), CommandFormat::Markdown, &[]);
        // One file per generated step, named by the *canonical* command —
        // Superpowers' `review` stage lands on `verify`, matching the spine.
        let names: Vec<&str> = cmds.iter().map(|c| c.filename.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "brainstorm.md",
                "plan.md",
                "implement.md",
                "verify.md",
                "ship.md"
            ]
        );

        let plan = cmds.iter().find(|c| c.filename == "plan.md").unwrap();
        assert!(plan.content.starts_with("---\ndescription: "));
        assert!(plan.content.contains("$ARGUMENTS"));
        // The plan stage writes the handoff artifact (path + env var).
        assert!(plan.content.contains(".loadout/workflow/artifacts/plan.md"));
        assert!(plan.content.contains("$LOADOUT_PLAN_PATH"));

        // The implement stage reads that same handoff.
        let implement = cmds.iter().find(|c| c.filename == "implement.md").unwrap();
        assert!(implement.content.contains("read the handoff"));
        assert!(implement.content.contains("plan.md"));

        // Superpowers' `review` stage generates as `verify`: a gate with an exit
        // checklist, and the heading shows the canonical name, not `review`.
        let verify = cmds.iter().find(|c| c.filename == "verify.md").unwrap();
        assert!(verify.content.contains("checkpoint"));
        assert!(verify.content.contains("Done when:"));
        assert!(verify.content.contains("**verify** stage"));
        assert!(!verify.content.contains("**review** stage"));
    }

    #[test]
    fn review_and_commit_land_in_separate_slots_extras_after() {
        // `review` → verify and `commit` → ship are now distinct phases (each its
        // own command), while two stages that truly collide on a slot collapse to
        // one (first wins), and a non-canonical name (`retro`) becomes an extra.
        let stg = |name: &str, purpose: &str| WorkflowStage {
            name: name.into(),
            purpose: Some(purpose.into()),
            instructions: None,
            reads: None,
            writes: None,
            gate: false,
            exit: vec![],
        };
        let wf = Workflow {
            id: "x".into(),
            name: Some("X".into()),
            description: None,
            icon: None,
            stages: vec![
                stg("review", "check the work"),
                stg("commit", "commit and push"),
                stg("qa", "second verify claimant — folds into review"),
                stg("retro", "capture lessons"),
            ],
            modeled_on: None,
            researched: None,
            source: None,
            disabled: false,
            origin: crate::fragment::Layer::Global,
        };
        let cmds = stage_commands(&wf, CommandFormat::Markdown, &[]);
        let names: Vec<&str> = cmds.iter().map(|c| c.filename.as_str()).collect();
        // review→verify, commit→ship (separate!), qa folds into verify, retro=extra.
        assert_eq!(names, vec!["verify.md", "ship.md", "retro.md"]);
        let verify = cmds.iter().find(|c| c.filename == "verify.md").unwrap();
        assert!(verify.content.contains("check the work")); // first verify claimant
        assert!(!verify.content.contains("second verify claimant")); // qa folded away
        let ship = cmds.iter().find(|c| c.filename == "ship.md").unwrap();
        assert!(ship.content.contains("commit and push")); // commit kept its own slot
    }

    #[test]
    fn plan_slot_commands_carry_the_visual_preview_epilogue() {
        // Built-ins: every workflow's plan-slot command gets the epilogue…
        for id in ["superpowers", "spec-driven"] {
            let cmds = stage_commands(&builtin(id), CommandFormat::Markdown, &[]);
            let plan = cmds.iter().find(|c| c.filename == "plan.md").unwrap();
            assert!(
                plan.content.contains(PLAN_PREVIEW_EPILOGUE),
                "{id} plan command must carry the preview epilogue"
            );
            // …and only the plan slot does.
            for c in cmds.iter().filter(|c| c.filename != "plan.md") {
                assert!(
                    !c.content.contains("load plan check"),
                    "{} must not carry the plan epilogue",
                    c.filename
                );
            }
        }
        // A custom workflow whose plan stage is named `decompose` still fills
        // the plan slot, so it gets the epilogue too — that's the point of
        // keying off the canonical command, not the stage name.
        let wf = Workflow {
            id: "x".into(),
            name: Some("X".into()),
            description: None,
            icon: None,
            stages: vec![WorkflowStage {
                name: "decompose".into(),
                purpose: Some("break it down".into()),
                instructions: None,
                reads: None,
                writes: None,
                gate: false,
                exit: vec![],
            }],
            modeled_on: None,
            researched: None,
            source: None,
            disabled: false,
            origin: crate::fragment::Layer::Global,
        };
        let cmds = stage_commands(&wf, CommandFormat::Markdown, &[]);
        let plan = cmds.iter().find(|c| c.filename == "plan.md").unwrap();
        assert!(plan.content.contains(PLAN_PREVIEW_EPILOGUE));
    }

    #[test]
    fn toml_command_is_valid_and_uses_gemini_args() {
        let cmds = stage_commands(&builtin("spec-driven"), CommandFormat::Toml, &[]);
        let plan = cmds.iter().find(|c| c.filename == "plan.toml").unwrap();
        // Parses as TOML with description + prompt.
        let v: toml::Value = toml::from_str(&plan.content).expect("valid TOML");
        assert!(v.get("description").and_then(|d| d.as_str()).is_some());
        let prompt = v.get("prompt").and_then(|p| p.as_str()).unwrap();
        // loadout's own arg placeholder is Gemini's `{{args}}`, not `$ARGUMENTS`.
        // (Vendored upstream content may itself contain `$ARGUMENTS` — that's the
        // source's text, not loadout's placeholder, so we don't assert its absence.)
        assert!(prompt.contains("{{args}}"), "gemini arg placeholder");
        // spec's plan reads spec.md and writes plan.md.
        assert!(prompt.contains("spec.md"));
        assert!(prompt.contains("plan.md"));
    }

    #[test]
    fn slug_cleans_free_string_stage_names() {
        assert_eq!(slug("plan"), "plan");
        assert_eq!(slug("Plan It!"), "plan-it");
        assert_eq!(slug("  spec / design  "), "spec-design");
        assert_eq!(slug("!!!"), "stage");
    }

    #[test]
    fn description_is_escaped_in_yaml_frontmatter() {
        // A purpose with a quote/colon must not break the markdown frontmatter.
        let wf = Workflow {
            id: "x".into(),
            name: None,
            description: None,
            icon: None,
            stages: vec![WorkflowStage {
                name: "plan".into(),
                purpose: Some("Write the \"spec\": be precise".into()),
                instructions: None,
                reads: None,
                writes: None,
                gate: false,
                exit: vec![],
            }],
            modeled_on: None,
            researched: None,
            source: None,
            disabled: false,
            origin: crate::fragment::Layer::Global,
        };
        let cmds = stage_commands(&wf, CommandFormat::Markdown, &[]);
        assert!(cmds[0]
            .content
            .contains(r#"description: "Write the \"spec\": be precise""#));
    }

    #[test]
    fn instructions_land_in_the_command_body_not_the_frontmatter() {
        // The elaborate `instructions` body rides in the command prompt (channel
        // 2), while the one-line `purpose` stays the frontmatter description.
        let wf = Workflow {
            id: "x".into(),
            name: Some("X".into()),
            description: None,
            icon: None,
            stages: vec![WorkflowStage {
                name: "plan".into(),
                purpose: Some("Plan the work".into()),
                instructions: Some(
                    "INSTRUCTIONS-MARKER: right-size each task to a few minutes.".into(),
                ),
                reads: None,
                writes: None,
                gate: false,
                exit: vec![],
            }],
            modeled_on: None,
            researched: None,
            source: None,
            disabled: false,
            origin: crate::fragment::Layer::Global,
        };
        let cmds = stage_commands(&wf, CommandFormat::Markdown, &[]);
        let plan = &cmds[0];
        // Body carries the elaborate instructions.
        assert!(plan.content.contains("INSTRUCTIONS-MARKER"));
        // Frontmatter description is the one-line purpose, not the instructions.
        assert!(plan.content.contains(r#"description: "Plan the work""#));
        let desc_line = plan
            .content
            .lines()
            .find(|l| l.starts_with("description:"))
            .unwrap();
        assert!(!desc_line.contains("INSTRUCTIONS-MARKER"));
        // Instructions render after the purpose summary in the body.
        let purpose_at = plan.content.find("Plan the work").unwrap();
        let instr_at = plan.content.find("INSTRUCTIONS-MARKER").unwrap();
        assert!(instr_at > purpose_at, "instructions follow the purpose");
    }

    #[test]
    fn superpowers_steps_carry_their_elaborate_instructions() {
        // The built-in Superpowers workflow ships a full prescriptive body for
        // each of its four active steps (channel 2). The `review` stage generates
        // as the canonical `/loadout:verify` command.
        let cmds = stage_commands(&builtin("superpowers"), CommandFormat::Markdown, &[]);
        let plan = cmds.iter().find(|c| c.filename == "plan.md").unwrap();
        // The real upstream writing-plans body lands in the command…
        assert!(plan.content.contains("bite-sized tasks"));
        // …with its loader-only YAML frontmatter stripped out.
        assert!(!plan.content.contains("name: writing-plans"));
        // …while the one-line summary stays the frontmatter description.
        assert!(plan
            .content
            .contains(r#"description: "Break the approved design"#));
        // The real requesting-code-review body lands on /loadout:verify.
        let verify = cmds.iter().find(|c| c.filename == "verify.md").unwrap();
        assert!(verify.content.contains("code reviewer subagent"));
    }

    #[test]
    fn unsafe_artifact_name_is_not_referenced() {
        // A hostile/malformed artifact name never becomes a path in the command.
        let wf = Workflow {
            id: "x".into(),
            name: None,
            description: None,
            icon: None,
            stages: vec![WorkflowStage {
                name: "plan".into(),
                purpose: Some("do".into()),
                instructions: None,
                reads: None,
                writes: Some("../escape.md".into()),
                gate: false,
                exit: vec![],
            }],
            modeled_on: None,
            researched: None,
            source: None,
            disabled: false,
            origin: crate::fragment::Layer::Global,
        };
        let cmds = stage_commands(&wf, CommandFormat::Markdown, &[]);
        assert!(!cmds[0].content.contains("escape.md"));
        assert!(!cmds[0].content.contains("Write your output"));
    }

    #[test]
    fn verify_body_lists_native_review_commands_when_the_agent_has_them() {
        let rc = vec!["/code-review".to_string(), "/security-review".to_string()];
        let cmds = stage_commands(&builtin("superpowers"), CommandFormat::Markdown, &rc);
        let verify = cmds.iter().find(|c| c.filename == "verify.md").unwrap();
        assert!(verify.content.contains(REVIEW_COMMANDS_INTRO));
        assert!(verify.content.contains("- `/code-review`"));
        assert!(verify.content.contains("- `/security-review`"));
        assert!(!verify.content.contains(SECURITY_CHECKLIST_INTRO));
        // Only the verify stage is enriched.
        let plan = cmds.iter().find(|c| c.filename == "plan.md").unwrap();
        assert!(!plan.content.contains(REVIEW_COMMANDS_INTRO));
    }

    #[test]
    fn verify_body_falls_back_to_the_vendored_security_checklist() {
        let cmds = stage_commands(&builtin("superpowers"), CommandFormat::Markdown, &[]);
        let verify = cmds.iter().find(|c| c.filename == "verify.md").unwrap();
        assert!(verify.content.contains(SECURITY_CHECKLIST_INTRO));
        assert!(!verify.content.contains(REVIEW_COMMANDS_INTRO));
        // The vendored body actually made it in (not just the intro line). The
        // vendored file is ~190 lines; assert on a distinctive phrase from it.
        assert!(verify.content.contains("senior security engineer"));
    }

    #[test]
    fn review_commands_is_not_part_of_the_config_schema() {
        let err = toml::from_str::<crate::adapters::AgentDescriptor>(
            "id = \"x\"\ngenerated_filename = \"x.md\"\nreview_commands = [\"/code-review\"]\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }
}
