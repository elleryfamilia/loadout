//! `maud` server-rendered HTML: the page shell and the htmx-swappable fragments
//! (library control surface, edit forms, diff/review, overlay preview). No
//! client framework — a tiny embedded JS shim drives fragment swaps from `hx-*`
//! attributes (and re-binds swapped-in content).
//!
//! Pane convention: `#library` (left) is the control surface (staged bar +
//! New/profiles/capabilities lists), `#center` is the work area (welcome,
//! forms, diff), `#overlay-pane` (right) is the live preview. Mutations target
//! `#center` and return a result fragment whose `hx-trigger="load"` refresher
//! re-pulls the preview; the preview fragment in turn re-pulls `#library`, so a
//! single cascade keeps the sim-dependent library marks truthful.

use std::path::Path;

use maud::{html, Markup, PreEscaped, DOCTYPE};

use crate::capability::{Capability, Layer, Risk};
use crate::context::Scope;
use crate::profile::ProfileConfig;
use crate::studio::edit::FileDiff;
use crate::studio::state::{
    AtomDot, AtomState, BindingState, CapView, LibraryView, OnboardingStage, PreviewOutcome,
    ProfileView, Simulated,
};

/// Coarse language/platform options offered in the simulator and as profile targets.
const LANGS: &[&str] = &["rust", "node", "nextjs", "go", "python", "android", "java"];
const TARGETS: &[&str] = &[
    "rust", "node", "nextjs", "go", "python", "android", "java", "machine",
];

// --- icons -------------------------------------------------------------------

/// A 16px feather-style inline SVG icon (1.5px stroke, `currentColor`). The name
/// is matched against a closed set of **static** strings — never interpolate a
/// dynamic value into `PreEscaped` (that would bypass escaping).
fn icon(name: &str) -> Markup {
    let body: &str = match name {
        "plus" => r#"<path d="M12 5v14M5 12h14"/>"#,
        "copy" => {
            r#"<rect x="9" y="9" width="12" height="12" rx="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/>"#
        }
        "pencil" => {
            r#"<path d="M12 20h9"/><path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z"/>"#
        }
        "trash" => {
            r#"<path d="M3 6h18"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>"#
        }
        "arrow-right" => r#"<path d="M5 12h14"/><path d="m12 5 7 7-7 7"/>"#,
        "layers" => {
            r#"<path d="M12 2 2 7l10 5 10-5-10-5Z"/><path d="m2 17 10 5 10-5"/><path d="m2 12 10 5 10-5"/>"#
        }
        "box" => {
            r#"<path d="M21 8v8a2 2 0 0 1-1 1.73l-7 4a2 2 0 0 1-2 0l-7-4A2 2 0 0 1 3 16V8a2 2 0 0 1 1-1.73l7-4a2 2 0 0 1 2 0l7 4A2 2 0 0 1 21 8Z"/><path d="m3.3 7 8.7 5 8.7-5"/><path d="M12 22V12"/>"#
        }
        "bolt" => r#"<path d="M13 2 3 14h9l-1 8 10-12h-9l1-8Z"/>"#,
        "eye" => {
            r#"<path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12Z"/><circle cx="12" cy="12" r="3"/>"#
        }
        "refresh" => {
            r#"<path d="M21 2v6h-6"/><path d="M3 12a9 9 0 0 1 15-6.7L21 8"/><path d="M3 22v-6h6"/><path d="M21 12a9 9 0 0 1-15 6.7L3 16"/>"#
        }
        "target" => {
            r#"<circle cx="12" cy="12" r="9"/><circle cx="12" cy="12" r="5"/><circle cx="12" cy="12" r="1.5"/>"#
        }
        "shield" => {
            r#"<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10Z"/><path d="M12 8v4"/><path d="M12 16h.01"/>"#
        }
        "alert" => {
            r#"<path d="M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0Z"/><path d="M12 9v4"/><path d="M12 17h.01"/>"#
        }
        "check" => r#"<path d="M20 6 9 17l-5-5"/>"#,
        "sliders" => {
            r#"<path d="M4 21v-7M4 10V3M12 21v-9M12 8V3M20 21v-5M20 12V3M1 14h6M9 8h6M17 16h6"/>"#
        }
        // Unknown name renders nothing rather than panicking.
        _ => "",
    };
    PreEscaped(format!(
        r#"<svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">{body}</svg>"#
    ))
}

fn risk_class(r: Risk) -> &'static str {
    match r {
        Risk::Info => "risk-info",
        Risk::Caution => "risk-caution",
        Risk::Dangerous => "risk-dangerous",
    }
}

// --- page shell --------------------------------------------------------------

/// The full page: top-bar context simulator, left control surface, center work
/// area, right live overlay preview.
pub fn shell(
    lib: &LibraryView,
    staged: usize,
    sim: &Simulated,
    agents: &[String],
    preview: &PreviewOutcome,
    stage: OnboardingStage,
) -> String {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "rosita studio" }
                link rel="stylesheet" href="/assets/studio.css";
                script src="/assets/studio.js" defer {}
            }
            body {
                header class="topbar" {
                    div class="brand" {
                        span class="brand-mark" { (icon("layers")) }
                        span class="brand-name" { "rosita studio" }
                    }
                    (simulator_bar(sim, agents))
                }
                main class="layout" {
                    nav class="pane nav" id="library" { (library(lib, staged, None)) }
                    section class="pane center" id="center" { (welcome_card(stage)) }
                    aside class="pane preview" id="overlay-pane" { (preview_pane(preview)) }
                }
            }
        }
    }
    .into_string()
}

fn simulator_bar(sim: &Simulated, agents: &[String]) -> Markup {
    html! {
        form class="simulator" hx-post="/preview" hx-target="#overlay-pane" hx-trigger="change" {
            span class="sim-label" title="Pretend you're working in this context — the overlay and bindings below update to match." {
                (icon("target")) "Simulating context"
            }
            label class="sim-field" { span { "lang" }
                select name="lang" {
                    option value="" selected[sim.lang.is_none()] { "auto-detect" }
                    @for &l in LANGS {
                        option value=(l) selected[sim.lang.as_deref() == Some(l)] { (l) }
                    }
                }
            }
            label class="sim-field" { span { "scope" }
                select name="scope" {
                    option value="" selected[sim.scope.is_none()] { "auto-detect" }
                    option value="repo" selected[matches!(sim.scope, Some(Scope::Repo))] { "repo" }
                    option value="machine" selected[matches!(sim.scope, Some(Scope::Machine))] { "machine" }
                }
            }
            label class="sim-field" { span { "agent" }
                select name="agent" {
                    @for a in agents {
                        option value=(a.as_str()) selected[&sim.agent == a] { (a.as_str()) }
                    }
                }
            }
        }
    }
}

// --- center: welcome / placeholder -------------------------------------------

/// The center work area on initial load — a stage-aware orientation card that
/// names the single next action. Once the user opens a form or the diff, this
/// pane is swapped away.
fn welcome_card(stage: OnboardingStage) -> Markup {
    html! {
        div class="welcome" {
            @match stage {
                OnboardingStage::Empty => {
                    h2 { "Build the AGENTS.md your agent reads." }
                    p class="lead" { "rosita composes guidance in three steps — and you keep the files." }
                    (pipeline_steps())
                    p class="hint" { "Nothing here yet. Start by adding a capability — duplicate a ready-made one from the palette in the left rail, or write your own." }
                    div class="cta-row" {
                        button class="btn btn-primary" hx-get="/capabilities/new" hx-target="#center" { (icon("plus")) "Write a capability" }
                        button class="btn btn-ghost" hx-get="/profiles/new" hx-target="#center" { (icon("plus")) "New profile" }
                    }
                }
                OnboardingStage::HasCaps => {
                    h2 { "Now bundle your capabilities into a profile." }
                    p class="lead" { "A profile groups capabilities and binds them to a kind of repo (its targets). That's what produces the overlay." }
                    (pipeline_steps())
                    div class="cta-row" {
                        button class="btn btn-primary" hx-get="/profiles/new" hx-target="#center" { (icon("plus")) "Create your first profile" }
                    }
                }
                OnboardingStage::HasProfile => {
                    h2 { "No profile binds to this context yet." }
                    p class="lead" { "Your profiles target specific stacks. Change the context simulator up top to match a profile's targets, or edit a profile's targets to include this context." }
                    (pipeline_steps())
                    p class="hint" { "Pick a profile or capability on the left to edit it." }
                }
                OnboardingStage::Bound => {
                    h2 { "Pick something on the left to edit." }
                    p class="lead" { "Edit a capability or profile, or create one. Stage changes, review the exact TOML diff, then apply. The overlay on the right updates as you stage." }
                    (pipeline_steps())
                }
            }
        }
    }
}

/// The compact five-stage pipeline strip used inside the welcome card.
fn pipeline_steps() -> Markup {
    html! {
        ol class="pipeline" {
            li { span class="step-n" { "1" } "Capabilities" span class="step-sub" { "reusable atoms" } }
            li { span class="step-n" { "2" } "Profile" span class="step-sub" { "a targeted bundle" } }
            li { span class="step-n" { "3" } "Overlay" span class="step-sub" { "what the agent sees" } }
        }
    }
}

/// The center work area's generic idle placeholder (used by `GET /welcome`,
/// i.e. Discard/Cancel — no snapshot in hand to pick a stage).
pub fn center_placeholder() -> Markup {
    html! {
        div class="welcome" {
            h2 { "Pick something on the left to edit." }
            p class="lead" { "Edit a capability or profile, or create one. Stage changes, review the exact TOML diff, then apply. The overlay on the right updates as you stage." }
            (pipeline_steps())
        }
    }
}

/// `GET /welcome` — reset the center pane to its placeholder.
pub fn center_placeholder_fragment() -> String {
    center_placeholder().into_string()
}

// --- left control surface ----------------------------------------------------

/// The left pane: the staged-changes bar, New buttons, and the library lists
/// (profiles first as the composition unit, then your capabilities, then the
/// read-only palette). `flash` shows a transient note.
pub fn library(lib: &LibraryView, staged: usize, flash: Option<&str>) -> Markup {
    html! {
        div class="library" {
            @if let Some(msg) = flash { p class="flash" { (msg) } }
            (staged_bar(staged))
            div class="actions" {
                button class="btn btn-ghost btn-sm" hx-get="/capabilities/new" hx-target="#center" { (icon("plus")) "Capability" }
                button class="btn btn-ghost btn-sm" hx-get="/profiles/new" hx-target="#center" { (icon("plus")) "Profile" }
            }

            div class="section" {
                h2 class="section-title" { (icon("layers")) "Profiles" span class="count" { (lib.profiles.len()) } }
                @if lib.profiles.is_empty() {
                    p class="empty" { "No profile yet. A profile bundles capabilities and binds them to a kind of repo." }
                } @else {
                    @for p in &lib.profiles { (profile_card(p)) }
                }
            }

            div class="section" {
                h2 class="section-title" { (icon("box")) "Capabilities" span class="count" { (lib.yours.len()) } }
                @if lib.yours.is_empty() {
                    p class="empty" { "None yet — duplicate a starter from the palette below, or write your own." }
                } @else {
                    @for c in &lib.yours { (cap_row(c, true)) }
                }
            }

            @if !lib.palette.is_empty() {
                details class="palette" {
                    summary { (icon("box")) "Starter palette" span class="count" { (lib.palette.len()) } }
                    p class="palette-hint" { "Read-only templates. Duplicate one to own and edit it." }
                    @for c in &lib.palette { (cap_row(c, false)) }
                }
            }
        }
    }
}

fn staged_bar(staged: usize) -> Markup {
    html! {
        @if staged > 0 {
            div class="staged-bar active" {
                span class="staged-count" { (icon("layers")) (staged) " staged" }
                div class="staged-actions" {
                    button class="btn btn-ghost btn-sm" hx-get="/diff" hx-target="#center" { "Review" }
                    button class="btn btn-primary btn-sm"
                        hx-post="/apply" hx-target="#center"
                        hx-confirm="Apply staged changes to your config files?" { (icon("check")) "Apply" }
                }
            }
        } @else {
            div class="staged-bar" {
                span class="muted" { "No staged changes" }
            }
        }
    }
}

/// `GET /library` fragment.
pub fn library_fragment(lib: &LibraryView, staged: usize) -> String {
    library(lib, staged, None).into_string()
}

fn cap_row(c: &CapView, owned: bool) -> Markup {
    let id = c.id.as_str();
    let e = enc(id);
    let row_class = format!(
        "cap-row {} {}",
        risk_class(c.risk),
        if c.active { "is-active" } else { "" }
    );
    html! {
        div class=(row_class) {
            span class="cap-mark" title=(if c.active { "in the current overlay" } else { "not in the current overlay" }) {}
            div class="cap-text" {
                div class="cap-line" {
                    span class="cap-id" { (id) }
                    @if c.kind != "static" {
                        span class="badge" title=(format!("{} capability", c.kind)) { (icon("bolt")) (c.kind) }
                    }
                }
                span class="cap-title" { (c.title) }
            }
            span class="row-actions" {
                @if owned {
                    button class="icon-btn" title="Edit" aria-label=(format!("Edit {id}"))
                        hx-get=(format!("/capabilities/{e}/edit")) hx-target="#center" { (icon("pencil")) }
                    button class="icon-btn danger" title="Stage deletion" aria-label=(format!("Delete {id}"))
                        hx-delete=(format!("/capabilities/{e}")) hx-target="#center"
                        hx-confirm=(format!("Stage deletion of capability \"{id}\"?")) { (icon("trash")) }
                } @else {
                    button class="icon-btn" title="Duplicate into your config to own it" aria-label=(format!("Duplicate {id}"))
                        hx-post=(format!("/capabilities/{e}/duplicate")) hx-target="#center" { (icon("copy")) }
                }
            }
        }
    }
}

fn profile_card(p: &ProfileView) -> Markup {
    let name = p.name.as_str();
    let e = enc(name);
    let state = if p.selected {
        "bound"
    } else if p.candidate {
        "candidate"
    } else {
        ""
    };
    let card_class = format!("profile-card {state}");
    html! {
        div class=(card_class) {
            div class="profile-head" {
                span class="profile-name" {
                    @if p.selected { span class="bound-mark" title="bound to the current context" { (icon("arrow-right")) } }
                    (name)
                }
                span class="row-actions" {
                    button class="icon-btn" title="Edit" aria-label=(format!("Edit profile {name}"))
                        hx-get=(format!("/profiles/{e}/edit")) hx-target="#center" { (icon("pencil")) }
                    button class="icon-btn danger" title="Stage deletion" aria-label=(format!("Delete profile {name}"))
                        hx-delete=(format!("/profiles/{e}")) hx-target="#center"
                        hx-confirm=(format!("Stage deletion of profile \"{name}\"?")) { (icon("trash")) }
                }
            }
            @if p.targets.is_empty() {
                div class="targets" { span class="target-chip muted" { "no targets" } }
            } @else {
                div class="targets" {
                    @for t in &p.targets { span class="target-chip" { (t) } }
                }
            }
            @if !p.atoms.is_empty() {
                div class="atoms" title="composed capabilities" {
                    @for a in &p.atoms { (atom_dot(a)) }
                }
            }
        }
    }
}

fn atom_dot(a: &AtomDot) -> Markup {
    let (cls, tip) = match a.state {
        AtomState::Owned => (
            format!("atom owned {}", risk_class(a.risk)),
            format!("{} — composed", a.id),
        ),
        AtomState::Palette => (
            "atom palette".to_string(),
            format!(
                "{} — in the palette, not duplicated (contributes nothing)",
                a.id
            ),
        ),
        AtomState::Unknown => (
            "atom unknown".to_string(),
            format!("{} — unknown capability (contributes nothing)", a.id),
        ),
    };
    html! { span class=(cls) title=(tip) {} }
}

// --- editor forms ------------------------------------------------------------

fn layer_scope(layer: Layer) -> (&'static str, bool) {
    match layer {
        Layer::Global => ("global", false),
        Layer::GlobalLocal => ("global", true),
        Layer::RepoLocal => ("repo", true),
        _ => ("repo", false),
    }
}

fn lives_in(layer: Layer) -> Markup {
    let (scope, private) = layer_scope(layer);
    html! {
        fieldset class="lives-in" {
            legend { "Lives in" }
            div class="radio-row" {
                label class="radio" { input type="radio" name="scope" value="repo" checked[scope == "repo"]; span { "repo" } }
                label class="radio" { input type="radio" name="scope" value="global" checked[scope == "global"]; span { "global" } }
            }
            div class="radio-row" {
                label class="radio" { input type="radio" name="visibility" value="public" checked[!private]; span { "public" } }
                label class="radio" { input type="radio" name="visibility" value="private" checked[private]; span { "private (local.toml)" } }
            }
        }
    }
}

/// The capability editor. `cap` populates an edit; `None` is a new capability.
/// A palette item (`owned == false`) renders read-only with a duplicate action.
pub fn capability_form(cap: Option<&Capability>, layer: Layer, owned: bool) -> String {
    let is_new = cap.is_none();
    let id = cap.map(|c| c.id.as_str()).unwrap_or("");
    let read_only = !is_new && !owned;
    html! {
        @if read_only {
            div class="form" {
                div class="form-head" {
                    h3 { "Palette capability" }
                    span class="pill" { "read-only" }
                }
                p class="hint" { "Palette items are templates. Duplicate “" (id) "” into your config to own and edit it." }
                button class="btn btn-primary" hx-post=(format!("/capabilities/{}/duplicate", enc(id))) hx-target="#center" { (icon("copy")) "Duplicate into my config" }
            }
        } @else {
            form class="form" hx-post="/capabilities" hx-target="#center" {
                div class="form-head" {
                    h3 { (if is_new { "New capability" } else { "Edit capability" }) }
                    span class="pill" { "one reusable atom of guidance" }
                }
                label class="field" { span class="field-label" { "id" }
                    @if is_new {
                        input type="text" name="id" value="" placeholder="rust-conventions" required;
                    } @else {
                        input type="text" name="id" value=(id) readonly;
                    }
                }
                label class="field" { span class="field-label" { "description" } input type="text" name="description" value=(cap.and_then(|c| c.description.as_deref()).unwrap_or("")) placeholder="Rust conventions"; }
                label class="field" { span class="field-label" { "tags" span class="field-hint" { "comma-separated" } } input type="text" name="tags" value=(cap.map(|c| c.tags.join(", ")).unwrap_or_default()) placeholder="stack, safety"; }
                fieldset class="risk" {
                    legend { "Risk" }
                    @let risk = cap.map(|c| c.risk).unwrap_or(Risk::Info);
                    div class="radio-row" {
                        label class="radio risk-info" { input type="radio" name="risk" value="info" checked[risk == Risk::Info]; span { "info" } }
                        label class="radio risk-caution" { input type="radio" name="risk" value="caution" checked[risk == Risk::Caution]; span { "caution" } }
                        label class="radio risk-dangerous" { input type="radio" name="risk" value="dangerous" checked[risk == Risk::Dangerous]; span { "dangerous" } }
                    }
                }
                label class="field" { span class="field-label" { "provider" span class="field-hint" { "dynamic; built-in probe" } } input type="text" name="provider" value=(cap.and_then(|c| c.provider.as_deref()).unwrap_or("")) placeholder="host | docker | …"; }
                label class="field" { span class="field-label" { "command" span class="field-hint" { "dynamic; trust-gated in a repo" } } input type="text" name="command" value=(cap.and_then(|c| c.command.as_deref()).unwrap_or("")); }
                label class="field" { span class="field-label" { "cache TTL" } input type="text" name="cache" value=(cap.and_then(|c| c.cache.as_deref()).unwrap_or("")) placeholder="60s"; }
                label class="field" { span class="field-label" { "requires" span class="field-hint" { "comma-separated capability ids" } } input type="text" name="requires" value=(cap.map(|c| c.requires.join(", ")).unwrap_or_default()); }
                label class="field" { span class="field-label" { "agents" span class="field-hint" { "comma-separated; empty = all" } } input type="text" name="agents" value=(cap.map(|c| c.agents.join(", ")).unwrap_or_default()); }
                label class="field" { span class="field-label" { "guidance" span class="field-hint" { "markdown / minijinja" } }
                    textarea name="guidance" rows="7" { (cap.map(|c| c.guidance.as_str()).unwrap_or("")) }
                }
                (lives_in(layer))
                div class="form-buttons" {
                    button type="button" class="btn btn-ghost" hx-get="/welcome" hx-target="#center" { "Discard" }
                    button type="submit" class="btn btn-primary" { (icon("check")) "Stage change" }
                }
            }
        }
    }
    .into_string()
}

/// The profile composer. `profile` populates an edit; `None` is a new profile.
/// `available` is every capability id you can compose (yours + palette).
pub fn profile_form(profile: Option<&ProfileConfig>, available: &[String]) -> String {
    let is_new = profile.is_none();
    let name = profile.map(|p| p.name.as_str()).unwrap_or("");
    let targets: Vec<&str> = profile
        .map(|p| p.targets.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let chosen: Vec<&str> = profile
        .map(|p| p.capabilities.iter().map(|r| r.id()).collect())
        .unwrap_or_default();
    html! {
        form class="form" hx-post="/profiles" hx-target="#center" {
            div class="form-head" {
                h3 { (if is_new { "New profile" } else { "Edit profile" }) }
                span class="pill" { "a targeted bundle of capabilities" }
            }
            label class="field" { span class="field-label" { "name" }
                @if is_new {
                    input type="text" name="name" value="" placeholder="rust — browser" required;
                } @else {
                    input type="text" name="name" value=(name) readonly;
                }
            }
            fieldset class="targets-picker" {
                legend { "Targets" span class="field-hint" { "applies when the repo looks like one of these" } }
                div class="checks" {
                    @for &t in TARGETS {
                        label class="check" { input type="checkbox" name="targets" value=(t) checked[targets.contains(&t)]; span { (t) } }
                    }
                }
            }
            fieldset class="cap-picker" {
                legend { "Capabilities" span class="field-hint" { "need ≥1 to save" } }
                @if available.is_empty() { p class="empty" { "No capabilities yet — create one first." } }
                div class="checks" {
                    @for id in available {
                        label class="check" { input type="checkbox" name="capabilities" value=(id.as_str()) checked[chosen.contains(&id.as_str())]; span { (id) } }
                    }
                }
            }
            label class="field" { span class="field-label" { "inline guidance" span class="field-hint" { "optional" } }
                textarea name="guidance" rows="3" { (profile.and_then(|p| p.guidance.as_deref()).unwrap_or("")) }
            }
            fieldset class="lives-in" {
                legend { "Lives in" }
                div class="radio-row" {
                    label class="radio" { input type="radio" name="scope" value="repo" checked; span { "repo" } }
                    label class="radio" { input type="radio" name="scope" value="global"; span { "global" } }
                }
            }
            div class="form-buttons" {
                button type="button" class="btn btn-ghost" hx-get="/welcome" hx-target="#center" { "Discard" }
                button type="submit" class="btn btn-primary" { (icon("check")) "Stage change" }
            }
        }
    }
    .into_string()
}

// --- diff / review -----------------------------------------------------------

/// Trust state to surface in the review when repo-authored `command` caps exist.
pub struct TrustBanner {
    /// Repo-layer command-backed capability ids (need trust to run).
    pub command_caps: Vec<String>,
    /// `trusted` / `stale (…)` / `untrusted`.
    pub status: String,
    pub trusted: bool,
}

/// The review pane: per-file diff vs raw bytes, the leak warning, the
/// external-edit banner, and the trust banner, plus Apply.
pub fn diff_view(
    diffs: &[FileDiff],
    leaks: &[String],
    fs_changed: &[std::path::PathBuf],
    trust: &TrustBanner,
    staged: usize,
) -> String {
    html! {
        div class="review" {
            div class="form-head" {
                h3 { "Review staged changes" }
                span class="pill" { (staged) " staged" }
            }

            @if !trust.command_caps.is_empty() {
                div class="banner warn" {
                    span class="banner-icon" { (icon("shield")) }
                    div class="banner-body" {
                        p { "Repo command capabilities (" (trust.command_caps.join(", ")) ") won't run until you trust this repo — currently "
                            span class="trust-status" { (trust.status) } "." }
                        @if !trust.trusted {
                            button class="btn btn-primary btn-sm" hx-post="/trust/allow" hx-target="#center"
                                hx-confirm="Trust this repo to run its command-backed capabilities?" { "Allow this repo" }
                        } @else {
                            button class="btn btn-danger btn-sm" hx-post="/trust/deny" hx-target="#center" { "Revoke trust" }
                        }
                        p class="muted" { "An apply changes the repo config bundle, which re-locks trust — re-allow afterward." }
                    }
                }
            }

            @if !leaks.is_empty() {
                div class="banner warn" {
                    span class="banner-icon" { (icon("alert")) }
                    div class="banner-body" {
                        p { "Leak check: these public values look machine-specific — consider moving to local.toml:" }
                        p class="mono" { (leaks.join(", ")) }
                    }
                }
            } @else {
                p class="ok-line" { (icon("check")) "Leak check: clean." }
            }

            @if !fs_changed.is_empty() {
                div class="banner warn" {
                    span class="banner-icon" { (icon("alert")) }
                    div class="banner-body" {
                        p { "Config changed on disk since load ("
                            (fs_changed.iter().map(|p| display_name(p)).collect::<Vec<_>>().join(", "))
                            ") — applying will overwrite it." }
                    }
                }
            }

            @if diffs.is_empty() {
                p class="empty" { "No staged changes." }
            } @else {
                @for d in diffs { (file_diff(d)) }
                div class="form-buttons" {
                    button type="button" class="btn btn-ghost" hx-get="/welcome" hx-target="#center" { "Cancel" }
                    button class="btn btn-primary" hx-post="/apply" hx-target="#center"
                        hx-confirm="Apply staged changes to your config files?" { (icon("check")) "Apply " (staged) " change(s)" }
                }
            }
        }
    }
    .into_string()
}

fn file_diff(d: &FileDiff) -> Markup {
    let (scope, private) = layer_scope(d.layer);
    let vis = if private { "private" } else { "public" };
    html! {
        div class="file-diff" {
            div class="file-head" {
                span class="file-path" { (display_name(&d.path)) }
                span class="file-meta" { (scope) " · " (vis) }
            }
            @if d.reformats_untouched {
                p class="hint small" { "rosita will also reformat some untouched lines it parsed." }
            }
            pre class="diff-body" { (d.unified) }
        }
    }
}

// --- preview -----------------------------------------------------------------

pub fn preview_pane(p: &PreviewOutcome) -> Markup {
    html! {
        div class="overlay" {
            div class="overlay-head" {
                span class="overlay-title" { (icon("eye")) "Live overlay · " span class="agent" { (p.agent) } }
                (binding_chip(&p.binding))
            }
            (provenance(p))
            @if let Some(note) = &p.note { p class="note" { (note) } }
            pre class="overlay-body" { (p.overlay) }
            p class="updates" { (icon("refresh")) "Reflects staged state — updates when you stage or change the context (ReadOnly: probes not executed)." }
        }
    }
}

fn binding_chip(b: &BindingState) -> Markup {
    match b {
        BindingState::Bound(name) => html! {
            span class="chip chip-bound" title="one profile binds to this context" {
                (icon("arrow-right")) span class="chip-name" { "profile " (name) }
            }
        },
        BindingState::None => html! {
            span class="chip chip-none" title="no profile applies to this context" {
                "profile none"
            }
        },
        BindingState::Ambiguous(n) => html! {
            span class="chip chip-ambiguous" title="multiple profiles match; bind one with `rosita run`" {
                (icon("alert")) (n) " profiles match"
            }
        },
    }
}

fn provenance(p: &PreviewOutcome) -> Markup {
    let profile = match &p.binding {
        BindingState::Bound(name) => name.clone(),
        BindingState::None => "none".to_string(),
        BindingState::Ambiguous(_) => "ambiguous".to_string(),
    };
    html! {
        div class="provenance" {
            span class="prov-node" { (p.context_summary.as_str()) }
            span class="prov-arrow" { (icon("arrow-right")) }
            span class="prov-node" { (profile) }
            span class="prov-arrow" { (icon("arrow-right")) }
            span class="prov-node" { (p.cap_count) " " (if p.cap_count == 1 { "capability" } else { "capabilities" }) }
        }
    }
}

/// `POST /preview` fragment — the live overlay, plus a one-shot loader that
/// re-pulls `#library` so the sim-dependent binding/active marks stay truthful.
/// (This cascade is also what refreshes the library after a mutation.)
pub fn preview_fragment(p: &PreviewOutcome) -> String {
    html! {
        (preview_pane(p))
        div hx-get="/library" hx-trigger="load" hx-target="#library" {}
    }
    .into_string()
}

// --- small result / error fragments ------------------------------------------

/// A mutation result swapped into `#center`: a note plus a one-shot loader that
/// refreshes the live preview — which in turn re-pulls `#library` (the cascade
/// in `preview_fragment`), so we don't refresh the library twice.
pub fn action_result(msg: &str) -> String {
    html! {
        div class="result" {
            p class="ok-line" { (icon("check")) (msg) }
            div hx-post="/preview" hx-trigger="load" hx-target="#overlay-pane" {}
        }
    }
    .into_string()
}

/// An inline error fragment (validation / minijinja / config errors never 500).
pub fn error_fragment(msg: &str) -> String {
    html! { div class="banner error" { span class="banner-icon" { (icon("alert")) } div class="banner-body" { (msg) } } }
        .into_string()
}

/// A minimal full-page error (when the shell itself can't be assembled).
pub fn error_page(msg: &str) -> String {
    html! {
        (DOCTYPE)
        html { head { title { "rosita studio — error" } } body { pre class="error" { (msg) } } }
    }
    .into_string()
}

/// `GET /fs-status` fragment — the light external-edit poll banner.
pub fn fs_status_fragment(changed: &[std::path::PathBuf]) -> String {
    if changed.is_empty() {
        return html! { span class="fs-clean" { "on-disk unchanged since load" } }.into_string();
    }
    let names: Vec<String> = changed.iter().map(|p| display_name(p)).collect();
    html! {
        div class="banner warn" {
            span class="banner-icon" { (icon("alert")) }
            div class="banner-body" { "Config changed on disk: " (names.join(", ")) " — reload before applying." }
        }
    }
    .into_string()
}

fn display_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// Percent-encode a path segment (profile names can contain spaces / em-dashes).
/// The server decodes with `state::percent_decode`; we never emit a bare `+`.
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
