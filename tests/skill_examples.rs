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
fn import_workflow_skill_reference_toml_examples_are_valid_config() {
    let blocks = check_skill_reference("loadout-import-workflow");

    // The worked example defines the `compound` workflow with all five steps —
    // it must parse as a real `[[workflows]]` entry, not just look right.
    let cfg = parse_global(&blocks[0]).unwrap();
    let wf = cfg
        .workflows
        .iter()
        .find(|w| w.id == "compound")
        .expect("worked example defines the compound workflow");
    assert_eq!(wf.stages.len(), 5);

    // The worked example shows the elaborate `instructions` body the import skill
    // teaches — the plan stage carries one, parsed from a TOML multi-line string.
    let plan = wf
        .stages
        .iter()
        .find(|s| s.name == "plan")
        .expect("worked example has a plan stage");
    let instr = plan
        .instructions
        .as_deref()
        .expect("the plan stage demonstrates an instructions body");
    assert!(instr.contains("Planning is where most of the work happens"));

    // An equipping snippet must bind the workflow on a loadout (the only way to
    // use a workflow now that the global default is gone).
    assert!(
        blocks.iter().any(|b| parse_global(b)
            .map(|c| c
                .profiles
                .iter()
                .any(|p| p.workflow.as_deref() == Some("compound")))
            .unwrap_or(false)),
        "an example should equip the workflow on a loadout"
    );
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

/// Every fenced ```json block in the plan-preview reference must parse and
/// validate under the real deserializer — the doc IS the schema.
#[test]
fn plan_preview_reference_json_examples_are_valid() {
    let path = format!(
        "{}/skills/loadout-plan-preview/reference.md",
        env!("CARGO_MANIFEST_DIR")
    );
    let md = std::fs::read_to_string(&path).expect("read reference.md");
    let mut checked = 0;
    let mut rest = md.as_str();
    while let Some(start) = rest.find("```json") {
        let after = &rest[start + 7..];
        let Some(end) = after.find("```") else { break };
        let block = after[..end].trim();
        if block.contains("\"loadout.plan/1\"") {
            let parsed = loadout::plan::model::parse(block, false)
                .unwrap_or_else(|e| panic!("reference example must parse: {e:?}"));
            assert!(loadout::plan::model::validate(&parsed.plan).is_empty());
            checked += 1;
        }
        rest = &after[end + 3..];
    }
    assert!(checked >= 1, "expected at least one loadout.plan/1 example");
}
