//! The plan viewer's closed icon vocabulary: a 16-name subset of Lucide
//! icons, vendored verbatim under `vendored/lucide/` (see
//! `vendored/sources.toml` for provenance/license). `Phase.icon` and
//! `PlanTask.icon` (see `plan::model`) validate against exactly this list —
//! an unknown name is a hard error (`unknown_icon`) whose hint names every
//! icon here, so an agent authoring `plan.json` always sees the full menu.
//!
//! This module only exposes lookups over static, compile-time-embedded data
//! (`include_str!`) — no filesystem access at runtime, so a rendered
//! `plan.html` stays self-contained and byte-stable for a given input.

/// The vocabulary, alphabetical — also the order `icon_names()` returns and
/// the order an `unknown_icon` hint lists them in.
const NAMES: &[&str] = &[
    "book-open",
    "bug",
    "database",
    "file-text",
    "flask-conical",
    "git-branch",
    "globe",
    "layout-dashboard",
    "package",
    "paintbrush",
    "rocket",
    "search",
    "shield",
    "terminal",
    "wrench",
    "zap",
];

/// The full vocabulary a `Phase.icon`/`PlanTask.icon` value must be one of.
pub fn icon_names() -> &'static [&'static str] {
    NAMES
}

/// The vendored SVG source for `name`, verbatim as downloaded (see
/// `vendored/lucide/<name>.svg`) — `None` if `name` isn't in the vocabulary.
/// This is the *whole* `<svg>…</svg>` document, upstream `width`/`height`
/// attributes included; a caller that wants to embed it inline at a
/// different size (the plan renderer) strips the outer element itself —
/// see `render::icon_markup`.
pub fn icon_svg(name: &str) -> Option<&'static str> {
    match name {
        "book-open" => Some(include_str!("../../vendored/lucide/book-open.svg")),
        "bug" => Some(include_str!("../../vendored/lucide/bug.svg")),
        "database" => Some(include_str!("../../vendored/lucide/database.svg")),
        "file-text" => Some(include_str!("../../vendored/lucide/file-text.svg")),
        "flask-conical" => Some(include_str!("../../vendored/lucide/flask-conical.svg")),
        "git-branch" => Some(include_str!("../../vendored/lucide/git-branch.svg")),
        "globe" => Some(include_str!("../../vendored/lucide/globe.svg")),
        "layout-dashboard" => Some(include_str!("../../vendored/lucide/layout-dashboard.svg")),
        "package" => Some(include_str!("../../vendored/lucide/package.svg")),
        "paintbrush" => Some(include_str!("../../vendored/lucide/paintbrush.svg")),
        "rocket" => Some(include_str!("../../vendored/lucide/rocket.svg")),
        "search" => Some(include_str!("../../vendored/lucide/search.svg")),
        "shield" => Some(include_str!("../../vendored/lucide/shield.svg")),
        "terminal" => Some(include_str!("../../vendored/lucide/terminal.svg")),
        "wrench" => Some(include_str!("../../vendored/lucide/wrench.svg")),
        "zap" => Some(include_str!("../../vendored/lucide/zap.svg")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_vocabulary_name_resolves() {
        for name in icon_names() {
            assert!(
                icon_svg(name).is_some(),
                "vocabulary lists `{name}` but icon_svg has no matching arm"
            );
        }
    }

    /// The vendored files are static, trusted assets we ship — not
    /// user-controlled input — which is what lets the renderer `PreEscaped`
    /// them. This is the sanity check backing that assumption: verbatim
    /// downloads from an external repo, so assert none of them smuggle a
    /// `<script` element before we ever trust one enough to inline it.
    #[test]
    fn vendored_svgs_carry_no_script_tags() {
        for name in icon_names() {
            let svg = icon_svg(name).expect("every vocabulary name resolves (see above)");
            assert!(
                !svg.to_lowercase().contains("<script"),
                "{name}: vendored SVG contains a <script tag"
            );
            assert!(
                svg.trim_start().starts_with("<svg"),
                "{name}: doesn't start with <svg"
            );
            assert!(
                svg.trim_end().ends_with("</svg>"),
                "{name}: doesn't end with </svg>"
            );
        }
    }

    #[test]
    fn unknown_name_resolves_to_none() {
        assert!(icon_svg("not-a-real-icon").is_none());
    }
}
