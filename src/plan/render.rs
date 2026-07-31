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
const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; \
                    img-src data:; font-src data:";

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

/// Split a phase's `summary_md` into a teaser that is safe inside the
/// phrasing-content `<summary>` element and whatever block content remains.
///
/// The teaser is the FIRST paragraph's inner HTML — a paragraph's content is
/// phrasing content by construction (the sanitizer already rewrites images,
/// the only exception, into links/emphasis), so no `<ul>`/`<pre>`/`<table>`
/// can leak into `<summary>` (Savio's PR #22 finding: stripping only `<p>`
/// tags let every other block element through). Everything after the first
/// paragraph — or the whole rendering when the summary doesn't START with a
/// paragraph — is returned as the remainder for the caller to render inside
/// the expanded `<details>` body, so block-heavy summaries lose nothing.
///
/// `</p>` cannot occur inside the first paragraph's inner HTML (the
/// sanitizer escapes literal `</p>` text and HTML forbids nested `<p>`), so
/// the split point is unambiguous.
fn phase_summary_parts(text: &Option<String>) -> (Option<Markup>, Option<Markup>) {
    let Some(t) = text.as_ref() else {
        return (None, None);
    };
    let html = crate::markdown::render_markdown(t);
    let html = html.trim();
    if let Some(after_open) = html.strip_prefix("<p>") {
        if let Some(close) = after_open.find("</p>") {
            let teaser = after_open[..close].trim().to_string();
            let rest = after_open[close + "</p>".len()..].trim().to_string();
            return (
                (!teaser.is_empty()).then_some(PreEscaped(teaser)),
                (!rest.is_empty()).then_some(PreEscaped(rest)),
            );
        }
    }
    // First block isn't a paragraph: there is no phrasing-safe teaser;
    // render the whole summary in the body instead.
    (
        None,
        (!html.is_empty()).then_some(PreEscaped(html.to_string())),
    )
}

/// A copy of `plan` with every markdown field (`goal_md`, `meta.summary_md`,
/// `meta.key_points`, phase/task `summary_md`, `mitigation_md`,
/// `question_md`) replaced by its sanitized HTML rendering, for the JSON
/// data island. `meta.out_of_scope` is left as-is: the visible page renders
/// it as plain escaped text (never through the markdown sanitizer), so
/// there's nothing to sanitize here either — same rationale as `title` and
/// other plain-text fields, which this function also leaves untouched.
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
    p.meta.summary_md = p
        .meta
        .summary_md
        .as_deref()
        .map(crate::markdown::render_markdown);
    p.meta.key_points = p
        .meta
        .key_points
        .iter()
        .map(|s| crate::markdown::render_markdown(s))
        .collect();
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

/// Plain-language label for an estimate, e.g. `small` for `Estimate::S`.
/// Everywhere the page shows an estimate to a human uses this, not the terse
/// wire-format letter — the letter only survives in the phase ledger's
/// `2s · 1m` distribution, where the column is too narrow for words.
fn estimate_label(estimate: &Estimate) -> &'static str {
    match estimate {
        Estimate::S => "small",
        Estimate::M => "medium",
        Estimate::L => "large",
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

/// A category chip: a severity, a status, a file action. `variant` is the
/// bare state name (`"high"`, `"done"`, `"create"`, …) and becomes the
/// `pv-chip-{variant}` modifier `plan.css` colors — so a chip's appearance is
/// always a direct function of the state it names, never a per-call choice.
fn chip(variant: &str, label: &str) -> Markup {
    html! { span class=(format!("pv-chip pv-chip-{variant}")) { (label) } }
}

/// A 6px state marker with its label beside it, for the task metadata rails
/// and the graph legends — the quietest way the page can state a status.
/// `aria-hidden` on the square itself: the label right next to it already
/// says the same thing in words.
fn dot_line(variant: &str, label: &str) -> Markup {
    html! {
        span {
            span class=(format!("pv-dot pv-dot-{variant}")) aria-hidden="true" {}
            (label)
        }
    }
}

/// A section opener: a mono label, a hairline running to the right margin, and
/// an optional slot for controls that belong to the section.
///
/// Every section on the page uses this and nothing else, which is most of what
/// makes plans of very different shapes read as one document. `actions_id`
/// gives `plan.js` a stable target to inject the expand/collapse controls into
/// — the markup ships the empty container so scripting never has to build the
/// surrounding structure (and `plan.css` hides it while it stays empty).
fn section_rule(label: &str, actions_id: Option<&str>) -> Markup {
    html! {
        div.pv-rule {
            span.pv-label { (label) }
            span.pv-rule-line {}
            @if let Some(id) = actions_id {
                span.pv-rule-actions id=(id) {}
            }
        }
    }
}

/// One cell of the stat strip: a figure and the caption under it. `hot` is the
/// single license the strip has to use the accent — reserved for a count a
/// reviewer must not scroll past (high-severity risks, blocking questions).
struct Stat {
    figure: String,
    label: String,
    hot: bool,
}

/// The stat strip's cells, in reading order.
///
/// Cells are emitted only when the plan HAS the thing they count, which is
/// what keeps the strip honest across plan shapes: a plan with no risks shows
/// no risk cell rather than a `0` that implies the author cleared them. Tasks
/// and phases are unconditional — every plan has both, even at zero, and their
/// absence is itself worth stating.
fn stat_cells(plan: &Plan) -> Vec<Stat> {
    let tasks: Vec<&PlanTask> = plan.phases.iter().flat_map(|p| p.tasks.iter()).collect();
    let done = tasks
        .iter()
        .filter(|t| matches!(t.status, Status::Done))
        .count();

    let mut cells = vec![Stat {
        figure: tasks.len().to_string(),
        // A progress suffix only once something is actually finished — on a
        // fresh plan "0 done" is noise, not information.
        label: if done > 0 {
            format!("Tasks · {done} done")
        } else {
            "Tasks".into()
        },
        hot: false,
    }];

    cells.push(Stat {
        figure: plan.phases.len().to_string(),
        label: "Phases".into(),
        hot: false,
    });

    if !plan.risks.is_empty() {
        let high = plan
            .risks
            .iter()
            .filter(|r| matches!(r.severity, RiskLevel::High))
            .count();
        cells.push(Stat {
            figure: plan.risks.len().to_string(),
            label: if high > 0 {
                format!("Risks · {high} high")
            } else {
                "Risks".into()
            },
            hot: high > 0,
        });
    }

    if !plan.open_questions.is_empty() {
        let blocking = plan.open_questions.iter().filter(|q| q.blocking).count();
        cells.push(Stat {
            figure: plan.open_questions.len().to_string(),
            label: if blocking > 0 {
                format!("Questions · {blocking} blocking")
            } else {
                "Questions".into()
            },
            hot: blocking > 0,
        });
    }

    cells
}

/// The orientation banner's sentence — the one line on the page that explains
/// the page itself.
///
/// The wording tracks real progress rather than asserting a fixed story: a
/// plan whose tasks are all still planned genuinely hasn't been built, but
/// saying "nothing is built yet" over a plan showing eight done tasks would be
/// plainly false to the reader looking at it.
fn banner_lead(plan: &Plan) -> (&'static str, &'static str) {
    let tasks: Vec<&PlanTask> = plan.phases.iter().flat_map(|p| p.tasks.iter()).collect();
    let started = tasks
        .iter()
        .any(|t| matches!(t.status, Status::Done | Status::InProgress));
    if started {
        (
            "This plan is being worked through — ",
            "the statuses below are the agent's own record.",
        )
    } else {
        ("An agent drafted this plan — ", "nothing is built yet.")
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

/// Whether the plan's risks span more than one severity. The stat strip
/// already reports the total and the high count, so the ledger's per-severity
/// breakdown only earns its line when there is a mix to break down.
fn risk_severity_spread(plan: &Plan) -> bool {
    let mut seen = [false; 3];
    for r in &plan.risks {
        match r.severity {
            RiskLevel::High => seen[0] = true,
            RiskLevel::Medium => seen[1] = true,
            RiskLevel::Low => seen[2] = true,
        }
    }
    seen.iter().filter(|s| **s).count() > 1
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

/// A phase's estimate distribution for the executive-summary rollup table,
/// e.g. `"2 small, 1 medium"` — only sizes that occur, empty when no task in
/// the phase carries an estimate.
fn phase_estimate_dist(phase: &Phase) -> String {
    let mut sizes = [0usize; 3]; // s, m, l
    for t in &phase.tasks {
        match t.estimate {
            Some(Estimate::S) => sizes[0] += 1,
            Some(Estimate::M) => sizes[1] += 1,
            Some(Estimate::L) => sizes[2] += 1,
            None => {}
        }
    }
    sizes
        .iter()
        .zip(["s", "m", "l"])
        .filter(|(n, _)| **n > 0)
        .map(|(n, label)| format!("{n}{label}"))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Whether the summary ledger's right-hand column would carry anything at
/// all: true as soon as one phase has an estimate distribution or a risk
/// heat. A plan whose tasks are all unestimated and unrated fills that column
/// with nothing in every row, and an `EST · RISK` header over a stack of empty
/// cells reads as a rendering failure rather than as "not stated" — so the
/// column is dropped instead (see the ledger markup).
fn ledger_has_figures(plan: &Plan) -> bool {
    plan.phases
        .iter()
        .any(|p| !phase_estimate_dist(p).is_empty() || !phase_risk_heat(p).is_empty())
}

/// The rail's phase cell: the head of a `title — subtitle` name, capped for
/// the narrow column (the full title is one click away on the phase row
/// itself). Titles without the separator just truncate.
fn short_phase_title(title: &str) -> String {
    let head = title.split(" — ").next().unwrap_or(title);
    truncate_chars(head, 28)
}

/// A phase's risk heat for the rollup table: the count of tasks at the
/// *highest* risk severity present in the phase, e.g. `"1 high"`. A phase
/// with one high-risk task and two medium-risk tasks reports only "1 high"
/// — once a higher severity is present, the lower counts don't also need
/// spelling out in this compact a cell. Empty when no task in the phase
/// carries a risk rating.
fn phase_risk_heat(phase: &Phase) -> String {
    let mut counts = [0usize; 3]; // high, medium, low
    for t in &phase.tasks {
        match t.risk {
            Some(RiskLevel::High) => counts[0] += 1,
            Some(RiskLevel::Medium) => counts[1] += 1,
            Some(RiskLevel::Low) => counts[2] += 1,
            None => {}
        }
    }
    counts
        .iter()
        .zip(["high", "medium", "low"])
        .find(|(n, _)| **n > 0)
        .map(|(n, label)| format!("{n} {label}"))
        .unwrap_or_default()
}

/// One task: a body column (heading, markdown summary, files, acceptance
/// criteria, validation commands, dependencies) and a fixed metadata rail on
/// the right carrying status, risk, estimate, and the task id.
///
/// The rail is why the split exists. A task's body can be one sentence or two
/// screens of markdown, but its state always appears in the same place at the
/// same size — so a reader scanning a forty-task plan reads state by position
/// instead of hunting for badges inside prose.
///
/// `id="task-{id}"` is what the SVG's `#task-{id}` links jump to.
/// `data-plan-ref="task:{id}"` is what the comment tooling anchors a comment
/// to; acceptance rows carry the *same* parent ref (a comment on a criterion
/// attaches to its task, and quotes the criterion's line).
fn task_row(task: &PlanTask) -> Markup {
    let task_ref = format!("task:{}", task.id);
    html! {
        div.task id=(format!("task-{}", task.id)) data-plan-ref=(task_ref) {
            div.task-body {
                div.task-head { h3 { (task.title) } }
                @if task.summary_md.is_some() {
                    div.pv-prose { (md(&task.summary_md)) }
                }
                @if !task.files.is_empty() {
                    ul.files {
                        @for f in &task.files {
                            li {
                                code { (f.path) }
                                (chip(file_action_str(&f.action), file_action_str(&f.action)))
                                @if let Some(note) = &f.note {
                                    span.file-note { (note) }
                                }
                            }
                        }
                    }
                }
                @if !task.acceptance.is_empty() {
                    div.pv-rule {
                        span.pv-label { "Acceptance" }
                        span.pv-rule-line {}
                    }
                    ul.acceptance {
                        @for item in &task.acceptance {
                            li data-plan-ref=(format!("task:{}", task.id)) { span { (item) } }
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
            div.task-rail {
                (dot_line(status_str(&task.status), status_str(&task.status)))
                @if let Some(risk) = &task.risk {
                    (dot_line(risk_str(risk), &format!("{} risk", risk_str(risk))))
                }
                @if let Some(estimate) = &task.estimate {
                    (dot_line("planned", &format!("{} effort", estimate_label(estimate))))
                }
                span.task-id { (task.id) }
            }
        }
    }
}

/// The key under a dependency graph. Fixed, not derived from the graph's
/// contents: a legend that changes shape per phase would make two graphs on
/// the same page disagree about what a colour means.
fn graph_legend() -> Markup {
    html! {
        div.pv-legend {
            (dot_line("done", "done"))
            (dot_line("in_progress", "in progress"))
            (dot_line("blocked", "blocked"))
            (dot_line("planned", "planned"))
        }
    }
}

/// A phase's task rollup for its heading line: how many, and how far along.
/// `"3 tasks · all done"` when every task is finished, `"3 tasks · 1 done"`
/// part-way, and just the count when nothing has started.
fn phase_progress(phase: &Phase) -> String {
    let n = phase.tasks.len();
    let done = phase
        .tasks
        .iter()
        .filter(|t| matches!(t.status, Status::Done))
        .count();
    let count = count_label(n, "task");
    if n > 0 && done == n {
        format!("{count} · all done")
    } else if done > 0 {
        format!("{count} · {done} done")
    } else {
        count
    }
}

pub fn render(plan: &Plan) -> String {
    let hash = plan_hash(plan);
    let island = escape_json_island(
        &serde_json::to_string(&sanitized_for_island(plan)).expect("plan serializes"),
    );
    // The bottom-of-page overview is now phase-level (nodes = phases, not
    // tasks) — see plan/svg.rs's module doc for why the old whole-plan task
    // graph was dropped. Per-phase task graphs render unconditionally below.
    let phase_graph = svg::phase_graph_svg(plan);
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
                // The whole document lives inside one sheet: the plan reads as
                // a page on a desk rather than a full-bleed app. Everything
                // below pads to --gutter, so the topbar, the banner, and the
                // body column share one left edge.
                div.pv-sheet {
                    // Topbar: names the surface in every serving context —
                    // file:// auto-open and studio's sandboxed /artifacts
                    // route alike — and carries the plan's identity so it
                    // stays on screen through a long scroll. Brand first,
                    // surface second (same hierarchy as studio's topbar):
                    // "Loadout" is the product, "Plan viewer" is a room in it.
                    // Static markup only; the theme toggle is injected by
                    // plan.js, since a control that does nothing without
                    // scripting has no business in the served HTML.
                    header.pv-topbar {
                        div.pv-brand {
                            span.pv-brand-mark aria-hidden="true" {}
                            span.pv-brand-name { "Loadout" }
                            span.pv-brand-surface { "Plan viewer" }
                        }
                        div.pv-topbar-right {
                            span.pv-topbar-id {
                                (plan.meta.id)
                                @if let Some(rev) = plan.meta.revision { " · rev " (rev) }
                            }
                        }
                    }
                    // Orientation banner: what this page is and what to do
                    // with it. A reviewer opening a rendered plan cold has no
                    // other way to know the page collects comments.
                    div.pv-banner {
                        span.pv-chip.pv-chip-solid { "For review" }
                        p.pv-banner-text {
                            (banner_lead(plan).0)
                            strong { (banner_lead(plan).1) }
                            " Comment on anything, then copy your feedback back into the conversation."
                        }
                        span.pv-banner-steps {
                            b { "01" } " skim  " b { "02" } " comment  " b { "03" } " copy feedback"
                        }
                    }
                    main.pv-main {
                        div.pv-head {
                            // Eyebrow: the metadata a reader wants placed
                            // before the name, not after it.
                            p.pv-eyebrow {
                                @if let Some(agent) = &plan.meta.agent { "by " (agent) }
                                @if plan.meta.agent.is_some() && plan.meta.created.is_some() { " · " }
                                @if let Some(created) = &plan.meta.created { (created) }
                                @if plan.meta.agent.is_none() && plan.meta.created.is_none() {
                                    "plan " code { (plan.meta.id) }
                                }
                            }
                            h1 { (plan.meta.title) }
                            @if plan.meta.goal_md.is_some() {
                                div.pv-lede { (md(&plan.meta.goal_md)) }
                            }
                        }
                        // Stat strip: the plan's shape in four figures or
                        // fewer. Cells with nothing to count are not emitted
                        // (see `stat_cells`), so the strip never shows a
                        // hollow zero.
                        div.pv-stats {
                            @for cell in stat_cells(plan) {
                                div class=(if cell.hot { "pv-stat is-hot" } else { "pv-stat" }) {
                                    span.pv-stat-n { (cell.figure) }
                                    span.pv-stat-l { (cell.label) }
                                }
                            }
                        }
                        (section_rule("Summary", None))
                        // The `meta:` comment anchor lives on the summary
                        // itself (not the tiny byline above): "comment on the
                        // plan as a whole" reads as commenting on the
                        // executive summary, and the byline gave the button no
                        // visible target worth quoting.
                        section.plan-summary data-plan-ref=(format!("meta:{}", plan.meta.id)) {
                            // Two zones on a wide viewport: the prose on the
                            // left at its reading measure, the phase ledger on
                            // the right where a scanner looks first.
                            div.summary-grid {
                                // (a) The executive summary — the top of the
                                // page, so a reader who stops here still gets
                                // a correct high-level picture. Never
                                // fabricated: absent summary_md gets a plain
                                // note, not invented content.
                                div.summary-exec {
                                    @if let Some(summary) = &plan.meta.summary_md {
                                        (PreEscaped(crate::markdown::render_markdown(summary)))
                                    } @else {
                                        p.summary-missing {
                                            "No executive summary — the plan author can set meta.summary_md."
                                        }
                                    }
                                }
                                // (b) The ledger: per-phase rollup, then the
                                // ask. Suppressed wholesale on a plan with no
                                // phases — an empty table with a header row
                                // reads as a rendering failure.
                                aside.summary-glance {
                                    @if !plan.phases.is_empty() {
                                        @let figures = ledger_has_figures(plan);
                                        table.pv-ledger {
                                            thead {
                                                tr {
                                                    th { "Phase" }
                                                    @if figures { th { "Est · risk" } }
                                                }
                                            }
                                            tbody {
                                                @for phase in &plan.phases {
                                                    tr data-phase-row=(phase.id) {
                                                        td {
                                                            a href=(format!("#phase-{}", phase.id)) {
                                                                (short_phase_title(&phase.title))
                                                            }
                                                        }
                                                        @if figures {
                                                            @let heat = phase_risk_heat(phase);
                                                            td class=(
                                                                if heat.contains("high") {
                                                                    "pv-ledger-fig is-hot"
                                                                } else {
                                                                    "pv-ledger-fig"
                                                                }
                                                            ) {
                                                                (phase_estimate_dist(phase))
                                                                @if !heat.is_empty() { " · " (heat) }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    // (c) The ask: whether this plan can move
                                    // forward as it stands — computed, not
                                    // authored.
                                    p class=(if !blocking.is_empty() { "summary-ask has-blocking" } else { "summary-ask" }) {
                                        span class=(
                                            if blocking.is_empty() { "pv-dot pv-dot-done" } else { "pv-dot pv-dot-high" }
                                        ) aria-hidden="true" {}
                                        span {
                                            @if !blocking.is_empty() {
                                                (format!(
                                                    "{} blocking question(s) must be resolved before implementation: ",
                                                    blocking.len()
                                                ))
                                                @for (i, q) in blocking.iter().enumerate() {
                                                    @if i > 0 { ", " }
                                                    a href=(format!("#question-{}", q.id)) {
                                                        (truncate_chars(&q.question_md, 100))
                                                    }
                                                }
                                            } @else {
                                                "No blocking questions"
                                            }
                                        }
                                    }
                                    // The risk register's own breakdown, but
                                    // only the part the stat strip above does
                                    // NOT already state. A plan whose risks are
                                    // all one severity is fully described by
                                    // "6 risks · 2 high" up there; spelling it
                                    // out again here is the same sentence
                                    // twice.
                                    @if let Some((line, has_high)) = &risk_line {
                                        @if risk_severity_spread(plan) {
                                            p class=(if *has_high { "summary-ask has-blocking" } else { "summary-ask" }) {
                                                span class=(
                                                    if *has_high { "pv-dot pv-dot-high" } else { "pv-dot pv-dot-medium" }
                                                ) aria-hidden="true" {}
                                                span { (line) }
                                            }
                                        }
                                    }
                                }
                            }
                            // (d) Supporting bullets, one per major workstream
                            // or decision, spanning the full width below both
                            // zones.
                            @if !plan.meta.key_points.is_empty() {
                                ul.summary-keypoints {
                                    @for kp in &plan.meta.key_points {
                                        li { (PreEscaped(crate::markdown::render_markdown(kp))) }
                                    }
                                }
                            }
                            // (e) Explicit non-goals, plain text (no markdown).
                            @if !plan.meta.out_of_scope.is_empty() {
                                p.summary-outofscope {
                                    strong { "Out of scope: " }
                                    @for (i, item) in plan.meta.out_of_scope.iter().enumerate() {
                                        @if i > 0 { ", " }
                                        (item)
                                    }
                                }
                            }
                        }
                        // Open questions and risks share one row shape — a
                        // severity chip, then a claim about it — because both
                        // are the same kind of statement and a reader should
                        // not have to learn two layouts for them.
                        @if !plan.open_questions.is_empty() {
                            (section_rule("Open questions", None))
                            section.pv-rows.questions {
                                @for q in &plan.open_questions {
                                    div.pv-row id=(format!("question-{}", q.id))
                                        data-plan-ref=(format!("question:{}", q.id)) {
                                        (chip(
                                            if q.blocking { "high" } else { "planned" },
                                            if q.blocking { "Blocking" } else { "Open" },
                                        ))
                                        div.pv-row-body {
                                            // The question is this row's
                                            // heading line, even though it is
                                            // prose rather than an <h3>. It
                                            // gets the heading wrapper so the
                                            // Answer button plan.js injects
                                            // lands beside it at the right
                                            // margin, the way a risk's Comment
                                            // button does — without it the
                                            // button drops onto a line of its
                                            // own under a one-line question.
                                            div.pv-row-head {
                                                div.pv-prose {
                                                    (PreEscaped(crate::markdown::render_markdown(&q.question_md)))
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        @if !plan.risks.is_empty() {
                            (section_rule("Risks", None))
                            section.pv-rows.risks {
                                @for r in &plan.risks {
                                    div.pv-row data-plan-ref=(format!("risk:{}", r.id)) {
                                        (chip(risk_str(&r.severity), risk_str(&r.severity)))
                                        div.pv-row-body {
                                            div.pv-row-head {
                                                h3 id=(format!("risk-{}", r.id)) { (r.title) }
                                            }
                                            @if r.mitigation_md.is_some() {
                                                div.pv-prose { (md(&r.mitigation_md)) }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        // The expand/collapse controls land in this rule's
                        // actions slot (plan.js fills `#phases-actions`), so
                        // they sit on the section divider rather than floating
                        // above the list as a stray toolbar.
                        @if !plan.phases.is_empty() {
                            (section_rule("Phases", Some("phases-actions")))
                        }
                        div.pv-phases {
                            @for (i, phase) in plan.phases.iter().enumerate() {
                                details.phase id=(format!("phase-{}", phase.id))
                                    data-plan-ref=(format!("phase:{}", phase.id)) {
                                    @let (teaser, summary_rest) = phase_summary_parts(&phase.summary_md);
                                    summary {
                                        // Numeral, content, marker. The numeral
                                        // is the phase's position in the plan,
                                        // not its id — a reader counts phases,
                                        // they don't parse slugs.
                                        span.phase-numeral aria-hidden="true" {
                                            (format!("{:02}", i + 1))
                                        }
                                        div.phase-head {
                                            div.phase-head-line {
                                                h2 { (phase.title) }
                                                span.phase-meta { (phase_progress(phase)) }
                                                @let high = phase
                                                    .tasks
                                                    .iter()
                                                    .filter(|t| matches!(t.risk, Some(RiskLevel::High)))
                                                    .count();
                                                @if high > 0 {
                                                    (chip("high", &format!("{high} high risk")))
                                                }
                                            }
                                            // The phase's plain-English
                                            // description is part of the
                                            // COLLAPSED row — a reader scanning
                                            // shut phases still learns what each
                                            // one is. First paragraph only: it
                                            // sits inside <summary>, which is
                                            // phrasing content (see
                                            // phase_summary_parts).
                                            @if let Some(teaser) = teaser {
                                                span.phase-teaser { (teaser) }
                                            }
                                        }
                                        span.phase-marker aria-hidden="true" { "›" }
                                    }
                                    div.phase-body {
                                        // Block content the teaser couldn't
                                        // carry (lists, tables, paragraphs past
                                        // the first) shows once expanded.
                                        @if let Some(rest) = summary_rest {
                                            div.phase-summary-rest.pv-prose { (rest) }
                                        }
                                        @if let Some(g) = svg::phase_svg(plan, &phase.id) {
                                            div.pv-deps {
                                                div.pv-rule {
                                                    span.pv-label { "Dependencies" }
                                                    span.pv-rule-line {}
                                                }
                                                (PreEscaped(g))
                                                (graph_legend())
                                            }
                                        }
                                        @for task in &phase.tasks { (task_row(task)) }
                                    }
                                }
                            }
                        }
                        @if let Some(g) = &phase_graph {
                            (section_rule("Phase dependencies", None))
                            div.pv-deps {
                                (PreEscaped(g.as_str()))
                                (graph_legend())
                            }
                        }
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
        // The embedded stylesheet's only url() references are the Inter
        // font's data: URIs — a url(http…)/url(//…) would be a fetch, which
        // the self-containment contract (and the CSP) forbids. Checked over
        // the stylesheet, not the whole document, because task summaries can
        // legitimately contain the literal text `url(` inside code spans.
        for (i, _) in CSS.match_indices("url(") {
            let after = &CSS[i + "url(".len()..];
            let after = after.trim_start_matches(['"', '\'']);
            assert!(
                after.starts_with("data:"),
                "plan.css url() must be a data: URI, found: {}",
                &CSS[i..CSS.len().min(i + 60)]
            );
        }
        assert!(!CSS.contains("url(http"), "no external url() in plan.css");
        // And the fonts did actually land: three faces across two families
        // (Newsreader upright + italic, JetBrains Mono upright). Regenerate
        // them with tools/build-plan-fonts.py, never by hand.
        assert_eq!(CSS.matches("@font-face").count(), 3);
        assert_eq!(CSS.matches("url(\"data:font/woff2;base64,").count(), 3);
        assert!(CSS.contains("font-family: \"Newsreader\""));
        assert!(CSS.contains("font-family: \"JetBrains Mono\""));
    }

    #[test]
    fn top_of_page_structure() {
        let html = render(&plan_from("kitchen-sink.json"));
        // The topbar is the first thing on the page, above the eyebrow — it
        // names the surface in both serving contexts and is static markup
        // only (no links, nothing fetched; the theme toggle is injected by
        // plan.js, so it is deliberately absent from the served HTML).
        let brand_pos = html.find("<header class=\"pv-topbar\">").expect("topbar");
        assert!(html.contains("pv-brand-mark"), "brand mark");
        assert!(html.contains(">Loadout</span>"), "brand name");
        assert!(html.contains(">Plan viewer</span>"), "surface label");
        // Tag-anchored: the bare class name also appears in the embedded
        // stylesheet's `.pv-theme { … }` rule whether or not it is used.
        assert!(
            !html.contains("<div class=\"pv-theme\""),
            "the theme toggle is script-injected, not served"
        );
        // The topbar carries the plan's identity through a long scroll.
        assert!(html.contains("auth-refactor · rev 2"), "plan id in topbar");
        // Eyebrow (byline + created) renders above the h1.
        let meta_pos = html
            .find("<p class=\"pv-eyebrow\">")
            .expect("byline eyebrow");
        let h1_pos = html.find("<h1>").expect("title");
        assert!(brand_pos < meta_pos, "brand strip above the eyebrow");
        assert!(meta_pos < h1_pos, "eyebrow above the title");
        assert!(html.contains("2026-07-07"), "created date in eyebrow");
        // Exec prose and the at-a-glance rail share the summary grid; the
        // ask lives at the rail's bottom; key points span below the grid.
        // (Tag-anchored substrings — bare class names also appear in the
        // embedded stylesheet.)
        let grid_pos = html.find("<div class=\"summary-grid\">").expect("grid");
        let glance_pos = html
            .find("<aside class=\"summary-glance\">")
            .expect("glance rail");
        // The stat strip sits between the masthead and the summary, and only
        // emits cells the plan can actually fill (see `stat_cells`).
        let stats_pos = html.find("<div class=\"pv-stats\">").expect("stat strip");
        assert!(
            h1_pos < stats_pos && stats_pos < grid_pos,
            "strip under the title"
        );
        assert_eq!(html.matches("class=\"pv-stat-n\"").count(), 4, "{html}");
        let ask_pos = html.find("<p class=\"summary-ask").expect("ask");
        let keypoints_pos = html
            .find("<ul class=\"summary-keypoints\">")
            .expect("keypoints");
        assert!(grid_pos < glance_pos, "rail inside the grid");
        assert!(glance_pos < ask_pos, "ask inside the rail");
        assert!(ask_pos < keypoints_pos, "key points after the grid");
    }

    #[test]
    fn summary_card_carries_the_meta_comment_anchor() {
        let html = render(&plan_from("kitchen-sink.json"));
        assert!(
            html.contains("<section class=\"plan-summary\" data-plan-ref=\"meta:auth-refactor\">"),
            "meta anchor should live on the summary card"
        );
        // …and only there: the byline is no longer a comment target.
        assert_eq!(html.matches("data-plan-ref=\"meta:").count(), 1);
    }

    #[test]
    fn phase_summary_is_visible_in_the_collapsed_row() {
        let html = render(&plan_from("kitchen-sink.json"));
        // p-core's summary_md renders as a teaser inside <summary> (visible
        // while collapsed) …
        let teaser_pos = html
            .find("<span class=\"phase-teaser\">The trait seam.</span>")
            .expect("teaser inside the phase heading");
        assert!(
            html[teaser_pos..].find("</summary>").is_some(),
            "teaser must sit inside the <summary> element"
        );
        // … not as a block after </summary>, which is hidden while collapsed.
        assert!(!html.contains("</summary><p>The trait seam.</p>"));
        // p-backend has no summary_md: exactly one teaser on the page.
        assert_eq!(html.matches("class=\"phase-teaser\"").count(), 1);
    }

    #[test]
    fn phase_summary_teaser_is_phrasing_safe() {
        // Single paragraph: all teaser, no remainder, inline markup kept.
        let (teaser, rest) = phase_summary_parts(&Some("Extract *session* handling.".into()));
        let teaser = teaser.expect("teaser").into_string();
        assert!(teaser.contains("<em>session</em>"), "{teaser}");
        assert!(!teaser.contains("<p>"), "{teaser}");
        assert!(rest.is_none());

        // Paragraph followed by a list: the list must NOT reach the teaser
        // (Savio's PR #22 finding) — it lands in the remainder instead.
        let (teaser, rest) = phase_summary_parts(&Some("Lead sentence.\n\n- alpha\n- beta".into()));
        let teaser = teaser.expect("teaser").into_string();
        assert_eq!(teaser, "Lead sentence.");
        let rest = rest.expect("remainder").into_string();
        assert!(rest.contains("<ul>"), "{rest}");

        // Summary that STARTS with a block element: no teaser at all, the
        // whole rendering goes to the body.
        let (teaser, rest) = phase_summary_parts(&Some("- only\n- a list".into()));
        assert!(teaser.is_none());
        assert!(rest.expect("remainder").into_string().contains("<ul>"));

        assert!(phase_summary_parts(&None).0.is_none());
        let (t, r) = phase_summary_parts(&Some(String::new()));
        assert!(t.is_none() && r.is_none());
    }

    /// Regression fixture: the first real plan written against the schema
    /// (the v0.15.0 learning release, 23 tasks / 7 phases, near-limit
    /// summaries, dependency edges, risks, open questions).
    #[test]
    fn real_learning_plan_fixture_parses_validates_renders() {
        let raw = std::fs::read_to_string(format!(
            "{}/tests/fixtures/plan/learning-v0-15.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let parsed = crate::plan::model::parse(&raw, false).unwrap();
        assert!(parsed.warnings.is_empty());
        assert!(crate::plan::model::validate(&parsed.plan).is_empty());
        let html = render(&parsed.plan);
        assert_eq!(html.matches("id=\"task-").count(), 23, "23 task cards");
        // Its revision-1 meta is exactly the shape the advisories exist for:
        // an overlong single-paragraph summary and a goal that reads as a
        // second summary. The kitchen sink trips none of them.
        let codes: Vec<String> = crate::plan::model::advisories(&parsed.plan)
            .into_iter()
            .map(|i| i.code)
            .collect();
        for expected in ["long_summary", "wall_of_text", "long_goal"] {
            assert!(
                codes.iter().any(|c| c == expected),
                "missing {expected} in {codes:?}"
            );
        }
        assert!(crate::plan::model::advisories(&plan_from("kitchen-sink.json")).is_empty());
        // A spec-compressed key point (the shape a later revision actually
        // shipped before review caught it) trips the fourth advisory.
        let mut bloated = parsed.plan.clone();
        bloated.meta.key_points.push("k".repeat(501));
        let codes: Vec<String> = crate::plan::model::advisories(&bloated)
            .into_iter()
            .map(|i| i.code)
            .collect();
        assert!(
            codes.iter().any(|c| c == "long_key_point"),
            "missing long_key_point in {codes:?}"
        );
    }

    #[test]
    fn summary_strip_and_order() {
        let plan = plan_from("kitchen-sink.json");
        let html = render(&plan);

        // (a) summary strip present with the task/phase counts.
        let summary_pos = html
            .find("<section class=\"plan-summary\"")
            .expect("plan-summary present");
        // Counts now live in the stat strip's cells rather than one prose
        // line, so assert on the figure/caption pair.
        assert!(
            html.contains("<span class=\"pv-stat-l\">Tasks · 1 done</span>"),
            "{html}"
        );
        assert!(
            html.contains("<span class=\"pv-stat-l\">Phases</span>"),
            "{html}"
        );

        // (b) executive summary block: present, with a distinctive
        // substring from the fixture's summary_md (through the sanitizer,
        // so the backtick becomes a <code> tag — assert on surrounding
        // plain text instead).
        let exec_pos = html
            .find("<div class=\"summary-exec\">")
            .expect("summary-exec");
        assert!(
            html.contains("closes the <em>lock contention</em> risk"),
            "{html}"
        );
        // (checked against the body markup, not a bare substring match —
        // the class name also appears once in the embedded <style> block's
        // `.summary-missing { … }` rule regardless of whether it's used).
        assert!(!html.contains("<p class=\"summary-missing\">"), "{html}");

        // (c) key points: 3 <li> inside summary-keypoints.
        let keypoints_start = html
            .find("<ul class=\"summary-keypoints\">")
            .expect("summary-keypoints");
        let keypoints_end = html[keypoints_start..]
            .find("</ul>")
            .map(|i| keypoints_start + i)
            .expect("summary-keypoints closes");
        let keypoints_html = &html[keypoints_start..keypoints_end];
        assert_eq!(
            keypoints_html.matches("<li>").count(),
            3,
            "{keypoints_html}"
        );
        assert!(
            keypoints_html.contains("<strong>Trait extraction</strong>"),
            "{keypoints_html}"
        );

        // (d) out-of-scope line.
        assert!(
            html.contains("<p class=\"summary-outofscope\"><strong>Out of scope: </strong>Migrating existing sessions between backends, Multi-region session replication</p>"),
            "{html}"
        );

        // (e) the ask: has-blocking, with the q-ttl link. The severity is
        // carried by a CSS-drawn dot rather than a dingbat character — no
        // glyph for one survives in both the serif and a fallback face.
        let ask_pos = html
            .find("<p class=\"summary-ask has-blocking\">")
            .expect("summary-ask has-blocking");
        assert!(
            html.contains("1 blocking question(s) must be resolved before implementation"),
            "{html}"
        );
        assert!(
            !html.contains("⚠"),
            "no dingbats in the rendered page: {html}"
        );
        assert!(html.contains("href=\"#question-q-ttl\""), "{html}");

        // (f) phase rollup table: link to #phase-p-core, matching anchor id
        // on the phase's own details element.
        assert!(html.contains("<table class=\"pv-ledger\">"), "{html}");
        assert!(html.contains("href=\"#phase-p-core\""), "{html}");
        assert!(html.contains("id=\"phase-p-core\""), "{html}");
        // This plan's tasks carry estimates and risk ratings, so the ledger's
        // figure column is present. (`no_summary_shows_missing_note_and_ready_state`
        // covers the plan that drops it.)
        assert!(html.contains("<th>Est · risk</th>"), "{html}");

        // (f2) an open question's text is wrapped as the row's heading line,
        // so the Answer button plan.js injects lands beside it rather than
        // dropping onto a line of its own. Same wrapper a risk row uses.
        let q_pos = html
            .find("id=\"question-q-ttl\"")
            .expect("blocking question row");
        assert!(
            html[q_pos..].starts_with(
                "id=\"question-q-ttl\" data-plan-ref=\"question:q-ttl\">\
                 <span class=\"pv-chip pv-chip-high\">Blocking</span>\
                 <div class=\"pv-row-body\"><div class=\"pv-row-head\"><div class=\"pv-prose\">"
            ),
            "question row: chip, then a heading line wrapping the prose: {}",
            &html[q_pos..q_pos + 260]
        );

        // (g) order by byte position: summary block pieces in document
        // order, "Summary" heading < summary card < open questions < risks <
        // "Phases" heading < first phase details < graph details.
        //
        // Every section now opens with the same label-and-hairline rule
        // (see `section_rule`) instead of an icon-and-heading pair, so these
        // are anchored on the rule's label span.
        let summary_heading_pos = html
            .find("<span class=\"pv-label\">Summary</span>")
            .expect("summary rule");
        let open_q_pos = html
            .find("<span class=\"pv-label\">Open questions</span>")
            .expect("open questions rule");
        let risks_pos = html
            .find("<span class=\"pv-label\">Risks</span>")
            .expect("risks rule");
        let phases_heading_pos = html
            .find("<span class=\"pv-label\">Phases</span>")
            .expect("phases rule");
        let phase_pos = html
            .find("<details class=\"phase\"")
            .expect("phase details");
        let graph_pos = html
            .find("<span class=\"pv-label\">Phase dependencies</span>")
            .expect("phase dependencies rule");
        assert!(
            summary_heading_pos < summary_pos,
            "\"Summary\" heading before the summary card"
        );
        assert!(summary_pos < exec_pos, "summary section before exec block");
        assert!(exec_pos < ask_pos, "exec block before ask banner");
        assert!(summary_pos < open_q_pos, "summary before open questions");
        assert!(open_q_pos < risks_pos, "open questions before risks");
        assert!(
            risks_pos < phases_heading_pos,
            "risks before \"Phases\" heading"
        );
        assert!(
            phases_heading_pos < phase_pos,
            "\"Phases\" heading before first phase details"
        );
        assert!(phase_pos < graph_pos, "phases before the phase graph");

        // (g2) each phase's row carries a 1-based, document-order ordinal —
        // now set as the big numeral in its own column, so a reader always
        // knows which phase they're looking at, even scrolled deep into a
        // long plan. Zero-padded so 01..09 and 10+ share a column width.
        assert!(
            html.contains("<span class=\"phase-numeral\" aria-hidden=\"true\">01</span>"),
            "{html}"
        );
        assert!(
            html.contains("<span class=\"phase-numeral\" aria-hidden=\"true\">02</span>"),
            "{html}"
        );

        // (h) phases are collapsed by default.
        assert!(!html.contains("<details class=\"phase\" open"), "{html}");

        // (i) the blocking link's target anchor exists.
        assert!(html.contains("id=\"question-q-ttl\""), "{html}");

        // (j) the bottom graph is now phase-level, not the old whole-plan
        // task graph: a "Phase dependencies" heading, phase nodes linking to
        // both phases' anchors.
        assert!(html.contains("Phase dependencies"), "{html}");
        assert!(
            html.contains("class=\"node-group phase-node status-"),
            "{html}"
        );
        assert!(html.contains("href=\"#phase-p-core\""), "{html}");
        assert!(html.contains("href=\"#phase-p-backend\""), "{html}");

        // (k) per-phase task graphs now render unconditionally (both of
        // kitchen-sink's phases have dependency edges), alongside the
        // phase-level overview: 2 per-phase task graphs + 1 phase graph.
        assert_eq!(html.matches("class=\"plan-graph\"").count(), 3, "{html}");
    }

    #[test]
    fn no_summary_shows_missing_note_and_ready_state() {
        let plan = plan_from("minimal.json");
        let html = render(&plan);

        assert!(plan.meta.summary_md.is_none());
        assert!(plan.open_questions.is_empty());

        assert!(
            html.contains(
                "<p class=\"summary-missing\">No executive summary — the plan author can set meta.summary_md.</p>"
            ),
            "{html}"
        );
        // (checked against the elements the renderer would emit, not a bare
        // substring match — both class names also appear once in the
        // embedded <style> block's rules regardless of whether they're used).
        assert!(!html.contains("<ul class=\"summary-keypoints\">"), "{html}");
        assert!(!html.contains("<p class=\"summary-outofscope\">"), "{html}");
        assert!(html.contains("No blocking questions</span>"), "{html}");
        // A plan with no risks and no questions emits neither of those stat
        // cells — a hollow "0" would imply the author cleared them.
        assert!(!html.contains("Risks</span>"), "{html}");
        assert!(!html.contains("Questions</span>"), "{html}");
        assert_eq!(html.matches("class=\"pv-stat-n\"").count(), 2, "{html}");
        // Nothing has started, so the banner says so rather than narrating
        // progress that does not exist.
        assert!(html.contains("nothing is built yet."), "{html}");
        // No task here carries an estimate or a risk rating, so the ledger is
        // one column: an `Est · risk` header over a stack of empty cells reads
        // as a rendering failure, not as "not stated".
        assert!(html.contains("<table class=\"pv-ledger\">"), "{html}");
        assert!(!html.contains("Est · risk"), "{html}");
        assert!(!html.contains("pv-ledger-fig\">"), "{html}");
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
