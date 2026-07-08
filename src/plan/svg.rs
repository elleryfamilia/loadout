//! Deterministic SVG layout for the task dependency graph. Pure function of
//! the model: longest-path layering, id-ordered columns, fixed geometry.

use std::collections::{BTreeMap, BTreeSet};

use crate::plan::model::{Plan, RiskLevel, Status};

pub const WHOLE_PLAN_MAX_NODES: usize = 30;
const NODE_W: i64 = 160;
const NODE_H: i64 = 36;
const GAP: i64 = 24;
/// Max chars kept from a title before appending `…`.
const TITLE_MAX_CHARS: usize = 22;

struct Node {
    id: String,
    title: String,
    status: Option<Status>,
    risk: String,
    stub: bool,
}

/// Per-phase dependency subgraph. Nodes = the phase's tasks plus stub nodes
/// for cross-phase dependencies. None when the phase has no dependency edges.
pub fn phase_svg(plan: &Plan, phase_id: &str) -> Option<String> {
    let phase = plan.phases.iter().find(|p| p.id == phase_id)?;
    let local: BTreeSet<&str> = phase.tasks.iter().map(|t| t.id.as_str()).collect();
    let mut nodes: BTreeMap<String, Node> = Default::default();
    let mut edges: Vec<(String, String)> = Vec::new();
    for t in &phase.tasks {
        nodes.insert(t.id.clone(), node_for(plan, &t.id, false));
        for dep in &t.depends_on {
            edges.push((dep.clone(), t.id.clone()));
            if !local.contains(dep.as_str()) {
                nodes
                    .entry(dep.clone())
                    .or_insert_with(|| node_for(plan, dep, true));
            }
        }
    }
    if edges.is_empty() {
        return None;
    }
    Some(render_graph(&nodes, &edges))
}

/// Whole-plan graph; None when the plan exceeds WHOLE_PLAN_MAX_NODES tasks.
pub fn whole_plan_svg(plan: &Plan) -> Option<String> {
    let n: usize = plan.phases.iter().map(|p| p.tasks.len()).sum();
    if n == 0 || n > WHOLE_PLAN_MAX_NODES {
        return None;
    }
    let mut nodes: BTreeMap<String, Node> = Default::default();
    let mut edges = Vec::new();
    for phase in &plan.phases {
        for t in &phase.tasks {
            nodes.insert(t.id.clone(), node_for(plan, &t.id, false));
            for dep in &t.depends_on {
                edges.push((dep.clone(), t.id.clone()));
            }
        }
    }
    if edges.is_empty() {
        return None;
    }
    Some(render_graph(&nodes, &edges))
}

/// Look up `id` across all phases and build its node. `stub` marks a
/// cross-phase dependency rendered inside a phase subgraph that doesn't own
/// the task — the node still carries the task's real status/risk when found.
fn node_for(plan: &Plan, id: &str, stub: bool) -> Node {
    let task = plan
        .phases
        .iter()
        .flat_map(|p| p.tasks.iter())
        .find(|t| t.id == id);
    match task {
        Some(t) => Node {
            id: t.id.clone(),
            title: t.title.clone(),
            status: Some(t.status.clone()),
            risk: risk_str(t.risk.as_ref()),
            stub,
        },
        None => Node {
            id: id.to_string(),
            title: id.to_string(),
            status: None,
            risk: risk_str(None),
            stub,
        },
    }
}

fn status_str(status: Option<&Status>) -> &'static str {
    match status {
        Some(Status::Planned) => "planned",
        Some(Status::InProgress) => "in_progress",
        Some(Status::Done) => "done",
        Some(Status::Blocked) => "blocked",
        Some(Status::Cut) => "cut",
        None => "planned",
    }
}

fn risk_str(risk: Option<&RiskLevel>) -> String {
    match risk {
        Some(RiskLevel::Low) => "low".into(),
        Some(RiskLevel::Medium) => "medium".into(),
        Some(RiskLevel::High) => "high".into(),
        None => String::new(),
    }
}

/// Longest-path column (depth) of every node in `nodes`, computed over the
/// subset of `edges` whose endpoints are both present. `dep` (edge.0) sits
/// strictly left of `task` (edge.1): depth(task) = 1 + max(depth(dep)), or 0
/// when a node has no incoming edge within this node set.
fn columns(nodes: &BTreeMap<String, Node>, edges: &[(String, String)]) -> BTreeMap<String, i64> {
    let mut incoming: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (dep, task) in edges {
        if nodes.contains_key(dep) && nodes.contains_key(task) {
            incoming.entry(task.as_str()).or_default().push(dep);
        }
    }
    let mut depth: BTreeMap<String, i64> = BTreeMap::new();
    fn resolve<'a>(
        id: &'a str,
        incoming: &BTreeMap<&'a str, Vec<&'a str>>,
        depth: &mut BTreeMap<String, i64>,
        visiting: &mut BTreeSet<String>,
    ) -> i64 {
        if let Some(d) = depth.get(id) {
            return *d;
        }
        // Guard against a cycle slipping through (shouldn't happen for a
        // validated plan, but stubs come from arbitrary dep ids): treat a
        // node already on the current path as depth 0 rather than
        // recursing forever.
        if !visiting.insert(id.to_string()) {
            return 0;
        }
        let d = match incoming.get(id) {
            Some(deps) if !deps.is_empty() => {
                1 + deps
                    .iter()
                    .map(|dep| resolve(dep, incoming, depth, visiting))
                    .max()
                    .unwrap_or(0)
            }
            _ => 0,
        };
        visiting.remove(id);
        depth.insert(id.to_string(), d);
        d
    }
    let mut visiting = BTreeSet::new();
    for id in nodes.keys() {
        resolve(id, &incoming, &mut depth, &mut visiting);
    }
    depth
}

/// Render `nodes`/`edges` as a self-contained `<svg>`. Column = longest-path
/// depth from roots; rows within a column ordered by id. Everything is
/// emitted ordered by (column, id) so output is byte-identical for the same
/// input.
fn render_graph(nodes: &BTreeMap<String, Node>, edges: &[(String, String)]) -> String {
    let depth = columns(nodes, edges);

    // Group ids by column, id-ordered within each column (BTreeMap keys are
    // already sorted lexicographically).
    let mut by_column: BTreeMap<i64, Vec<&str>> = BTreeMap::new();
    for (id, node) in nodes {
        let col = *depth.get(id).unwrap_or(&0);
        by_column.entry(col).or_default().push(node.id.as_str());
    }

    // Position: x by column, y by row-within-column (both already sorted).
    let mut pos: BTreeMap<&str, (i64, i64)> = BTreeMap::new();
    for (col, ids) in &by_column {
        for (row, id) in ids.iter().enumerate() {
            let x = col * (NODE_W + GAP);
            let y = row as i64 * (NODE_H + GAP);
            pos.insert(id, (x, y));
        }
    }

    let max_col = by_column.keys().next_back().copied().unwrap_or(0);
    let max_rows = by_column.values().map(|v| v.len()).max().unwrap_or(1);
    let width = (max_col + 1) * (NODE_W + GAP) - GAP;
    let height = max_rows as i64 * (NODE_H + GAP) - GAP;

    let svg_w = width.max(NODE_W);
    let svg_h = height.max(NODE_H);
    let mut out = String::new();
    out.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {svg_w} {svg_h}\" \
         width=\"{svg_w}\" height=\"{svg_h}\" class=\"plan-graph\">",
    ));
    out.push_str(
        "<defs><marker id=\"arrow\" viewBox=\"0 0 10 10\" refX=\"9\" refY=\"5\" \
         markerWidth=\"6\" markerHeight=\"6\" orient=\"auto-start-reverse\">\
         <path d=\"M 0 0 L 10 5 L 0 10 z\"/></marker></defs>",
    );

    // Edges ordered by (column of source, id of source, id of target) so
    // that iterating `edges` directly (already insertion order from the
    // caller, which is task/dep order) is not relied upon — sort explicitly.
    let mut sorted_edges: Vec<&(String, String)> = edges
        .iter()
        .filter(|(dep, task)| nodes.contains_key(dep) && nodes.contains_key(task))
        .collect();
    sorted_edges.sort();
    sorted_edges.dedup();
    for (dep, task) in &sorted_edges {
        let (x1, y1) = pos[dep.as_str()];
        let (x2, y2) = pos[task.as_str()];
        let sx = x1 + NODE_W;
        let sy = y1 + NODE_H / 2;
        let tx = x2;
        let ty = y2 + NODE_H / 2;
        out.push_str(&format!(
            "<line class=\"edge\" x1=\"{sx}\" y1=\"{sy}\" x2=\"{tx}\" y2=\"{ty}\" marker-end=\"url(#arrow)\"/>",
        ));
    }

    // Nodes ordered by (column, id).
    let mut ordered_ids: Vec<&str> = Vec::new();
    for ids in by_column.values() {
        ordered_ids.extend(ids.iter());
    }
    for id in ordered_ids {
        let node = &nodes[id];
        let (x, y) = pos[id];
        let mut class = String::from("node");
        if node.stub {
            class.push_str(" stub");
        }
        class.push_str(" status-");
        class.push_str(status_str(node.status.as_ref()));
        if !node.risk.is_empty() {
            class.push_str(" risk-");
            class.push_str(&node.risk);
        }
        out.push_str(&format!("<a href=\"#task-{}\">", esc(&node.id)));
        out.push_str(&format!(
            "<rect class=\"{}\" x=\"{x}\" y=\"{y}\" width=\"{NODE_W}\" height=\"{NODE_H}\"/>",
            esc(&class)
        ));
        let tx = x + NODE_W / 2;
        let ty = y + NODE_H / 2;
        out.push_str(&format!(
            "<text x=\"{tx}\" y=\"{ty}\">{}</text>",
            esc(&truncate_title(&node.title))
        ));
        out.push_str("</a>");
    }

    out.push_str("</svg>");
    out
}

/// Truncate `s` to at most `TITLE_MAX_CHARS` characters, appending `…` when
/// truncated. Counted in chars (not bytes) so multibyte titles never split
/// mid-codepoint.
fn truncate_title(s: &str) -> String {
    if s.chars().count() <= TITLE_MAX_CHARS {
        return s.to_string();
    }
    let mut truncated: String = s.chars().take(TITLE_MAX_CHARS).collect();
    truncated.push('…');
    truncated
}

/// Escape the five XML-special characters. `&` first so entity references
/// introduced by the other replacements aren't themselves escaped.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::model::parse;

    fn kitchen() -> crate::plan::model::Plan {
        let raw = std::fs::read_to_string(format!(
            "{}/tests/fixtures/plan/kitchen-sink.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        parse(&raw, false).unwrap().plan
    }

    #[test]
    fn phase_svg_is_deterministic_and_links_tasks() {
        let p = kitchen();
        let a = phase_svg(&p, "p-core").unwrap();
        let b = phase_svg(&p, "p-core").unwrap();
        assert_eq!(a, b);
        assert!(a.contains("href=\"#task-t-session-store\""));
        assert!(a.starts_with("<svg"));
    }

    #[test]
    fn cross_phase_dependency_appears_as_stub() {
        let p = kitchen();
        let backend = phase_svg(&p, "p-backend").unwrap();
        // t-redis depends on t-session-store, which lives in p-core.
        assert!(backend.contains("class=\"node stub"), "{backend}");
        assert!(backend.contains("t-session-store"));
    }

    #[test]
    fn phase_without_edges_renders_nothing() {
        let raw = std::fs::read_to_string(format!(
            "{}/tests/fixtures/plan/minimal.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let p = parse(&raw, false).unwrap().plan;
        assert!(phase_svg(&p, "p-one").is_none());
    }

    #[test]
    fn whole_plan_respects_threshold() {
        let p = kitchen();
        assert!(whole_plan_svg(&p).is_some()); // 5 tasks < 30
    }

    #[test]
    fn hostile_titles_are_escaped() {
        let mut p = kitchen();
        p.phases[0].tasks[1].title = "<script>alert(1)</script>".into();
        let svg = phase_svg(&p, "p-core").unwrap();
        assert!(!svg.contains("<script>alert"));
    }

    #[test]
    fn golden_phase_svg() {
        let expected = std::fs::read_to_string(format!(
            "{}/tests/fixtures/plan/kitchen-sink-p-core.svg",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let got = phase_svg(&kitchen(), "p-core").unwrap();
        if std::env::var("UPDATE_GOLDEN").is_ok() {
            std::fs::write(
                format!(
                    "{}/tests/fixtures/plan/kitchen-sink-p-core.svg",
                    env!("CARGO_MANIFEST_DIR")
                ),
                &got,
            )
            .unwrap();
        } else {
            assert_eq!(
                got, expected,
                "golden drift — rerun with UPDATE_GOLDEN=1 if intended"
            );
        }
    }
}
