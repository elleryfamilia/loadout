//! End-to-end CLI tests driving the real `rosita` binary against temp repos.
//!
//! Each test isolates the global config via `ROSITA_CONFIG_DIR` so it never
//! reads the developer's real `~/.config/rosita`.

use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// A temp repo plus an (empty) isolated global config dir.
struct Fixture {
    repo: TempDir,
    global: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Fixture {
            repo: TempDir::new().unwrap(),
            global: TempDir::new().unwrap(),
        }
    }

    fn write(&self, rel: &str, content: &str) {
        let p = self.repo.path().join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, content).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.repo.path().join(rel)).unwrap()
    }

    fn exists(&self, rel: &str) -> bool {
        self.repo.path().join(rel).exists()
    }

    /// Turn the fixture into a real git repo (so `.gitignore` management applies).
    fn git_init(&self) {
        let ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(self.repo.path())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git init failed in fixture");
    }

    /// A configured `rosita` command pointed at this repo, globally isolated.
    fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("rosita").unwrap();
        // Point the global config dir at an empty location → no global layer.
        c.env("ROSITA_CONFIG_DIR", self.global.path().join("empty"));
        c.arg("--cwd").arg(self.repo.path());
        c
    }

    fn rust_project(&self) {
        self.write(
            "Cargo.toml",
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        );
        self.write("src/main.rs", "fn main() { println!(\"hi\"); }\n");
    }
}

#[test]
fn detect_emits_json_with_rust_stack() {
    let fx = Fixture::new();
    fx.rust_project();

    fx.cmd()
        .arg("detect")
        .arg("--json")
        .assert()
        .success()
        .stdout(predicate::str::contains("\"stacks\""))
        .stdout(predicate::str::contains("\"rust\""))
        .stdout(predicate::str::contains("\"cargo\""))
        .stdout(predicate::str::contains("\"Rust\""));
}

#[test]
fn render_claude_creates_overlay_marker_and_gitignore() {
    let fx = Fixture::new();
    fx.rust_project();
    fx.git_init(); // gitignore management only applies inside a repo

    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success()
        .stdout(predicate::str::contains("claude"))
        .stdout(predicate::str::contains("profile rust"));

    // Generated overlay exists and carries the header + a detected command.
    assert!(fx.exists(".rosita/generated/claude.md"));
    let overlay = fx.read(".rosita/generated/claude.md");
    assert!(overlay.contains("rosita:generated"));
    assert!(overlay.contains("cargo test"));
    assert!(overlay.contains("not enforced policy"));

    // CLAUDE.local.md has the managed import block.
    let local = fx.read("CLAUDE.local.md");
    assert!(local.contains("BEGIN rosita (managed)"));
    assert!(local.contains("@.rosita/generated/claude.md"));

    // gitignore covers the generated dir.
    assert!(fx.read(".gitignore").contains(".rosita/generated/"));

    // Audit log written.
    assert!(fx.exists(".rosita/logs/events.jsonl"));
    let audit = fx.read(".rosita/logs/events.jsonl");
    assert!(audit.contains("\"agent\":\"claude\""));
    assert!(audit.contains("\"profile\":\"rust\""));
}

#[test]
fn render_in_non_repo_writes_overlay_but_no_gitignore() {
    // First-class non-repo use case (e.g. running in $HOME): the overlay and
    // the CLAUDE.local.md import are written, but no stray .gitignore is made.
    let fx = Fixture::new(); // deliberately NOT a git repo
    fx.rust_project();

    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success()
        .stdout(predicate::str::contains("profile rust"));

    assert!(fx.exists(".rosita/generated/claude.md"));
    assert!(fx.exists("CLAUDE.local.md"));
    // The key guarantee: no .gitignore is created outside a repo.
    assert!(!fx.exists(".gitignore"));

    // detect labels the directory as non-repo and still names the project.
    fx.cmd()
        .arg("detect")
        .assert()
        .success()
        .stdout(predicate::str::contains("non-repo mode"))
        .stdout(predicate::str::contains("name       :"));
}

#[test]
fn render_is_idempotent() {
    let fx = Fixture::new();
    fx.rust_project();

    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success();
    // Second render: nothing changed → reported unchanged.
    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success()
        .stdout(predicate::str::contains("unchanged"));
}

#[test]
fn render_preserves_user_content_in_claude_local() {
    let fx = Fixture::new();
    fx.rust_project();
    fx.write("CLAUDE.local.md", "# My personal notes\n\nKeep this.\n");

    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success();

    let local = fx.read("CLAUDE.local.md");
    assert!(local.contains("Keep this."));
    assert!(local.contains("BEGIN rosita (managed)"));
    // user content precedes the managed block
    assert!(local.find("Keep this.").unwrap() < local.find("BEGIN rosita").unwrap());
}

#[test]
fn codex_does_not_touch_agents_md_without_override() {
    let fx = Fixture::new();
    fx.rust_project();
    fx.write("AGENTS.md", "# Hand-written AGENTS\n\nDo not clobber.\n");

    fx.cmd()
        .args(["render", "--agent", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--override"));

    // AGENTS.md untouched, no override file created.
    assert_eq!(
        fx.read("AGENTS.md"),
        "# Hand-written AGENTS\n\nDo not clobber.\n"
    );
    assert!(!fx.exists("AGENTS.override.md"));
    assert!(fx.exists(".rosita/generated/agents.md"));
}

#[test]
fn codex_override_merges_existing_agents_md() {
    let fx = Fixture::new();
    fx.rust_project();
    fx.write("AGENTS.md", "# Hand-written AGENTS\n\nPreserve me.\n");

    fx.cmd()
        .args(["render", "--agent", "codex", "--override"])
        .assert()
        .success();

    assert!(fx.exists("AGENTS.override.md"));
    let ov = fx.read("AGENTS.override.md");
    assert!(ov.contains("Preserve me.")); // original AGENTS.md content kept
    assert!(ov.contains("BEGIN rosita (managed)")); // managed block appended
    assert!(ov.contains("agent context")); // inlined generated content
                                           // original AGENTS.md still intact
    assert_eq!(
        fx.read("AGENTS.md"),
        "# Hand-written AGENTS\n\nPreserve me.\n"
    );
}

#[test]
fn dry_run_writes_nothing() {
    let fx = Fixture::new();
    fx.rust_project();

    fx.cmd()
        .args(["--dry-run", "render", "--agent", "claude"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dry run"))
        .stdout(predicate::str::contains("would create"));

    assert!(!fx.exists(".rosita/generated/claude.md"));
    assert!(!fx.exists("CLAUDE.local.md"));
    // Dry-run writes nothing at all — not even the audit log.
    assert!(!fx.exists(".rosita/logs/events.jsonl"));
}

#[test]
fn explain_reports_selection_and_plan() {
    let fx = Fixture::new();
    fx.rust_project();

    fx.cmd()
        .arg("explain")
        .assert()
        .success()
        .stdout(predicate::str::contains("Profile selection → rust"))
        .stdout(predicate::str::contains("Write plan"))
        .stdout(predicate::str::contains("Profiles considered"));
}

#[test]
fn init_scaffolds_config_and_templates() {
    let fx = Fixture::new();
    fx.git_init(); // so the gitignore step runs

    fx.cmd()
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("Initializing rosita"));

    assert!(fx.exists(".rosita/config.toml"));
    assert!(fx.exists(".rosita/templates/overlay.md.j2"));
    assert!(fx.read(".gitignore").contains(".rosita/generated/"));
    assert!(fx.read(".rosita/config.toml").contains("[profiles]"));
}

#[test]
fn run_dry_run_reports_would_exec_without_launching() {
    let fx = Fixture::new();
    fx.rust_project();

    fx.cmd()
        .args(["--dry-run", "run", "claude", "chat", "--model", "sonnet"])
        .assert()
        .success()
        // rosita injects --append-system-prompt for Claude, then the user args.
        .stdout(predicate::str::contains(
            "would exec: claude --append-system-prompt",
        ))
        .stdout(predicate::str::contains("chat --model sonnet"));

    // dry-run preflight wrote nothing.
    assert!(!fx.exists(".rosita/generated/claude.md"));
}

#[test]
fn doctor_runs_and_reports() {
    let fx = Fixture::new();
    fx.rust_project();

    fx.cmd()
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("Environment"))
        .stdout(predicate::str::contains("Agents"))
        .stdout(predicate::str::contains("Templates"))
        .stdout(predicate::str::contains("doctor:"));
}

#[test]
fn render_all_six_agents_emit_gitignored_overlays() {
    let fx = Fixture::new();
    fx.rust_project();

    fx.cmd()
        .args(["render", "--agent", "all"])
        .assert()
        .success();

    for f in [
        "claude.md",
        "agents.md",
        "gemini.md",
        "opencode.md",
        "copilot.md",
        "generic.md",
    ] {
        assert!(fx.exists(&format!(".rosita/generated/{f}")), "missing {f}");
    }
    // Emit-only agents never touch committed instruction files.
    assert!(!fx.exists("AGENTS.md"));
    assert!(!fx.exists("GEMINI.md"));
    assert!(!fx.exists(".github/copilot-instructions.md"));
    // Only Claude (local-file agent) is auto-wired.
    assert!(fx.exists("CLAUDE.local.md"));
}

#[test]
fn gemini_emit_only_prints_wire_hint() {
    let fx = Fixture::new();
    fx.rust_project();

    fx.cmd()
        .args(["render", "--agent", "gemini"])
        .assert()
        .success()
        .stdout(predicate::str::contains("gemini"))
        .stdout(predicate::str::contains("@.rosita/generated/gemini.md"));
    assert!(fx.exists(".rosita/generated/gemini.md"));
    assert!(!fx.exists("GEMINI.md"));
}

#[test]
fn overlay_has_self_healing_banner() {
    let fx = Fixture::new();
    fx.rust_project();
    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success();
    let overlay = fx.read(".rosita/generated/claude.md");
    assert!(overlay.contains("rosita refresh"));
    assert!(overlay.contains("rosita clean"));
    assert!(overlay.contains("$ROSITA_RUN"));
}

#[test]
fn render_in_repo_gitignores_the_importer() {
    let fx = Fixture::new();
    fx.rust_project();
    fx.git_init();
    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success();
    // We created CLAUDE.local.md, so it must be gitignored (it's a derived,
    // machine-specific artifact).
    let gi = fx.read(".gitignore");
    assert!(gi.contains(".rosita/generated/"));
    assert!(gi.contains("CLAUDE.local.md"));
}

#[test]
fn clean_removes_rosita_artifacts() {
    let fx = Fixture::new();
    fx.rust_project();
    fx.git_init();
    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success();
    assert!(fx.exists(".rosita/generated/claude.md"));
    assert!(fx.exists("CLAUDE.local.md"));

    fx.cmd()
        .args(["clean", "--agent", "claude"])
        .assert()
        .success();
    // Generated overlay gone; CLAUDE.local.md (only our block) removed.
    assert!(!fx.exists(".rosita/generated/claude.md"));
    assert!(!fx.exists("CLAUDE.local.md"));
}

#[test]
fn clean_preserves_user_content_in_importer() {
    let fx = Fixture::new();
    fx.rust_project();
    fx.write("CLAUDE.local.md", "# my notes\n\nkeep this\n");
    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success();

    fx.cmd()
        .args(["clean", "--agent", "claude"])
        .assert()
        .success();
    // The importer survives with the managed block stripped; user text intact.
    assert!(fx.exists("CLAUDE.local.md"));
    let local = fx.read("CLAUDE.local.md");
    assert!(local.contains("keep this"));
    assert!(!local.contains("BEGIN rosita"));
    assert!(!fx.exists(".rosita/generated/claude.md"));
}

#[test]
fn unknown_agent_is_an_error() {
    let fx = Fixture::new();
    fx.rust_project();
    fx.cmd()
        .args(["render", "--agent", "nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown agent 'nope'"));
}

#[test]
fn custom_agent_via_config_is_first_class() {
    let fx = Fixture::new();
    fx.rust_project();
    // A user-defined agent in repo config — no code change required.
    fx.write(
        ".rosita/config.toml",
        "[[agents]]\nid = \"myagent\"\ngenerated_filename = \"myagent.md\"\nlaunch = \"echo\"\nwire_hint = \"include myagent.md\"\n",
    );

    fx.cmd()
        .args(["render", "--agent", "myagent"])
        .assert()
        .success();
    assert!(fx.exists(".rosita/generated/myagent.md"));

    // …and it's launchable via `run` (dry-run shows the configured program).
    fx.cmd()
        .args(["--dry-run", "run", "myagent", "hello"])
        .assert()
        .success()
        .stdout(predicate::str::contains("would exec: echo"))
        .stdout(predicate::str::contains("hello"));
}

#[test]
fn additive_composition_layers_stack_and_baseline() {
    // A plain Rust repo matches both `rust` and the always-on `default`, so the
    // overlay composes the Rust capability *and* the baseline — they layer
    // instead of the single highest-priority profile winning.
    let fx = Fixture::new();
    fx.rust_project();

    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success()
        .stdout(predicate::str::contains("profile rust"));

    let overlay = fx.read(".rosita/generated/claude.md");
    // Each capability is its own section…
    assert!(overlay.contains("### Rust conventions"));
    assert!(overlay.contains("### Baseline"));
    // …and both bodies are present (rust + baseline guidance).
    assert!(overlay.contains("clippy"));
    assert!(overlay.contains("minimal, focused"));

    // The audit log records the composed capability set.
    let audit = fx.read(".rosita/logs/events.jsonl");
    assert!(audit.contains("rust-conventions"));
    assert!(audit.contains("baseline"));
}

#[test]
fn user_capability_via_config_is_composed() {
    let fx = Fixture::new();
    fx.rust_project();
    // A reusable capability plus a profile that composes it — no code change.
    fx.write(
        ".rosita/config.toml",
        "[[capabilities]]\n\
         id = \"house-style\"\n\
         description = \"House style\"\n\
         risk = \"caution\"\n\
         guidance = \"Always run the formatter before committing.\"\n\
         \n\
         [[profiles]]\n\
         name = \"house\"\n\
         priority = 60\n\
         capabilities = [\"house-style\"]\n",
    );

    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success();

    let overlay = fx.read(".rosita/generated/claude.md");
    // The custom capability renders with its risk annotation and body…
    assert!(overlay.contains("### House style — ⚠️ caution"));
    assert!(overlay.contains("Always run the formatter before committing."));
    // …and still composes alongside the stack capability.
    assert!(overlay.contains("### Rust conventions"));

    let audit = fx.read(".rosita/logs/events.jsonl");
    assert!(audit.contains("house-style"));
}

#[test]
fn explain_lists_active_capabilities() {
    let fx = Fixture::new();
    fx.rust_project();

    fx.cmd()
        .arg("explain")
        .assert()
        .success()
        .stdout(predicate::str::contains("Active capabilities"))
        .stdout(predicate::str::contains("rust-conventions"))
        .stdout(predicate::str::contains("baseline"));
}
