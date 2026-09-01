// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! # evo-auth-bearer
//!
//! Capability-scoped bearer-token substrate.
//!
//! ## Architecture
//!
//! Every projection layer (REST, WebSocket, gRPC, GraphQL,
//! HTTP/3) calls into this crate to verify the inbound
//! operator credential against the dispatched wire op's
//! [`CapabilityRequirement`]. Tokens are issued by the
//! framework's auth layer (typically after a successful
//! step-up flow), carry the operator's capability set
//! inline, and are signed by a per-device ed25519 key the
//! steward holds in its persistent state directory.
//!
//! ## Token model
//!
//! - **Capability set**: the operator's typed scope set
//!   ([`CapabilitySet`]), carrying one or more
//!   [`Capability`] values. A capability satisfies a
//!   requirement when the scope matches and the rank
//!   ordering allows it: `Write` satisfies `Read` of the
//!   same scope; `StepUp` satisfies `Write` + `Read` +
//!   `StepUp` of the same scope.
//! - **Token id**: 128 bits of OS-rng URL-safe-base64,
//!   the revocation key.
//! - **Issued / expires timestamps**: UTC milliseconds.
//!   The validator refuses tokens past expiry and
//!   tokens whose `issued_at` is in the future
//!   (defence in depth against clock skew + malicious
//!   tokens with hand-set timestamps).
//! - **Signature**: ed25519 over the canonical encoding
//!   of `(token_id, capabilities, issued_at, expires_at)`.
//!   The framework holds the signing key; every projection
//!   holds the verifying key.
//!
//! ## Wire encoding
//!
//! Tokens travel over the network as
//! `Authorization: Bearer <base64-url-encoded-bytes>`
//! headers (HTTP / WS), or as connection-level
//! authentication frames (gRPC metadata, WebTransport
//! handshake). The canonical binary encoding is a
//! length-prefixed CBOR-like format; clients use
//! [`BearerToken::encode`] / [`BearerToken::decode`].
//!
//! ## Lifecycle
//!
//! - **Issuance**: [`BearerTokenIssuer::issue`] mints a
//!   signed token for a caller-supplied capability set
//!   and TTL. The TTL ceiling is operator-configurable
//!   (default 24 hours; ceiling 30 days).
//! - **Validation**: [`BearerTokenValidator::verify`]
//!   checks signature + expiry + future-issuance + not
//!   revoked. [`BearerTokenValidator::check_capability`]
//!   composes verify with a capability-satisfaction check
//!   against a `CapabilityRequirement`.
//! - **Revocation**: [`RevocationList::revoke`] marks a
//!   token id as revoked. The validator refuses revoked
//!   tokens immediately. The revocation list is
//!   in-memory in this substrate; persistence to the
//!   steward's substrate layer wires in alongside the
//!   bearer-token table migration.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod capability;
pub mod credential_record;
pub mod error;
pub mod issuer;
pub mod revocation;
pub mod token;
pub mod validator;

pub use capability::{Capability, CapabilitySet};
pub use credential_record::{
    CapabilityRef, CredentialRecord, CredentialStore, CredentialStoreError,
    ExpiryPolicy,
};
pub use error::TokenError;
pub use issuer::BearerTokenIssuer;
pub use revocation::RevocationList;
pub use token::BearerToken;
pub use validator::BearerTokenValidator;

/// Default TTL for a new bearer token: 24 hours in
/// milliseconds. Operator config may shorten; the framework
/// imposes a ceiling of [`MAX_TOKEN_TTL_MS`].
pub const DEFAULT_TOKEN_TTL_MS: u64 = 24 * 60 * 60 * 1000;

/// Hard ceiling on bearer-token TTL: 100 years in
/// milliseconds. The framework primitive admits credentials
/// up to this ceiling so operator-set `ExpiryPolicy::Never`
/// (typical for tier-1 IoT consumers that do not maintain a
/// refresh loop) maps to a "long enough to be effectively
/// indefinite" TTL while still keeping every credential
/// fingerprinted with a finite expiry on the wire.
///
/// The framework's own default remains [`DEFAULT_TOKEN_TTL_MS`]
/// (24 hours). Operator policy on top of the framework may
/// pick stricter ceilings (Secure-industrial tier exposes
/// per-credential expiry policy in the UI).
pub const MAX_TOKEN_TTL_MS: u64 = 100 * 365 * 24 * 60 * 60 * 1000;
