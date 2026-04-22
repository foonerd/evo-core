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
//! - [`contract`]: (placeholder) Rust traits every plugin implements, per
//!   `docs/engineering/PLUGIN_CONTRACT.md` sections 2-7.
//! - [`wire`]: (placeholder) Wire-protocol message types for out-of-process
//!   plugins, per `PLUGIN_CONTRACT.md` sections 6-9.
//! - [`codec`]: (placeholder) JSON and CBOR codec implementations of the wire
//!   schema.
//! - [`testing`]: (placeholder) Mock steward harness for plugin authors to
//!   develop against without a running steward.
//!
//! The plugin runtime contract, artefact contract, and vendor contract are
//! authoritative specifications; this crate is the mechanical representation
//! of those specifications in Rust. When the crate and the documents disagree,
//! the documents are authoritative and the crate is wrong.
//!
//! ## Versioning
//!
//! This crate follows semver. Major version bumps indicate breaking changes to
//! the plugin contract or the manifest schema. Minor bumps are additive. Patch
//! bumps are bug fixes with no contract surface changes.
//!
//! The crate version moves in lockstep with the `evo-core` workspace version;
//! a plugin's `evo_min_version` manifest field refers to this workspace
//! version.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod manifest;
pub mod contract;
pub mod wire;
pub mod codec;
pub mod testing;
pub mod error;

pub use error::ManifestError;
pub use manifest::Manifest;
