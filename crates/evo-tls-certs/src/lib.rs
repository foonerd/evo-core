// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! # evo-tls-certs
//!
//! TLS certificate lifecycle substrate.
//!
//! ## Architecture
//!
//! Three cert acquisition modes, each producing the same
//! [`CertBundle`] shape (private key + leaf cert + the
//! issuing chain) that the runtime HTTPS termination
//! consumes:
//!
//! 1. **Device-CA** — at first boot the device generates a
//!    self-signed root CA, then issues a leaf cert for its
//!    operator-supplied hostnames. The CA persists under
//!    the steward state directory; operators trust the CA
//!    once at first connection and every subsequent leaf
//!    issued by that CA validates automatically. This is
//!    the default mode for first-boot devices without a
//!    routable DNS name.
//! 2. **ACME** — when the device has a routable DNS name,
//!    automatic Let's Encrypt issuance + renewal via an
//!    ACME client. This crate ships the cert-shape /
//!    persistence layer; the network-side ACME negotiation
//!    is a separate runtime concern.
//! 3. **Manual** — operator supplies a PEM bundle from an
//!    enterprise CA. The crate parses + validates the
//!    bundle shape; the runtime hot-loads on operator
//!    update.
//!
//! ## Listener-agnostic
//!
//! This crate emits the typed cert artefacts. It does not
//! bind a TCP listener, run a TLS handshake, or implement
//! the ACME protocol. The runtime mount that stitches the
//! certs onto a concrete HTTPS listener (rustls + axum)
//! consumes [`CertBundle::to_rustls`] (forthcoming when the
//! runtime mount lands).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod bundle;
pub mod device_ca;
pub mod error;
pub mod lifecycle;
pub mod manual;
pub mod pem_io;
pub mod rotation;

pub use bundle::{CertBundle, CertChain, PemBytes, PrivateKey};
pub use device_ca::{
    generate_device_ca, generate_leaf, DeviceCaConfig, GeneratedCa, LeafConfig,
};
pub use error::CertError;
pub use lifecycle::CertLifecycle;
pub use manual::load_manual_bundle;
pub use pem_io::{read_pem_file, write_pem_file};
pub use rotation::{AutoRotator, CertRotationPolicy, RotationStats, SwapFn};
