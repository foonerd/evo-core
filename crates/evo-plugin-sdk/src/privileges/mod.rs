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

mod schema;
mod types;
mod validator;

pub use schema::SCHEMA_V1_BYTES;
pub use types::{
    CapabilityIntent, HostProvisioning, HostProvisioningBlock, Isolation,
    PolkitProvisioning, PrivilegesError, PrivilegesV1, RequiredBinary,
    SchemaViolation, SystemdProvisioning, Verification,
};
pub use validator::{ValidationError, ValidationIssue, ValidationSeverity};
