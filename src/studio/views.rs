//! `maud` server-rendered HTML for rosita studio: a tabbed shell (Fragments /
//! Profiles), the profile rail + per-fragment detail, the fragment list +
//! modal dialog, the full-width profile editor, and the diff/review.
//!
//! Targets the tiny embedded htmx-shim. Swap targets:
//! - `#main` — the active tab's content (caps list, profile rail, editor, diff).
//! - `#profile-main` — the selected profile's detail (a rail click swaps it in).
//! - `#modal` — the fragment dialog (CSS shows it when non-empty; `/close`
//!   swaps it empty).
//! - `#staged` — the top-bar staged-changes indicator (mutations re-pull it).

use std::path::Path;

use maud::{html, Markup, PreEscaped, DOCTYPE};
use pulldown_cmark::{html as md_html, Event, Options, Parser};

use crate::context::Scope;
use crate::fragment::{Fragment, Layer};
use crate::profile::ProfileConfig;
use crate::studio::edit::FileDiff;
use crate::studio::state::{
    AtomDot, AtomState, BindingState, FragmentView, LibraryView, Onboarding, PackView, PreviewCap,
    PreviewOutcome, ProfileView,
};

/// Language/platform targets a profile can declare.
const TARGETS: &[&str] = &[
    "rust", "node", "nextjs", "go", "python", "android", "java", "machine",
];

/// Script interpreters offered in the fragment dialog.
const SCRIPT_LANGS: &[(&str, &str)] = &[("bash", "Bash"), ("python", "Python"), ("sh", "POSIX sh")];

// --- icons -------------------------------------------------------------------

/// A 16px feather-style inline SVG icon (1.5px stroke, `currentColor`). Matched
/// against a closed set of **static** strings — never interpolate a dynamic
/// value into `PreEscaped` (that would bypass escaping).
fn icon(name: &str) -> Markup {
    let body: &str = match name {
        "plus" => r#"<path d="M12 5v14M5 12h14"/>"#,
        "sun" => {
            r#"<circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"/>"#
        }
        "moon" => r#"<path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8Z"/>"#,
        "monitor" => {
            r#"<rect x="2" y="3" width="20" height="14" rx="2"/><path d="M8 21h8M12 17v4"/>"#
        }
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
        "help" => {
            r#"<circle cx="12" cy="12" r="10"/><path d="M9.1 9a3 3 0 0 1 5.8 1c0 2-3 3-3 3"/><path d="M12 17h.01"/>"#
        }
        "refresh" => {
            r#"<path d="M21 2v6h-6"/><path d="M3 12a9 9 0 0 1 15-6.7L21 8"/><path d="M3 22v-6h6"/><path d="M21 12a9 9 0 0 1-15 6.7L3 16"/>"#
        }
        "check" => r#"<path d="M20 6 9 17l-5-5"/>"#,
        "x" => r#"<path d="M18 6 6 18M6 6l12 12"/>"#,
        "chevron-down" => r#"<path d="m6 9 6 6 6-6"/>"#,
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

/// The rosita brandmark: a small red rose (multi-color, so it ignores the
/// monochrome icon treatment).
fn brand_mark() -> Markup {
    PreEscaped(
        r##"<svg class="rose" viewBox="0 0 24 24" aria-hidden="true">
  <path fill="#36a35b" d="M11.6 13h.8v7h-.8z"/>
  <path fill="#36a35b" d="M12.2 16.4c1.5-.1 2.8-1 3.3-2.4-1.5.1-2.8 1-3.3 2.4z"/>
  <path fill="#36a35b" d="M11.8 15c-1.4-.1-2.6-.9-3.1-2.2 1.4.1 2.6.9 3.1 2.2z"/>
  <circle cx="12" cy="9" r="5.3" fill="#e23b54"/>
  <path fill="none" stroke="#a82338" stroke-width="1.3" stroke-linecap="round" d="M12 5.1a3.9 3.9 0 1 0 3.7 5.1"/>
  <path fill="none" stroke="#a82338" stroke-width="1.2" stroke-linecap="round" d="M12 6.8a2.2 2.2 0 1 0 2 3"/>
  <circle cx="12" cy="9" r="0.95" fill="#a82338"/>
</svg>"##
            .to_string(),
    )
}

/// Inline `<head>` script that resolves the stored theme preference
/// (`auto`/`light`/`dark`) against the system `prefers-color-scheme` and stamps
/// `<html data-theme>` (resolved) + `<html data-theme-pref>` (preference) before
/// the stylesheet paints — preventing a dark→light flash on load.
const THEME_INIT_JS: &str = "(function(){try{var p=localStorage.getItem('rosita-theme')||'auto';\
var m=window.matchMedia&&matchMedia('(prefers-color-scheme: light)').matches;\
var e=p==='auto'?(m?'light':'dark'):p;var r=document.documentElement;\
r.dataset.theme=e;r.dataset.themePref=p;}catch(_){}})();";

/// The theme toggle: one button cycling auto → light → dark. All three glyphs are
/// rendered; CSS shows the one matching `<html data-theme-pref>`, and `studio.js`
/// flips the preference + persists it on click. Defaults to the auto (monitor)
/// glyph until JS/the inline init sets the preference.
fn theme_toggle() -> Markup {
    html! {
        button id="theme-toggle" type="button" class="icon-btn theme-toggle"
            title="Theme: auto" aria-label="Switch color theme" {
            span class="ti ti-auto" { (icon("monitor")) }
            span class="ti ti-light" { (icon("sun")) }
            span class="ti ti-dark" { (icon("moon")) }
        }
    }
}

/// The glyph for a fragment row, derived from its content type.
fn fragment_icon_name(c: &FragmentView) -> &'static str {
    crate::studio::state::type_glyph(c.kind, c.script_lang.as_deref())
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
                // No-flash theme init: resolve the stored preference (auto/light/
                // dark) against the system setting and stamp <html> before the
                // stylesheet paints, so there's no dark→light flicker on load.
                script { (PreEscaped(THEME_INIT_JS)) }
                title { "Rosita studio" }
                link rel="stylesheet" href="/assets/studio.css";
                script src="/assets/studio.js" defer {}
            }
            body {
                header class="topbar" {
                    div class="brand" { span class="brand-mark" { (brand_mark()) } span class="brand-name" { "Rosita" } }
                    (tab_bar(active_tab))
                    div class="topbar-right" {
                        div id="staged" class="staged-wrap" { (staged_indicator(staged)) }
                        button type="button" class="icon-btn" title="Show me around" hx-get="/onboarding/welcome" hx-target="#main" { (icon("help")) }
                        (theme_toggle())
                    }
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
            button class=(cls("fragments")) data-tab="fragments" hx-get="/tab/fragments" hx-target="#main" { (icon("box")) "Fragments" }
        }
    }
}

/// The staged-changes indicator (top-bar right). Re-pulled via `GET /staged`.
pub fn staged_indicator(staged: usize) -> Markup {
    html! {
        @if staged > 0 {
            span class="staged-count" { (icon("layers")) (staged) " staged" }
            button class="btn btn-ghost btn-sm" hx-get="/diff" hx-target="#main" { "Review" }
            button class="btn btn-ghost btn-sm" hx-post="/discard" hx-target="#main"
                hx-confirm="Discard all staged changes? Your config files won't be modified." { (icon("x")) "Discard" }
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
/// append it to a non-fragment response, e.g. the inline fragment-add editor reload).
pub fn staged_indicator_loader() -> String {
    staged_refresh().into_string()
}

/// A one-shot loader that closes the modal after a fragment mutation.
fn modal_close() -> Markup {
    html! { div hx-get="/close" hx-trigger="load" hx-target="#modal" {} }
}

/// The modal-close loader as a standalone string (appended to a profile-detail
/// re-render when a fragment was edited from inside a profile).
pub fn modal_close_loader() -> String {
    modal_close().into_string()
}

// --- Profiles tab ------------------------------------------------------------

/// The selected profile's rendered preview, bundled for the detail view.
pub struct ProfileDetail<'a> {
    pub name: &'a str,
    pub outcome: &'a PreviewOutcome,
    pub disabled: bool,
    /// Which fragment card(s) to render expanded. Cards collapse by default; a
    /// fragment that was just run opens so its fresh output is visible.
    pub expand: Expand<'a>,
}

/// Which fragment cards open after an action. Passive views ([`Expand::None`])
/// collapse everything; running one fragment opens that one ([`Expand::One`]);
/// "Run all" opens every dynamic card ([`Expand::AllDynamic`]).
#[derive(Clone, Copy)]
pub enum Expand<'a> {
    None,
    One(&'a str),
    AllDynamic,
}

/// The Profiles tab: a vertical profile rail (left) + the selected profile's
/// detail (right). `selected` is the profile whose detail fills the main area
/// (the server default-selects the bound/first profile so it's never empty).
pub fn profiles_tab(
    lib: &LibraryView,
    selected: Option<ProfileDetail>,
    flash: Option<&str>,
    onboarding: Option<&Onboarding>,
    packs: &[PackView],
) -> Markup {
    let sel_name = selected.as_ref().map(|d| d.name);
    html! {
        div class="tab-profiles" {
            aside class="profile-rail" {
                div class="rail-head" {
                    h1 { "Profiles" }
                    div class="rail-head-actions" {
                        button class="btn btn-ghost btn-sm" hx-get="/packs" hx-target="#main" { (icon("grid")) "Starter packs" }
                        button class="btn btn-primary btn-sm" hx-get="/profiles/new" hx-target="#main" { (icon("plus")) "New" }
                    }
                }
                @if let Some(msg) = flash { p class="flash" { (icon("check")) (msg) } }
                @if lib.profiles.is_empty() {
                    p class="rail-empty muted" { "No profiles yet." }
                } @else {
                    nav class="rail-list" {
                        @for p in &lib.profiles { (profile_rail_item(p, sel_name == Some(p.name.as_str()))) }
                    }
                }
            }
            section class="profile-main" id="profile-main" {
                @match &selected {
                    Some(detail) => (profile_detail(detail)),
                    None => {
                        @if lib.profiles.is_empty() {
                            @match onboarding {
                                Some(o) => (studio_welcome(o, packs)),
                                None => (profiles_empty_main()),
                            }
                        } @else { (profile_pick_prompt()) }
                    }
                }
            }
        }
    }
}

/// First-launch welcome shown on the Profiles tab when the config is fresh (no
/// profiles and no own fragments): confirm what was detected, explain what a
/// profile is for (and why the overlay is empty), then offer the starter-pack
/// gallery (recommended pack first) or a from-scratch composer.
fn studio_welcome(o: &Onboarding, packs: &[PackView]) -> Markup {
    let scope_label = match o.scope {
        Scope::Repo => "repo",
        Scope::Machine => "machine",
    };
    html! {
        div class="welcome" {
            div class="welcome-head" {
                span class="welcome-wave" { "👋" }
                h1 { "Welcome to Rosita studio" }
            }
            div class="welcome-detect" {
                span class="muted small" { "rosita detected" }
                span class="welcome-chips" {
                    @match &o.stack {
                        Some(s) => span class="target-chip" { (s) },
                        None => span class="target-chip muted" { "no specific stack" },
                    }
                    span class="target-chip" { (scope_label) }
                    @if let Some(b) = &o.branch { span class="target-chip muted" { "branch " (b) } }
                }
            }
            p class="welcome-lead" { "A " strong { "profile" } " decides what guidance your agent gets here. Apply a " strong { "starter pack" } " to get one in a click." }
            p class="muted" { "Each pack copies a curated set of fragments into your library and creates a ready-made profile — all staged; nothing is saved until you Apply. You can customize everything afterward." }
            (legend())
            div class="pack-grid" { @for p in packs { (pack_card(p)) } }
            div class="welcome-actions" {
                button class="btn btn-ghost" hx-get="/profiles/new" hx-target="#main" { (icon("plus")) "Start from scratch" }
            }
        }
    }
}

/// The first-launch welcome as a standalone `#main` fragment — used by the "?"
/// tour button so it's reachable any time, not just on a fresh config.
pub fn welcome_fragment(o: &Onboarding, packs: &[PackView]) -> String {
    studio_welcome(o, packs).into_string()
}

/// One row in the profile rail: name + status + targets + fragment dots.
/// Selecting it swaps the detail into `#profile-main`.
fn profile_rail_item(p: &ProfileView, active: bool) -> Markup {
    let name = p.name.as_str();
    let e = enc(name);
    let mut cls = String::from("rail-item");
    if active {
        cls.push_str(" active");
    }
    if p.disabled {
        cls.push_str(" disabled");
    }
    if p.selected {
        cls.push_str(" bound");
    }
    html! {
        div class=(cls) role="button" tabindex="0" data-profile=(name)
            hx-get=(format!("/profiles/{e}/select")) hx-target="#profile-main" {
            span class="rail-top" {
                span class="rail-name" { (name) }
                @if p.selected { span class="tag bound-tag" { (icon("arrow-right")) "bound" } }
                @else if p.disabled { span class="tag off-tag" { "off" } }
            }
            @if p.targets.is_empty() {
                span class="rail-targets" { span class="target-chip muted" { "no targets" } }
            } @else {
                span class="rail-targets" { @for t in &p.targets { span class="target-chip" { (t) } } }
            }
            span class="rail-foot" {
                @if p.atoms.is_empty() {
                    span class="muted small" { "no fragments" }
                } @else {
                    span class="atoms" { @for a in &p.atoms { (atom_dot(a)) } }
                    span class="muted small" { (p.atoms.len()) }
                }
            }
        }
    }
}

/// The selected profile's detail (fills `#profile-main`): a header with the
/// profile name + actions, a provenance breadcrumb, then one expandable card
/// per composed fragment.
pub fn profile_detail(d: &ProfileDetail) -> Markup {
    let p = d.outcome;
    let name = d.name;
    let e = enc(name);
    let n = p.caps.len();
    html! {
        div class="detail" {
            div class="detail-head" {
                div class="detail-title" {
                    h1 { (name) }
                    @if d.disabled { span class="tag off-tag" { "disabled" } }
                }
                div class="detail-actions" {
                    @if !p.agent.is_empty() { span class="chip chip-agent" title="rendered for this agent" { (p.agent.as_str()) } }
                    button class="toggle" title=(if d.disabled { "Enable profile" } else { "Disable profile" }) aria-label="Toggle profile"
                        hx-post=(format!("/profiles/{e}/disable")) hx-target="#main" {
                        span class=(if d.disabled { "switch off" } else { "switch on" }) {}
                    }
                    button class="icon-btn" title="Edit" aria-label=(format!("Edit {name}"))
                        hx-get=(format!("/profiles/{e}/edit")) hx-target="#main" { (icon("pencil")) }
                    button class="icon-btn danger" title="Delete" aria-label=(format!("Delete {name}"))
                        hx-delete=(format!("/profiles/{e}")) hx-target="#main"
                        hx-confirm=(format!("Stage deletion of profile \"{name}\"?")) { (icon("trash")) }
                }
            }
            div class="provenance" {
                span class="prov-node" { (p.context_summary.as_str()) }
                span class="prov-arrow" { (icon("arrow-right")) }
                span class="prov-node" { (n) " " (if n == 1 { "fragment" } else { "fragments" }) }
                @if p.caps.iter().any(|c| c.dynamic) {
                    span class="prov-spacer" {}
                    button type="button" class="btn btn-ghost btn-sm run-all"
                        title="Run every script/provider in this profile and show the live output it adds"
                        hx-post=(format!("/profiles/{e}/run")) hx-target="#profile-main" {
                        (icon("play")) "Run all scripts"
                    }
                }
            }
            @if let Some(note) = &p.note { p class="note" { (note) } }
            @if p.caps.is_empty() {
                div class="detail-blank" {
                    (icon("eye"))
                    p class="muted" { "This profile composes no guidance for " (p.agent.as_str()) " in this context." }
                }
            } @else {
                div class="detail-doc" { @for c in &p.caps { (preview_fragment_card(c, name, d.expand)) } }
            }
        }
    }
}

pub fn profile_detail_fragment(d: &ProfileDetail) -> String {
    profile_detail(d).into_string()
}

/// One collapsible fragment section inside the profile "document": a compact
/// summary row that, when opened, reveals the fragment's rendered-markdown
/// guidance (the prominent content) plus an "Edit fragment" action.
fn preview_fragment_card(c: &PreviewCap, profile: &str, expand: Expand) -> Markup {
    let glyph = c.glyph;
    // Cards start collapsed on a passive view (the user opens what they care
    // about), but a just-run fragment stays open so its fresh output is visible.
    // `has_output`/`prompt` pick what the body shows once expanded: live output
    // for a dynamic cap that ran, or a centered "Run" prompt for one that hasn't.
    // A dynamic cap can also be run from the summary's corner button.
    let has_output = c.dynamic && !c.pending && !c.skipped;
    let prompt = c.dynamic && c.pending;
    let open = match expand {
        Expand::None => false,
        Expand::One(id) => c.id == id,
        Expand::AllDynamic => c.dynamic,
    };
    let run_url = format!("/fragments/{}/run?profile={}", enc(&c.id), enc(profile));
    html! {
        details class="fragment-detail" open[open] {
            summary class="fragment-detail-head" {
                span class="fragment-glyph" { (icon(glyph)) }
                span class="fragment-detail-title" { (c.title) }
                span class="fragment-detail-id" { (c.id) }
                span class="fragment-detail-spacer" {}
                @if c.dynamic {
                    button type="button" class="btn btn-ghost btn-xs fragment-run"
                        title="Run this script now and show its output"
                        hx-post=(run_url.clone())
                        hx-target="#profile-main" {
                        (icon("play")) (if c.pending { "Run" } else { "Re-run" })
                    }
                }
                @if c.skipped { span class="tag off-tag" { (icon("shield")) "exec off" } }
                span class="fragment-chev" { (icon("chevron-down")) }
            }
            div class="fragment-detail-body" {
                @if has_output {
                    pre class="fragment-output" { (c.markdown) }
                } @else if prompt {
                    // Centered run prompt — clicking it (or the corner button)
                    // re-renders this pane with the script's live output in place.
                    div class="fragment-run-prompt" {
                        button type="button" class="btn btn-primary fragment-run-center"
                            hx-post=(run_url) hx-target="#profile-main" {
                            (icon("play")) "Run script"
                        }
                        p class="run-hint muted" { "Runs this script and shows the live context it adds — output stays cached in the preview." }
                    }
                } @else {
                    div class="markdown-body" { (render_markdown(&c.markdown)) }
                }
                @if c.editable {
                    div class="fragment-detail-foot" {
                        button class="btn btn-ghost btn-sm"
                            hx-get=(format!("/fragments/{}/edit?profile={}", enc(&c.id), enc(profile)))
                            hx-target="#modal" { (icon("pencil")) "Edit fragment" }
                    }
                }
            }
        }
    }
}

fn profiles_empty_main() -> Markup {
    html! {
        div class="detail-blank" {
            (icon("layers"))
            p { "No profiles yet." }
            p class="muted" { "A profile bundles fragments and binds them to a kind of repo." }
            button class="btn btn-primary" hx-get="/profiles/new" hx-target="#main" { (icon("plus")) "Create your first profile" }
        }
    }
}

fn profile_pick_prompt() -> Markup {
    html! {
        div class="detail-blank" {
            (icon("arrow-right"))
            p class="muted" { "Select a profile to see what it composes." }
        }
    }
}

// --- Fragments tab --------------------------------------------------------

/// The Fragments tab: a grid of *your* fragment cards (open a dialog on
/// click). Only owned caps appear here — the shipped palette is a read-only
/// catalog you duplicate from when composing a profile, not an active layer.
pub fn fragments_tab(lib: &LibraryView, flash: Option<&str>) -> Markup {
    html! {
        div class="tab-fragments" {
            div class="dash-head" {
                h1 { "Fragments" }
                div class="head-actions" {
                    (legend())
                    button class="btn btn-primary" hx-get="/fragments/new" hx-target="#modal" { (icon("plus")) "New fragment" }
                }
            }
            @if let Some(msg) = flash { p class="flash" { (icon("check")) (msg) } }
            @if lib.yours.is_empty() {
                div class="empty-card" {
                    p { "No fragments yet." }
                    p class="muted" { "A fragment is a reusable chunk of guidance (or a script) that profiles compose. Write one here, or apply a Starter pack from the Profiles tab to get a curated set plus a ready-made profile." }
                    div class="empty-actions" {
                        button class="btn btn-primary" hx-get="/fragments/new" hx-target="#modal" { (icon("plus")) "Write your first fragment" }
                    }
                }
            } @else {
                @let groups = group_fragments(&lib.yours);
                @if groups.len() <= 1 {
                    div class="fragment-grid" { @for c in &lib.yours { (fragment_card(c)) } }
                } @else {
                    @for (label, caps) in &groups {
                        section class="fragment-group" {
                            h2 class="fragment-group-head" { (label) span class="fragment-group-count" { (caps.len()) } }
                            div class="fragment-grid" { @for c in caps { (fragment_card(c)) } }
                        }
                    }
                }
            }
        }
    }
}

pub fn fragments_tab_fragment(lib: &LibraryView, flash: Option<&str>) -> String {
    fragments_tab(lib, flash).into_string()
}

/// Order known categories sensibly; unknown categories fall after them
/// (alphabetical), and the uncategorized "General" bucket sorts last. Lists the
/// friendly `category` values first, then the legacy first-tag fallback keys.
const CATEGORY_ORDER: &[&str] = &[
    // friendly `category` values (the dedicated field)
    "Operating Style",
    "Local Environment",
    "Stack Conventions",
    "Dev Workflow",
    "Engineering Standards",
    "Quality",
    "Safety",
    "Security",
    // legacy first-tag fallback (fragments with no explicit category)
    "awareness",
    "stack",
    "comms",
    "dev-workflow",
    "quality",
    "infra",
    "safety",
    "security",
];

/// Friendly category names offered as autocomplete in the fragment editor.
const CATEGORY_SUGGESTIONS: &[&str] = &[
    "Operating Style",
    "Local Environment",
    "Stack Conventions",
    "Dev Workflow",
    "Engineering Standards",
    "Quality",
    "Safety",
    "Security",
];

/// A friendly heading for a fragment category (its primary tag).
fn category_label(cat: Option<&str>) -> String {
    match cat {
        Some("stack") => "Stack conventions".to_string(),
        Some("comms") => "Communication".to_string(),
        Some("awareness") => "Awareness".to_string(),
        Some("infra") => "Infrastructure".to_string(),
        Some("safety") => "Safety".to_string(),
        Some("security") => "Security".to_string(),
        Some("quality") => "Quality".to_string(),
        Some("dev-workflow") => "Workflow".to_string(),
        Some(other) => title_case(other),
        None => "General".to_string(),
    }
}

/// "my-tag" → "My tag" for an unmapped category.
fn title_case(s: &str) -> String {
    let spaced = s.replace(['-', '_'], " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => spaced,
    }
}

/// Group fragments by their primary category, in a stable, friendly order.
/// Within a group, the caps keep their library order.
fn group_fragments(caps: &[FragmentView]) -> Vec<(String, Vec<&FragmentView>)> {
    let key_of = |c: &FragmentView| c.category.clone().unwrap_or_default();
    // Distinct keys in first-seen order, then sorted by rank.
    let mut keys: Vec<String> = Vec::new();
    for c in caps {
        let k = key_of(c);
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    keys.sort_by_key(|k| category_rank(k));
    keys.into_iter()
        .map(|k| {
            let label = category_label(if k.is_empty() { None } else { Some(&k) });
            let members: Vec<&FragmentView> = caps.iter().filter(|c| key_of(c) == k).collect();
            (label, members)
        })
        .collect()
}

/// Sort key: known categories by their `CATEGORY_ORDER` index, then unknown
/// categories alphabetically, then the uncategorized "General" bucket last.
fn category_rank(key: &str) -> (u8, String) {
    if key.is_empty() {
        return (3, String::new());
    }
    match CATEGORY_ORDER.iter().position(|&c| c == key) {
        Some(i) => (1, format!("{i:02}")),
        None => (2, key.to_string()),
    }
}

fn fragment_card(c: &FragmentView) -> Markup {
    let id = c.id.as_str();
    let e = enc(id);
    html! {
        div class="fragment-card" hx-get=(format!("/fragments/{e}/edit")) hx-target="#modal" role="button" tabindex="0" {
            span class="fragment-glyph" { (icon(fragment_icon_name(c))) }
            div class="fragment-main" {
                span class="fragment-title" { (c.title) }
                @if let Some(s) = &c.summary { span class="fragment-summary" { (s) } }
                span class="fragment-id" { (id) }
            }
            // The glyph already conveys the type; only flag the exceptions —
            // a private (local.toml) fragment. Shared is the unmarked default.
            @if c.private {
                div class="fragment-tags" {
                    span class="tag" { (icon("lock")) "private" }
                }
            }
        }
    }
}

// --- starter packs + legend --------------------------------------------------

/// A compact, collapsible key to studio's visual language: the type glyphs, the
/// private flag, and the profile/pack atom-dot states.
fn legend() -> Markup {
    html! {
        details class="legend" {
            summary { (icon("eye")) "Legend" }
            div class="legend-body" {
                div class="legend-group" {
                    span class="legend-head" { "Type" }
                    span class="legend-row" { span class="fragment-glyph" { (icon("file")) } "markdown" }
                    span class="legend-row" { span class="fragment-glyph" { (icon("terminal")) } "script" }
                    span class="legend-row" { span class="fragment-glyph" { (icon("bolt")) } "live provider" }
                    span class="legend-row" { span class="tag" { (icon("lock")) "private" } "local.toml" }
                }
                div class="legend-group" {
                    span class="legend-head" { "Fragment dots" }
                    span class="legend-row" { span class="atom owned" {} "owned — composes" }
                    span class="legend-row" { span class="atom palette" {} "palette only" }
                    span class="legend-row" { span class="atom unknown" {} "unknown id" }
                }
            }
        }
    }
}

/// The starter-pack gallery (`#main`): a header, the legend, and a grid of pack
/// cards (recommended first). Applying a card stages the pack's caps + profile.
pub fn packs_gallery(packs: &[PackView]) -> Markup {
    html! {
        div class="tab-packs" {
            div class="dash-head" {
                div class="editor-head" {
                    button type="button" class="icon-btn" title="Back" hx-get="/tab/profiles" hx-target="#main" { (icon("arrow-right")) }
                    h1 { "Starter packs" }
                }
                (legend())
            }
            p class="muted gallery-lead" { "A pack copies a curated set of fragments into your library and creates a ready-made profile — all staged for you to review and Apply. " strong { "Preview" } " any pack first, and customize it freely once added." }
            div class="pack-grid" { @for p in packs { (pack_card(p)) } }
        }
    }
}

pub fn packs_gallery_fragment(packs: &[PackView]) -> String {
    packs_gallery(packs).into_string()
}

/// One starter-pack card: icon + name (+ recommended/applied badge), a short
/// description, the composed fragments as atom dots, and an
/// Apply action (disabled once the pack's profile already exists).
fn pack_card(p: &PackView) -> Markup {
    let e = enc(&p.id);
    let mut cls = String::from("pack-card");
    if p.recommended {
        cls.push_str(" recommended");
    }
    if p.applied {
        cls.push_str(" applied");
    }
    html! {
        div class=(cls) {
            div class="pack-head" {
                span class="pack-glyph" { (icon(&p.icon)) }
                span class="pack-name" { (p.name) }
                @if p.recommended { span class="tag bound-tag" { (icon("check")) "recommended" } }
            }
            p class="pack-desc" { (p.description) }
            div class="pack-foot" {
                span class="atoms" { @for a in &p.atoms { (atom_dot(a)) } }
                span class="muted small" { (p.atoms.len()) " caps" }
                span class="pack-spacer" {}
                button class="btn btn-ghost btn-sm" hx-get=(format!("/packs/{e}/preview")) hx-target="#modal" { (icon("eye")) "Preview" }
                @if p.applied {
                    button class="btn btn-ghost btn-sm" disabled { (icon("check")) "Applied" }
                } @else {
                    button class="btn btn-primary btn-sm" hx-post=(format!("/packs/{e}/apply")) hx-target="#main" { (icon("plus")) "Apply" }
                }
            }
        }
    }
}

/// The starter-pack preview modal: the pack's profile rendered as a full
/// document, each composed fragment demarcated by its glyph + title + id, plus a
/// note that the profile is fully customizable once added.
pub fn pack_preview(pack: &crate::pack::Pack, outcome: &PreviewOutcome) -> String {
    let e = enc(pack.id);
    html! {
        div class="modal-root" {
            div class="modal-backdrop" hx-get="/close" hx-target="#modal" {}
            div class="modal" {
                div class="modal-head" {
                    h2 { "Preview · " (pack.name) }
                    (close_btn())
                }
                div class="modal-body" {
                    p class="muted" {
                        "Applying stages " (outcome.caps.len()) " fragments and the "
                        strong { (pack.profile_name) } " profile. You review the diff before "
                        "anything is saved — and can edit, add, or remove any of it afterward."
                    }
                    @if outcome.caps.is_empty() {
                        p class="empty-card muted" { "This pack composes nothing in the current context." }
                    } @else {
                        div class="pack-preview-doc" {
                            @for c in &outcome.caps {
                                section class="pack-preview-frag" {
                                    div class="pack-preview-frag-head" {
                                        span class="fragment-glyph" { (icon(c.glyph)) }
                                        span class="pack-preview-frag-title" { (c.title) }
                                        span class="fragment-id" { (c.id) }
                                    }
                                    div class="markdown-body" { (render_markdown(&c.markdown)) }
                                }
                            }
                        }
                    }
                }
                div class="modal-foot" {
                    button type="button" class="btn btn-ghost" hx-get="/close" hx-target="#modal" { "Close" }
                    @if !outcome.caps.is_empty() {
                        button type="button" class="btn btn-primary" hx-post=(format!("/packs/{e}/apply")) hx-target="#main" { (icon("plus")) "Apply" }
                    }
                }
            }
        }
    }
    .into_string()
}

// --- guided onboarding beats -------------------------------------------------

/// Pluralize a count: `n` + a singular/plural noun ("1 fragment" / "3 fragments").
fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Beat 2 of the guided first-run: after a starter pack is staged, a friendly
/// "review what will change" summary (counts, not raw diffs) that stresses
/// nothing is written yet — with an escape hatch to the exact unified diff.
pub fn onboarding_review(summary: &crate::studio::state::StagedSummary) -> String {
    html! {
        div class="onboard onboard-review" {
            div class="onboard-head" {
                span class="onboard-badge" { (icon("eye")) }
                h1 { "Review what will change" }
            }
            p class="muted" { "Nothing is written to disk yet. Applying will add:" }
            ul class="onboard-summary" {
                @if summary.fragments_added > 0 {
                    li {
                        span class="fragment-glyph" { (icon("file")) }
                        (plural(summary.fragments_added, "fragment", "fragments"))
                    }
                }
                @for p in &summary.profiles {
                    li {
                        span class="fragment-glyph" { (icon("layers")) }
                        "profile " strong { (p.name) }
                        @if !p.targets.is_empty() {
                            span class="welcome-chips" {
                                @for t in &p.targets { span class="target-chip" { (t) } }
                            }
                        }
                    }
                }
            }
            div class="onboard-actions" {
                button class="btn btn-primary" hx-post="/apply" hx-target="#main" { (icon("check")) "Apply" }
                button class="btn btn-ghost" hx-get="/diff" hx-target="#main" { (icon("eye")) "See exact diff" }
                button class="btn btn-ghost" hx-post="/discard" hx-target="#main"
                    hx-confirm="Discard staged changes and start over?" { (icon("x")) "Start over" }
            }
        }
    }
    .into_string()
}

/// Beat 3 of the guided first-run: after Apply, confirm the setup is live and —
/// the piece that was missing — name the one command that actually uses it
/// (`rosita run <agent>`) plus how to reopen the studio.
pub fn onboarding_done(summary: &crate::studio::state::StagedSummary, agent: &str) -> String {
    let targets: Vec<&String> = summary
        .profiles
        .iter()
        .flat_map(|p| p.targets.iter())
        .collect();
    html! {
        div class="onboard onboard-done" {
            div class="onboard-head" {
                span class="onboard-badge ok" { (icon("check")) }
                h1 { "You're set" }
            }
            p class="welcome-lead" {
                "Your guidance is live. When you launch an AI agent in a matching repo, "
                "rosita injects it automatically — no per-project setup."
            }
            @if !targets.is_empty() {
                div class="welcome-detect" {
                    span class="muted small" { "active for" }
                    span class="welcome-chips" {
                        @for t in &targets { span class="target-chip" { (t) } }
                    }
                }
            }
            div class="cmd-block" {
                span class="muted small" { "Use it in any agent session:" }
                code { "rosita run " (agent) }
            }
            div class="cmd-block" {
                span class="muted small" { "Reopen this studio anytime:" }
                code { "rosita studio" }
            }
            div class="onboard-actions" {
                button class="btn btn-primary" hx-get="/tab/profiles" hx-target="#main" { (icon("arrow-right")) "Explore your setup" }
            }
        }
    }
    .into_string()
}

// --- fragment dialog (modal) ----------------------------------------------

/// The fragment dialog content (swapped into `#modal`). A palette item is
/// read-only with a duplicate action; an advanced cap is read-only with an
/// "edit in TOML" note; otherwise the content-first editor.
pub fn fragment_dialog(
    cap: Option<&Fragment>,
    layer: Layer,
    owned: bool,
    return_profile: Option<&str>,
    used_by: &[String],
) -> String {
    let is_new = cap.is_none();
    let id = cap.map(|c| c.id.as_str()).unwrap_or("");
    let read_only_palette = !is_new && !owned;
    let advanced = cap
        .map(crate::studio::state::is_advanced_fragment)
        .unwrap_or(false);
    // Deleting a composed fragment also cleans it out of the profiles using it;
    // warn up front and name them so it isn't a surprise.
    let delete_confirm = if used_by.is_empty() {
        format!("Delete fragment “{id}”? This stages its removal.")
    } else {
        let names = used_by
            .iter()
            .map(|n| format!("“{n}”"))
            .collect::<Vec<_>>()
            .join(", ");
        let those = if used_by.len() == 1 {
            "that profile"
        } else {
            "those profiles"
        };
        format!(
            "Delete fragment “{id}”? It's composed by {names} — deleting it will also remove it from {those}. This stages all the changes."
        )
    };
    html! {
        div class="modal-backdrop" hx-get="/close" hx-target="#modal" {}
        div class="modal" {
            @if read_only_palette {
                div class="modal-head" { h2 { "Palette fragment" } (close_btn()) }
                div class="modal-body" {
                    p class="hint" { "Starter template. Duplicate “" (id) "” into your library to own and edit it." }
                }
                div class="modal-foot" {
                    button class="btn btn-ghost" hx-get="/close" hx-target="#modal" { "Close" }
                    button class="btn btn-primary" hx-post=(format!("/fragments/{}/duplicate", enc(id))) hx-target="#main" { (icon("copy")) "Duplicate into my library" }
                }
            } @else if advanced {
                div class="modal-head" { h2 { "Advanced fragment" } (close_btn()) }
                div class="modal-body" {
                    p class="hint" { "“" (id) "” uses features the quick editor can't show without dropping one side (a built-in provider, or a script with a custom template). Edit it directly in your config TOML." }
                }
                div class="modal-foot" { button class="btn btn-ghost" hx-get="/close" hx-target="#modal" { "Close" } }
            } @else {
                @let is_script = cap.map(|c| c.command.is_some()).unwrap_or(false);
                @let allow_exec = cap.map(|c| c.allow_exec).unwrap_or(true);
                @let lang = cap.and_then(|c| c.script_lang.as_deref()).unwrap_or("bash");
                form class="fragment-form" hx-post="/fragments" hx-target="#main" {
                    div class="modal-head" {
                        h2 { (if is_new { "New fragment" } else { "Edit fragment" }) }
                        (close_btn())
                    }
                    div class="modal-body" {
                        @if !is_new { input type="hidden" name="id" value=(id); }
                        @if let Some(rp) = return_profile { input type="hidden" name="return_profile" value=(rp); }
                        label class="field grow" { span class="field-label" { "title" }
                            input type="text" name="name" value=(cap.and_then(|c| c.description.as_deref()).unwrap_or(id)) placeholder="Rust conventions" required;
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
                            div class="code-edit-wrap" {
                                pre class="code-hl" aria-hidden="true" { code {} }
                                textarea name="command" rows="7" class="mono code-edit" spellcheck="false" placeholder="echo 'last deploy: green'" { (cap.and_then(|c| c.command.as_deref()).unwrap_or("")) }
                            }
                            div class="script-actions" {
                                label class="check exec-check" { input type="checkbox" name="allow_exec" checked[allow_exec]; span { "Allow execution" } }
                                button type="button" class="btn btn-ghost btn-sm script-try"
                                    hx-post="/fragments/try" hx-target="#script-tryout"
                                    title="Run this script now and show its output (nothing is saved)" {
                                    (icon("play")) "Run"
                                }
                            }
                            // Empty until the user clicks Run; `.script-tryout:empty`
                            // is hidden so this adds no noise to the editor.
                            div id="script-tryout" class="script-tryout" {}
                            p class="hint small" { "The script runs at render and its output is embedded. Uncheck " strong { "Allow execution" } " to keep it from running. " strong { "Run" } " tests it now without saving." }
                        }
                        @let cur_category = cap.and_then(|c| c.category.as_deref()).unwrap_or("");
                        div class="meta-row" {
                            label class="field grow" { span class="field-label" { "category" span class="field-hint" { "groups it in the tree" } }
                                input type="text" name="category" value=(cur_category) placeholder="Operating Style" list="fragment-categories";
                            }
                        }
                        datalist id="fragment-categories" {
                            @for c in CATEGORY_SUGGESTIONS { option value=(c) {} }
                        }
                        (lives_in(layer))
                        @if !is_new {
                            p class="hint small" { "Save updates this fragment in every profile that uses it. Use " strong { "Save as a copy" } " to make a separate version under a new name." }
                        }
                    }
                    div class="modal-foot" {
                        @if !is_new {
                            button type="button" class="btn btn-danger delete-left"
                                hx-delete=(format!("/fragments/{}", enc(id))) hx-target="#main"
                                hx-confirm=(delete_confirm) {
                                (icon("trash")) "Delete"
                            }
                        }
                        button type="button" class="btn btn-ghost" hx-get="/close" hx-target="#modal" { "Cancel" }
                        @if !is_new {
                            button type="button" class="btn" hx-post="/fragments?as=copy" hx-target="#main" { (icon("copy")) "Save as a copy" }
                        }
                        button type="submit" class="btn btn-primary" { (icon("check")) "Save" }
                    }
                }
            }
        }
    }
    .into_string()
}

/// The output panel for a draft script "test run" (swapped into `#script-tryout`).
/// Shows stdout, an exit-code badge, and stderr when present — what the script
/// actually produces, so the user can confirm it works before saving.
pub fn script_tryout(out: &crate::providers::ProviderOutput) -> String {
    // `data` is Null only when the interpreter itself failed to spawn; then the
    // human-readable reason lives in `text`.
    let spawn_err = out.data.is_null();
    let stdout = out
        .data
        .get("stdout")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let stderr = out
        .data
        .get("stderr")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let status = out.data.get("status").and_then(|v| v.as_i64());
    html! {
        div class="tryout" {
            div class="tryout-head" {
                span class="tryout-label" { "Output" }
                @if spawn_err {
                    span class="tryout-status err" { "failed to run" }
                } @else if let Some(code) = status {
                    span class=(if code == 0 { "tryout-status ok" } else { "tryout-status err" }) {
                        "exit " (code)
                    }
                } @else {
                    span class="tryout-status err" { "killed" }
                }
            }
            @if spawn_err {
                pre class="tryout-body err" { (out.text) }
            } @else {
                @if stdout.is_empty() && stderr.is_empty() {
                    p class="tryout-empty muted small" { "(ran, no output)" }
                }
                @if !stdout.is_empty() { pre class="tryout-body" { (stdout) } }
                @if !stderr.is_empty() {
                    div class="tryout-stderr small muted" { "stderr" }
                    pre class="tryout-body err" { (stderr) }
                }
            }
        }
    }
    .into_string()
}

/// Shown when Run is clicked with an empty script.
pub fn script_tryout_empty() -> String {
    html! { p class="tryout-empty muted small" { "Nothing to run — the script is empty." } }
        .into_string()
}

fn close_btn() -> Markup {
    html! { button class="icon-btn" type="button" title="Close" aria-label="Close" hx-get="/close" hx-target="#modal" { (icon("x")) } }
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

/// The full-width profile editor: a form (left) with name, targets, a fragment
/// picker, an inline quick-create, and a live preview (right). `draft` carries
/// the in-progress values (so an inline add re-renders without losing state).
pub fn profile_editor(
    draft: &ProfileConfig,
    is_new: bool,
    lib: &LibraryView,
    preview: &PreviewOutcome,
    error: Option<&str>,
) -> String {
    let name = draft.name.as_str();
    let selected: Vec<&str> = draft.fragments.iter().map(|r| r.id()).collect();
    let chosen = |id: &str| selected.contains(&id);
    html! {
        div class="profile-editor" {
            form class="editor-form" hx-post="/profiles/preview" hx-trigger="change delay:200ms" hx-target="#editor-preview" {
                @if !is_new { input type="hidden" name="new" value="0"; } @else { input type="hidden" name="new" value="1"; }
                div class="editor-head" {
                    button type="button" class="icon-btn" title="Back" hx-get="/tab/profiles" hx-target="#main" { (icon("arrow-right")) }
                    h1 { (if is_new { "New profile" } else { "Edit profile" }) }
                }
                @if let Some(err) = error {
                    div class="banner error" { span class="banner-icon" { (icon("alert")) } div class="banner-body" { (err) } }
                }
                label class="field" { span class="field-label" { "name" span class="field-hint" { "required" } }
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
                fieldset class="fragment-picker" {
                    legend { "Fragments" span class="field-hint" { "tick the ones to compose" } }
                    div class="pick-list" {
                        @for c in &lib.yours {
                            label class="pick" {
                                input type="checkbox" name="fragments" value=(c.id.as_str()) checked[chosen(c.id.as_str())];
                                span class="pick-glyph" { (icon(fragment_icon_name(c))) }
                                span class="pick-main" { span class="pick-title" { (c.title) } span class="pick-id" { (c.id.as_str()) } }
                            }
                        }
                    }
                    (inline_new_cap())
                }
                fieldset class="lives-in" {
                    legend { "Where it lives" }
                    p class="hint small" { "Global — every repo can use it; the profile whose targets match a repo binds there." }
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

/// The collapsible inline "new fragment" mini-form inside the profile editor.
/// Its fields are `fragment_*`-namespaced so they don't collide with the profile form;
/// "Add" posts the whole editor form to `/profiles/draft`.
fn inline_new_cap() -> Markup {
    html! {
        details class="inline-cap" {
            summary { (icon("plus")) "New fragment" }
            div class="inline-grid" {
                label class="field" { span class="field-label" { "title" }
                    input type="text" name="fragment_name" placeholder="New fragment";
                }
                div class="seg seg-sm" {
                    input type="radio" name="fragment_kind" id="fragment-kind-md" value="markdown" checked;
                    label class="seg-opt" for="fragment-kind-md" { "Markdown" }
                    input type="radio" name="fragment_kind" id="fragment-kind-sc" value="script";
                    label class="seg-opt" for="fragment-kind-sc" { "Script" }
                }
                label class="field" { span class="field-label" { "content" }
                    textarea name="fragment_content" rows="3" placeholder="Guidance markdown, or the script body." {}
                }
                label class="check" { input type="checkbox" name="fragment_private"; span { "private (local.toml)" } }
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
            span class="prov-node" { (p.fragment_count) " " (if p.fragment_count == 1 { "fragment" } else { "fragments" }) }
        }
        @if let Some(note) = &p.note { p class="note" { (note) } }
        div class="markdown-body" { (render_markdown(&p.overlay)) }
    }
}

pub fn editor_preview_fragment(p: &PreviewOutcome) -> String {
    editor_preview(p).into_string()
}

// --- diff / review -----------------------------------------------------------

pub fn diff_view(
    diffs: &[FileDiff],
    leaks: &[String],
    fs_changed: &[std::path::PathBuf],
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
                    button type="button" class="btn btn-danger discard-left" hx-post="/discard" hx-target="#main"
                        hx-confirm="Discard all staged changes? Your config files won't be modified." { (icon("x")) "Discard all" }
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

/// A fragment mutation: re-render the Fragments tab into `#main`, close the
/// modal, and refresh the staged indicator. (`flash` keeps the "staged …" note.)
pub fn fragment_result(lib: &LibraryView, flash: &str) -> String {
    html! {
        (fragments_tab(lib, Some(flash)))
        (modal_close())
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
    html! { (DOCTYPE) html { head { title { "Rosita studio — error" } } body { pre class="error" { (msg) } } } }.into_string()
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

/// A status chip for whether the previewed profile would be auto-selected in the
/// context it's rendered for. Names nothing (the surrounding view already names
/// the profile), so it reads as a state, not a label.
fn binding_chip(b: &BindingState) -> Markup {
    match b {
        BindingState::Bound(_) => {
            html! { span class="chip chip-bound" title="binds in this context" { (icon("arrow-right")) "binds here" } }
        }
        BindingState::None => html! { span class="chip chip-none" { "not selected here" } },
        BindingState::Ambiguous(n) => {
            html! { span class="chip chip-ambiguous" { (icon("alert")) (n) " match" } }
        }
    }
}

fn atom_dot(a: &AtomDot) -> Markup {
    let (cls, tip) = match a.state {
        AtomState::Owned => ("atom owned".to_string(), format!("{} — composed", a.id)),
        AtomState::Palette => (
            "atom palette".to_string(),
            format!("{} — palette only (not duplicated)", a.id),
        ),
        AtomState::Unknown => (
            "atom unknown".to_string(),
            format!("{} — unknown fragment", a.id),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cv(id: &str, category: Option<&str>) -> FragmentView {
        FragmentView {
            id: id.into(),
            title: id.into(),
            summary: None,
            kind: "static",
            category: category.map(str::to_string),
            script_lang: None,
            private: false,
            active: false,
        }
    }

    #[test]
    fn shell_has_capitalized_brand_and_theme_toggle() {
        let html = shell(maud::html! {}, 0, "fragments");
        // Wordmark + page title are capitalized; the lowercase command name is
        // not what the chrome shows.
        assert!(html.contains(r#"<span class="brand-name">Rosita</span>"#));
        assert!(html.contains("Rosita studio"));
        // Right-side controls are grouped so the nav tabs can center.
        assert!(html.contains(r#"class="topbar-right""#));
        // Theme toggle button with all three preference glyphs present.
        assert!(html.contains(r#"id="theme-toggle""#));
        assert!(html.contains("ti-auto"));
        assert!(html.contains("ti-light"));
        assert!(html.contains("ti-dark"));
    }

    #[test]
    fn shell_inlines_no_flash_theme_init() {
        let html = shell(maud::html! {}, 0, "fragments");
        // The inline head script must set the resolved theme + preference before
        // the stylesheet link, so there's no dark→light flash on load. (The
        // attribute is set at runtime via `dataset.theme`; it isn't in the SSR
        // markup, so assert on the script's own tokens instead.)
        assert!(html.contains("dataset.theme"));
        assert!(html.contains("prefers-color-scheme"));
        let init = html.find("rosita-theme").expect("theme init present");
        let css = html.find("studio.css").expect("stylesheet link present");
        assert!(
            init < css,
            "theme init must run before the stylesheet paints"
        );
    }

    #[test]
    fn fragments_group_in_friendly_order() {
        let caps = vec![
            cv("a", Some("comms")),
            cv("b", None),
            cv("c", Some("stack")),
            cv("d", Some("awareness")),
            cv("e", Some("stack")),
            cv("f", Some("zebra-custom")),
        ];
        let groups = group_fragments(&caps);
        let labels: Vec<&str> = groups.iter().map(|(l, _)| l.as_str()).collect();
        // Known categories in CATEGORY_ORDER, then unknown categories (alpha),
        // then the uncategorized "General" bucket last.
        assert_eq!(
            labels,
            vec![
                "Awareness",
                "Stack conventions",
                "Communication",
                "Zebra custom",
                "General"
            ]
        );
        // A group keeps its members in library order.
        let stack = groups
            .iter()
            .find(|(l, _)| l == "Stack conventions")
            .unwrap();
        let ids: Vec<&str> = stack.1.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["c", "e"]);
    }

    #[test]
    fn category_label_titlecases_unknown_tags() {
        assert_eq!(category_label(Some("stack")), "Stack conventions");
        assert_eq!(category_label(Some("my-custom_tag")), "My custom tag");
        assert_eq!(category_label(None), "General");
    }

    #[test]
    fn friendly_categories_group_by_name_in_logical_order() {
        // The dedicated `category` field carries friendly names; they keep their
        // own label and sort in CATEGORY_ORDER before the legacy tag fallback.
        let caps = vec![
            cv("a", Some("Engineering Standards")),
            cv("b", Some("Operating Style")),
            cv("c", Some("Local Environment")),
        ];
        let groups = group_fragments(&caps);
        let labels: Vec<&str> = groups.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Operating Style",
                "Local Environment",
                "Engineering Standards"
            ]
        );
    }
}
