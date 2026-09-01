// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Default update source plugin implementations.
//!
//! Sub-primitive I of the three-channel update model. The
//! framework substrate (sub-primitive H, in `crates/evo/src/updates.rs`)
//! exposes the [`evo_plugin_sdk::update::UpdateSource`] trait
//! and the [`crate::updates::UpdateRegistry`] runtime that
//! aggregates inventory + drives check orchestration. This
//! module ships the framework-default sources implementing
//! that contract:
//!
//! - [`plugin_registry`] (sub-primitive I1): the `"plugins"`
//!   source — checks every registered plugin registry's
//!   cached manifest for newer versions of admitted
//!   plugins; applies by staging the bundle to the plugin
//!   stage directory where the existing watcher admits it
//!   asynchronously.
//! - [`core`] (sub-primitive I2): the `"core"` source —
//!   fetches a release-channel `build-info.toml` + signed
//!   binary, verifies against the device's release-trust
//!   roots, stages the binary, and triggers a graceful
//!   steward restart pointing at the staged path.
//!
//! Out of scope, named explicitly:
//!
//! - **Vendor OS update sources** (apt / dnf / Mender /
//!   OSTree / etc.) — vendor distributions ship their own
//!   `UpdateSource` plugin implementing the trait against
//!   their underlying mechanism. The framework declares the
//!   contract; vendor builds carry the implementation.

pub mod core;
pub mod plugin_registry;
