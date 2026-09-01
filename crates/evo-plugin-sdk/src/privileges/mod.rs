// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Plugin privileges contract — Rust types, YAML parser, and validator.
//!
//! Each plugin / framework surface ships a `privileges.yaml` declaring its
//! capability intent, required binaries / kernel modules / system services,
//! verification commands, and per-distribution host provisioning. The
//! framework's admission gate refuses to admit a surface whose declared
//! contract diverges from the host's actual provisioning in either
//! direction (under-provisioning OR over-provisioning).
//!
//! The shipped JSON Schema at `schemas/privileges.v1.json` is the canonical
//! contract for IDE / external tooling. The Rust types here, together with
//! the [`validator`] module, are the framework's independent enforcement.
//!
//! ## Loading a record
//!
//! ```no_run
//! use evo_plugin_sdk::privileges::PrivilegesV1;
//!
//! let yaml = std::fs::read_to_string("privileges.yaml").unwrap();
//! let record = PrivilegesV1::from_yaml(&yaml).unwrap();
//! record.validate().unwrap();
//! println!("{}: {} intents", record.plugin, record.capability_intent.len());
//! ```

#![allow(missing_docs)]

mod parity_gate;
mod probe;
mod remediation;
mod resolution;
mod schema;
mod types;
mod validator;

pub use parity_gate::{
    enforce_os_dependency_parity, MissingPrerequisite, ParityFailure,
};
pub use probe::{
    run_probes, run_probes_with_counts, which, AccessMode, BinaryPresentProbe,
    FilesystemAccessProbe, Probe, ProbeOutcome, ProbePlan, SudoersCommandProbe,
    DEFAULT_PROBE_TIMEOUT,
};
pub use remediation::{
    hint_for_missing_binary, hint_for_missing_group, hint_for_missing_module,
    hint_for_missing_service, hint_for_polkit, parse_os_release_id,
    DistroFamily, RemediationHint, RemediationList,
};
pub use resolution::{
    CapabilityResolution, CapabilityResolutionMap, CapabilityResolutionMapExt,
    ResolutionCounts,
};
pub use schema::SCHEMA_V1_BYTES;
pub use types::{
    CapabilityIntent, HostProvisioning, HostProvisioningBlock, Isolation,
    PolkitProvisioning, PrivilegesError, PrivilegesV1, RequiredBinary,
    SchemaViolation, SystemdProvisioning, Verification,
};
pub use validator::{ValidationError, ValidationIssue, ValidationSeverity};
