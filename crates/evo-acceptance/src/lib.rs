#![allow(missing_docs)]
//! Adaptive acceptance harness: target descriptor + scenario plan in,
//! readiness report out.
//!
//! The library exposes the data types (target descriptor, scenario
//! plan, scenario, verdict, readiness report) and the runner that
//! sequences scenarios against a connection. The bin (`evo-acceptance
//! run`) is a thin clap wrapper around the runner.
//!
//! Vendors building distributions extend the harness for their own
//! targets by writing a target descriptor against `TargetDescriptor`,
//! authoring a scenario plan against `ScenarioPlan`, and invoking
//! `Runner::run` either via the bin or against the lib directly.

pub mod connection;
pub mod discrepancy;
pub mod error;
pub mod inspect_cmd;
pub mod inspector;
pub mod plan;
pub mod report;
pub mod runner;
pub mod scenario;
pub mod target;

pub use crate::error::{HarnessError, Result};
pub use crate::plan::{ScenarioPlan, Tier};
pub use crate::report::{ReadinessReport, Verdict};
pub use crate::runner::{RunOptions, Runner};
pub use crate::scenario::{
    Assertion, Scenario, ScenarioOutcome, ScenarioStatus,
};
pub use crate::target::{Capability, ConnectionType, TargetDescriptor};
