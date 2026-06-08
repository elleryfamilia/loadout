//! Build/test/lint/run command discovery.
//!
//! Driven by the stacks and package managers detected upstream plus, for Node
//! projects, the actual `scripts` in `package.json`.

use std::path::Path;

use crate::context::{Context, ContextDetector, DetectInput, ProjectCommands};

/// Detector that populates [`Context::commands`].
pub struct CommandDetector;

impl ContextDetector for CommandDetector {
    fn name(&self) -> &'static str {
        "commands"
    }

    fn detect(&self, input: &DetectInput, ctx: &mut Context) -> crate::Result<()> {
        let base = &input.repo_base;
        let mut cmds = ProjectCommands::default();

        for stack in &ctx.stacks {
            match stack.as_str() {
                "rust" => {
                    push(&mut cmds.build, "cargo build");
                    push(&mut cmds.test, "cargo test");
                    push(&mut cmds.lint, "cargo clippy --all-targets");
                }
                "go" => {
                    push(&mut cmds.build, "go build ./...");
                    push(&mut cmds.test, "go test ./...");
                    push(&mut cmds.lint, "go vet ./...");
                    if base.join(".golangci.yml").exists() || base.join(".golangci.yaml").exists() {
                        push(&mut cmds.lint, "golangci-lint run");
                    }
                }
                "python" => {
                    let pm = ctx.package_managers.first().map(String::as_str);
                    let runner = match pm {
                        Some("uv") => "uv run ",
                        Some("poetry") => "poetry run ",
                        _ => "",
                    };
                    push(&mut cmds.test, &format!("{runner}pytest"));
                    push(&mut cmds.lint, &format!("{runner}ruff check"));
                }
                "node" | "nextjs" => {
                    let pm = ctx
                        .package_managers
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "npm".into());
                    add_node_scripts(base, &pm, &mut cmds);
                }
                _ => {}
            }
        }

        ctx.commands = cmds;
        Ok(())
    }
}

/// Translate package.json scripts into PM-prefixed commands.
fn add_node_scripts(base: &Path, pm: &str, cmds: &mut ProjectCommands) {
    let scripts = read_scripts(base);

    // `test` is conventionally a top-level command in every PM.
    if scripts.iter().any(|s| s == "test") {
        push(&mut cmds.test, &format!("{pm} test"));
    }
    if scripts.iter().any(|s| s == "build") {
        push(&mut cmds.build, &run_script(pm, "build"));
    }
    if scripts.iter().any(|s| s == "lint") {
        push(&mut cmds.lint, &run_script(pm, "lint"));
    }
    for dev in ["dev", "start"] {
        if scripts.iter().any(|s| s == dev) {
            push(&mut cmds.run, &run_script(pm, dev));
        }
    }
}

/// `pnpm <script>` vs `npm run <script>` etc.
fn run_script(pm: &str, script: &str) -> String {
    match pm {
        "npm" => format!("npm run {script}"),
        "yarn" => format!("yarn {script}"),
        "pnpm" => format!("pnpm {script}"),
        "bun" => format!("bun run {script}"),
        other => format!("{other} run {script}"),
    }
}

fn read_scripts(base: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(base.join("package.json")) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    json.get("scripts")
        .and_then(|s| s.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

fn push(v: &mut Vec<String>, cmd: &str) {
    let cmd = cmd.to_string();
    if !v.contains(&cmd) {
        v.push(cmd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::fs;

    fn detect_in(dir: &Path, stacks: &[&str], pms: &[&str]) -> ProjectCommands {
        let cfg = Config::defaults();
        let input = DetectInput {
            cwd: dir.to_path_buf(),
            repo_base: dir.to_path_buf(),
            config: &cfg,
            live: false,
        };
        let mut ctx = crate::context::test_support::sample_context();
        ctx.cwd = dir.to_path_buf();
        ctx.repo_base = dir.to_path_buf();
        ctx.stacks = stacks.iter().map(|s| s.to_string()).collect();
        ctx.package_managers = pms.iter().map(|s| s.to_string()).collect();
        CommandDetector.detect(&input, &mut ctx).unwrap();
        ctx.commands
    }

    #[test]
    fn rust_commands() {
        let d = tempfile::tempdir().unwrap();
        let cmds = detect_in(d.path(), &["rust"], &["cargo"]);
        assert_eq!(cmds.build, vec!["cargo build"]);
        assert_eq!(cmds.test, vec!["cargo test"]);
        assert_eq!(cmds.lint, vec!["cargo clippy --all-targets"]);
    }

    #[test]
    fn node_scripts_with_pnpm() {
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("package.json"),
            r#"{"scripts":{"build":"next build","test":"vitest","lint":"eslint .","dev":"next dev"}}"#,
        )
        .unwrap();
        let cmds = detect_in(d.path(), &["node", "nextjs"], &["pnpm"]);
        assert_eq!(cmds.build, vec!["pnpm build"]);
        assert_eq!(cmds.test, vec!["pnpm test"]);
        assert_eq!(cmds.lint, vec!["pnpm lint"]);
        assert_eq!(cmds.run, vec!["pnpm dev"]);
    }

    #[test]
    fn python_uv_commands() {
        let d = tempfile::tempdir().unwrap();
        let cmds = detect_in(d.path(), &["python"], &["uv"]);
        assert_eq!(cmds.test, vec!["uv run pytest"]);
        assert_eq!(cmds.lint, vec!["uv run ruff check"]);
    }
}
