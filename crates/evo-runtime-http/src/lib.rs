// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! # evo-runtime-http
//!
//! HTTPS runtime mount.
//!
//! ## What this crate is
//!
//! This is the layer that stitches every preceding substrate
//! into an actual listening HTTPS endpoint:
//!
//! - the canonical wire schema (the 124-op surface the
//!   framework exposes),
//! - the REST projection (HTTP method + path per op),
//! - the bearer-token validator (capability-scoped
//!   authorisation),
//! - the cert bundle (private key + leaf + issuing chain).
//!
//! It binds `axum` on top of `rustls`, registers one route
//! per [`WireOp`] in the supplied schema, runs the
//! bearer-token middleware over every route, and forwards
//! the authenticated request through a [`Dispatcher`] seam
//! that the steward implements.
//!
//! ## Hot-reload
//!
//! The TLS server config is held behind a
//! [`HotReloadCertResolver`] so the cert bundle can be
//! swapped at runtime (ACME renewal, manual operator update)
//! without dropping live connections.
//!
//! ## What this crate is not
//!
//! - It is not the steward. It depends on a steward-supplied
//!   [`Dispatcher`] to turn wire ops into work.
//! - It does not own the canonical schema. The caller passes
//!   the schema in so the same mount can serve framework or
//!   vendor-extended surfaces.
//! - It does not own cert acquisition (device-CA, ACME,
//!   manual). It consumes whichever [`CertBundle`] the
//!   caller wires.
//!
//! [`WireOp`]: evo_projection_core::WireOp
//! [`CertBundle`]: evo_tls_certs::CertBundle

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod acme;
pub mod artwork_admission;
pub mod artwork_cascade;
pub mod artwork_endpoint;
pub mod artwork_negative_cache;
pub mod artwork_resolve_coalescer;
pub mod artwork_resolve_endpoint;
pub mod artwork_resolve_index;
pub mod audit;
pub mod auth_tier;
pub mod captive_session_endpoint;
pub mod cert_resolver;
pub mod config;
pub mod dispatcher;
pub mod error;
pub mod middleware;
pub mod mtls;
pub mod observatory_endpoints;
pub mod oidc;
pub mod pq;
pub mod principal;
pub mod redirect;
pub mod rotation_bridge;
pub mod router;
pub mod server;
pub mod tls;
pub mod track_detail_endpoint;
pub mod witness_endpoints;
pub mod ws_endpoint;

pub use acme::{
    AcmeChallengeStore, AcmeConfig, AcmeError, AcmeIssuer,
    DEFAULT_CHECK_INTERVAL as ACME_DEFAULT_CHECK_INTERVAL,
    DEFAULT_RENEWAL_WINDOW_DAYS as ACME_DEFAULT_RENEWAL_WINDOW_DAYS,
};
pub use audit::{AuditEvent, AuditOutcome, AuditSink, NoopAuditSink};
pub use auth_tier::{
    auth_tier_from_env, default_auth_tier_provider, AuthTier, AuthTierProvider,
    StaticAuthTier,
};
pub use cert_resolver::HotReloadCertResolver;
pub use config::{HttpsListenerConfig, ListenAddr};
pub use dispatcher::{
    DispatchError, Dispatcher, NoopSubscriptionDispatcher,
    SubscriptionDispatcher, SubscriptionEventStream, SubscriptionOpened,
};
pub use error::RuntimeHttpError;
pub use mtls::{build_client_verifier, build_client_verifier_from_pem};
pub use oidc::{
    OidcConfig, OidcError, OidcPrincipal, OidcVerifier,
    DEFAULT_FETCH_TIMEOUT as OIDC_DEFAULT_FETCH_TIMEOUT,
    DEFAULT_JWKS_REFRESH as OIDC_DEFAULT_JWKS_REFRESH,
};
pub use pq::{
    advertised_kx_groups, install_crypto_provider, pq_compiled_in,
    HYBRID_KX_GROUP_NAME,
};
pub use principal::Principal;
pub use redirect::{
    build_redirect_router, build_redirect_router_with_acme,
    serve_http_redirect, serve_http_redirect_with_acme,
};
pub use rotation_bridge::install_auto_rotation;
pub use router::{attach_static_assets, build_router};
pub use server::{serve_https, serve_https_with_mtls, ServerHandle};
pub use tls::build_server_config;
