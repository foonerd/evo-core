// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Convenience bridge: wire the [`AutoRotator`] substrate
//! to the runtime mount's [`HotReloadCertResolver`].
//!
//! Operators who want self-healing rotation alongside the
//! HTTPS listener call [`install_auto_rotation`] with the
//! `Arc<HotReloadCertResolver>` returned by
//! [`crate::server::serve_https`] in `ServerHandle`. The
//! bridge constructs a [`SwapFn`] that calls
//! [`HotReloadCertResolver::swap`] on each rotation,
//! spawns the [`AutoRotator`], and returns its
//! [`JoinHandle`] so the caller can `await` it after the
//! listener shuts down.

use crate::cert_resolver::HotReloadCertResolver;
use evo_tls_certs::{
    AutoRotator, CertError, CertLifecycle, CertRotationPolicy, GeneratedCa,
    LeafConfig, RotationStats, SwapFn,
};
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Spawn an [`AutoRotator`] that periodically regenerates
/// the leaf cert against the supplied device-CA and
/// hot-swaps the result into the resolver — zero operator
/// action.
///
/// Returns the rotator's [`JoinHandle`]. Drop the handle to
/// detach the rotator from the caller's lifecycle; signal
/// `shutdown` to make the rotator exit cleanly with its
/// accumulated [`RotationStats`].
pub fn install_auto_rotation(
    resolver: Arc<HotReloadCertResolver>,
    ca: Arc<GeneratedCa>,
    leaf_config: LeafConfig,
    lifecycle: Arc<CertLifecycle>,
    policy: CertRotationPolicy,
    shutdown: Arc<Notify>,
) -> JoinHandle<RotationStats> {
    let resolver_for_swap = Arc::clone(&resolver);
    let swap: SwapFn = Arc::new(move |bundle| {
        resolver_for_swap
            .swap(bundle)
            .map_err(|e| CertError::Generation(e.to_string()))
    });
    AutoRotator::new(ca, leaf_config, lifecycle, swap, policy).spawn(shutdown)
}
