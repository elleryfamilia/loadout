//! Deterministic maud renderer: model → single self-contained HTML document.
//!
//! `render` never touches the filesystem, the clock, or any HashMap
//! iteration order — same `Plan` in, byte-identical HTML out. The document
//! embeds its own styles/script (no CDN, no external fetches) and starts
//! with the bare `<!-- loadout:generated context=… -->` marker line (never
//! the full multi-line header, which carries a timestamp).

use maud::{html, Markup, PreEscaped, DOCTYPE};

use crate::plan::icons;
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

/// Markdown rendered for a phrasing-content position (inside `<summary>`):
/// the block renderer's output with paragraph tags stripped, so inline
/// markup (code spans, emphasis, links) survives but no block element lands
/// where HTML forbids one. Multi-paragraph input flattens to one line —
/// acceptable for the 1-2 sentence phase summaries this renders.
fn inline_md(text: &Option<String>) -> Option<Markup> {
    let t = text.as_ref()?;
    let inline = crate::markdown::render_markdown(t)
        .replace("<p>", "")
        .replace("</p>", " ")
        .trim()
        .to_string();
    if inline.is_empty() {
        return None;
    }
    Some(PreEscaped(inline))
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

fn estimate_str(estimate: &Estimate) -> &'static str {
    match estimate {
        Estimate::S => "s",
        Estimate::M => "m",
        Estimate::L => "l",
    }
}

/// Plain-language label for an estimate, e.g. `small` for `Estimate::S`.
/// Everywhere the page shows an estimate to a human — badges, distributions
/// — uses this, not the terse wire-format letter. `estimate_str`'s
/// abbreviation still names the CSS class (`estimate-s`) so existing
/// stylesheets/selectors keep working; only the visible text spells the word
/// out.
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

/// One vendored icon (see `plan::icons`), inlined as `<svg class="pv-icon">`.
/// The vendored file's own 24x24 `width`/`height` attributes are dropped —
/// `plan.css`'s `.pv-icon` rule sizes it instead — everything else (viewBox,
/// stroke, line caps/joins) is copied through unchanged so the glyph reads
/// exactly like upstream Lucide. `aria-hidden` because these sit right next
/// to the text that already says the same thing (a title, a section
/// heading); they're decoration, not information a screen reader needs to
/// announce separately.
///
/// `None` for a name outside the vocabulary. `validate()` (see `plan::model`)
/// rejects that before `render()` ever runs on a real CLI path, so this is a
/// quiet fallback for any other caller, not a panic.
fn icon_markup(name: &str) -> Option<Markup> {
    let inner = strip_svg_wrapper(icons::icon_svg(name)?)?;
    Some(html! {
        svg class="pv-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor"
            stroke-width="2" stroke-linecap="round" stroke-linejoin="round"
            aria-hidden="true" focusable="false" {
            (PreEscaped(inner))
        }
    })
}

/// `icon_markup(name)` followed by a space, or nothing for `None` — shared by
/// phase/task headings (the author-chosen `icon` field) and the handful of
/// section headings with a fixed icon (summary, risks, open questions,
/// phases, phase dependencies).
fn icon_prefix(name: Option<&str>) -> Markup {
    match name.and_then(icon_markup) {
        Some(svg) => html! { (svg) " " },
        None => PreEscaped(String::new()),
    }
}

/// The inner content of a vendored `<svg>…</svg>` document (everything
/// between the opening tag's `>` and the closing tag) — shared by
/// `icon_markup` and `chevron_markup`, both of which drop the vendored
/// file's own wrapper attributes and re-wrap the inner paths in their own
/// `<svg class="…">` with the sizing/styling this renderer wants.
fn strip_svg_wrapper(raw: &str) -> Option<&str> {
    let inner_start = raw.find('>')? + 1;
    let inner_end = raw.rfind("</svg>")?;
    Some(&raw[inner_start..inner_end])
}

/// The disclosure chevron drawn at the start of every `<details>` summary
/// line (phases + the phase-dependency graph) — UI chrome, not part of the
/// author-facing icon vocabulary (see `icons::ui_chevron`'s doc comment).
/// `class="pv-chevron"` is what `plan.css` sizes, colors, and rotates 90°
/// via `details[open] > summary .pv-chevron` — CSS-only, no JS involved.
/// `aria-hidden` for the same reason `icon_markup`'s icons are: native
/// `<details>` already conveys expanded/collapsed state to assistive tech,
/// so this is decoration layered on top, not a second source of truth.
fn chevron_markup() -> Markup {
    let inner =
        strip_svg_wrapper(icons::ui_chevron()).expect("vendored chevron-right.svg is well-formed");
    html! {
        svg class="pv-chevron" viewBox="0 0 24 24" fill="none" stroke="currentColor"
            stroke-width="2" stroke-linecap="round" stroke-linejoin="round"
            aria-hidden="true" focusable="false" {
            (PreEscaped(inner))
        }
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
/// distribution (small/medium/large, only sizes that occur) when any task
/// carries an estimate, then a status distribution when more than one
/// distinct status appears (a single-status plan doesn't need it spelled
/// out).
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
    for (n, label) in sizes.iter().zip(["small", "medium", "large"]) {
        if *n > 0 {
            parts.push(format!("{n} {label}"));
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
                (icon_prefix(task.icon.as_deref()))
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
                        (estimate_label(estimate))
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
                header {
                    // Eyebrow above the title: the metadata a reader wants
                    // placed before the name, not after it.
                    p.meta {
                        "plan " code { (plan.meta.id) }
                        @if let Some(rev) = plan.meta.revision { " · revision " (rev) }
                        @if let Some(agent) = &plan.meta.agent { " · by " (agent) }
                        @if let Some(created) = &plan.meta.created { " · " (created) }
                    }
                    h1 { (plan.meta.title) }
                    (md(&plan.meta.goal_md))
                }
                // A labeled section heading, same convention as Open
                // questions/Risks below: an h2 with a fixed icon, sitting
                // OUTSIDE the card it introduces (those sections' h2s sit
                // outside each individual question/risk card; this one sits
                // outside the single .plan-summary card).
                h2 { (icon_prefix(Some("file-text"))) "Summary" }
                // The `meta:` comment anchor lives on the summary card itself
                // (not the tiny byline above): "comment on the plan as a
                // whole" reads as commenting on the executive summary, and
                // the byline gave the button no visible target worth quoting.
                section.plan-summary data-plan-ref=(format!("meta:{}", plan.meta.id)) {
                    // Two zones side by side on wide viewports (first-dogfood
                    // feedback: the exec prose alone left half the card
                    // empty): the prose on the left, an "at a glance" rail —
                    // counts, per-phase rollup, risk register, the ask — on
                    // the right, where a scanner looks first.
                    div.summary-grid {
                        // (a) The executive summary itself — the top of the
                        // page, so a reader who stops here still gets a
                        // correct high-level picture. Never fabricated:
                        // absent summary_md gets a plain note, not invented
                        // content.
                        div.summary-exec {
                            @if let Some(summary) = &plan.meta.summary_md {
                                (PreEscaped(crate::markdown::render_markdown(summary)))
                            } @else {
                                p.summary-missing {
                                    "No executive summary — the plan author can set meta.summary_md."
                                }
                            }
                        }
                        aside.summary-glance {
                            p.glance-title { "At a glance" }
                            // (e) Whole-plan counts, the per-phase rollup,
                            // the risk register counts (distinct from the
                            // per-task risk heat shown per phase below).
                            p.summary-counts { (summary_counts_line(plan)) }
                            @if !plan.phases.is_empty() {
                                table.summary-phases {
                                    thead {
                                        tr { th { "Phase" } th { "Tasks" } th { "Est." } th { "Risk" } }
                                    }
                                    tbody {
                                        @for phase in &plan.phases {
                                            tr {
                                                td { a href=(format!("#phase-{}", phase.id)) { (short_phase_title(&phase.title)) } }
                                                td { (phase.tasks.len().to_string()) }
                                                td { (phase_estimate_dist(phase)) }
                                                td { (phase_risk_heat(phase)) }
                                            }
                                        }
                                    }
                                }
                            }
                            @if let Some((line, has_high)) = &risk_line {
                                p class=(if *has_high { "summary-risks has-high" } else { "summary-risks" }) {
                                    (line)
                                }
                            }
                            // (d) The ask: whether this plan can move
                            // forward as-is — the rail's bottom line.
                            p class=(if !blocking.is_empty() { "summary-ask has-blocking" } else { "summary-ask" }) {
                                @if !blocking.is_empty() {
                                    (format!(
                                        "⚠ {} blocking question(s) must be resolved before implementation: ",
                                        blocking.len()
                                    ))
                                    @for (i, q) in blocking.iter().enumerate() {
                                        @if i > 0 { ", " }
                                        a href=(format!("#question-{}", q.id)) {
                                            (truncate_chars(&q.question_md, 100))
                                        }
                                    }
                                } @else {
                                    "No blocking questions — plan is ready to review and approve."
                                }
                            }
                        }
                    }
                    // (b) Supporting bullets, one per major workstream or
                    // decision, spanning the card's full width below both
                    // zones.
                    @if !plan.meta.key_points.is_empty() {
                        ul.summary-keypoints {
                            @for kp in &plan.meta.key_points {
                                li { (PreEscaped(crate::markdown::render_markdown(kp))) }
                            }
                        }
                    }
                    // (c) Explicit non-goals, plain text (no markdown).
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
                @if !plan.open_questions.is_empty() {
                    section.questions {
                        h2 { (icon_prefix(Some("search"))) "Open questions" }
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
                        h2 { (icon_prefix(Some("shield"))) "Risks" }
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
                // A "Phases" section heading, same convention as Open
                // questions/Risks above (icon + h2, outside the phase list
                // it introduces). `layout-dashboard` over `git-branch` here:
                // `git-branch` already means something specific on this page
                // (the phase-DEPENDENCY graph below) — reusing it on a plain
                // "here's the list" heading would suggest a relationship
                // that isn't there; `layout-dashboard` reads as a neutral
                // "a list of sections" glyph instead. The expand/collapse
                // control (JS-injected right before the first `details.phase`
                // — see plan.js) lands between this heading and the phase
                // list, not before it.
                @if !plan.phases.is_empty() {
                    h2 { (icon_prefix(Some("layout-dashboard"))) "Phases" }
                }
                @for (i, phase) in plan.phases.iter().enumerate() {
                    details.phase id=(format!("phase-{}", phase.id)) data-plan-ref=(format!("phase:{}", phase.id)) {
                        summary {
                            (chevron_markup())
                            h2 {
                                (icon_prefix(phase.icon.as_deref()))
                                (format!("Phase {} · ", i + 1))
                                (phase.title)
                                " "
                                span.phase-meta { (phase_meta_text(phase)) }
                                // The phase's plain-english description is
                                // part of the collapsed row — a reader
                                // scanning closed phases still learns what
                                // each one is. Inline-rendered (block
                                // structure stripped) because it sits inside
                                // <summary>, which is phrasing content.
                                @if let Some(teaser) = inline_md(&phase.summary_md) {
                                    span.phase-teaser { (teaser) }
                                }
                            }
                        }
                        @if let Some(g) = svg::phase_svg(plan, &phase.id) { (PreEscaped(g)) }
                        @for task in &phase.tasks { (task_card(task)) }
                    }
                }
                @if let Some(g) = &phase_graph {
                    details.graph {
                        summary {
                            (chevron_markup())
                            (icon_prefix(Some("git-branch"))) "Phase dependencies"
                        }
                        (PreEscaped(g.as_str()))
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
        // And the font did actually land: two @font-face blocks (400 + 600).
        assert_eq!(CSS.matches("@font-face").count(), 2);
        assert_eq!(CSS.matches("url(\"data:font/woff2;base64,").count(), 2);
    }

    #[test]
    fn top_of_page_structure() {
        let html = render(&plan_from("kitchen-sink.json"));
        // Eyebrow (byline + created) renders above the h1.
        let meta_pos = html.find("<p class=\"meta\">").expect("byline eyebrow");
        let h1_pos = html.find("<h1>").expect("title");
        assert!(meta_pos < h1_pos, "eyebrow above the title");
        assert!(html.contains(" · 2026-07-07"), "created date in eyebrow");
        // Exec prose and the at-a-glance rail share the summary grid; the
        // ask lives at the rail's bottom; key points span below the grid.
        // (Tag-anchored substrings — bare class names also appear in the
        // embedded stylesheet.)
        let grid_pos = html.find("<div class=\"summary-grid\">").expect("grid");
        let glance_pos = html
            .find("<aside class=\"summary-glance\">")
            .expect("glance rail");
        let ask_pos = html.find("<p class=\"summary-ask").expect("ask");
        let keypoints_pos = html
            .find("<ul class=\"summary-keypoints\">")
            .expect("keypoints");
        assert!(grid_pos < glance_pos, "rail inside the grid");
        assert!(glance_pos < ask_pos, "ask inside the rail");
        assert!(ask_pos < keypoints_pos, "key points after the grid");
        assert!(html.contains("At a glance"));
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
    fn inline_md_flattens_paragraphs_and_keeps_inline_markup() {
        let got = inline_md(&Some("Extract *session* handling.\n\nSecond.".into()))
            .expect("some")
            .into_string();
        assert!(got.contains("<em>session</em>"), "{got}");
        assert!(!got.contains("<p>"), "{got}");
        assert!(got.contains("Second."), "{got}");
        assert!(inline_md(&None).is_none());
        assert!(inline_md(&Some(String::new())).is_none());
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
        assert!(html.contains("5 tasks"), "{html}");
        assert!(html.contains("2 phases"), "{html}");

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

        // (e) ask banner: has-blocking, with the q-ttl link.
        let ask_pos = html
            .find("<p class=\"summary-ask has-blocking\">")
            .expect("summary-ask has-blocking");
        assert!(
            html.contains("⚠ 1 blocking question(s) must be resolved before implementation"),
            "{html}"
        );
        assert!(html.contains("href=\"#question-q-ttl\""), "{html}");

        // (f) phase rollup table: link to #phase-p-core, matching anchor id
        // on the phase's own details element.
        assert!(html.contains("<table class=\"summary-phases\">"), "{html}");
        assert!(html.contains("href=\"#phase-p-core\""), "{html}");
        assert!(html.contains("id=\"phase-p-core\""), "{html}");

        // (g) order by byte position: summary block pieces in document
        // order, "Summary" heading < summary card < open questions < risks <
        // "Phases" heading < first phase details < graph details.
        //
        // Each section heading now carries a fixed icon before its text
        // (see `icon_prefix`), so none of these are anchored on the `<h2>`
        // opening tag directly abutting the word.
        let summary_heading_pos = html.find("Summary</h2>").expect("summary heading");
        let open_q_pos = html.find("Open questions").expect("open questions heading");
        let risks_pos = html.find("Risks</h2>").expect("risks heading");
        let phases_heading_pos = html.find("Phases</h2>").expect("phases heading");
        let phase_pos = html
            .find("<details class=\"phase\"")
            .expect("phase details");
        let graph_pos = html
            .find("<details class=\"graph\"")
            .expect("graph details");
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

        // (g2) each phase's `<details>` summary line carries a 1-based,
        // document-order ordinal prefix ahead of its title — owner-requested
        // affordance so a reader always knows which phase they're looking
        // at, even scrolled deep into a long plan.
        assert!(html.contains("Phase 1 · Core"), "{html}");
        assert!(html.contains("Phase 2 · Backend"), "{html}");

        // (h) phases (and the graph) are collapsed by default.
        assert!(!html.contains("<details class=\"phase\" open"), "{html}");
        assert!(!html.contains("<details class=\"graph\" open"), "{html}");

        // (i) the blocking link's target anchor exists.
        assert!(html.contains("id=\"question-q-ttl\""), "{html}");

        // (j) the bottom graph is now phase-level, not the old whole-plan
        // task graph: a "Phase dependencies" heading, phase nodes linking to
        // both phases' anchors.
        assert!(html.contains("Phase dependencies"), "{html}");
        assert!(html.contains("class=\"node phase-node\""), "{html}");
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
        assert!(
            html.contains(
                "<p class=\"summary-ask\">No blocking questions — plan is ready to review and approve.</p>"
            ),
            "{html}"
        );
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
