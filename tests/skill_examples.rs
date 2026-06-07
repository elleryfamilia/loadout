//! The TOML examples shipped in the `rosita-migrate` skill must stay valid
//! rosita config — otherwise the skill would teach people a schema that no
//! longer parses. This guards every ```toml block in the skill's reference.

use std::path::PathBuf;

use rosita::fragment::Layer;
use rosita::config::Config;

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

fn parse_global(toml: &str) -> rosita::Result<Config> {
    Config::from_layer_strs(vec![(
        Layer::Global,
        PathBuf::from("/g/config.toml"),
        toml.to_string(),
    )])
}

#[test]
fn skill_reference_toml_examples_are_valid_config() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/skills/rosita-migrate/reference.md"
    );
    let md = std::fs::read_to_string(path).expect("read skill reference.md");
    let blocks = toml_blocks(&md);
    assert!(
        !blocks.is_empty(),
        "expected ```toml examples in the skill reference"
    );

    for block in &blocks {
        parse_global(block).unwrap_or_else(|e| {
            panic!("skill example must parse as config:\n{block}\n\nerror: {e}")
        });
    }

    // The first (complete) example defines the documented profiles + a dynamic
    // fragment — assert the schema the skill teaches still resolves.
    let cfg = parse_global(&blocks[0]).unwrap();
    assert!(cfg.profiles.iter().any(|p| p.name == "machine"));
    assert!(cfg.profiles.iter().any(|p| p.name == "rust"));
    assert!(cfg.fragments.iter().any(|c| c.id == "host"));
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
