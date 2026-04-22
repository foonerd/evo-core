//! Wire protocol types (placeholder).
//!
//! This module will hold the structured message types exchanged between the
//! steward and out-of-process plugins, per
//! `docs/engineering/PLUGIN_CONTRACT.md` sections 6, 9, and 10. Every message
//! carries the protocol version `v`, the verb name `op`, the correlation ID
//! `cid`, and the canonical plugin name.
//!
//! The wire types are defined once with `serde` derivations and serialised
//! either as JSON (development) or CBOR (production) by the
//! [`crate::codec`] module.
//!
//! TODO(evo-plugin-sdk pass 3): populate message types for every verb in
//! `PLUGIN_CONTRACT.md` sections 2 through 5. See `PLUGIN_CONTRACT.md`
//! section 9 as the schema source of truth.
