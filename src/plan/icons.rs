//! The plan viewer's closed `icon` vocabulary: 16 names that `Phase.icon`
//! and `PlanTask.icon` (see `plan::model`) validate against. An unknown name
//! is a hard error (`unknown_icon`) whose hint lists every name here, so an
//! agent authoring `plan.json` always sees the full menu.
//!
//! **Nothing draws these.** The viewer's redesign carries hierarchy with
//! type, rules and colour rather than glyphs, so the renderer emits no
//! `<svg>` icons and the `<details>` disclosure marker is a text character.
//! The Lucide SVGs that used to back these names were vendored under
//! `vendored/lucide/`; they were removed once nothing referenced them, along
//! with the lookup that inlined them.
//!
//! What survives is the NAME LIST, and it survives on purpose. `plan.json`
//! files in the wild set `icon`, and dropping the field would turn every one
//! of them into a validation error. Keeping it a *closed* vocabulary is also
//! what keeps it from becoming an injection point: the value is checked
//! against this list and then ignored, never interpolated into the page. If
//! drawing ever returns, it re-vendors assets keyed by these same names —
//! a rendering change and nothing more.

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

/// Whether `name` is in the vocabulary — the whole of what validation needs,
/// now that nothing resolves a name to an asset.
pub fn is_icon_name(name: &str) -> bool {
    NAMES.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocabulary_is_sorted_and_free_of_duplicates() {
        // The list doubles as `unknown_icon`'s hint text, which a human
        // reads; alphabetical is the only order that stays scannable as it
        // changes, and a duplicate would print twice in that hint.
        let mut sorted = icon_names().to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.as_slice(), icon_names(), "vocabulary drifted");
        assert_eq!(icon_names().len(), 16);
    }

    #[test]
    fn membership_matches_the_vocabulary() {
        for name in icon_names() {
            assert!(is_icon_name(name), "{name} is listed but not recognised");
        }
        assert!(!is_icon_name("not-a-real-icon"));
        assert!(!is_icon_name(""));
        // Never part of the author-facing vocabulary: it was fixed UI chrome
        // back when the disclosure marker was an SVG, not something a
        // plan.json could select.
        assert!(!is_icon_name("chevron-right"));
    }
}
