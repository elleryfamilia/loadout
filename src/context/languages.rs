//! Language / stack / package-manager detection.
//!
//! Two signals are combined:
//! 1. A bounded walk of the repo counting source-file extensions → languages.
//! 2. Marker files at the repo root (`Cargo.toml`, `package.json`, `go.mod`, …)
//!    → stacks and package managers (with lockfile-based PM disambiguation).

use std::collections::HashMap;
use std::path::Path;

use crate::context::{Context, ContextDetector, DetectInput};

/// Directories never worth walking for language stats.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".venv",
    "venv",
    "__pycache__",
    "vendor",
    ".turbo",
    ".cache",
    ".idea",
    ".gradle",
];

/// Cap the walk so a huge monorepo can't stall detection.
const MAX_FILES: usize = 8000;
const MAX_DEPTH: usize = 8;

/// Detector for languages, stacks and package managers.
pub struct LanguageDetector;

impl ContextDetector for LanguageDetector {
    fn name(&self) -> &'static str {
        "languages"
    }

    fn detect(&self, input: &DetectInput, ctx: &mut Context) -> crate::Result<()> {
        let base = &input.repo_base;
        ctx.languages = detect_languages(base);
        let (stacks, pms) = detect_stacks_and_pms(base);
        ctx.stacks = stacks;
        ctx.package_managers = pms;
        // User-defined custom targets detected against the repo, kept separate
        // from built-in `stacks`. Script predicates execute only when `live`
        // (real renders); otherwise their cached verdict is served.
        ctx.custom_targets = crate::target::detect_custom(&input.config.targets, base, input.live);
        Ok(())
    }
}

/// Map a file extension to a human language name.
fn ext_to_language(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "rs" => "Rust",
        "ts" | "mts" | "cts" => "TypeScript",
        "tsx" => "TypeScript",
        "js" | "mjs" | "cjs" => "JavaScript",
        "jsx" => "JavaScript",
        "py" | "pyi" => "Python",
        "go" => "Go",
        "rb" => "Ruby",
        "java" => "Java",
        "kt" | "kts" => "Kotlin",
        "swift" => "Swift",
        "c" | "h" => "C",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "C++",
        "cs" => "C#",
        "php" => "PHP",
        "sh" | "bash" | "zsh" => "Shell",
        "sql" => "SQL",
        "scala" => "Scala",
        "ex" | "exs" => "Elixir",
        "dart" => "Dart",
        _ => return None,
    })
}

/// Count source files by language across a bounded walk; return languages
/// ordered by prevalence (then alphabetically), capped to a useful set.
pub fn detect_languages(base: &Path) -> Vec<String> {
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    let mut budget = MAX_FILES;
    walk(base, 0, &mut budget, &mut |path| {
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if let Some(lang) = ext_to_language(&ext.to_ascii_lowercase()) {
                *counts.entry(lang).or_insert(0) += 1;
            }
        }
    });

    let mut ranked: Vec<(&'static str, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    ranked
        .into_iter()
        .take(8)
        .map(|(lang, _)| lang.to_string())
        .collect()
}

/// Inspect root marker files to determine stacks and package managers.
pub fn detect_stacks_and_pms(base: &Path) -> (Vec<String>, Vec<String>) {
    let mut stacks: Vec<String> = Vec::new();
    let mut pms: Vec<String> = Vec::new();
    let has = |name: &str| base.join(name).exists();

    // Rust
    if has("Cargo.toml") {
        stacks.push("rust".into());
        pms.push("cargo".into());
    }

    // Node / Next.js
    if has("package.json") {
        stacks.push("node".into());
        if package_json_has_next(base) {
            stacks.push("nextjs".into());
        }
        pms.push(detect_node_pm(base));
    }

    // Go
    if has("go.mod") {
        stacks.push("go".into());
        pms.push("go".into());
    }

    // Python
    if has("pyproject.toml") || has("requirements.txt") || has("setup.py") || has("Pipfile") {
        stacks.push("python".into());
        pms.push(detect_python_pm(base));
    }

    dedup(&mut stacks);
    dedup(&mut pms);
    (stacks, pms)
}

fn package_json_has_next(base: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(base.join("package.json")) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    ["dependencies", "devDependencies"]
        .iter()
        .any(|k| json.get(k).and_then(|d| d.get("next")).is_some())
}

fn detect_node_pm(base: &Path) -> String {
    let pm = if base.join("bun.lockb").exists() || base.join("bun.lock").exists() {
        "bun"
    } else if base.join("pnpm-lock.yaml").exists() {
        "pnpm"
    } else if base.join("yarn.lock").exists() {
        "yarn"
    } else if base.join("package-lock.json").exists() {
        "npm"
    } else {
        // No lockfile: honor packageManager field if present, else npm.
        return package_json_pm_field(base).unwrap_or_else(|| "npm".to_string());
    };
    pm.to_string()
}

fn package_json_pm_field(base: &Path) -> Option<String> {
    let text = std::fs::read_to_string(base.join("package.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let field = json.get("packageManager")?.as_str()?;
    let name = field.split('@').next()?;
    Some(name.to_string())
}

fn detect_python_pm(base: &Path) -> String {
    if base.join("uv.lock").exists() {
        return "uv".into();
    }
    if base.join("poetry.lock").exists() || pyproject_has_poetry(base) {
        return "poetry".into();
    }
    if base.join("Pipfile").exists() {
        return "pipenv".into();
    }
    "pip".into()
}

fn pyproject_has_poetry(base: &Path) -> bool {
    std::fs::read_to_string(base.join("pyproject.toml"))
        .map(|t| t.contains("[tool.poetry]"))
        .unwrap_or(false)
}

fn dedup(v: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    v.retain(|x| seen.insert(x.clone()));
}

/// Depth-bounded, ignore-aware recursive walk invoking `f` for each file.
fn walk(dir: &Path, depth: usize, budget: &mut usize, f: &mut impl FnMut(&Path)) {
    if depth > MAX_DEPTH || *budget == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if *budget == 0 {
            return;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if file_type.is_dir() {
            if SKIP_DIRS.contains(&name.as_ref()) || name.starts_with('.') {
                continue;
            }
            walk(&path, depth + 1, budget, f);
        } else if file_type.is_file() {
            *budget -= 1;
            f(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn detects_rust_stack() {
        let d = tmp();
        fs::write(d.path().join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
        fs::write(d.path().join("main.rs"), "fn main() {}").unwrap();
        let (stacks, pms) = detect_stacks_and_pms(d.path());
        assert!(stacks.contains(&"rust".to_string()));
        assert!(pms.contains(&"cargo".to_string()));
        assert_eq!(detect_languages(d.path()), vec!["Rust".to_string()]);
    }

    #[test]
    fn detects_nextjs_and_pnpm() {
        let d = tmp();
        fs::write(
            d.path().join("package.json"),
            r#"{"dependencies":{"next":"14.0.0","react":"18"}}"#,
        )
        .unwrap();
        fs::write(d.path().join("pnpm-lock.yaml"), "lockfileVersion: 9\n").unwrap();
        let (stacks, pms) = detect_stacks_and_pms(d.path());
        assert!(stacks.contains(&"node".to_string()));
        assert!(stacks.contains(&"nextjs".to_string()));
        assert_eq!(pms, vec!["pnpm".to_string()]);
    }

    #[test]
    fn detects_python_uv() {
        let d = tmp();
        fs::write(d.path().join("pyproject.toml"), "[project]\nname='x'\n").unwrap();
        fs::write(d.path().join("uv.lock"), "").unwrap();
        let (stacks, pms) = detect_stacks_and_pms(d.path());
        assert_eq!(stacks, vec!["python".to_string()]);
        assert_eq!(pms, vec!["uv".to_string()]);
    }

    #[test]
    fn walk_skips_node_modules() {
        let d = tmp();
        fs::create_dir_all(d.path().join("node_modules/pkg")).unwrap();
        fs::write(d.path().join("node_modules/pkg/index.js"), "x").unwrap();
        fs::write(d.path().join("app.ts"), "const x = 1").unwrap();
        let langs = detect_languages(d.path());
        // Only app.ts is counted; node_modules is skipped.
        assert_eq!(langs, vec!["TypeScript".to_string()]);
    }
}
