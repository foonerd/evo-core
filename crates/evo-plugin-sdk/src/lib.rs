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
//! - [`wire`]: Wire-protocol message types for out-of-process plugins,
//!   per `PLUGIN_CONTRACT.md` sections 6, 9, 10. Feature-gated on
//!   `wire`.
//! - [`codec`]: JSON codec and length-prefixed framing over async I/O
//!   streams. Feature-gated on `wire`.
//! - [`host`]: Plugin-side wire server. Drives a `Plugin + Respondent`
//!   via [`host::serve`] or a `Plugin + Warden` via
//!   [`host::serve_warden`], each over a single async I/O connection,
//!   per `PLUGIN_CONTRACT.md` sections 6 through 11. Feature-gated on
//!   `wire`.
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
//! - `wire` (default): enables the [`wire`] and [`codec`] modules,
//!   which carry the out-of-process wire protocol. Pulls in
//!   `serde_json` and `tokio` with `io-util`/`net` features.
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
pub mod error_taxonomy;
pub mod manifest;

#[cfg(feature = "contract")]
pub mod contract;

#[cfg(feature = "wire")]
pub mod codec;
#[cfg(feature = "wire")]
pub mod host;
#[cfg(feature = "wire")]
pub mod wire;

pub use error::ManifestError;
pub use error_taxonomy::ErrorClass;
pub use manifest::Manifest;

#[cfg(feature = "contract")]
pub use contract::{
    Assignment, BuildInfo, CallDeadline, CanonicalSubjectId, ClaimConfidence,
    CourseCorrection, CustodyHandle, CustodyStateReporter, ExternalAddressing,
    Factory, HealthCheck, HealthReport, HealthStatus, InstanceAnnouncement,
    InstanceAnnouncer, InstanceId, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, RelationAnnouncer, RelationAssertion,
    RelationRetraction, ReportError, ReportPriority, Request, Respondent,
    Response, RetractionPolicy, RuntimeCapabilities, StateReporter,
    SubjectAnnouncement, SubjectAnnouncer, SubjectClaim, SubjectQuerier,
    SubjectQueryResult, SubjectRecord, UserInteraction,
    UserInteractionRequester, Warden,
};

#[cfg(feature = "wire")]
pub use codec::{
    decode_json, encode_json, read_frame_json, write_frame_json, WireError,
    MAX_FRAME_SIZE,
};
#[cfg(feature = "wire")]
pub use host::{
    run_oop, run_oop_warden, serve, serve_warden, HostConfig, HostError,
    DEFAULT_EVENT_CHANNEL_CAPACITY,
};
#[cfg(feature = "wire")]
pub use wire::{WireFrame, PROTOCOL_VERSION};
