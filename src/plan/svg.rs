//! Deterministic SVG layout for dependency graphs — a phase-level overview
//! graph and per-phase task subgraphs. Pure function of the model:
//! longest-path layering, id-ordered columns, fixed geometry.

use std::collections::{BTreeMap, BTreeSet};

use crate::plan::model::{Phase, Plan, RiskLevel, Status};

/// Fixed geometry + text budget for one graph's nodes. Task graphs and the
/// phase graph each get their own: the phase graph draws fewer, wider boxes
/// with longer titles, since phase titles matter more than the 28-char
/// task-title budget once each box represents a whole phase.
struct GraphStyle {
    node_w: i64,
    node_h: i64,
    gap: i64,
    /// `rx`/`ry` corner radius on the node `<rect>`.
    corner_radius: i64,
    /// Max chars kept from a title before appending `…`.
    title_max_chars: usize,
}

const TASK_STYLE: GraphStyle = GraphStyle {
    node_w: 200,
    node_h: 40,
    gap: 32,
    corner_radius: 6,
    title_max_chars: 28,
};

const PHASE_STYLE: GraphStyle = GraphStyle {
    node_w: 240,
    node_h: 44,
    gap: 32,
    corner_radius: 8,
    title_max_chars: 40,
};

struct Node {
    id: String,
    title: String,
    /// Classes appended after the base `node` class, space-separated (e.g.
    /// `"stub status-planned risk-medium"` or `"phase-node has-high"`).
    class: String,
    /// The anchor-target prefix: `#{href_prefix}-{id}`.
    href_prefix: &'static str,
}

/// Per-phase dependency subgraph. Nodes = the phase's tasks plus stub nodes
/// for cross-phase dependencies. None when the phase has no dependency edges.
pub fn phase_svg(plan: &Plan, phase_id: &str) -> Option<String> {
    let phase = plan.phases.iter().find(|p| p.id == phase_id)?;
    let local: BTreeSet<&str> = phase.tasks.iter().map(|t| t.id.as_str()).collect();
    let mut nodes: BTreeMap<String, Node> = Default::default();
    let mut edges: Vec<(String, String)> = Vec::new();
    for t in &phase.tasks {
        nodes.insert(t.id.clone(), task_node(plan, &t.id, false));
        for dep in &t.depends_on {
            edges.push((dep.clone(), t.id.clone()));
            if !local.contains(dep.as_str()) {
                nodes
                    .entry(dep.clone())
                    .or_insert_with(|| task_node(plan, dep, true));
            }
        }
    }
    if edges.is_empty() {
        return None;
    }
    Some(render_graph(&nodes, &edges, &TASK_STYLE))
}

/// Phase-level dependency graph: nodes are phases (full titles, no per-task
/// detail); an edge phase A → phase B means some task in B depends on a task
/// in A. Edges are deduped and self-edges (a phase depending on its own
/// task) are dropped. None when the plan has fewer than two phases, or none
/// of its dependencies cross a phase boundary — nothing to draw either way.
pub fn phase_graph_svg(plan: &Plan) -> Option<String> {
    if plan.phases.len() < 2 {
        return None;
    }
    let mut owner: BTreeMap<&str, &str> = BTreeMap::new();
    for phase in &plan.phases {
        for t in &phase.tasks {
            owner.insert(t.id.as_str(), phase.id.as_str());
        }
    }
    let mut edge_set: BTreeSet<(String, String)> = BTreeSet::new();
    for phase in &plan.phases {
        for t in &phase.tasks {
            for dep in &t.depends_on {
                if let Some(&dep_phase) = owner.get(dep.as_str()) {
                    if dep_phase != phase.id {
                        edge_set.insert((dep_phase.to_string(), phase.id.clone()));
                    }
                }
            }
        }
    }
    if edge_set.is_empty() {
        return None;
    }
    let nodes: BTreeMap<String, Node> = plan
        .phases
        .iter()
        .map(|p| (p.id.clone(), phase_node(p)))
        .collect();
    let edges: Vec<(String, String)> = edge_set.into_iter().collect();
    Some(render_graph(&nodes, &edges, &PHASE_STYLE))
}

/// Look up `id` across all phases and build its task node. `stub` marks a
/// cross-phase dependency rendered inside a phase subgraph that doesn't own
/// the task — the node still carries the task's real status/risk when found.
fn task_node(plan: &Plan, id: &str, stub: bool) -> Node {
    let task = plan
        .phases
        .iter()
        .flat_map(|p| p.tasks.iter())
        .find(|t| t.id == id);
    let (title, status, risk) = match task {
        Some(t) => (
            t.title.clone(),
            Some(t.status.clone()),
            risk_str(t.risk.as_ref()),
        ),
        None => (id.to_string(), None, risk_str(None)),
    };
    let mut parts: Vec<String> = Vec::new();
    if stub {
        parts.push("stub".to_string());
    }
    parts.push(format!("status-{}", status_str(status.as_ref())));
    if !risk.is_empty() {
        parts.push(format!("risk-{risk}"));
    }
    Node {
        id: id.to_string(),
        title,
        class: parts.join(" "),
        href_prefix: "task",
    }
}

/// A phase's node for the phase-level graph: `has-high` marks a phase that
/// contains at least one high-risk task, so the overview surfaces heat
/// without drawing every task.
fn phase_node(phase: &Phase) -> Node {
    let has_high = phase
        .tasks
        .iter()
        .any(|t| matches!(t.risk, Some(RiskLevel::High)));
    let mut class = String::from("phase-node");
    if has_high {
        class.push_str(" has-high");
    }
    Node {
        id: phase.id.clone(),
        title: phase.title.clone(),
        class,
        href_prefix: "phase",
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
fn render_graph(
    nodes: &BTreeMap<String, Node>,
    edges: &[(String, String)],
    style: &GraphStyle,
) -> String {
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
            let x = col * (style.node_w + style.gap);
            let y = row as i64 * (style.node_h + style.gap);
            pos.insert(id, (x, y));
        }
    }

    let max_col = by_column.keys().next_back().copied().unwrap_or(0);
    let max_rows = by_column.values().map(|v| v.len()).max().unwrap_or(1);
    let width = (max_col + 1) * (style.node_w + style.gap) - style.gap;
    let height = max_rows as i64 * (style.node_h + style.gap) - style.gap;

    let svg_w = width.max(style.node_w);
    let svg_h = height.max(style.node_h);
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
        out.push_str(&edge_path(pos[dep.as_str()], pos[task.as_str()], style));
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
        if !node.class.is_empty() {
            class.push(' ');
            class.push_str(&node.class);
        }
        out.push_str(&format!(
            "<a href=\"#{}-{}\">",
            node.href_prefix,
            esc(&node.id)
        ));
        out.push_str(&format!(
            "<rect class=\"{}\" x=\"{x}\" y=\"{y}\" width=\"{}\" height=\"{}\" rx=\"{}\"/>",
            esc(&class),
            style.node_w,
            style.node_h,
            style.corner_radius
        ));
        let tx = x + style.node_w / 2;
        let ty = y + style.node_h / 2;
        out.push_str(&format!(
            "<text x=\"{tx}\" y=\"{ty}\">{}</text>",
            esc(&truncate_title(&node.title, style.title_max_chars))
        ));
        out.push_str("</a>");
    }

    out.push_str("</svg>");
    out
}

/// One orthogonal elbow edge from `from`'s right edge to `to`'s left edge:
/// horizontal out of the source, vertical across, horizontal into the
/// target. Replaces a straight diagonal `<line>` so edges never visually
/// cross a node they don't touch as readily as a direct line would.
fn edge_path(from: (i64, i64), to: (i64, i64), style: &GraphStyle) -> String {
    let (x1, y1) = from;
    let (x2, y2) = to;
    let sx = x1 + style.node_w;
    let sy = y1 + style.node_h / 2;
    let tx = x2;
    let ty = y2 + style.node_h / 2;
    let midx = (sx + tx) / 2;
    format!(
        "<path class=\"edge\" d=\"M {sx} {sy} H {midx} V {ty} H {tx}\" marker-end=\"url(#arrow)\"/>",
    )
}

/// Truncate `s` to at most `max_chars` characters, appending `…` when
/// truncated. Counted in chars (not bytes) so multibyte titles never split
/// mid-codepoint.
fn truncate_title(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut truncated: String = s.chars().take(max_chars).collect();
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

    #[test]
    fn phase_graph_links_phases_with_one_cross_phase_edge() {
        let p = kitchen();
        let svg = phase_graph_svg(&p).unwrap();
        let a = phase_graph_svg(&p).unwrap();
        assert_eq!(svg, a, "phase_graph_svg must be deterministic");
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("href=\"#phase-p-backend\""));
        assert!(svg.contains("href=\"#phase-p-core\""));
        // t-redis (p-backend) depends_on t-session-store (p-core): exactly
        // one collapsed cross-phase edge, so exactly one edge path.
        assert_eq!(svg.matches("<path class=\"edge\"").count(), 1);
        assert!(svg.contains("class=\"node phase-node\""));
    }

    #[test]
    fn phase_graph_none_for_single_phase_plan() {
        let raw = std::fs::read_to_string(format!(
            "{}/tests/fixtures/plan/minimal.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let p = parse(&raw, false).unwrap().plan;
        assert!(phase_graph_svg(&p).is_none());
    }

    #[test]
    fn phase_graph_hostile_title_is_escaped() {
        let mut p = kitchen();
        p.phases[0].title = "<script>alert(1)</script>".into();
        let svg = phase_graph_svg(&p).unwrap();
        assert!(!svg.contains("<script>alert"));
        assert!(svg.contains("&lt;script&gt;"));
    }
}
