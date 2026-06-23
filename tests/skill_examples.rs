//! The TOML examples shipped in the embedded skills must stay valid loadout
//! config — otherwise a skill would teach people a schema that no longer
//! parses. This guards every ```toml block in each skill's reference.

use std::path::PathBuf;

use loadout::config::Config;
use loadout::fragment::Layer;

/// Every fenced ```toml block in `md`, trimmed.
fn toml_blocks(md: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = md;
    while let Some(start) = rest.find("```toml") {
        let after = &rest[start + "```toml".len()..];
        let Some(end) = after.find("```") else { break };
        out.push(after[..end].trim().to_string());
        rest = &after[end + 3..];
    }
    out
}

fn parse_global(toml: &str) -> loadout::Result<Config> {
    Config::from_layer_strs(vec![(
        Layer::Global,
        PathBuf::from("/g/config.toml"),
        toml.to_string(),
    )])
}

/// Parse-check every ```toml block in a skill's reference.md, returning them.
fn check_skill_reference(skill: &str) -> Vec<String> {
    let path = format!("{}/skills/{skill}/reference.md", env!("CARGO_MANIFEST_DIR"));
    let md = std::fs::read_to_string(&path).expect("read skill reference.md");
    let blocks = toml_blocks(&md);
    assert!(
        !blocks.is_empty(),
        "{skill}: expected ```toml examples in the skill reference"
    );
    for block in &blocks {
        parse_global(block).unwrap_or_else(|e| {
            panic!("{skill}: example must parse as config:\n{block}\n\nerror: {e}")
        });
    }
    blocks
}

#[test]
fn skill_reference_toml_examples_are_valid_config() {
    let blocks = check_skill_reference("loadout-migrate");

    // The first (complete) example defines the documented profiles + a dynamic
    // fragment — assert the schema the skill teaches still resolves.
    let cfg = parse_global(&blocks[0]).unwrap();
    assert!(cfg.profiles.iter().any(|p| p.name == "machine"));
    assert!(cfg.profiles.iter().any(|p| p.name == "rust"));
    assert!(cfg.fragments.iter().any(|c| c.id == "host"));
}

#[test]
fn remember_skill_reference_toml_examples_are_valid_config() {
    let blocks = check_skill_reference("loadout-remember");

    // The editing example teaches a minimal fragment edit — it must keep
    // resolving as a fragment with guidance.
    let cfg = parse_global(&blocks[0]).unwrap();
    assert!(cfg.fragments.iter().any(|c| c.id == "conventional-commits"));
}

#[test]
fn shipped_example_config_is_a_valid_global_config() {
    // `examples/config.toml` (+ the private `local.toml`) is the annotated
    // global config we point people at — it must stay valid as one.
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");
    let config =
        std::fs::read_to_string(format!("{dir}/config.toml")).expect("examples/config.toml");
    let local = std::fs::read_to_string(format!("{dir}/local.toml")).expect("examples/local.toml");

    let cfg = Config::from_layer_strs(vec![
        (Layer::Global, PathBuf::from("/g/config.toml"), config),
        (Layer::GlobalLocal, PathBuf::from("/g/local.toml"), local),
    ])
    .expect("examples must form a valid global config");

    assert!(cfg.profiles.iter().any(|p| p.name == "rust"));
    assert!(cfg.profiles.iter().any(|p| p.name == "machine"));
    assert!(cfg.fragments.iter().any(|c| c.id == "rust-conventions"));
    assert!(cfg.fragments.iter().any(|c| c.id == "work-strict"));
}
