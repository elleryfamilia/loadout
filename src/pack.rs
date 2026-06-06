//! Starter packs — curated bundles of palette capabilities plus one ready-made,
//! self-contained profile. Packs are how rosita ships *default profiles* without
//! breaking the "own your config, no magic" model: applying a pack stages the
//! same edits you'd make by hand — it duplicates each capability from the
//! read-only [`palette`](crate::capability::palette) into your own config, then
//! creates the profile that composes them. Nothing is auto-active; everything is
//! staged → reviewed → applied like any other studio edit.
//!
//! Because composition is one-profile-per-repo (no profile stacking), every
//! pack's profile is **self-contained**: the shared "everyday" essentials are
//! baked into each one. Duplicating an already-owned capability is a no-op, so
//! applying several packs never conflicts.

use crate::profile::{CapabilityRef, ProfileConfig};

/// A curated starter bundle: capabilities to duplicate + a profile to create.
#[derive(Debug, Clone)]
pub struct Pack {
    /// Stable pack id (also the `/packs/<id>/apply` route segment).
    pub id: &'static str,
    /// Display name for the gallery card.
    pub name: &'static str,
    /// One-line description for the gallery card.
    pub description: &'static str,
    /// Curated icon (from studio's icon set) for the gallery card.
    pub icon: &'static str,
    /// Detected stack/scope ids that make this the recommended pack (e.g. `rust`,
    /// `machine`). Drives the "recommended" badge + ordering on the gallery.
    pub recommended_for: &'static [&'static str],
    /// The name of the profile this pack creates.
    pub profile_name: &'static str,
    /// The created profile's selection targets.
    pub targets: &'static [&'static str],
    /// The palette capability ids this pack duplicates into the library *and*
    /// composes into the profile, in this order. Every id must exist in
    /// [`palette`](crate::capability::palette) — guarded by a test.
    pub caps: &'static [&'static str],
}

impl Pack {
    /// The self-contained profile this pack creates (composes every `caps` id, in
    /// order). `origin`/layer is assigned when the staged config is assembled.
    pub fn profile(&self) -> ProfileConfig {
        ProfileConfig {
            name: self.profile_name.to_string(),
            targets: self.targets.iter().map(|s| s.to_string()).collect(),
            capabilities: self
                .caps
                .iter()
                .map(|s| CapabilityRef::Id(s.to_string()))
                .collect(),
            template: None,
            guidance: None,
            disabled: false,
        }
    }

    /// Whether `target` (a detected stack key or `machine`) makes this the
    /// recommended pack for the current context.
    pub fn is_recommended_for(&self, target: &str) -> bool {
        self.recommended_for.contains(&target)
    }
}

// Each pack's capability set is spelled out below (a stack cap + the shared
// "everyday" essentials) so each profile is self-contained; the integrity tests
// keep the shared tail consistent across the stack packs.
const EVERYDAY: &[&str] = &[
    "terse-comms",
    "conventional-commits",
    "baseline",
    "ask-before-risky",
    "secrets-hygiene",
    "validate-before-done",
    "infra-caution",
];

const RUST: &[&str] = &[
    "rust-conventions",
    "baseline",
    "terse-comms",
    "conventional-commits",
    "branch-discipline",
    "secrets-hygiene",
    "ask-before-risky",
    "validate-before-done",
    "testing-discipline",
];
const NODE: &[&str] = &[
    "node-conventions",
    "baseline",
    "terse-comms",
    "conventional-commits",
    "branch-discipline",
    "secrets-hygiene",
    "ask-before-risky",
    "validate-before-done",
    "testing-discipline",
];
const NEXTJS: &[&str] = &[
    "nextjs-conventions",
    "baseline",
    "terse-comms",
    "conventional-commits",
    "branch-discipline",
    "secrets-hygiene",
    "ask-before-risky",
    "validate-before-done",
    "testing-discipline",
];
const GO: &[&str] = &[
    "go-conventions",
    "baseline",
    "terse-comms",
    "conventional-commits",
    "branch-discipline",
    "secrets-hygiene",
    "ask-before-risky",
    "validate-before-done",
    "testing-discipline",
];
const PYTHON: &[&str] = &[
    "python-conventions",
    "baseline",
    "terse-comms",
    "conventional-commits",
    "branch-discipline",
    "secrets-hygiene",
    "ask-before-risky",
    "validate-before-done",
    "testing-discipline",
];

/// The shipped starter packs, in gallery display order (the stack-agnostic
/// "everyday" base first, then the per-stack packs).
pub fn packs() -> Vec<Pack> {
    vec![
        Pack {
            id: "everyday",
            name: "Everyday essentials",
            description: "Safe, sensible defaults for general or no-repo work: terse \
                          communication, conventional commits, secrets discipline, ask \
                          before risky actions, and validate-before-done.",
            icon: "shield",
            recommended_for: &["machine"],
            profile_name: "everyday",
            targets: &["machine"],
            caps: EVERYDAY,
        },
        Pack {
            id: "rust",
            name: "Rust",
            description: "Rust conventions (cargo, clippy, rustfmt) on top of the everyday \
                          safety, commit, and quality essentials.",
            icon: "code",
            recommended_for: &["rust"],
            profile_name: "rust",
            targets: &["rust"],
            caps: RUST,
        },
        Pack {
            id: "node",
            name: "Node.js / TypeScript",
            description: "Node.js conventions (pnpm, TypeScript) plus the everyday safety, \
                          commit, and quality essentials.",
            icon: "code",
            recommended_for: &["node"],
            profile_name: "node",
            targets: &["node"],
            caps: NODE,
        },
        Pack {
            id: "nextjs",
            name: "Next.js",
            description: "Next.js conventions (router + server/client boundaries, pnpm) plus \
                          the everyday safety, commit, and quality essentials.",
            icon: "code",
            recommended_for: &["nextjs"],
            profile_name: "nextjs",
            targets: &["nextjs"],
            caps: NEXTJS,
        },
        Pack {
            id: "go",
            name: "Go",
            description: "Go conventions (standard toolchain + golangci-lint) plus the \
                          everyday safety, commit, and quality essentials.",
            icon: "code",
            recommended_for: &["go"],
            profile_name: "go",
            targets: &["go"],
            caps: GO,
        },
        Pack {
            id: "python",
            name: "Python",
            description: "Python conventions (uv, ruff, pytest) plus the everyday safety, \
                          commit, and quality essentials.",
            icon: "code",
            recommended_for: &["python"],
            profile_name: "python",
            targets: &["python"],
            caps: PYTHON,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::palette;
    use std::collections::HashSet;

    /// The shared "everyday" essentials every stack pack must bake in to stay
    /// self-contained (no profile stacking exists).
    const STACK_TAIL: &[&str] = &[
        "baseline",
        "terse-comms",
        "conventional-commits",
        "branch-discipline",
        "secrets-hygiene",
        "ask-before-risky",
        "validate-before-done",
        "testing-discipline",
    ];

    #[test]
    fn pack_ids_and_profile_names_are_unique() {
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        for p in packs() {
            assert!(ids.insert(p.id), "duplicate pack id {}", p.id);
            assert!(
                names.insert(p.profile_name),
                "duplicate pack profile name {}",
                p.profile_name
            );
        }
    }

    #[test]
    fn every_pack_cap_exists_in_the_palette() {
        let palette_ids: HashSet<String> = palette().into_iter().map(|c| c.id).collect();
        for p in packs() {
            assert!(!p.caps.is_empty(), "pack {} has no caps", p.id);
            for cap in p.caps {
                assert!(
                    palette_ids.contains(*cap),
                    "pack {} references unknown palette cap {cap}",
                    p.id
                );
            }
        }
    }

    #[test]
    fn pack_caps_have_no_duplicates() {
        for p in packs() {
            let mut seen = HashSet::new();
            for cap in p.caps {
                assert!(seen.insert(*cap), "pack {} lists cap {cap} twice", p.id);
            }
        }
    }

    #[test]
    fn pack_profile_composes_exactly_its_caps() {
        for p in packs() {
            let prof = p.profile();
            assert_eq!(prof.name, p.profile_name);
            assert!(
                !prof.targets.is_empty(),
                "pack {} profile has no targets",
                p.id
            );
            let prof_caps: Vec<&str> = prof.capabilities.iter().map(|r| r.id()).collect();
            let pack_caps: Vec<&str> = p.caps.to_vec();
            assert_eq!(
                prof_caps, pack_caps,
                "pack {} profile must compose exactly its caps in order",
                p.id
            );
        }
    }

    #[test]
    fn stack_packs_bake_in_the_everyday_tail() {
        // Each stack pack must include every shared essential (self-contained).
        for p in packs().into_iter().filter(|p| p.id != "everyday") {
            for essential in STACK_TAIL {
                assert!(
                    p.caps.contains(essential),
                    "stack pack {} is missing essential {essential}",
                    p.id
                );
            }
        }
    }
}
