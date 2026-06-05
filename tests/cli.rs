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

    /// Author a minimal library: a `rust-conventions` capability plus a `rust`
    /// profile that targets the rust stack and composes it. Capabilities and
    /// profiles are global-only, so this writes the *global* config.
    fn rust_profile(&self) {
        self.author(
            "[[capabilities]]\n\
             id = \"rust-conventions\"\n\
             description = \"Rust conventions\"\n\
             tags = [\"stack\"]\n\
             guidance = \"Rust project. Build with cargo, lint with clippy.\"\n\
             \n\
             [[profiles]]\n\
             name = \"rust\"\n\
             targets = [\"rust\"]\n\
             capabilities = [\"rust-conventions\"]\n",
        );
    }

    /// Author global capabilities/profiles — the only layer that honors them.
    /// (A repo layer declaring caps/profiles is dropped by the loader.)
    fn author(&self, content: &str) {
        self.write_global("config.toml", content);
    }

    /// Write a file into the isolated global config dir (the one `cmd()` points
    /// `ROSITA_CONFIG_DIR` at), e.g. a trusted global `config.toml`.
    fn write_global(&self, rel: &str, content: &str) {
        let p = self.global.path().join("empty").join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    /// Read a file back from the isolated global config dir.
    fn read_global(&self, rel: &str) -> String {
        fs::read_to_string(self.global.path().join("empty").join(rel)).unwrap()
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
    fx.rust_profile();
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
    fx.rust_profile();

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
fn editing_the_global_library_re_renders_a_repo_with_unchanged_context() {
    // The overlay freshness fingerprint folds in the composition, so editing the
    // GLOBAL library re-renders a repo whose detected context is identical.
    // Regression: the fingerprint used to be context-only, so a config change
    // left a stale overlay and `render`/`run` falsely reported "unchanged".
    let fx = Fixture::new();
    fx.rust_project();
    fx.author(
        "[[capabilities]]\nid = \"rc\"\ndescription = \"Rust\"\nguidance = \"VERSION-ONE guidance.\"\n\
         \n[[profiles]]\nname = \"rust\"\ntargets = [\"rust\"]\ncapabilities = [\"rc\"]\n",
    );
    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success();
    assert!(fx
        .read(".rosita/generated/claude.md")
        .contains("VERSION-ONE"));

    // No change → still idempotent.
    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success()
        .stdout(predicate::str::contains("unchanged"));

    // Edit the capability's guidance in the global config; the repo's detected
    // context is unchanged.
    fx.author(
        "[[capabilities]]\nid = \"rc\"\ndescription = \"Rust\"\nguidance = \"VERSION-TWO guidance.\"\n\
         \n[[profiles]]\nname = \"rust\"\ntargets = [\"rust\"]\ncapabilities = [\"rc\"]\n",
    );
    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success();

    // The overlay must reflect the edit — proof the cache was invalidated.
    let overlay = fx.read(".rosita/generated/claude.md");
    assert!(
        overlay.contains("VERSION-TWO"),
        "a global-config edit must re-render the overlay; got:\n{overlay}"
    );
    assert!(!overlay.contains("VERSION-ONE"));
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
    fx.rust_profile();

    fx.cmd()
        .arg("explain")
        .assert()
        .success()
        .stdout(predicate::str::contains("Profile selection → rust"))
        .stdout(predicate::str::contains("Write plan"))
        .stdout(predicate::str::contains("Profiles considered"));
}

#[test]
fn render_auto_manages_gitignore_and_init_is_gone() {
    // There is no `rosita init` — a repo needs no scaffolding. Rendering an
    // agent gitignores everything rosita manages, automatically.
    let fx = Fixture::new();
    fx.rust_project();
    fx.rust_profile();
    fx.git_init();

    fx.cmd()
        .arg("init")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand"));

    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success();
    let gi = fx.read(".gitignore");
    assert!(gi.contains(".rosita/generated/"));
    assert!(gi.contains(".rosita/cache/"));
    assert!(gi.contains(".rosita/logs/"));
    assert!(gi.contains(".rosita/local.toml"));
}

#[test]
fn local_toml_supplies_private_params_to_capabilities() {
    // Public config defines a capability whose guidance references params but
    // names no machine; the private local.toml fills them in. The rendered
    // overlay carries the private values; the public config never does.
    let fx = Fixture::new();
    fx.rust_project();
    fx.author(
        "[[capabilities]]\n\
         id = \"deploy\"\n\
         description = \"Deploy target\"\n\
         guidance = \"Deploy as {{ params.user }}@{{ params.host }}.\"\n\
         \n\
         [[profiles]]\n\
         name = \"deploy\"\n\
         targets = [\"rust\"]\n\
         capabilities = [\"deploy\"]\n",
    );
    // Private params still come from the repo's local.toml (merged by id onto
    // the global capability).
    fx.write(
        ".rosita/local.toml",
        "[capability_params.deploy]\nhost = \"box.private.example\"\nuser = \"deployer\"\n",
    );

    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success();

    let overlay = fx.read(".rosita/generated/claude.md");
    assert!(overlay.contains("Deploy as deployer@box.private.example."));
    // The shareable (global) config never contained the private host.
    assert!(!fx
        .read_global("config.toml")
        .contains("box.private.example"));
}

#[test]
fn doctor_leak_lint_flags_public_but_not_local() {
    // A machine-specific literal in the PUBLIC config.toml is flagged…
    let fx = Fixture::new();
    fx.rust_project();
    fx.write(
        ".rosita/config.toml",
        "[host_classes]\nwork = [\"*.corp.example.com\"]\n",
    );
    fx.cmd()
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("looks private"))
        .stdout(predicate::str::contains("corp.example.com"));

    // …but the same literal in the PRIVATE local.toml is not.
    let fx2 = Fixture::new();
    fx2.rust_project();
    fx2.write(".rosita/config.toml", "[defaults]\nagent = \"claude\"\n");
    fx2.write(
        ".rosita/local.toml",
        "[host_classes]\nwork = [\"*.corp.example.com\"]\n",
    );
    fx2.cmd()
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("no private-looking literals"))
        .stdout(predicate::str::contains("looks private").not());
}

#[test]
fn doctor_flags_a_profile_referencing_an_unknown_capability() {
    // A hand-deleted capability leaves a dangling profile reference that renders
    // nothing — doctor surfaces it.
    let fx = Fixture::new();
    fx.rust_project();
    fx.author(
        "[[capabilities]]\nid = \"present\"\nguidance = \"hi\"\n\
         \n[[profiles]]\nname = \"rust\"\ntargets = [\"rust\"]\ncapabilities = [\"present\", \"gone\"]\n",
    );

    fx.cmd()
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("unknown capability 'gone'"))
        .stdout(predicate::str::contains("unknown capability 'present'").not());
}

#[test]
fn doctor_flags_repo_declared_caps_and_profiles() {
    // Capabilities and profiles are global-only; a repo that declares them is
    // ignored at render time, so doctor surfaces the otherwise-invisible mistake.
    let fx = Fixture::new();
    fx.rust_project();
    fx.write(
        ".rosita/config.toml",
        "[[capabilities]]\nid = \"x\"\nguidance = \"hi\"\n\
         \n[[profiles]]\nname = \"p\"\ntargets = [\"rust\"]\ncapabilities = [\"x\"]\n",
    );

    fx.cmd()
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("global-only"))
        .stdout(predicate::str::contains("capabilities and profiles"));

    // A clean repo (no repo-declared caps/profiles) is not flagged.
    let clean = Fixture::new();
    clean.rust_project();
    clean
        .cmd()
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("global-only").not());
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
fn profile_composes_its_capability_set_with_no_baseline() {
    // Pick-one: the selected profile renders exactly its own capabilities, each
    // as its own section. There is no always-on baseline layered underneath.
    let fx = Fixture::new();
    fx.rust_project();
    fx.author(
        "[[capabilities]]\n\
         id = \"rust-conventions\"\n\
         description = \"Rust conventions\"\n\
         guidance = \"Rust project. Lint with clippy.\"\n\
         \n\
         [[capabilities]]\n\
         id = \"terse\"\n\
         description = \"Terse communication\"\n\
         guidance = \"Be terse; lead with the result.\"\n\
         \n\
         [[profiles]]\n\
         name = \"rust\"\n\
         targets = [\"rust\"]\n\
         capabilities = [\"rust-conventions\", \"terse\"]\n",
    );

    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success()
        .stdout(predicate::str::contains("profile rust"));

    let overlay = fx.read(".rosita/generated/claude.md");
    // Both of the profile's capabilities render, each its own section…
    assert!(overlay.contains("### Rust conventions"));
    assert!(overlay.contains("### Terse communication"));
    assert!(overlay.contains("clippy"));
    assert!(overlay.contains("lead with the result"));
    // …and nothing is auto-injected: no baseline section appears.
    assert!(!overlay.contains("### Baseline"));

    // The audit log records exactly the composed capability set.
    let audit = fx.read(".rosita/logs/events.jsonl");
    assert!(audit.contains("rust-conventions"));
    assert!(audit.contains("terse"));
    assert!(!audit.contains("baseline"));
}

#[test]
fn user_capability_via_config_is_composed() {
    let fx = Fixture::new();
    fx.rust_project();
    // Reusable capabilities plus a profile that composes them — no code change.
    fx.author(
        "[[capabilities]]\n\
         id = \"house-style\"\n\
         description = \"House style\"\n\
         risk = \"caution\"\n\
         guidance = \"Always run the formatter before committing.\"\n\
         \n\
         [[capabilities]]\n\
         id = \"rust-conventions\"\n\
         description = \"Rust conventions\"\n\
         guidance = \"Rust project. Lint with clippy.\"\n\
         \n\
         [[profiles]]\n\
         name = \"house\"\n\
         targets = [\"rust\"]\n\
         capabilities = [\"house-style\", \"rust-conventions\"]\n",
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
fn detect_probes_is_opt_in_and_shows_host() {
    let fx = Fixture::new();
    fx.rust_project();

    // The `host` provider always resolves (no exec), so --probes is deterministic.
    fx.cmd()
        .args(["detect", "--probes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Probes"))
        .stdout(predicate::str::contains("host"));

    // JSON form nests provider output under a "probes" key.
    fx.cmd()
        .args(["detect", "--probes", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"probes\""))
        .stdout(predicate::str::contains("\"host\""));

    // Bare detect never probes (no subprocesses, no "probes" key).
    fx.cmd()
        .args(["detect", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"probes\"").not());
}

#[test]
fn dynamic_provider_capability_renders_live_output() {
    let fx = Fixture::new();
    fx.rust_project();
    // A capability backed by the built-in `host` provider (always available,
    // no exec, no trust needed).
    fx.author(
        "[[capabilities]]\n\
         id = \"machine\"\n\
         description = \"Machine\"\n\
         provider = \"host\"\n\
         guidance = \"OS={{ provider.data.os }}\"\n\
         \n\
         [[profiles]]\n\
         name = \"dyn\"\n\
         targets = [\"rust\"]\n\
         capabilities = [\"machine\"]\n",
    );

    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success();
    let overlay = fx.read(".rosita/generated/claude.md");
    assert!(overlay.contains(&format!("OS={}", std::env::consts::OS)));
}

#[test]
fn global_layer_command_runs_without_allow() {
    // A command authored in the GLOBAL config is trusted (you wrote it) and
    // runs without `rosita allow`.
    let fx = Fixture::new();
    fx.rust_project();
    fx.write_global(
        "config.toml",
        "[[capabilities]]\n\
         id = \"greet\"\n\
         command = \"echo global-ok\"\n\
         \n\
         [[profiles]]\n\
         name = \"g\"\n\
         targets = [\"rust\"]\n\
         capabilities = [\"greet\"]\n",
    );

    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success();
    let overlay = fx.read(".rosita/generated/claude.md");
    assert!(overlay.contains("global-ok"));
    assert!(!overlay.contains("skipped untrusted"));
}

#[test]
fn repo_command_capability_is_ignored() {
    // Capabilities are global-only: a `command` capability authored in a repo
    // layer is dropped by the loader. It never renders, and there is nothing to
    // trust — the per-repo command-trust gate is therefore dormant. (A command
    // authored in the GLOBAL config still runs; see `global_layer_command_runs`.)
    let fx = Fixture::new();
    fx.rust_project();
    fx.write(
        ".rosita/config.toml",
        "[[capabilities]]\n\
         id = \"greet\"\n\
         command = \"echo hello-rosita\"\n\
         \n\
         [[profiles]]\n\
         name = \"dyn\"\n\
         targets = [\"rust\"]\n\
         capabilities = [\"greet\"]\n",
    );

    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success();
    let overlay = fx.read(".rosita/generated/claude.md");
    // Neither the command output nor a trust-skip marker — the cap is gone.
    assert!(!overlay.contains("hello-rosita"));
    assert!(!overlay.contains("skipped untrusted"));
}

#[test]
fn explain_lists_active_capabilities() {
    let fx = Fixture::new();
    fx.rust_project();
    fx.rust_profile();

    fx.cmd()
        .arg("explain")
        .assert()
        .success()
        .stdout(predicate::str::contains("Active capabilities"))
        .stdout(predicate::str::contains("rust-conventions"));
}

#[test]
fn capabilities_list_marks_active_and_shows_one() {
    let fx = Fixture::new();
    fx.rust_project();
    // Your library: two capabilities, with only rust-conventions composed by the
    // selected rust profile (terse-comms is present but inactive here).
    fx.author(
        "[[capabilities]]\n\
         id = \"rust-conventions\"\n\
         description = \"Rust conventions\"\n\
         guidance = \"Rust project. Lint with clippy.\"\n\
         \n\
         [[capabilities]]\n\
         id = \"terse-comms\"\n\
         description = \"Terse communication\"\n\
         guidance = \"Be terse.\"\n\
         \n\
         [[profiles]]\n\
         name = \"rust\"\n\
         targets = [\"rust\"]\n\
         capabilities = [\"rust-conventions\"]\n",
    );

    // `list` (default): your library, with rust-conventions active on a rust
    // repo and the unreferenced terse-comms present but inactive.
    fx.cmd()
        .arg("capabilities")
        .assert()
        .success()
        .stdout(predicate::str::contains("Capabilities ("))
        .stdout(predicate::str::contains("● rust-conventions"))
        .stdout(predicate::str::contains("· terse-comms"));

    // `show <id>`: full details including active-via-profile.
    fx.cmd()
        .args(["capabilities", "show", "rust-conventions"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Capability: rust-conventions"))
        .stdout(predicate::str::contains("via profile 'rust'"));

    // Unknown id errors.
    fx.cmd()
        .args(["capabilities", "show", "nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown capability 'nope'"));

    // JSON form.
    fx.cmd()
        .args(["capabilities", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"active\""))
        .stdout(predicate::str::contains("\"rust-conventions\""));
}

#[test]
fn profiles_list_marks_matching() {
    let fx = Fixture::new();
    fx.rust_project();
    fx.rust_profile();
    fx.cmd()
        .arg("profiles")
        .assert()
        .success()
        .stdout(predicate::str::contains("Profiles ("))
        // the rust profile is selected (→) on a rust repo.
        .stdout(predicate::str::contains("→ rust"))
        .stdout(predicate::str::contains("capabilities: rust-conventions"));
}

#[test]
fn agents_list_shows_delivery() {
    let fx = Fixture::new();
    fx.rust_project();
    fx.cmd()
        .arg("agents")
        .assert()
        .success()
        .stdout(predicate::str::contains("Agents ("))
        .stdout(predicate::str::contains("claude"))
        .stdout(predicate::str::contains("import → CLAUDE.local.md"))
        .stdout(predicate::str::contains("emit-only"));
}

/// Two profiles that both target the rust stack — an ambiguous selection.
const TWO_RUST_PROFILES: &str = "[[capabilities]]\n\
     id = \"ca\"\n\
     description = \"Cap A\"\n\
     guidance = \"AAA guidance\"\n\
     \n\
     [[capabilities]]\n\
     id = \"cb\"\n\
     description = \"Cap B\"\n\
     guidance = \"BBB guidance\"\n\
     \n\
     [[profiles]]\n\
     name = \"rust-a\"\n\
     targets = [\"rust\"]\n\
     capabilities = [\"ca\"]\n\
     \n\
     [[profiles]]\n\
     name = \"rust-b\"\n\
     targets = [\"rust\"]\n\
     capabilities = [\"cb\"]\n";

#[test]
fn ambiguous_profiles_render_empty_and_warn() {
    // 2 profiles match and nothing is remembered → non-interactive commands warn
    // and apply no profile (empty overlay) rather than guessing.
    let fx = Fixture::new();
    fx.rust_project();
    fx.git_init();
    fx.author(TWO_RUST_PROFILES);

    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success()
        .stderr(predicate::str::contains("profiles match this project"))
        .stdout(predicate::str::contains("profile none"));

    let overlay = fx.read(".rosita/generated/claude.md");
    assert!(!overlay.contains("AAA guidance"));
    assert!(!overlay.contains("BBB guidance"));
}

#[test]
fn binding_in_local_toml_selects_profile_without_prompt() {
    // A remembered choice in the repo's private local.toml resolves selection
    // straight to that profile — no prompt, no ambiguity warning.
    let fx = Fixture::new();
    fx.rust_project();
    fx.git_init(); // repo scope → binding is read from .rosita/local.toml
    fx.author(TWO_RUST_PROFILES);
    fx.write(".rosita/local.toml", "[binding]\nprofile = \"rust-b\"\n");

    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success()
        .stdout(predicate::str::contains("profile rust-b"))
        .stderr(predicate::str::contains("profiles match").not());

    let overlay = fx.read(".rosita/generated/claude.md");
    assert!(overlay.contains("BBB guidance"));
    assert!(!overlay.contains("AAA guidance"));
}

#[test]
fn stale_binding_targets_hash_redetects() {
    // A remembered binding whose `targets_hash` no longer matches the profile's
    // targets (the profile was retargeted since binding) is treated as stale:
    // the name is ignored and selection re-detects. With two profiles matching
    // that means the ambiguity warning + no profile — not a silent stale pick.
    let fx = Fixture::new();
    fx.rust_project();
    fx.git_init();
    fx.author(TWO_RUST_PROFILES);
    fx.write(
        ".rosita/local.toml",
        "[binding]\nprofile = \"rust-b\"\ntargets_hash = \"sha256:stale\"\n",
    );

    fx.cmd()
        .args(["render", "--agent", "claude"])
        .assert()
        .success()
        .stderr(predicate::str::contains("profiles match this project"))
        .stdout(predicate::str::contains("profile none"));

    let overlay = fx.read(".rosita/generated/claude.md");
    assert!(!overlay.contains("BBB guidance"));
}

#[test]
fn run_with_ambiguous_profiles_non_tty_falls_back_without_blocking() {
    // The interactive `run` chooser must never block when there's no terminal
    // (CI/piped): it warns and applies no profile instead of reading stdin.
    let fx = Fixture::new();
    fx.rust_project();
    fx.git_init();
    fx.author(TWO_RUST_PROFILES);

    fx.cmd()
        .args(["--dry-run", "run", "claude"])
        .assert()
        .success()
        .stderr(predicate::str::contains("isn't an interactive terminal"))
        .stdout(predicate::str::contains("would exec: claude"));
}
