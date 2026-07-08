//! Deterministic maud renderer: model → single self-contained HTML document.
//!
//! `render` never touches the filesystem, the clock, or any HashMap
//! iteration order — same `Plan` in, byte-identical HTML out. The document
//! embeds its own styles/script (no CDN, no external fetches) and starts
//! with the bare `<!-- loadout:generated context=… -->` marker line (never
//! the full multi-line header, which carries a timestamp).

use maud::{html, Markup, PreEscaped, DOCTYPE};

use crate::plan::model::{
    plan_hash, Estimate, FileAction, OpenQuestion, Phase, Plan, PlanTask, RiskLevel, Status,
};
use crate::plan::svg;

const CSS: &str = include_str!("assets/plan.css");
const JS: &str = include_str!("assets/plan.js");
const CSP: &str =
    "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src data:";

/// Escape the canonical JSON for a `<script type="application/json">` island:
/// `<`, `>`, `&`, U+2028, U+2029 become JSON unicode escapes, so the island
/// can never contain `</script>` or `<!--` yet parses back identically
/// (`\uXXXX` is a normal JSON string escape — `JSON.parse`/`serde_json`
/// decode it to the original character).
pub(crate) fn escape_json_island(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        match c {
            '<' | '>' | '&' | '\u{2028}' | '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            _ => out.push(c),
        }
    }
    out
}

/// Render an optional markdown field via the shared sanitizer; empty markup
/// for `None` rather than an empty paragraph.
fn md(text: &Option<String>) -> Markup {
    match text {
        Some(t) => PreEscaped(crate::markdown::render_markdown(t)),
        None => PreEscaped(String::new()),
    }
}

/// A copy of `plan` with every markdown field (`goal_md`, `summary_md`,
/// `mitigation_md`, `question_md`) replaced by its sanitized HTML rendering,
/// for the JSON data island.
///
/// `escape_json_island`'s character escaping (below) already makes the
/// island inert as HTML/script content — `</script>` and `<!--` can't
/// appear, full stop. But raw markdown source can still carry things that
/// are inert in that context yet meaningless or misleading if read out of
/// context (a `[text](javascript:…)` link's scheme, verbatim `<img
/// onerror=…>` text): nothing client-side reads these fields today (`plan.js`
/// only ever reads `plan.meta.id`), so there's no fidelity cost to carrying
/// the same sanitized form already used for the visible document body.
///
/// The island embeds a DISPLAY-SANITIZED copy of the plan: markdown fields are
/// pre-rendered through the sanitizer so the artifact never contains raw
/// javascript:/HTML payloads anywhere, even inertly. The island is therefore
/// NOT the canonical model and is not what `data-plan-fingerprint` hashes
/// (that covers the original plan.json model); consumers needing the canonical
/// plan read plan.json from disk.
fn sanitized_for_island(plan: &Plan) -> Plan {
    let mut p = plan.clone();
    p.meta.goal_md = p
        .meta
        .goal_md
        .as_deref()
        .map(crate::markdown::render_markdown);
    for phase in &mut p.phases {
        phase.summary_md = phase
            .summary_md
            .as_deref()
            .map(crate::markdown::render_markdown);
        for task in &mut phase.tasks {
            task.summary_md = task
                .summary_md
                .as_deref()
                .map(crate::markdown::render_markdown);
        }
    }
    for r in &mut p.risks {
        r.mitigation_md = r
            .mitigation_md
            .as_deref()
            .map(crate::markdown::render_markdown);
    }
    for q in &mut p.open_questions {
        q.question_md = crate::markdown::render_markdown(&q.question_md);
    }
    p
}

fn status_str(status: &Status) -> &'static str {
    match status {
        Status::Planned => "planned",
        Status::InProgress => "in_progress",
        Status::Done => "done",
        Status::Blocked => "blocked",
        Status::Cut => "cut",
    }
}

fn risk_str(risk: &RiskLevel) -> &'static str {
    match risk {
        RiskLevel::Low => "low",
        RiskLevel::Medium => "medium",
        RiskLevel::High => "high",
    }
}

fn estimate_str(estimate: &Estimate) -> &'static str {
    match estimate {
        Estimate::S => "s",
        Estimate::M => "m",
        Estimate::L => "l",
    }
}

fn file_action_str(action: &FileAction) -> &'static str {
    match action {
        FileAction::Create => "create",
        FileAction::Modify => "modify",
        FileAction::Delete => "delete",
        FileAction::Test => "test",
    }
}

/// `"{n} {word}"`, pluralized with a trailing `s` above one — used for the
/// counting nouns in the summary strip (tasks, phases, risks).
fn count_label(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// The first `max` characters of `s`, followed by `…` if anything was cut.
/// Truncates on a `char` boundary (never splits a multi-byte codepoint), so
/// the result is always valid UTF-8 to hand to maud for escaping. This
/// truncates the *raw* markdown source (backticks, `*`, etc. can show up
/// literally) rather than parsing it — the simplest option that stays
/// correct, since the caller only needs a short plain-text preview, not a
/// faithful rendering.
fn truncate_chars(s: &str, max: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// The summary strip's headline: task/phase counts, then an estimate
/// distribution (S/M/L, only sizes that occur) when any task carries an
/// estimate, then a status distribution when more than one distinct status
/// appears (a single-status plan doesn't need it spelled out).
///
/// The status order below is deliberately not the `Status` enum's
/// declaration order — it reads like a progress readout: what's finished
/// first, then what's active, then what's stuck, what's left, and finally
/// what was abandoned.
fn summary_counts_line(plan: &Plan) -> String {
    let tasks: Vec<&PlanTask> = plan.phases.iter().flat_map(|p| p.tasks.iter()).collect();
    let mut parts = vec![
        count_label(tasks.len(), "task"),
        count_label(plan.phases.len(), "phase"),
    ];

    let mut sizes = [0usize; 3]; // s, m, l
    for t in &tasks {
        match t.estimate {
            Some(Estimate::S) => sizes[0] += 1,
            Some(Estimate::M) => sizes[1] += 1,
            Some(Estimate::L) => sizes[2] += 1,
            None => {}
        }
    }
    for (n, label) in sizes.iter().zip(["S", "M", "L"]) {
        if *n > 0 {
            parts.push(format!("{n}×{label}"));
        }
    }

    let mut statuses = [0usize; 5]; // done, in_progress, blocked, planned, cut
    for t in &tasks {
        let i = match t.status {
            Status::Done => 0,
            Status::InProgress => 1,
            Status::Blocked => 2,
            Status::Planned => 3,
            Status::Cut => 4,
        };
        statuses[i] += 1;
    }
    if statuses.iter().filter(|&&n| n > 0).count() > 1 {
        let labels = ["done", "in_progress", "blocked", "planned", "cut"];
        for (n, label) in statuses.iter().zip(labels) {
            if *n > 0 {
                parts.push(format!("{n} {label}"));
            }
        }
    }

    parts.join(" · ")
}

/// The risk line's text plus whether any risk is high severity (the caller
/// tints the line when it is). `None` when the plan has no risks at all —
/// the caller omits the line entirely rather than showing "0 risks".
fn summary_risk_line(plan: &Plan) -> Option<(String, bool)> {
    if plan.risks.is_empty() {
        return None;
    }
    let mut severities = [0usize; 3]; // high, medium, low
    for r in &plan.risks {
        match r.severity {
            RiskLevel::High => severities[0] += 1,
            RiskLevel::Medium => severities[1] += 1,
            RiskLevel::Low => severities[2] += 1,
        }
    }
    let parts: Vec<String> = severities
        .iter()
        .zip(["high", "medium", "low"])
        .filter(|(n, _)| **n > 0)
        .map(|(n, label)| format!("{n} {label}"))
        .collect();
    let line = format!(
        "{} ({})",
        count_label(plan.risks.len(), "risk"),
        parts.join(", ")
    );
    Some((line, severities[0] > 0))
}

/// A collapsed phase's `summary` still needs to convey its size and heat:
/// "(N tasks)", or "(N tasks · high risk)" when any task in it carries a
/// high risk rating.
fn phase_meta_text(phase: &Phase) -> String {
    let n = phase.tasks.len();
    let has_high = phase
        .tasks
        .iter()
        .any(|t| matches!(t.risk, Some(RiskLevel::High)));
    format!(
        "({}{})",
        count_label(n, "task"),
        if has_high { " · high risk" } else { "" }
    )
}

/// One task card: heading with status/risk/estimate badges, the markdown
/// summary, a file touch list, an acceptance checklist, validation commands,
/// and a "depends on" line linking to the other cards' anchors.
///
/// `id="task-{id}"` is what the SVG's `#task-{id}` links jump to.
/// `data-plan-ref="task:{id}"` is what the comment tooling anchors a comment
/// to; acceptance sub-items carry the *same* parent ref (the design has no
/// per-criterion anchor, so a comment on a criterion attaches to its task).
fn task_card(task: &PlanTask) -> Markup {
    let task_ref = format!("task:{}", task.id);
    html! {
        div.task id=(format!("task-{}", task.id)) data-plan-ref=(task_ref) {
            h3 {
                (task.title)
                " "
                span class=(format!("badge status-{}", status_str(&task.status))) {
                    (status_str(&task.status))
                }
                @if let Some(risk) = &task.risk {
                    " "
                    span class=(format!("badge risk-{}", risk_str(risk))) { (risk_str(risk)) }
                }
                @if let Some(estimate) = &task.estimate {
                    " "
                    span class=(format!("badge estimate-{}", estimate_str(estimate))) {
                        (estimate_str(estimate))
                    }
                }
            }
            (md(&task.summary_md))
            @if !task.files.is_empty() {
                ul.files {
                    @for f in &task.files {
                        li {
                            code { (f.path) }
                            " "
                            span class=(format!("badge action-{}", file_action_str(&f.action))) {
                                (file_action_str(&f.action))
                            }
                            @if let Some(note) = &f.note { " — " (note) }
                        }
                    }
                }
            }
            @if !task.acceptance.is_empty() {
                ul.acceptance {
                    @for item in &task.acceptance {
                        li data-plan-ref=(format!("task:{}", task.id)) { (item) }
                    }
                }
            }
            @if !task.validation.is_empty() {
                ul.validation {
                    @for cmd in &task.validation {
                        li { code { (cmd) } }
                    }
                }
            }
            @if !task.depends_on.is_empty() {
                p.depends {
                    "depends on "
                    @for (i, dep) in task.depends_on.iter().enumerate() {
                        @if i > 0 { ", " }
                        a href=(format!("#task-{dep}")) { (dep) }
                    }
                }
            }
        }
    }
}

pub fn render(plan: &Plan) -> String {
    let hash = plan_hash(plan);
    let island = escape_json_island(
        &serde_json::to_string(&sanitized_for_island(plan)).expect("plan serializes"),
    );
    // Computed once: the whole-plan graph doubles as "should each phase draw
    // its own subgraph too" — a phase graph would be redundant once the
    // whole plan already fits in one picture.
    let overview = svg::whole_plan_svg(plan);
    let risk_line = summary_risk_line(plan);
    let blocking: Vec<&OpenQuestion> = plan.open_questions.iter().filter(|q| q.blocking).collect();
    let page = html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta http-equiv="Content-Security-Policy" content=(CSP);
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (plan.meta.title) " — loadout plan" }
                style { (PreEscaped(CSS)) }
            }
            body data-plan-fingerprint=(hash) {
                header {
                    h1 { (plan.meta.title) }
                    p.meta data-plan-ref=(format!("meta:{}", plan.meta.id)) {
                        "plan " code { (plan.meta.id) }
                        @if let Some(rev) = plan.meta.revision { " · revision " (rev) }
                        @if let Some(agent) = &plan.meta.agent { " · by " (agent) }
                    }
                    (md(&plan.meta.goal_md))
                }
                section.plan-summary {
                    p.summary-counts { (summary_counts_line(plan)) }
                    @if let Some((line, has_high)) = &risk_line {
                        p class=(if *has_high { "summary-risks has-high" } else { "summary-risks" }) {
                            (line)
                        }
                    }
                    @if !blocking.is_empty() {
                        div.summary-blocking {
                            p.blocking-warn {
                                (format!("⚠ {} blocking question(s)", blocking.len()))
                            }
                            ul.blocking-list {
                                @for q in &blocking {
                                    li {
                                        a href=(format!("#question-{}", q.id)) {
                                            (truncate_chars(&q.question_md, 100))
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                @if !plan.open_questions.is_empty() {
                    section.questions {
                        h2 { "Open questions" }
                        @for q in &plan.open_questions {
                            div.task id=(format!("question-{}", q.id)) data-plan-ref=(format!("question:{}", q.id)) {
                                @if q.blocking { span.badge.blocking { "blocking" } }
                                (PreEscaped(crate::markdown::render_markdown(&q.question_md)))
                            }
                        }
                    }
                }
                @if !plan.risks.is_empty() {
                    section.risks {
                        h2 { "Risks" }
                        @for r in &plan.risks {
                            div.task data-plan-ref=(format!("risk:{}", r.id)) {
                                h3 id=(format!("risk-{}", r.id)) {
                                    (r.title)
                                    " "
                                    span class=(format!("badge risk-{}", risk_str(&r.severity))) {
                                        (risk_str(&r.severity))
                                    }
                                }
                                (md(&r.mitigation_md))
                            }
                        }
                    }
                }
                @for phase in &plan.phases {
                    details.phase data-plan-ref=(format!("phase:{}", phase.id)) {
                        summary {
                            h2 {
                                (phase.title)
                                " "
                                span.phase-meta { (phase_meta_text(phase)) }
                            }
                        }
                        (md(&phase.summary_md))
                        @if overview.is_none() {
                            @if let Some(g) = svg::phase_svg(plan, &phase.id) { (PreEscaped(g)) }
                        }
                        @for task in &phase.tasks { (task_card(task)) }
                    }
                }
                @if let Some(overview_svg) = &overview {
                    details.graph {
                        summary { "Dependency graph" }
                        (PreEscaped(overview_svg.as_str()))
                    }
                }
                script type="application/json" id="plan-data" { (PreEscaped(island)) }
                script { (PreEscaped(JS)) }
            }
        }
    };
    format!(
        "{} context={hash} -->\n{}",
        crate::render::header::GENERATED_MARKER,
        page.into_string()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::model::parse;

    fn plan_from(name: &str) -> crate::plan::model::Plan {
        let raw = std::fs::read_to_string(format!(
            "{}/tests/fixtures/plan/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        parse(&raw, false).unwrap().plan
    }

    #[test]
    fn island_escaping_neutralizes_terminators() {
        let out = escape_json_island("a</script><!--b\u{2028}c");
        assert!(!out.contains("</script>"));
        assert!(!out.contains("<!--"));
        assert!(!out.contains('\u{2028}'));
        let round: String = serde_json::from_str(&format!("\"{out}\"")).unwrap();
        assert_eq!(round, "a</script><!--b\u{2028}c");
    }

    #[test]
    fn hostile_plan_renders_inert() {
        let html = render(&plan_from("hostile.json"));
        assert!(!html.contains("<script>alert"));
        assert!(!html.contains("javascript:"));
        assert!(!html.contains("<img"));
        assert!(!html.contains("evil.example/p.png\" ")); // no fetching attr context
                                                          // Island still parses as valid JSON with the same ids (markdown fields are display-sanitized, so it is not byte-identical to the input model).
        let island = html
            .split("id=\"plan-data\">")
            .nth(1)
            .unwrap()
            .split("</script>")
            .next()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(island).unwrap();
        assert_eq!(v["meta"]["id"], "auth-refactor");
    }

    #[test]
    fn document_structure() {
        let plan = plan_from("kitchen-sink.json");
        let html = render(&plan);
        let hash = crate::plan::model::plan_hash(&plan);
        assert!(html.starts_with(&format!("<!-- loadout:generated context={hash} -->")));
        assert!(html.contains("Content-Security-Policy"));
        assert!(html.contains("data-plan-ref=\"task:t-session-store\""));
        assert!(html.contains("data-plan-ref=\"risk:r-locking\""));
        assert!(html.contains(&format!("data-plan-fingerprint=\"{hash}\"")));
        assert!(html.contains("<details"));
        // Dropped per the plan's own note on this assertion ("if you add
        // links to fixtures keep them https-free or drop that assertion
        // line"): kitchen-sink.json has dependency edges, so the SVG always
        // renders, and every SVG root carries a static XML namespace
        // declaration (`xmlns="http://www.w3.org/2000/svg"`) — a namespace
        // identifier, not a fetched resource, but literal `http://` text
        // all the same. `!html.to_lowercase().contains("@import")` below
        // still guards the actual external-fetch vector.
        assert!(!html.to_lowercase().contains("@import"));
    }

    #[test]
    fn summary_strip_and_order() {
        let plan = plan_from("kitchen-sink.json");
        let html = render(&plan);

        // (a) summary strip present with the task/phase counts.
        let summary_pos = html
            .find("<section class=\"plan-summary\"")
            .expect("plan-summary present");
        assert!(html.contains("5 tasks"), "{html}");
        assert!(html.contains("2 phases"), "{html}");

        // (b) blocking question link, anchored to its full entry.
        assert!(html.contains("href=\"#question-q-ttl\""), "{html}");

        // (c) order by byte position: summary < open questions < risks <
        // first phase details < graph details.
        let open_q_pos = html.find("Open questions").expect("open questions heading");
        let risks_pos = html.find(">Risks<").expect("risks heading");
        let phase_pos = html
            .find("<details class=\"phase\"")
            .expect("phase details");
        let graph_pos = html
            .find("<details class=\"graph\"")
            .expect("graph details");
        assert!(summary_pos < open_q_pos, "summary before open questions");
        assert!(open_q_pos < risks_pos, "open questions before risks");
        assert!(risks_pos < phase_pos, "risks before first phase");
        assert!(phase_pos < graph_pos, "phases before the whole-plan graph");

        // (d) phases (and the graph) are collapsed by default.
        assert!(!html.contains("<details class=\"phase\" open"), "{html}");
        assert!(!html.contains("<details class=\"graph\" open"), "{html}");

        // (e) the blocking link's target anchor exists.
        assert!(html.contains("id=\"question-q-ttl\""), "{html}");
    }

    #[test]
    fn render_is_deterministic_and_matches_golden() {
        let plan = plan_from("kitchen-sink.json");
        let a = render(&plan);
        assert_eq!(a, render(&plan));
        let path = format!(
            "{}/tests/fixtures/plan/kitchen-sink.html",
            env!("CARGO_MANIFEST_DIR")
        );
        if std::env::var("UPDATE_GOLDEN").is_ok() {
            std::fs::write(&path, &a).unwrap();
        } else {
            let expected = std::fs::read_to_string(&path).unwrap();
            assert_eq!(a, expected, "golden drift — UPDATE_GOLDEN=1 to regenerate");
        }
    }
}
