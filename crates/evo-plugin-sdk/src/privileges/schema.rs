//! Embedded JSON Schema bytes for v1.0 of the privileges contract.
//!
//! The schema file at `schemas/privileges.v1.json` is the authoritative
//! contract for downstream tooling (IDE schema-aware editing, external
//! linters, plugin store submission validation). It is embedded here so the
//! framework can serve it over the wire (CLI verb, future wire op) without
//! the consumer needing to vendor or download it independently.
//!
//! The Rust validator in [`super::validator`] enforces the same rules
//! programmatically. The two MUST stay in sync; any schema change requires
//! a matching Rust types + validator change in the same commit.

/// Raw bytes of the v1.0 JSON Schema, as compiled into the binary.
pub const SCHEMA_V1_BYTES: &[u8] =
    include_bytes!("../../schemas/privileges.v1.json");
