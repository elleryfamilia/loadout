use serde::{Deserialize, Serialize};

pub const FORMAT: &str = "loadout.plan/1";
pub const MAX_INPUT_BYTES: usize = 2 * 1024 * 1024;

pub const PLAN_FIELDS: &[&str] = &["format", "meta", "phases", "risks", "open_questions"];
pub const META_FIELDS: &[&str] = &["id", "title", "goal_md", "agent", "created", "revision"];
pub const PHASE_FIELDS: &[&str] = &["id", "title", "summary_md", "tasks"];
pub const TASK_FIELDS: &[&str] = &[
    "id",
    "title",
    "summary_md",
    "status",
    "risk",
    "depends_on",
    "files",
    "acceptance",
    "validation",
    "estimate",
];
pub const FILE_FIELDS: &[&str] = &["path", "action", "note"];
pub const RISK_FIELDS: &[&str] = &["id", "title", "severity", "mitigation_md"];
pub const QUESTION_FIELDS: &[&str] = &["id", "question_md", "blocking"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub format: String,
    pub meta: Meta,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<Phase>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risks: Vec<Risk>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_questions: Vec<OpenQuestion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_md: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Phase {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_md: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<PlanTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanTask {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_md: Option<String>,
    #[serde(default)]
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<RiskLevel>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validation: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate: Option<Estimate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRef {
    pub path: String,
    pub action: FileAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Risk {
    pub id: String,
    pub title: String,
    pub severity: RiskLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mitigation_md: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenQuestion {
    pub id: String,
    pub question_md: String,
    #[serde(default)]
    pub blocking: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Planned,
    InProgress,
    Done,
    Blocked,
    Cut,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Estimate {
    S,
    M,
    L,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileAction {
    Create,
    Modify,
    Delete,
    Test,
}

#[derive(Debug, Clone, Serialize)]
pub struct Issue {
    pub path: String,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Issue {
    pub fn new(path: impl Into<String>, code: &str, message: impl Into<String>) -> Self {
        Issue {
            path: path.into(),
            code: code.into(),
            message: message.into(),
            hint: None,
        }
    }
}

#[derive(Debug)]
pub struct Parsed {
    pub plan: Plan,
    pub warnings: Vec<Issue>,
}

/// Parse `input`. Strict mode errors on unknown fields; lenient mode prunes
/// them and reports warnings. Both modes gate on size and format version.
pub fn parse(input: &str, lenient: bool) -> Result<Parsed, Vec<Issue>> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(vec![Issue::new(
            "/",
            "input_too_large",
            format!(
                "plan.json is {} bytes; the limit is {MAX_INPUT_BYTES}",
                input.len()
            ),
        )]);
    }
    let value: serde_json::Value = serde_json::from_str(input)
        .map_err(|e| vec![Issue::new("/", "invalid_json", e.to_string())])?;

    // Format gate before anything else so a newer plan gets one clear error.
    match value.get("format").and_then(|f| f.as_str()) {
        Some(f) if f == FORMAT => {}
        Some(f) if f.starts_with("loadout.plan/") => {
            return Err(vec![Issue::new("/format", "format_too_new",
                format!("plan format {f} is newer than this loadout understands ({FORMAT}) — run `load update`"))]);
        }
        _ => {
            return Err(vec![Issue::new(
                "/format",
                "bad_format",
                format!("expected \"format\": \"{FORMAT}\""),
            )])
        }
    }

    // Unknown-field walk (the single authority for unknown-field policy).
    let mut pruned = value.clone();
    let mut unknown = Vec::new();
    walk_unknown(&mut pruned, "", PLAN_FIELDS, &mut unknown);
    if !unknown.is_empty() && !lenient {
        return Err(unknown);
    }

    // Typed deserialize with pointer paths for shape errors.
    let pruned_str = pruned.to_string();
    let de = &mut serde_json::Deserializer::from_str(&pruned_str);
    let plan: Plan = serde_path_to_error::deserialize(de).map_err(|e| {
        vec![Issue::new(
            e.path().to_string(),
            "invalid_shape",
            e.inner().to_string(),
        )]
    })?;
    Ok(Parsed {
        plan,
        warnings: unknown,
    })
}

/// Recursively record (and remove) fields not in the schema. Descends into
/// the known nested collections with their own field sets.
fn walk_unknown(v: &mut serde_json::Value, path: &str, fields: &[&str], out: &mut Vec<Issue>) {
    let Some(map) = v.as_object_mut() else { return };
    let stray: Vec<String> = map
        .keys()
        .filter(|k| !fields.contains(&k.as_str()))
        .cloned()
        .collect();
    for k in stray {
        out.push(Issue::new(
            format!("{path}/{k}"),
            "unknown_field",
            format!("unknown field `{k}` (known: {})", fields.join(", ")),
        ));
        map.remove(&k);
    }
    type DescendRule<'a> = (&'a str, &'a [&'a str], Option<(&'a str, &'a [&'a str])>);
    let descend: &[DescendRule] = &[
        ("meta", META_FIELDS, None),
        ("phases", PHASE_FIELDS, Some(("tasks", TASK_FIELDS))),
        ("risks", RISK_FIELDS, None),
        ("open_questions", QUESTION_FIELDS, None),
    ];
    for (key, sub_fields, nested) in descend {
        let Some(sub) = map.get_mut(*key) else {
            continue;
        };
        match sub {
            serde_json::Value::Object(_) => {
                walk_unknown(sub, &format!("{path}/{key}"), sub_fields, out)
            }
            serde_json::Value::Array(items) => {
                for (i, item) in items.iter_mut().enumerate() {
                    let ipath = format!("{path}/{key}/{i}");
                    walk_items(item, &ipath, sub_fields, *nested, out);
                }
            }
            _ => {}
        }
    }
}

fn walk_items(
    item: &mut serde_json::Value,
    path: &str,
    fields: &[&str],
    nested: Option<(&str, &[&str])>,
    out: &mut Vec<Issue>,
) {
    let Some(map) = item.as_object_mut() else {
        return;
    };
    let stray: Vec<String> = map
        .keys()
        .filter(|k| !fields.contains(&k.as_str()))
        .cloned()
        .collect();
    for k in stray {
        out.push(Issue::new(
            format!("{path}/{k}"),
            "unknown_field",
            format!("unknown field `{k}` (known: {})", fields.join(", ")),
        ));
        map.remove(&k);
    }
    if let Some((sub_key, sub_fields)) = nested {
        if let Some(serde_json::Value::Array(subs)) = map.get_mut(sub_key) {
            for (i, sub) in subs.iter_mut().enumerate() {
                walk_items(sub, &format!("{path}/{sub_key}/{i}"), sub_fields, None, out);
            }
        }
    }
    // Tasks nest files with their own field set.
    if fields == TASK_FIELDS {
        if let Some(serde_json::Value::Array(files)) = map.get_mut("files") {
            for (i, f) in files.iter_mut().enumerate() {
                walk_items(f, &format!("{path}/files/{i}"), FILE_FIELDS, None, out);
            }
        }
    }
}

/// Canonical fingerprint of the validated model.
pub fn plan_hash(plan: &Plan) -> String {
    crate::hash::context_hash(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/plan/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn minimal_and_kitchen_sink_parse() {
        let p = parse(&fixture("minimal.json"), false).unwrap();
        assert_eq!(p.plan.meta.id, "demo");
        assert!(p.warnings.is_empty());
        let k = parse(&fixture("kitchen-sink.json"), false).unwrap();
        assert_eq!(k.plan.phases.len(), 2);
        assert_eq!(k.plan.phases[0].tasks[1].files.len(), 3);
    }

    #[test]
    fn canonical_roundtrip_is_stable() {
        let p = parse(&fixture("kitchen-sink.json"), false).unwrap().plan;
        let a = serde_json::to_string(&p).unwrap();
        let b = serde_json::to_string(&parse(&a, false).unwrap().plan).unwrap();
        assert_eq!(a, b);
        assert!(plan_hash(&p).starts_with("sha256:"));
    }

    #[test]
    fn type_error_reports_pointer_path() {
        let bad = fixture("kitchen-sink.json").replace("\"revision\": 2", "\"revision\": \"two\"");
        let errs = parse(&bad, false).unwrap_err();
        assert_eq!(errs[0].code, "invalid_shape");
        assert!(errs[0].path.contains("meta.revision"), "{}", errs[0].path);
    }

    #[test]
    fn unknown_field_strict_errors_lenient_warns() {
        let extra = fixture("minimal.json").replace(
            "\"title\": \"Demo plan\"",
            "\"title\": \"Demo plan\", \"vibes\": \"good\"",
        );
        let errs = parse(&extra, false).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| e.code == "unknown_field" && e.path == "/meta/vibes"));
        let ok = parse(&extra, true).unwrap();
        assert!(ok.warnings.iter().any(|w| w.code == "unknown_field"));
    }

    #[test]
    fn newer_format_gets_clear_error() {
        let newer = fixture("minimal.json").replace("loadout.plan/1", "loadout.plan/2");
        let errs = parse(&newer, false).unwrap_err();
        assert_eq!(errs[0].code, "format_too_new");
        assert!(errs[0].message.contains("load update"));
    }

    #[test]
    fn oversized_input_is_rejected() {
        let big = "x".repeat(2 * 1024 * 1024 + 1);
        let errs = parse(&big, false).unwrap_err();
        assert_eq!(errs[0].code, "input_too_large");
    }

    #[test]
    fn known_fields_match_serde() {
        // Guards walker/struct drift: a fully-populated struct serializes to
        // exactly the walker's field list.
        let k = parse(&fixture("kitchen-sink.json"), false).unwrap().plan;
        let v = serde_json::to_value(&k.phases[0].tasks[1]).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
        for k in &keys {
            assert!(TASK_FIELDS.contains(k), "walker missing field {k}");
        }
    }
}
