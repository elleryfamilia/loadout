use serde::{Deserialize, Serialize};

pub const FORMAT: &str = "loadout.plan/1";
pub const MAX_INPUT_BYTES: usize = 2 * 1024 * 1024;

pub const PLAN_FIELDS: &[&str] = &["format", "meta", "phases", "risks", "open_questions"];
pub const META_FIELDS: &[&str] = &[
    "id",
    "title",
    "goal_md",
    "summary_md",
    "key_points",
    "out_of_scope",
    "agent",
    "created",
    "revision",
];
pub const PHASE_FIELDS: &[&str] = &["id", "title", "icon", "summary_md", "tasks"];
pub const TASK_FIELDS: &[&str] = &[
    "id",
    "title",
    "icon",
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
    /// Executive summary (markdown). Rendered at the very top of the page,
    /// above open questions/risks/phases — see `render::render`'s
    /// `section.plan-summary`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_md: Option<String>,
    /// Bullet points backing the executive summary (each item is markdown).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub key_points: Vec<String>,
    /// Explicit non-goals, rendered as plain text (no markdown).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub out_of_scope: Vec<String>,
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
    /// A name from `plan::icons::icon_names()`, shown before the title in
    /// the phase's summary line. Optional — omit rather than force one on
    /// every phase; `validate()` rejects a name outside the vocabulary
    /// (`unknown_icon`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_md: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<PlanTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanTask {
    pub id: String,
    pub title: String,
    /// See `Phase::icon` — same vocabulary, same validation. Reserve this
    /// for notable tasks rather than setting it on every task; the phase
    /// icon already carries the section's theme.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
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

pub const MAX_TASKS: usize = 500;
pub const MAX_PHASES: usize = 50;
pub const MAX_RISKS: usize = 100;
pub const MAX_QUESTIONS: usize = 100;
pub const MAX_EDGES: usize = 2000;
// Generous on purpose: the limit exists to bound a pathological single
// string, not to shape how much a plan author writes — big plans are a
// supported case (the 2 MiB document cap is the real ceiling). Raised from
// 10k after the first real plan's task summaries pressed against it.
pub const MAX_STRING: usize = 65_536;
pub const MAX_KEY_POINTS: usize = 25;
pub const MAX_OUT_OF_SCOPE: usize = 25;
const ID_HINT: &str = "ids match ^[a-z][a-z0-9_-]{0,63}$ and are unique document-wide";

fn id_ok(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some('a'..='z'))
        && id.len() <= 64
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-'))
}

fn check_strings(plan: &Plan, errs: &mut Vec<Issue>) {
    // Meta fields
    if plan.meta.id.len() > MAX_STRING {
        errs.push(Issue::new(
            "/meta/id",
            "string_too_long",
            format!("id is {} chars (limit {MAX_STRING})", plan.meta.id.len()),
        ));
    }
    if plan.meta.title.len() > MAX_STRING {
        errs.push(Issue::new(
            "/meta/title",
            "string_too_long",
            format!(
                "title is {} chars (limit {MAX_STRING})",
                plan.meta.title.len()
            ),
        ));
    }
    if let Some(goal_md) = &plan.meta.goal_md {
        if goal_md.len() > MAX_STRING {
            errs.push(Issue::new(
                "/meta/goal_md",
                "string_too_long",
                format!("goal_md is {} chars (limit {MAX_STRING})", goal_md.len()),
            ));
        }
    }
    if let Some(agent) = &plan.meta.agent {
        if agent.len() > MAX_STRING {
            errs.push(Issue::new(
                "/meta/agent",
                "string_too_long",
                format!("agent is {} chars (limit {MAX_STRING})", agent.len()),
            ));
        }
    }
    if let Some(created) = &plan.meta.created {
        if created.len() > MAX_STRING {
            errs.push(Issue::new(
                "/meta/created",
                "string_too_long",
                format!("created is {} chars (limit {MAX_STRING})", created.len()),
            ));
        }
    }
    if let Some(summary_md) = &plan.meta.summary_md {
        if summary_md.len() > MAX_STRING {
            errs.push(Issue::new(
                "/meta/summary_md",
                "string_too_long",
                format!(
                    "summary_md is {} chars (limit {MAX_STRING})",
                    summary_md.len()
                ),
            ));
        }
    }
    for (ki, item) in plan.meta.key_points.iter().enumerate() {
        if item.len() > MAX_STRING {
            errs.push(Issue::new(
                format!("/meta/key_points/{ki}"),
                "string_too_long",
                format!(
                    "key_points item is {} chars (limit {MAX_STRING})",
                    item.len()
                ),
            ));
        }
    }
    for (oi, item) in plan.meta.out_of_scope.iter().enumerate() {
        if item.len() > MAX_STRING {
            errs.push(Issue::new(
                format!("/meta/out_of_scope/{oi}"),
                "string_too_long",
                format!(
                    "out_of_scope item is {} chars (limit {MAX_STRING})",
                    item.len()
                ),
            ));
        }
    }

    // Phase and task fields
    for (pi, phase) in plan.phases.iter().enumerate() {
        if phase.id.len() > MAX_STRING {
            errs.push(Issue::new(
                format!("/phases/{pi}/id"),
                "string_too_long",
                format!("id is {} chars (limit {MAX_STRING})", phase.id.len()),
            ));
        }
        if phase.title.len() > MAX_STRING {
            errs.push(Issue::new(
                format!("/phases/{pi}/title"),
                "string_too_long",
                format!("title is {} chars (limit {MAX_STRING})", phase.title.len()),
            ));
        }
        if let Some(summary_md) = &phase.summary_md {
            if summary_md.len() > MAX_STRING {
                errs.push(Issue::new(
                    format!("/phases/{pi}/summary_md"),
                    "string_too_long",
                    format!(
                        "summary_md is {} chars (limit {MAX_STRING})",
                        summary_md.len()
                    ),
                ));
            }
        }

        for (ti, task) in phase.tasks.iter().enumerate() {
            if task.id.len() > MAX_STRING {
                errs.push(Issue::new(
                    format!("/phases/{pi}/tasks/{ti}/id"),
                    "string_too_long",
                    format!("id is {} chars (limit {MAX_STRING})", task.id.len()),
                ));
            }
            if task.title.len() > MAX_STRING {
                errs.push(Issue::new(
                    format!("/phases/{pi}/tasks/{ti}/title"),
                    "string_too_long",
                    format!("title is {} chars (limit {MAX_STRING})", task.title.len()),
                ));
            }
            if let Some(summary_md) = &task.summary_md {
                if summary_md.len() > MAX_STRING {
                    errs.push(Issue::new(
                        format!("/phases/{pi}/tasks/{ti}/summary_md"),
                        "string_too_long",
                        format!(
                            "summary_md is {} chars (limit {MAX_STRING})",
                            summary_md.len()
                        ),
                    ));
                }
            }
            for (di, item) in task.depends_on.iter().enumerate() {
                if item.len() > MAX_STRING {
                    errs.push(Issue::new(
                        format!("/phases/{pi}/tasks/{ti}/depends_on/{di}"),
                        "string_too_long",
                        format!(
                            "depends_on item is {} chars (limit {MAX_STRING})",
                            item.len()
                        ),
                    ));
                }
            }
            for (ai, item) in task.acceptance.iter().enumerate() {
                if item.len() > MAX_STRING {
                    errs.push(Issue::new(
                        format!("/phases/{pi}/tasks/{ti}/acceptance/{ai}"),
                        "string_too_long",
                        format!(
                            "acceptance item is {} chars (limit {MAX_STRING})",
                            item.len()
                        ),
                    ));
                }
            }
            for (vi, item) in task.validation.iter().enumerate() {
                if item.len() > MAX_STRING {
                    errs.push(Issue::new(
                        format!("/phases/{pi}/tasks/{ti}/validation/{vi}"),
                        "string_too_long",
                        format!(
                            "validation item is {} chars (limit {MAX_STRING})",
                            item.len()
                        ),
                    ));
                }
            }
            for (fi, file) in task.files.iter().enumerate() {
                if file.path.len() > MAX_STRING {
                    errs.push(Issue::new(
                        format!("/phases/{pi}/tasks/{ti}/files/{fi}/path"),
                        "string_too_long",
                        format!("path is {} chars (limit {MAX_STRING})", file.path.len()),
                    ));
                }
                if let Some(note) = &file.note {
                    if note.len() > MAX_STRING {
                        errs.push(Issue::new(
                            format!("/phases/{pi}/tasks/{ti}/files/{fi}/note"),
                            "string_too_long",
                            format!("note is {} chars (limit {MAX_STRING})", note.len()),
                        ));
                    }
                }
            }
        }
    }

    // Risk fields
    for (ri, risk) in plan.risks.iter().enumerate() {
        if risk.id.len() > MAX_STRING {
            errs.push(Issue::new(
                format!("/risks/{ri}/id"),
                "string_too_long",
                format!("id is {} chars (limit {MAX_STRING})", risk.id.len()),
            ));
        }
        if risk.title.len() > MAX_STRING {
            errs.push(Issue::new(
                format!("/risks/{ri}/title"),
                "string_too_long",
                format!("title is {} chars (limit {MAX_STRING})", risk.title.len()),
            ));
        }
        if let Some(mitigation_md) = &risk.mitigation_md {
            if mitigation_md.len() > MAX_STRING {
                errs.push(Issue::new(
                    format!("/risks/{ri}/mitigation_md"),
                    "string_too_long",
                    format!(
                        "mitigation_md is {} chars (limit {MAX_STRING})",
                        mitigation_md.len()
                    ),
                ));
            }
        }
    }

    // Open question fields
    for (qi, question) in plan.open_questions.iter().enumerate() {
        if question.id.len() > MAX_STRING {
            errs.push(Issue::new(
                format!("/open_questions/{qi}/id"),
                "string_too_long",
                format!("id is {} chars (limit {MAX_STRING})", question.id.len()),
            ));
        }
        if question.question_md.len() > MAX_STRING {
            errs.push(Issue::new(
                format!("/open_questions/{qi}/question_md"),
                "string_too_long",
                format!(
                    "question_md is {} chars (limit {MAX_STRING})",
                    question.question_md.len()
                ),
            ));
        }
    }
}

/// Author-guidance advisories: never errors, never gate a render. `load plan
/// check` surfaces them as warnings so the plan author sees them in the
/// write→check loop, where they're actionable.
pub fn advisories(plan: &Plan) -> Vec<Issue> {
    let mut out = Vec::new();
    if let Some(s) = &plan.meta.summary_md {
        let n = s.chars().count();
        if n > 1500 {
            let mut issue = Issue::new(
                "/meta/summary_md",
                "long_summary",
                format!("executive summary is {n} chars — it reads best at 4-6 sentences"),
            );
            issue.hint = Some(
                "move workstream detail into key_points and global constraints \
                 into phase or task summaries"
                    .into(),
            );
            out.push(issue);
        }
        if n > 600 && !s.contains("\n\n") {
            let mut issue = Issue::new(
                "/meta/summary_md",
                "wall_of_text",
                format!("executive summary is {n} chars in a single paragraph"),
            );
            issue.hint = Some(
                "break it into 2-4 short paragraphs (blank lines between them) — \
                 one block of prose renders as a wall of text"
                    .into(),
            );
            out.push(issue);
        }
    }
    out
}

pub fn validate(plan: &Plan) -> Vec<Issue> {
    let mut errs = Vec::new();
    let mut seen = std::collections::BTreeMap::new(); // id -> first path

    let mut check_id = |id: &str, path: String, errs: &mut Vec<Issue>| {
        if !id_ok(id) {
            let mut e = Issue::new(path.clone(), "bad_id", format!("invalid id `{id}`"));
            e.hint = Some(ID_HINT.into());
            errs.push(e);
        }
        if let Some(first) = seen.get(id).cloned() {
            errs.push(Issue::new(
                path,
                "duplicate_id",
                format!("id `{id}` already used at {first}"),
            ));
        } else {
            seen.insert(id.to_string(), path);
        }
    };

    // Icons are validated against the closed vendored vocabulary (see
    // `plan::icons`), not the id-syntax/uniqueness rules `check_id` covers —
    // a separate closure so the hint (naming every valid icon) doesn't leak
    // into `check_id`'s.
    let check_icon = |icon: &str, path: String, errs: &mut Vec<Issue>| {
        if !crate::plan::icons::icon_names().contains(&icon) {
            let mut e = Issue::new(path, "unknown_icon", format!("unknown icon `{icon}`"));
            e.hint = Some(format!(
                "known icons: {}",
                crate::plan::icons::icon_names().join(", ")
            ));
            errs.push(e);
        }
    };

    check_id(&plan.meta.id, "/meta/id".into(), &mut errs);
    let mut task_ids = std::collections::BTreeSet::new();
    for (pi, phase) in plan.phases.iter().enumerate() {
        check_id(&phase.id, format!("/phases/{pi}/id"), &mut errs);
        if let Some(icon) = &phase.icon {
            check_icon(icon, format!("/phases/{pi}/icon"), &mut errs);
        }
        for (ti, t) in phase.tasks.iter().enumerate() {
            check_id(&t.id, format!("/phases/{pi}/tasks/{ti}/id"), &mut errs);
            if let Some(icon) = &t.icon {
                check_icon(icon, format!("/phases/{pi}/tasks/{ti}/icon"), &mut errs);
            }
            task_ids.insert(t.id.as_str());
        }
    }
    for (ri, r) in plan.risks.iter().enumerate() {
        check_id(&r.id, format!("/risks/{ri}/id"), &mut errs);
    }
    for (qi, q) in plan.open_questions.iter().enumerate() {
        check_id(&q.id, format!("/open_questions/{qi}/id"), &mut errs);
    }

    // depends_on refs over the task graph (also collects edges for the
    // cycle check below).
    let mut edges: Vec<(&str, &str)> = Vec::new();
    for (pi, phase) in plan.phases.iter().enumerate() {
        for (ti, t) in phase.tasks.iter().enumerate() {
            for (di, dep) in t.depends_on.iter().enumerate() {
                if task_ids.contains(dep.as_str()) {
                    edges.push((t.id.as_str(), dep.as_str()));
                } else {
                    let mut e = Issue::new(
                        format!("/phases/{pi}/tasks/{ti}/depends_on/{di}"),
                        "unknown_ref",
                        format!("depends_on `{dep}` matches no task id"),
                    );
                    e.hint = Some(format!(
                        "declared task ids: {}",
                        task_ids.iter().cloned().collect::<Vec<_>>().join(", ")
                    ));
                    errs.push(e);
                }
            }
        }
    }

    // Limits — checked before cycle detection. The recursive DFS below has
    // stack depth bounded by the task count, so it must not run on an
    // oversized plan; report `too_many` and skip the cycle check instead.
    let n_tasks: usize = plan.phases.iter().map(|p| p.tasks.len()).sum();
    for (what, n, max, path) in [
        ("tasks", n_tasks, MAX_TASKS, "/phases"),
        ("phases", plan.phases.len(), MAX_PHASES, "/phases"),
        ("risks", plan.risks.len(), MAX_RISKS, "/risks"),
        (
            "open_questions",
            plan.open_questions.len(),
            MAX_QUESTIONS,
            "/open_questions",
        ),
        ("dependency edges", edges.len(), MAX_EDGES, "/phases"),
        (
            "key_points",
            plan.meta.key_points.len(),
            MAX_KEY_POINTS,
            "/meta/key_points",
        ),
        (
            "out_of_scope",
            plan.meta.out_of_scope.len(),
            MAX_OUT_OF_SCOPE,
            "/meta/out_of_scope",
        ),
    ] {
        if n > max {
            errs.push(Issue::new(
                path,
                "too_many",
                format!("{n} {what} (limit {max})"),
            ));
        }
    }

    // Cycle detection recurses one stack frame per node on the DFS path, so
    // it's only safe once the task count is within MAX_TASKS (500 frames).
    if n_tasks <= MAX_TASKS {
        if let Some(cycle_node) = find_cycle(&task_ids, &edges) {
            errs.push(Issue::new(
                "/phases",
                "dependency_cycle",
                format!("dependency cycle involving `{cycle_node}`"),
            ));
        }
    }

    check_strings(plan, &mut errs);
    errs
}

/// DFS three-color cycle detection; returns a node on a cycle.
fn find_cycle<'a>(
    nodes: &std::collections::BTreeSet<&'a str>,
    edges: &[(&'a str, &'a str)],
) -> Option<&'a str> {
    let mut adj: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    for (a, b) in edges {
        adj.entry(a).or_default().push(b);
    }
    let mut state: std::collections::BTreeMap<&str, u8> = Default::default();
    fn dfs<'a>(
        n: &'a str,
        adj: &std::collections::BTreeMap<&'a str, Vec<&'a str>>,
        state: &mut std::collections::BTreeMap<&'a str, u8>,
    ) -> Option<&'a str> {
        match state.get(n) {
            Some(1) => return Some(n),
            Some(2) => return None,
            _ => {}
        }
        state.insert(n, 1);
        for m in adj.get(n).into_iter().flatten() {
            if let Some(c) = dfs(m, adj, state) {
                return Some(c);
            }
        }
        state.insert(n, 2);
        None
    }
    nodes.iter().find_map(|n| dfs(n, &adj, &mut state))
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

    #[test]
    fn dangling_ref_cycle_and_dup_are_caught() {
        for (fix, code, path_frag) in [
            ("invalid-dangling-ref.json", "unknown_ref", "/depends_on/0"),
            ("invalid-cycle.json", "dependency_cycle", "/phases"),
            ("invalid-dup-id.json", "duplicate_id", "/phases"),
        ] {
            let p = parse(&fixture(fix), false).unwrap().plan;
            let errs = validate(&p);
            assert!(
                errs.iter()
                    .any(|e| e.code == code && e.path.contains(path_frag)),
                "{fix}: {errs:?}"
            );
        }
    }

    #[test]
    fn bad_slug_is_rejected_with_hint() {
        let bad = fixture("minimal.json").replace("\"id\": \"t-a\"", "\"id\": \"T A!\"");
        let p = parse(&bad, false).unwrap().plan;
        let errs = validate(&p);
        let e = errs.iter().find(|e| e.code == "bad_id").unwrap();
        assert!(e.hint.as_deref().unwrap_or("").contains("^[a-z]"));
    }

    #[test]
    fn unknown_icon_is_rejected_with_full_vocabulary_hint() {
        let mut p = parse(&fixture("minimal.json"), false).unwrap().plan;
        p.phases[0].icon = Some("not-a-real-icon".into());
        p.phases[0].tasks[0].icon = Some("shield".into()); // a valid one alongside — should not also error
        let errs = validate(&p);
        let e = errs
            .iter()
            .find(|e| e.code == "unknown_icon" && e.path == "/phases/0/icon")
            .unwrap_or_else(|| panic!("expected unknown_icon at /phases/0/icon, got {errs:?}"));
        let hint = e.hint.as_deref().unwrap_or("");
        for name in crate::plan::icons::icon_names() {
            assert!(hint.contains(name), "hint should list `{name}`: {hint}");
        }
        assert!(
            !errs.iter().any(|e| e.path == "/phases/0/tasks/0/icon"),
            "valid task icon should not error: {errs:?}"
        );
    }

    #[test]
    fn known_icon_is_accepted() {
        let mut p = parse(&fixture("minimal.json"), false).unwrap().plan;
        p.phases[0].icon = Some("database".into());
        assert!(validate(&p).is_empty());
    }

    #[test]
    fn collection_and_string_limits_enforced() {
        let mut p = parse(&fixture("minimal.json"), false).unwrap().plan;
        p.phases[0].tasks[0].summary_md = Some("y".repeat(MAX_STRING + 1));
        assert!(validate(&p).iter().any(|e| e.code == "string_too_long"));
        for i in 0..501 {
            p.phases[0].tasks.push(PlanTask {
                id: format!("t-x{i}"),
                title: "x".into(),
                icon: None,
                summary_md: None,
                status: Status::Planned,
                risk: None,
                depends_on: vec![],
                files: vec![],
                acceptance: vec![],
                validation: vec![],
                estimate: None,
            });
        }
        assert!(validate(&p).iter().any(|e| e.code == "too_many"));
    }

    #[test]
    fn long_dependency_chain_reports_too_many_without_overflowing_stack() {
        // 35,000 tasks in a single dependency chain fits under the 2 MiB
        // input cap, but would overflow the stack if cycle detection's
        // recursive DFS ran before the MAX_TASKS collection-limit check.
        //
        // The shape here is deliberate, not incidental: ids are zero-padded
        // so BTreeSet iteration order (lexicographic) matches chain order
        // (numeric), and each task depends on the *next* task rather than
        // the previous one. That means the very first node `find_cycle`
        // visits (`t-x00000`, lexicographically smallest) has a forward
        // edge into `t-x00001`, which has a forward edge into `t-x00002`,
        // and so on to the end of the chain — so the first DFS call made by
        // `find_cycle` descends the *entire* chain in one recursion, giving
        // a descent depth of exactly `n` on ungated code. (An earlier
        // version of this test pointed `depends_on` at the previous task
        // with unpadded ids; lexicographic iteration over unpadded ids does
        // not match chain order, which only ever produced a ~900-frame
        // descent at n=10,000 — not enough to actually exercise the guard.)
        let mut p = parse(&fixture("minimal.json"), false).unwrap().plan;
        p.phases[0].tasks.clear();
        let n = 35_000;
        for i in 0..n {
            p.phases[0].tasks.push(PlanTask {
                id: format!("t-x{i:05}"),
                title: "x".into(),
                icon: None,
                summary_md: None,
                status: Status::Planned,
                risk: None,
                depends_on: if i == n - 1 {
                    vec![]
                } else {
                    vec![format!("t-x{:05}", i + 1)]
                },
                files: vec![],
                acceptance: vec![],
                validation: vec![],
                estimate: None,
            });
        }
        let errs = validate(&p);
        assert!(
            errs.iter().any(|e| e.code == "too_many"),
            "expected a too_many issue, got {errs:?}"
        );
    }

    #[test]
    fn kitchen_sink_is_fully_valid() {
        let p = parse(&fixture("kitchen-sink.json"), false).unwrap().plan;
        assert!(validate(&p).is_empty());
    }

    #[test]
    fn too_many_key_points_is_rejected() {
        let mut p = parse(&fixture("minimal.json"), false).unwrap().plan;
        p.meta.key_points = (0..26).map(|i| format!("point {i}")).collect();
        let errs = validate(&p);
        assert!(
            errs.iter()
                .any(|e| e.code == "too_many" && e.path == "/meta/key_points"),
            "{errs:?}"
        );
    }

    #[test]
    fn too_many_out_of_scope_is_rejected() {
        let mut p = parse(&fixture("minimal.json"), false).unwrap().plan;
        p.meta.out_of_scope = (0..26).map(|i| format!("out of scope {i}")).collect();
        let errs = validate(&p);
        assert!(
            errs.iter()
                .any(|e| e.code == "too_many" && e.path == "/meta/out_of_scope"),
            "{errs:?}"
        );
    }

    #[test]
    fn new_meta_string_caps_enforced() {
        let mut p = parse(&fixture("minimal.json"), false).unwrap().plan;
        p.meta.summary_md = Some("x".repeat(MAX_STRING + 1));
        p.meta.key_points = vec!["y".repeat(MAX_STRING + 1)];
        let errs = validate(&p);
        assert!(
            errs.iter()
                .any(|e| e.code == "string_too_long" && e.path == "/meta/summary_md"),
            "expected string_too_long at /meta/summary_md, got {errs:?}"
        );
        assert!(
            errs.iter()
                .any(|e| e.code == "string_too_long" && e.path == "/meta/key_points/0"),
            "expected string_too_long at /meta/key_points/0, got {errs:?}"
        );
    }
}
