//! `maud` server-rendered HTML for rosita studio: a tabbed shell (Profiles /
//! Capabilities), the profile dashboard + live preview pane, the capability
//! list + modal dialog, the full-width profile editor, and the diff/review.
//!
//! Targets the tiny embedded htmx-shim. Swap targets:
//! - `#main` — the active tab's content (dashboard, caps list, editor, diff).
//! - `#preview` — the rendered-markdown preview inside the Profiles tab.
//! - `#modal` — the capability dialog (CSS shows it when non-empty; `/close`
//!   swaps it empty).
//! - `#staged` — the top-bar staged-changes indicator (mutations re-pull it).

use std::path::Path;

use maud::{html, Markup, PreEscaped, DOCTYPE};
use pulldown_cmark::{html as md_html, Event, Options, Parser};

use crate::capability::{Capability, Layer, Risk};
use crate::profile::ProfileConfig;
use crate::studio::edit::FileDiff;
use crate::studio::state::{
    AtomDot, AtomState, BindingState, CapView, LibraryView, PreviewOutcome, ProfileView,
};

/// Language/platform targets a profile can declare.
const TARGETS: &[&str] = &[
    "rust", "node", "nextjs", "go", "python", "android", "java", "machine",
];

/// Script interpreters offered in the capability dialog.
const SCRIPT_LANGS: &[(&str, &str)] = &[("bash", "Bash"), ("python", "Python"), ("sh", "POSIX sh")];

/// The curated icon set a capability can pick from.
const CAP_ICONS: &[&str] = &[
    "box",
    "bolt",
    "terminal",
    "code",
    "git-branch",
    "database",
    "server",
    "cloud",
    "package",
    "wrench",
    "flask",
    "rocket",
    "book",
    "file",
    "folder",
    "gear",
    "globe",
    "cpu",
    "lock",
    "shield",
];

// --- icons -------------------------------------------------------------------

/// A 16px feather-style inline SVG icon (1.5px stroke, `currentColor`). Matched
/// against a closed set of **static** strings — never interpolate a dynamic
/// value into `PreEscaped` (that would bypass escaping).
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
        "eye" => {
            r#"<path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12Z"/><circle cx="12" cy="12" r="3"/>"#
        }
        "refresh" => {
            r#"<path d="M21 2v6h-6"/><path d="M3 12a9 9 0 0 1 15-6.7L21 8"/><path d="M3 22v-6h6"/><path d="M21 12a9 9 0 0 1-15 6.7L3 16"/>"#
        }
        "check" => r#"<path d="M20 6 9 17l-5-5"/>"#,
        "x" => r#"<path d="M18 6 6 18M6 6l12 12"/>"#,
        "power" => r#"<path d="M12 2v10"/><path d="M18.4 6.6a9 9 0 1 1-12.8 0"/>"#,
        "play" => r#"<path d="m6 3 14 9-14 9V3z"/>"#,
        "shield" => r#"<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10Z"/>"#,
        "alert" => {
            r#"<path d="M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0Z"/><path d="M12 9v4"/><path d="M12 17h.01"/>"#
        }
        "grid" => {
            r#"<rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="14" y="14" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/>"#
        }
        "box" => {
            r#"<path d="M21 8v8a2 2 0 0 1-1 1.73l-7 4a2 2 0 0 1-2 0l-7-4A2 2 0 0 1 3 16V8a2 2 0 0 1 1-1.73l7-4a2 2 0 0 1 2 0l7 4A2 2 0 0 1 21 8Z"/><path d="m3.3 7 8.7 5 8.7-5"/><path d="M12 22V12"/>"#
        }
        "bolt" => r#"<path d="M13 2 3 14h9l-1 8 10-12h-9l1-8Z"/>"#,
        "terminal" => r#"<path d="m4 17 6-6-6-6"/><path d="M12 19h8"/>"#,
        "code" => r#"<path d="m16 18 6-6-6-6"/><path d="m8 6-6 6 6 6"/>"#,
        "git-branch" => {
            r#"<path d="M6 3v12"/><circle cx="18" cy="6" r="3"/><circle cx="6" cy="18" r="3"/><path d="M18 9a9 9 0 0 1-9 9"/>"#
        }
        "database" => {
            r#"<ellipse cx="12" cy="5" rx="9" ry="3"/><path d="M3 5v14a9 3 0 0 0 18 0V5"/><path d="M3 12a9 3 0 0 0 18 0"/>"#
        }
        "server" => {
            r#"<rect x="2" y="3" width="20" height="8" rx="2"/><rect x="2" y="13" width="20" height="8" rx="2"/><path d="M6 7h.01M6 17h.01"/>"#
        }
        "cloud" => r#"<path d="M17.5 19a4.5 4.5 0 1 0 0-9h-1.26A7 7 0 1 0 4 15.25"/>"#,
        "package" => {
            r#"<path d="M21 8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16Z"/><path d="m3.3 7 8.7 5 8.7-5"/><path d="M12 22V12"/><path d="m7.5 4.3 9 5.1"/>"#
        }
        "wrench" => {
            r#"<path d="M14.7 6.3a4 4 0 0 0-5.4 5.4L3 18v3h3l6.3-6.3a4 4 0 0 0 5.4-5.4l-2.5 2.5-2-2 2.5-2.5z"/>"#
        }
        "flask" => {
            r#"<path d="M9 3h6M10 3v6l-5 9a2 2 0 0 0 1.8 3h10.4a2 2 0 0 0 1.8-3l-5-9V3"/><path d="M7 14h10"/>"#
        }
        "rocket" => {
            r#"<path d="M5 16c-1.5 1.3-2 5-2 5s3.7-.5 5-2c.7-.8.7-2 0-2.8a2 2 0 0 0-3 .8z"/><path d="M12 15l-3-3a16 16 0 0 1 6-10 5 5 0 0 1 7 7 16 16 0 0 1-10 6z"/>"#
        }
        "book" => {
            r#"<path d="M4 19.5A2.5 2.5 0 0 1 6.5 17H20"/><path d="M6.5 2H20v20H6.5A2.5 2.5 0 0 1 4 19.5v-15A2.5 2.5 0 0 1 6.5 2z"/>"#
        }
        "file" => {
            r#"<path d="M14 3v5h5"/><path d="M14 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/>"#
        }
        "folder" => {
            r#"<path d="M4 20a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2z"/>"#
        }
        "gear" => {
            r#"<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-2.82 1.17V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 8 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.6 15H4.5a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 6 9.4a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 11 4.6V4.5a2 2 0 0 1 4 0v.09A1.65 1.65 0 0 0 18 6.6l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 11.6"/>"#
        }
        "globe" => {
            r#"<circle cx="12" cy="12" r="9"/><path d="M3 12h18"/><path d="M12 3a14 14 0 0 1 0 18 14 14 0 0 1 0-18z"/>"#
        }
        "cpu" => {
            r#"<rect x="6" y="6" width="12" height="12" rx="1"/><rect x="9" y="9" width="6" height="6"/><path d="M9 2v2M15 2v2M9 20v2M15 20v2M2 9h2M2 15h2M20 9h2M20 15h2"/>"#
        }
        "lock" => {
            r#"<rect x="4" y="11" width="16" height="10" rx="2"/><path d="M8 11V7a4 4 0 0 1 8 0v4"/>"#
        }
        _ => "",
    };
    PreEscaped(format!(
        r#"<svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">{body}</svg>"#
    ))
}

/// The icon to show for a capability (its chosen icon, else a kind default).
fn cap_icon_name(c: &CapView) -> &str {
    match &c.icon {
        Some(name) => name,
        None if c.kind == "command" => "terminal",
        None if c.kind == "provider" => "bolt",
        None => "box",
    }
}

fn risk_class(r: Risk) -> &'static str {
    match r {
        Risk::Info => "risk-info",
        Risk::Caution => "risk-caution",
        Risk::Dangerous => "risk-dangerous",
    }
}

// --- markdown ----------------------------------------------------------------

/// Render overlay markdown to HTML. Raw HTML is escaped (studio can open an
/// untrusted cloned repo's guidance) and generated header comments are stripped.
fn render_markdown(md: &str) -> Markup {
    let body = strip_leading_comments(md);
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(body, opts).map(|ev| match ev {
        Event::Html(s) | Event::InlineHtml(s) => Event::Text(s),
        other => other,
    });
    let mut out = String::new();
    md_html::push_html(&mut out, parser);
    PreEscaped(out)
}

fn strip_leading_comments(md: &str) -> &str {
    let mut t = md.trim_start();
    while let Some(rest) = t.strip_prefix("<!--") {
        match rest.find("-->") {
            Some(end) => t = rest[end + 3..].trim_start(),
            None => break,
        }
    }
    t
}

// --- page shell --------------------------------------------------------------

/// The full page: top bar (brand + tabs + staged indicator), the `#main` tab
/// content, and the empty `#modal` container.
pub fn shell(main: Markup, staged: usize, active_tab: &str) -> String {
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
                    div class="brand" { span class="brand-mark" { (icon("layers")) } span class="brand-name" { "rosita studio" } }
                    (tab_bar(active_tab))
                    div id="staged" class="staged-wrap" { (staged_indicator(staged)) }
                }
                main class="main" id="main" { (main) }
                div id="modal" class="modal-root" {}
            }
        }
    }
    .into_string()
}

fn tab_bar(active: &str) -> Markup {
    let cls = |name: &str| if name == active { "tab active" } else { "tab" };
    html! {
        nav class="tabs" {
            button class=(cls("profiles")) data-tab="profiles" hx-get="/tab/profiles" hx-target="#main" { (icon("layers")) "Profiles" }
            button class=(cls("capabilities")) data-tab="capabilities" hx-get="/tab/capabilities" hx-target="#main" { (icon("box")) "Capabilities" }
        }
    }
}

/// The staged-changes indicator (top-bar right). Re-pulled via `GET /staged`.
pub fn staged_indicator(staged: usize) -> Markup {
    html! {
        @if staged > 0 {
            span class="staged-count" { (icon("layers")) (staged) " staged" }
            button class="btn btn-ghost btn-sm" hx-get="/diff" hx-target="#main" { "Review" }
            button class="btn btn-primary btn-sm" hx-post="/apply" hx-target="#main"
                hx-confirm="Apply staged changes to your config files?" { (icon("check")) "Apply" }
        } @else {
            span class="muted small" { "No staged changes" }
        }
    }
}

pub fn staged_indicator_fragment(staged: usize) -> String {
    staged_indicator(staged).into_string()
}

/// A one-shot loader that re-pulls the staged indicator after a mutation.
fn staged_refresh() -> Markup {
    html! { div hx-get="/staged" hx-trigger="load" hx-target="#staged" {} }
}

/// The staged-indicator refresh loader as a standalone string (for handlers that
/// append it to a non-fragment response, e.g. the inline cap-add editor reload).
pub fn staged_indicator_loader() -> String {
    staged_refresh().into_string()
}

/// A one-shot loader that closes the modal after a capability mutation.
fn modal_close() -> Markup {
    html! { div hx-get="/close" hx-trigger="load" hx-target="#modal" {} }
}

// --- Profiles tab ------------------------------------------------------------

/// The Profiles tab: a dashboard of profile cards (left) + the preview pane
/// (right). `previewing` marks the card whose preview is shown.
pub fn profiles_tab(lib: &LibraryView, previewing: Option<&str>, flash: Option<&str>) -> Markup {
    html! {
        div class="tab-profiles" {
            section class="dashboard" {
                div class="dash-head" {
                    h1 { "Profiles" }
                    button class="btn btn-primary" hx-get="/profiles/new" hx-target="#main" { (icon("plus")) "New profile" }
                }
                @if let Some(msg) = flash { p class="flash" { (icon("check")) (msg) } }
                @if lib.profiles.is_empty() {
                    div class="empty-card" {
                        p { "No profiles yet." }
                        p class="muted" { "A profile bundles capabilities and binds them to a kind of repo." }
                        button class="btn btn-primary" hx-get="/profiles/new" hx-target="#main" { (icon("plus")) "Create your first profile" }
                    }
                } @else {
                    div class="profile-grid" {
                        @for p in &lib.profiles { (profile_card(p, previewing == Some(p.name.as_str()))) }
                    }
                }
            }
            aside class="preview-col" id="preview" { (profile_preview_empty()) }
        }
    }
}

pub fn profiles_tab_fragment(
    lib: &LibraryView,
    previewing: Option<&str>,
    flash: Option<&str>,
) -> String {
    let preview_loader = previewing.map(|name| {
        html! { div hx-get=(format!("/profiles/{}/preview", enc(name))) hx-trigger="load" hx-target="#preview" {} }
    });
    html! {
        (profiles_tab(lib, previewing, flash))
        @if let Some(l) = preview_loader { (l) }
    }
    .into_string()
}

fn profile_card(p: &ProfileView, previewing: bool) -> Markup {
    let name = p.name.as_str();
    let e = enc(name);
    let mut cls = String::from("profile-card");
    if p.disabled {
        cls.push_str(" disabled");
    }
    if previewing {
        cls.push_str(" previewing");
    }
    if p.selected {
        cls.push_str(" bound");
    }
    html! {
        div class=(cls) {
            div class="card-body" hx-get=(format!("/profiles/{e}/select")) hx-target="#main" {
                div class="card-top" {
                    span class="profile-name" { (name) }
                    @if p.selected { span class="tag bound-tag" { (icon("arrow-right")) "bound" } }
                    @if p.disabled { span class="tag off-tag" { "disabled" } }
                }
                @if p.targets.is_empty() {
                    div class="targets" { span class="target-chip muted" { "no targets" } }
                } @else {
                    div class="targets" { @for t in &p.targets { span class="target-chip" { (t) } } }
                }
                div class="card-foot" {
                    @if p.atoms.is_empty() {
                        span class="muted small" { "no capabilities" }
                    } @else {
                        div class="atoms" { @for a in &p.atoms { (atom_dot(a)) } }
                        span class="muted small" { (p.atoms.len()) " " (if p.atoms.len() == 1 { "capability" } else { "capabilities" }) }
                    }
                }
            }
            div class="card-actions" {
                button class="toggle" title=(if p.disabled { "Enable profile" } else { "Disable profile" }) aria-label="Toggle profile"
                    hx-post=(format!("/profiles/{e}/disable")) hx-target="#main" {
                    span class=(if p.disabled { "switch off" } else { "switch on" }) {}
                }
                button class="icon-btn" title="Edit" aria-label=(format!("Edit {name}"))
                    hx-get=(format!("/profiles/{e}/edit")) hx-target="#main" { (icon("pencil")) }
                button class="icon-btn danger" title="Delete" aria-label=(format!("Delete {name}"))
                    hx-delete=(format!("/profiles/{e}")) hx-target="#main"
                    hx-confirm=(format!("Stage deletion of profile \"{name}\"?")) { (icon("trash")) }
            }
        }
    }
}

fn profile_preview_empty() -> Markup {
    html! {
        div class="preview-empty" {
            (icon("eye"))
            p { "Select a profile to preview the AGENTS.md it produces." }
        }
    }
}

/// The rendered-markdown preview for a selected profile, with an agent picker
/// (shown only when more than one agent is configured).
pub fn profile_preview_pane(p: &PreviewOutcome, agents: &[String], profile_name: &str) -> Markup {
    let e = enc(profile_name);
    html! {
        div class="preview-pane" {
            div class="preview-head" {
                span class="preview-title" { (icon("eye")) "Preview" }
                div class="preview-meta" {
                    (binding_chip(&p.binding))
                    @if agents.len() > 1 {
                        form class="agent-form" hx-post=(format!("/profiles/{e}/preview")) hx-target="#preview" hx-trigger="change" {
                            select name="agent" {
                                @for a in agents { option value=(a.as_str()) selected[a == &p.agent] { (a.as_str()) } }
                            }
                        }
                    } @else {
                        span class="chip" { (p.agent.as_str()) }
                    }
                }
            }
            div class="provenance" {
                span class="prov-node" { (p.context_summary.as_str()) }
                span class="prov-arrow" { (icon("arrow-right")) }
                span class="prov-node" { (p.profile_label.as_str()) }
                span class="prov-arrow" { (icon("arrow-right")) }
                span class="prov-node" { (p.cap_count) " " (if p.cap_count == 1 { "capability" } else { "capabilities" }) }
            }
            @if let Some(note) = &p.note { p class="note" { (note) } }
            div class="overlay-toggle" {
                input type="radio" name="ov" id="ov-rendered" checked;
                label class="seg-opt" for="ov-rendered" { "Rendered" }
                input type="radio" name="ov" id="ov-raw";
                label class="seg-opt" for="ov-raw" { "Raw" }
            }
            div class="overlay-rendered markdown-body" { (render_markdown(&p.overlay)) }
            pre class="overlay-body overlay-raw" { (p.overlay) }
        }
    }
}

pub fn profile_preview_fragment(
    p: &PreviewOutcome,
    agents: &[String],
    profile_name: &str,
) -> String {
    profile_preview_pane(p, agents, profile_name).into_string()
}

// --- Capabilities tab --------------------------------------------------------

/// The Capabilities tab: a grid of capability cards (open a dialog on click),
/// then the read-only starter palette.
pub fn capabilities_tab(lib: &LibraryView, flash: Option<&str>) -> Markup {
    html! {
        div class="tab-capabilities" {
            div class="dash-head" {
                h1 { "Capabilities" }
                button class="btn btn-primary" hx-get="/capabilities/new" hx-target="#modal" { (icon("plus")) "New capability" }
            }
            @if let Some(msg) = flash { p class="flash" { (icon("check")) (msg) } }
            @if lib.yours.is_empty() {
                div class="empty-card" {
                    p { "No capabilities yet." }
                    p class="muted" { "Write one, or duplicate a starter from the palette below." }
                    button class="btn btn-primary" hx-get="/capabilities/new" hx-target="#modal" { (icon("plus")) "New capability" }
                }
            } @else {
                div class="cap-grid" { @for c in &lib.yours { (cap_card(c, true)) } }
            }
            @if !lib.palette.is_empty() {
                details class="palette" {
                    summary { (icon("box")) "Starter palette" span class="count" { (lib.palette.len()) } }
                    p class="palette-hint" { "Read-only templates — open one to duplicate it into your library." }
                    div class="cap-grid" { @for c in &lib.palette { (cap_card(c, false)) } }
                }
            }
        }
    }
}

pub fn capabilities_tab_fragment(lib: &LibraryView, flash: Option<&str>) -> String {
    capabilities_tab(lib, flash).into_string()
}

fn cap_card(c: &CapView, owned: bool) -> Markup {
    let id = c.id.as_str();
    let e = enc(id);
    let route = if owned {
        format!("/capabilities/{e}/edit")
    } else {
        format!("/capabilities/{e}/view")
    };
    let cls = format!("cap-card {}", risk_class(c.risk));
    html! {
        div class=(cls) hx-get=(route) hx-target="#modal" role="button" tabindex="0" {
            span class="cap-glyph" { (icon(cap_icon_name(c))) }
            div class="cap-main" {
                span class="cap-title" { (c.title) }
                span class="cap-id" { (id) }
            }
            div class="cap-tags" {
                @if let Some(lang) = &c.script_lang { span class="tag script-tag" { (icon("terminal")) (lang) } }
                @else if c.kind == "command" { span class="tag script-tag" { (icon("terminal")) "script" } }
                @else if c.kind == "provider" { span class="tag script-tag" { (icon("bolt")) "dynamic" } }
                @if !owned { span class="tag palette-tag" { "palette" } }
                @else if c.private { span class="tag" { (icon("lock")) "private" } }
                @else { span class="tag" { "shared" } }
            }
        }
    }
}

// --- capability dialog (modal) ----------------------------------------------

/// The capability dialog content (swapped into `#modal`). A palette item is
/// read-only with a duplicate action; an advanced cap is read-only with an
/// "edit in TOML" note; otherwise the content-first editor.
pub fn cap_dialog(cap: Option<&Capability>, layer: Layer, owned: bool) -> String {
    let is_new = cap.is_none();
    let id = cap.map(|c| c.id.as_str()).unwrap_or("");
    let read_only_palette = !is_new && !owned;
    let advanced = cap
        .map(crate::studio::state::is_advanced_capability)
        .unwrap_or(false);
    html! {
        div class="modal-backdrop" hx-get="/close" hx-target="#modal" {}
        div class="modal" {
            @if read_only_palette {
                div class="modal-head" { h2 { "Palette capability" } (close_btn()) }
                div class="modal-body" {
                    p class="hint" { "Starter template. Duplicate “" (id) "” into your library to own and edit it." }
                }
                div class="modal-foot" {
                    button class="btn btn-ghost" hx-get="/close" hx-target="#modal" { "Close" }
                    button class="btn btn-primary" hx-post=(format!("/capabilities/{}/duplicate", enc(id))) hx-target="#main" { (icon("copy")) "Duplicate into my library" }
                }
            } @else if advanced {
                div class="modal-head" { h2 { "Advanced capability" } (close_btn()) }
                div class="modal-body" {
                    p class="hint" { "“" (id) "” uses features the quick editor can't show without dropping one side (a built-in provider, or a script with a custom template). Edit it directly in your config TOML." }
                }
                div class="modal-foot" { button class="btn btn-ghost" hx-get="/close" hx-target="#modal" { "Close" } }
            } @else {
                @let is_script = cap.map(|c| c.command.is_some()).unwrap_or(false);
                @let allow_exec = cap.map(|c| c.allow_exec).unwrap_or(true);
                @let lang = cap.and_then(|c| c.script_lang.as_deref()).unwrap_or("bash");
                form class="cap-form" hx-post="/capabilities" hx-target="#main" {
                    div class="modal-head" {
                        h2 { (if is_new { "New capability" } else { "Edit capability" }) }
                        (close_btn())
                    }
                    div class="modal-body" {
                        @if !is_new { input type="hidden" name="id" value=(id); }
                        div class="title-row" {
                            (icon_picker(cap.and_then(|c| c.icon.as_deref())))
                            label class="field grow" { span class="field-label" { "title" }
                                input type="text" name="name" value=(cap.and_then(|c| c.description.as_deref()).unwrap_or(id)) placeholder="Rust conventions" required;
                            }
                        }
                        div class="seg" {
                            input type="radio" name="kind" id="kind-md" value="markdown" checked[!is_script];
                            label class="seg-opt" for="kind-md" { "Markdown" }
                            input type="radio" name="kind" id="kind-sc" value="script" checked[is_script];
                            label class="seg-opt" for="kind-sc" { "Script" }
                        }
                        div class="kind-md" {
                            label class="field" { span class="field-label" { "content" span class="field-hint" { "markdown" } }
                                textarea name="guidance" rows="9" placeholder="# Rust conventions&#10;Build with cargo; lint with clippy." { (cap.map(|c| c.guidance.as_str()).unwrap_or("")) }
                            }
                        }
                        div class="kind-sc" {
                            div class="script-head" {
                                label class="field grow" { span class="field-label" { "script" span class="field-hint" { "its output is embedded" } } }
                                div class="seg seg-sm" {
                                    @for (val, lbl) in SCRIPT_LANGS {
                                        @let lid = format!("lang-{val}");
                                        input type="radio" name="script_lang" id=(lid) value=(val) checked[lang == *val];
                                        label class="seg-opt" for=(lid) { (lbl) }
                                    }
                                }
                            }
                            textarea name="command" rows="7" class="mono" placeholder="echo 'last deploy: green'" { (cap.and_then(|c| c.command.as_deref()).unwrap_or("")) }
                            label class="check exec-check" { input type="checkbox" name="allow_exec" checked[allow_exec]; span { "Allow execution" } }
                            p class="hint small" { "Runs only when the repo is trusted (" code { "rosita allow" } ") and execution is allowed. Review trust in the diff before applying." }
                        }
                        (lives_in(layer))
                    }
                    div class="modal-foot" {
                        button type="button" class="btn btn-ghost" hx-get="/close" hx-target="#modal" { "Cancel" }
                        button type="submit" class="btn btn-primary" { (icon("check")) "Stage change" }
                    }
                }
            }
        }
    }
    .into_string()
}

fn close_btn() -> Markup {
    html! { button class="icon-btn" type="button" title="Close" aria-label="Close" hx-get="/close" hx-target="#modal" { (icon("x")) } }
}

/// The curated icon picker: a hidden radio per icon + a "none" option.
fn icon_picker(selected: Option<&str>) -> Markup {
    html! {
        div class="icon-picker" {
            label class="field-label" { "icon" }
            div class="icon-grid" {
                label class="icon-opt none-opt" title="No icon" {
                    input type="radio" name="icon" value="" checked[selected.is_none()];
                    span class="icon-cell" { (icon("x")) }
                }
                @for name in CAP_ICONS {
                    label class="icon-opt" title=(name) {
                        input type="radio" name="icon" value=(name) checked[selected == Some(*name)];
                        span class="icon-cell" { (icon(name)) }
                    }
                }
            }
        }
    }
}

/// Location control: a hidden, preserved `scope` (repo/global) + shared/private.
fn lives_in(layer: Layer) -> Markup {
    let (scope, private) = layer_scope(layer);
    html! {
        fieldset class="lives-in" {
            legend { "Where it lives" }
            input type="hidden" name="scope" value=(scope);
            div class="radio-row" {
                label class="radio" { input type="radio" name="visibility" value="public" checked[!private]; span { "shared" span class="radio-sub" { "config.toml" } } }
                label class="radio" { input type="radio" name="visibility" value="private" checked[private]; span { "private" span class="radio-sub" { "local.toml" } } }
            }
            @if scope == "global" { p class="hint small" { "Global — applies across all your repos." } }
        }
    }
}

// --- profile editor (full view) ----------------------------------------------

/// The full-width profile editor: a form (left) with name, targets, a capability
/// picker, an inline quick-create, and a live preview (right). `draft` carries
/// the in-progress values (so an inline add re-renders without losing state).
pub fn profile_editor(
    draft: &ProfileConfig,
    is_new: bool,
    lib: &LibraryView,
    preview: &PreviewOutcome,
) -> String {
    let name = draft.name.as_str();
    let selected: Vec<&str> = draft.capabilities.iter().map(|r| r.id()).collect();
    let chosen = |id: &str| selected.contains(&id);
    html! {
        div class="profile-editor" {
            form class="editor-form" hx-post="/profiles/preview" hx-trigger="change delay:200ms" hx-target="#editor-preview" {
                @if !is_new { input type="hidden" name="new" value="0"; } @else { input type="hidden" name="new" value="1"; }
                div class="editor-head" {
                    button type="button" class="icon-btn" title="Back" hx-get="/tab/profiles" hx-target="#main" { (icon("arrow-right")) }
                    h1 { (if is_new { "New profile" } else { "Edit profile" }) }
                }
                label class="field" { span class="field-label" { "name" }
                    @if is_new {
                        input type="text" name="name" value=(name) placeholder="rust — web" required;
                    } @else {
                        input type="text" name="name" value=(name) readonly;
                    }
                }
                fieldset class="targets-picker" {
                    legend { "Targets" span class="field-hint" { "applies when the repo looks like one of these" } }
                    div class="checks" {
                        @for &t in TARGETS {
                            label class="check" { input type="checkbox" name="targets" value=(t) checked[draft.targets.iter().any(|x| x == t)]; span { (t) } }
                        }
                    }
                }
                fieldset class="cap-picker" {
                    legend { "Capabilities" span class="field-hint" { "tick the ones to compose" } }
                    div class="pick-list" {
                        @for c in lib.yours.iter().chain(lib.palette.iter()) {
                            label class="pick" {
                                input type="checkbox" name="capabilities" value=(c.id.as_str()) checked[chosen(c.id.as_str())];
                                span class="pick-glyph" { (icon(cap_icon_name(c))) }
                                span class="pick-main" { span class="pick-title" { (c.title) } span class="pick-id" { (c.id.as_str()) } }
                            }
                        }
                    }
                    (inline_new_cap())
                }
                label class="field" { span class="field-label" { "inline guidance" span class="field-hint" { "optional" } }
                    textarea name="guidance" rows="2" { (draft.guidance.as_deref().unwrap_or("")) }
                }
                fieldset class="lives-in" {
                    legend { "Where it lives" }
                    input type="hidden" name="scope" value="repo";
                    label class="check" { input type="checkbox" name="disabled" checked[draft.disabled]; span { "Disabled (kept, but never selected)" } }
                }
                div class="form-buttons" {
                    button type="button" class="btn btn-ghost" hx-get="/tab/profiles" hx-target="#main" { "Cancel" }
                    button type="button" class="btn btn-primary" hx-post="/profiles" hx-target="#main" { (icon("check")) "Stage profile" }
                }
            }
            aside class="editor-preview-col" {
                div class="preview-head" { span class="preview-title" { (icon("eye")) "Live preview" } (binding_chip(&preview.binding)) }
                div id="editor-preview" { (editor_preview(preview)) }
            }
        }
    }
    .into_string()
}

/// The collapsible inline "new capability" mini-form inside the profile editor.
/// Its fields are `cap_*`-namespaced so they don't collide with the profile form;
/// "Add" posts the whole editor form to `/profiles/draft`.
fn inline_new_cap() -> Markup {
    html! {
        details class="inline-cap" {
            summary { (icon("plus")) "New capability" }
            div class="inline-grid" {
                label class="field" { span class="field-label" { "title" }
                    input type="text" name="cap_name" placeholder="New capability";
                }
                div class="seg seg-sm" {
                    input type="radio" name="cap_kind" id="cap-kind-md" value="markdown" checked;
                    label class="seg-opt" for="cap-kind-md" { "Markdown" }
                    input type="radio" name="cap_kind" id="cap-kind-sc" value="script";
                    label class="seg-opt" for="cap-kind-sc" { "Script" }
                }
                label class="field" { span class="field-label" { "content" }
                    textarea name="cap_content" rows="3" placeholder="Guidance markdown, or the script body." {}
                }
                label class="check" { input type="checkbox" name="cap_private"; span { "private (local.toml)" } }
                button type="button" class="btn btn-primary btn-sm" hx-post="/profiles/draft" hx-target="#main" { (icon("plus")) "Add to library & profile" }
            }
        }
    }
}

fn editor_preview(p: &PreviewOutcome) -> Markup {
    html! {
        div class="provenance" {
            span class="prov-node" { (p.context_summary.as_str()) }
            span class="prov-arrow" { (icon("arrow-right")) }
            span class="prov-node" { (p.cap_count) " " (if p.cap_count == 1 { "capability" } else { "capabilities" }) }
        }
        @if let Some(note) = &p.note { p class="note" { (note) } }
        div class="markdown-body" { (render_markdown(&p.overlay)) }
    }
}

pub fn editor_preview_fragment(p: &PreviewOutcome) -> String {
    editor_preview(p).into_string()
}

// --- diff / review -----------------------------------------------------------

/// Trust state surfaced in the review when repo-authored `command` caps exist.
pub struct TrustBanner {
    pub command_caps: Vec<String>,
    pub status: String,
    pub trusted: bool,
}

pub fn diff_view(
    diffs: &[FileDiff],
    leaks: &[String],
    fs_changed: &[std::path::PathBuf],
    trust: &TrustBanner,
    staged: usize,
) -> String {
    html! {
        div class="review" {
            div class="dash-head" {
                div class="editor-head" {
                    button type="button" class="icon-btn" title="Back" hx-get="/tab/profiles" hx-target="#main" { (icon("arrow-right")) }
                    h1 { "Review staged changes" }
                }
                span class="pill" { (staged) " staged" }
            }

            @if !trust.command_caps.is_empty() {
                div class="banner warn" {
                    span class="banner-icon" { (icon("shield")) }
                    div class="banner-body" {
                        p { "Repo command capabilities (" (trust.command_caps.join(", ")) ") won't run until you trust this repo — currently " span class="trust-status" { (trust.status) } "." }
                        @if !trust.trusted {
                            button class="btn btn-primary btn-sm" hx-post="/trust/allow" hx-target="#main" hx-confirm="Trust this repo to run its command-backed capabilities?" { "Allow this repo" }
                        } @else {
                            button class="btn btn-danger btn-sm" hx-post="/trust/deny" hx-target="#main" { "Revoke trust" }
                        }
                        p class="muted" { "An apply changes the repo config bundle, which re-locks trust — re-allow afterward." }
                    }
                }
            }

            @if !leaks.is_empty() {
                div class="banner warn" {
                    span class="banner-icon" { (icon("alert")) }
                    div class="banner-body" { p { "Leak check: these public values look machine-specific — consider moving to local.toml:" } p class="mono" { (leaks.join(", ")) } }
                }
            } @else {
                p class="ok-line" { (icon("check")) "Leak check: clean." }
            }

            @if !fs_changed.is_empty() {
                div class="banner warn" {
                    span class="banner-icon" { (icon("alert")) }
                    div class="banner-body" { p { "Config changed on disk since load (" (fs_changed.iter().map(|p| display_name(p)).collect::<Vec<_>>().join(", ")) ") — applying will overwrite it." } }
                }
            }

            @if diffs.is_empty() {
                p class="empty" { "No staged changes." }
            } @else {
                @for d in diffs { (file_diff(d)) }
                div class="form-buttons" {
                    button type="button" class="btn btn-ghost" hx-get="/tab/profiles" hx-target="#main" { "Back" }
                    button class="btn btn-primary" hx-post="/apply" hx-target="#main" hx-confirm="Apply staged changes to your config files?" { (icon("check")) "Apply " (staged) " change(s)" }
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
            div class="file-head" { span class="file-path" { (display_name(&d.path)) } span class="file-meta" { (scope) " · " (vis) } }
            @if d.reformats_untouched { p class="hint small" { "rosita will also reformat some untouched lines it parsed." } }
            pre class="diff-body" { (d.unified) }
        }
    }
}

// --- mutation results --------------------------------------------------------

/// A capability mutation: re-render the Capabilities tab into `#main`, close the
/// modal, and refresh the staged indicator. (`flash` keeps the "staged …" note.)
pub fn cap_result(lib: &LibraryView, flash: &str) -> String {
    html! {
        (capabilities_tab(lib, Some(flash)))
        (modal_close())
        (staged_refresh())
    }
    .into_string()
}

/// A profile mutation: re-render the Profiles tab into `#main` + refresh staged.
pub fn profile_result(lib: &LibraryView, flash: &str) -> String {
    html! {
        (profiles_tab(lib, None, Some(flash)))
        (staged_refresh())
    }
    .into_string()
}

/// An inline error fragment (validation / config errors never 500).
pub fn error_fragment(msg: &str) -> String {
    html! { div class="banner error" { span class="banner-icon" { (icon("alert")) } div class="banner-body" { (msg) } } }.into_string()
}

/// A minimal full-page error (when the shell itself can't be assembled).
pub fn error_page(msg: &str) -> String {
    html! { (DOCTYPE) html { head { title { "rosita studio — error" } } body { pre class="error" { (msg) } } } }.into_string()
}

/// `GET /fs-status` — the light external-edit poll banner.
pub fn fs_status_fragment(changed: &[std::path::PathBuf]) -> String {
    if changed.is_empty() {
        return html! { span class="fs-clean" { "on-disk unchanged since load" } }.into_string();
    }
    let names: Vec<String> = changed.iter().map(|p| display_name(p)).collect();
    html! { div class="banner warn" { span class="banner-icon" { (icon("alert")) } div class="banner-body" { "Config changed on disk: " (names.join(", ")) " — reload before applying." } } }.into_string()
}

// --- shared bits -------------------------------------------------------------

fn binding_chip(b: &BindingState) -> Markup {
    match b {
        BindingState::Bound(name) => {
            html! { span class="chip chip-bound" title="binds in this context" { (icon("arrow-right")) span class="chip-name" { "profile " (name) } } }
        }
        BindingState::None => html! { span class="chip chip-none" { "profile none" } },
        BindingState::Ambiguous(n) => {
            html! { span class="chip chip-ambiguous" { (icon("alert")) (n) " profiles match" } }
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
            format!("{} — palette only (not duplicated)", a.id),
        ),
        AtomState::Unknown => (
            "atom unknown".to_string(),
            format!("{} — unknown capability", a.id),
        ),
    };
    html! { span class=(cls) title=(tip) {} }
}

fn layer_scope(layer: Layer) -> (&'static str, bool) {
    match layer {
        Layer::Global => ("global", false),
        Layer::GlobalLocal => ("global", true),
        Layer::RepoLocal => ("repo", true),
        _ => ("repo", false),
    }
}

fn display_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// Percent-encode a path segment (profile names can contain spaces / em-dashes).
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
