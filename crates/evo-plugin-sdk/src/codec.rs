//! Wire codec (placeholder).
//!
//! This module will hold the JSON (dev) and CBOR (production) codecs for the
//! wire protocol messages in [`crate::wire`], plus the length-prefixed framing
//! ( [4-byte big-endian length] [payload] ) documented in
//! `PLUGIN_CONTRACT.md` section 6.
//!
//! Codec selection is controlled by the steward's configuration: a single
//! steward instance speaks one codec at a time. Plugins that must support
//! both build with both codec features enabled.
//!
//! TODO(evo-plugin-sdk pass 3): populate JSON codec, CBOR codec, and the
//! framing layer. Introduce `json` and `cbor` feature flags when codecs land;
//! the framing layer is unconditional.
