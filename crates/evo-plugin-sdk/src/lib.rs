//! # evo-plugin-sdk
//!
//! SDK for authoring plugins on the evo steward fabric.
//!
//! This crate provides the Rust-side definitions of the contracts declared in
//! the evo-core engineering documents:
//!
//! - [`manifest`]: Types modelling the plugin manifest format documented in
//!   `docs/engineering/PLUGIN_PACKAGING.md` section 2. TOML parsing and
//!   serialisation for plugin artefact manifests.
//! - [`contract`]: Rust traits every plugin implements, per
//!   `docs/engineering/PLUGIN_CONTRACT.md` sections 2 through 7. Feature-
//!   gated on `contract` (enabled by default).
//! - [`wire`]: (placeholder) Wire-protocol message types for out-of-process
//!   plugins, per `PLUGIN_CONTRACT.md` sections 6, 9, 10. Populated in SDK
//!   pass 3.
//! - [`codec`]: (placeholder) JSON and CBOR codec implementations of the wire
//!   schema. Populated in SDK pass 3.
//! - [`testing`]: (placeholder) Mock steward harness for plugin authors.
//!   Populated in SDK pass 4.
//!
//! The plugin runtime contract, artefact contract, vendor contract, and
//! logging contract are authoritative specifications; this crate is the
//! mechanical representation of those specifications in Rust. When the crate
//! and the documents disagree, the documents are authoritative and the crate
//! is wrong.
//!
//! ## Feature flags
//!
//! - `contract` (default): enables the [`contract`] module and its
//!   dependency on `tracing`.
//! - `manifest`: always enabled; the manifest module is the SDK's
//!   foundation and has no optional dependencies.
//!
//! Consumers that only need manifest parsing - for example, a CLI manifest
//! validator - can opt out of the contract module with
//! `default-features = false` in their `Cargo.toml`.
//!
//! ## Versioning
//!
//! This crate follows semver. Major version bumps indicate breaking changes
//! to the plugin contract or the manifest schema. Minor bumps are additive.
//! Patch bumps are bug fixes with no contract surface changes.
//!
//! The crate version moves in lockstep with the `evo-core` workspace
//! version; a plugin's `evo_min_version` manifest field refers to this
//! workspace version.
//!
//! ## Logging
//!
//! Plugins log via the [`tracing`](https://crates.io/crates/tracing) crate.
//! The evo project's logging conventions (log levels, field names, plugin
//! identification) are defined in `docs/engineering/LOGGING.md`; plugin
//! authors should follow those conventions so their plugins' log output is
//! legible to operators alongside the steward's own output.
//!
//! The default log level on a production device is `warn`; plugins should
//! emit `info` sparingly (only for operator-visible lifecycle events) and
//! reserve `debug` and `trace` for developer-facing detail.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Version of this SDK crate, read from the crate's Cargo.toml at build
/// time. Plugins use this to populate [`contract::BuildInfo::sdk_version`]
/// without hardcoding a string that drifts on every workspace bump.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod error;
pub mod manifest;

#[cfg(feature = "contract")]
pub mod contract;

pub mod codec;
pub mod testing;
pub mod wire;

pub use error::ManifestError;
pub use manifest::Manifest;

#[cfg(feature = "contract")]
pub use contract::{
    Assignment, BuildInfo, CallDeadline, CourseCorrection, CustodyHandle,
    CustodyStateReporter, Factory, HealthCheck, HealthReport, HealthStatus,
    InstanceAnnouncement, InstanceAnnouncer, InstanceId, LoadContext, Plugin,
    PluginDescription, PluginError, PluginIdentity, ReportError,
    ReportPriority, Request, Respondent, Response, RetractionPolicy,
    RuntimeCapabilities, StateReporter, UserInteraction,
    UserInteractionRequester, Warden,
};
