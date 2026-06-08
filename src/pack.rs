//! Starter packs — curated bundles of palette fragments plus one ready-made,
//! self-contained profile. Packs are how rosita ships *default profiles* without
//! breaking the "own your config, no magic" model: applying a pack stages the
//! same edits you'd make by hand — it duplicates each fragment from the
//! read-only [`palette`](crate::fragment::palette) into your own config, then
//! creates the profile that composes them. Nothing is auto-active; everything is
//! staged → reviewed → applied like any other studio edit.
//!
//! Because composition is one-profile-per-repo (no profile stacking), every
//! pack's profile is **self-contained**: the shared "everyday" essentials are
//! baked into each one. Duplicating an already-owned fragment is a no-op, so
//! applying several packs never conflicts.

use crate::profile::{FragmentRef, ProfileConfig};

/// A curated starter bundle: fragments to duplicate + a profile to create.
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
    /// The palette fragment ids this pack duplicates into the library *and*
    /// composes into the profile, in this order. Every id must exist in
    /// [`palette`](crate::fragment::palette) — guarded by a test.
    pub fragments: &'static [&'static str],
}

impl Pack {
    /// The self-contained profile this pack creates (composes every `fragments` id, in
    /// order). `origin`/layer is assigned when the staged config is assembled.
    pub fn profile(&self) -> ProfileConfig {
        ProfileConfig {
            name: self.profile_name.to_string(),
            targets: self.targets.iter().map(|s| s.to_string()).collect(),
            fragments: self
                .fragments
                .iter()
                .map(|s| FragmentRef::Id(s.to_string()))
                .collect(),
            template: None,
            disabled: false,
        }
    }

    /// Whether `target` (a detected stack key or `machine`) makes this the
    /// recommended pack for the current context.
    pub fn is_recommended_for(&self, target: &str) -> bool {
        self.recommended_for.contains(&target)
    }
}

// Each pack's fragment set is spelled out below (a stack cap + the shared
// "everyday" essentials) so each profile is self-contained; the integrity tests
// keep the shared tail consistent across the stack packs.
const EVERYDAY: &[&str] = &[
    // Static guidance essentials.
    "terse-comms",
    "conventional-commits",
    "baseline",
    "ask-before-risky",
    "secrets-hygiene",
    "validate-before-done",
    "infra-caution",
    // Live machine grounding (dynamic probes). `environment` frames the probed
    // sections that follow; each script embeds its redacted stdout at render
    // and degrades to nothing when its tool/daemon is absent. `tailnet` (full
    // Tailscale peer dump) is left out of the default — it ships in the palette
    // for those who want to pick it.
    "environment",
    "host",
    "toolchain",
    "containers",
    "ai-tools",
    "vpn-posture",
    "secrets-posture",
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
            description: "Safe, sensible defaults for general or no-repo work — terse \
                          communication, conventional commits, secrets discipline, ask \
                          before risky actions, validate-before-done — plus live machine \
                          grounding (host, toolchain, containers, AI tools, VPN/egress, \
                          and secret-store posture) probed fresh at render.",
            icon: "shield",
            recommended_for: &["machine"],
            profile_name: "everyday",
            targets: &["machine"],
            fragments: EVERYDAY,
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
            fragments: RUST,
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
            fragments: NODE,
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
            fragments: NEXTJS,
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
            fragments: GO,
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
            fragments: PYTHON,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fragment::palette;
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
    fn every_pack_fragment_exists_in_the_palette() {
        let palette_ids: HashSet<String> = palette().into_iter().map(|c| c.id).collect();
        for p in packs() {
            assert!(!p.fragments.is_empty(), "pack {} has no fragments", p.id);
            for cap in p.fragments {
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
            for cap in p.fragments {
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
            let prof_caps: Vec<&str> = prof.fragments.iter().map(|r| r.id()).collect();
            let pack_caps: Vec<&str> = p.fragments.to_vec();
            assert_eq!(
                prof_caps, pack_caps,
                "pack {} profile must compose exactly its fragments in order",
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
                    p.fragments.contains(essential),
                    "stack pack {} is missing essential {essential}",
                    p.id
                );
            }
        }
    }
}
