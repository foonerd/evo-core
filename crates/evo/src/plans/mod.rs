// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Listening-plans substrate.
//!
//! Plans-as-data: an operator-defined or vendor-shipped plan is
//! pure data the framework reads from disk, validates, and (in
//! Phase 2) executes through trigger registration + verb dispatch
//! against source plugins. This module owns the storage layer:
//! the trait that abstracts plan-store access plus a filesystem-
//! backed and an in-memory backend.
//!
//! Plan _definitions_ live on disk as TOML; the framework reads
//! them at boot, registers triggers based on each plan's
//! [`evo_plugin_sdk::contract::PlanTrigger`], and dispatches verbs
//! when triggers fire. Runtime plan _state_ (active / pre-empted
//! snapshots) flows through the existing subject-state substrate
//! (`subject_states` table), not through plan storage — plan
//! storage only persists the plan schema, not the engine's
//! execution state.
//!
//! ## Substrate cut
//!
//! Phase 1 (this module) lands the storage primitive: a trait,
//! a filesystem implementation, and an in-memory implementation
//! used by tests. Phase 2 lands the engine that consumes the
//! storage layer to drive plan execution.

pub mod engine;
pub mod storage;
pub mod wizard;

pub use engine::{
    PlanEngine, PlanEngineError, PlanRegistration, PlanTerminalObserver,
    PlanTerminalOutcome,
};
pub use storage::{
    FilesystemPlanStorage, InMemoryPlanStorage, PlanStorage, PlanStorageError,
};
pub use wizard::{
    load_and_register_wizard_plan, WizardLoadError, WizardRuntime,
    WizardTerminalObserver,
};
