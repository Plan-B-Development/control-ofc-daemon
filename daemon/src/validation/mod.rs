//! AIO-MB Phase 5 — validation sessions, evidence collection, and result semantics.
//!
//! A validation session records what an already-configured cooling device
//! actually did, and produces typed, machine-readable evidence about it. The
//! four modules split along one axis — who is allowed to have side effects:
//!
//! | Module | Side effects |
//! |---|---|
//! | [`session`] | none — pure data model and stable tokens |
//! | [`summary`] | none — pure derivation of §8's findings |
//! | [`store`] | filesystem only, under `{state_dir}/validation/` |
//! | [`recorder`] | reads live state; **never writes hardware** |
//!
//! # The safety posture, stated once
//!
//! The engine is an **observer that may orchestrate**. It samples state the poll
//! loop already collects and, where a session asked for one, invokes the
//! **existing** Phase 3 characterisation or PWM verify. It acquires no second PWM
//! ownership path (§2) and contains no code that writes a duty. Everything that
//! drives hardware here is machinery that already owned the hwmon lease, the pump
//! floor clamp, the thermal guard and restore-on-drop before this phase existed.
//!
//! Consequently: a validation session cannot lower a floor, cannot stop a pump,
//! and cannot outlive the safety rules that were already in force.

pub mod recorder;
pub mod session;
pub mod store;
pub mod summary;
