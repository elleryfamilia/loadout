//! `load plan` — deterministic plan-visualizer pipeline.
//!
//! An agent emits `plan.json` (schema: [`model`]); loadout validates and
//! renders it to a self-contained `plan.html`. See
//! `.loadout/workflow/artifacts/design-plan-visualizer.md` for the design.

pub mod model;
pub mod svg;
