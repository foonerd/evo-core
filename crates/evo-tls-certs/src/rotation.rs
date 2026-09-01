// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Self-healing certificate rotation.
//!
//! [`AutoRotator`] is a substrate that runs a background
//! tokio task and periodically issues a fresh leaf cert
//! against a long-lived device-CA, hot-swapping the result
//! into a [`crate::lifecycle::CertLifecycle`]-aware
//! consumer (typically the
//! `evo_runtime_http::HotReloadCertResolver`).
//!
//! No operator action is required. Once spawned, the
//! rotator wakes on its configured interval, issues a new
//! leaf, calls the supplied `swap` callback with the new
//! [`CertBundle`], and records a rotation observation if a
//! [`CertLifecycle`] is attached. Failures (issuance, swap)
//! are logged via `tracing` and the rotator continues on
//! its next tick — a single transient failure does not
//! stop future rotations.

use crate::bundle::CertBundle;
use crate::device_ca::{GeneratedCa, LeafConfig};
use crate::error::CertError;
use crate::lifecycle::CertLifecycle;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Configuration for the automatic rotation loop.
#[derive(Debug, Clone, Copy)]
pub struct CertRotationPolicy {
    /// How often to issue a fresh leaf cert and swap it
    /// into the resolver. Operators typically configure
    /// this to be 1/3 to 1/2 of the leaf's
    /// [`LeafConfig::ttl_days`] so the leaf is rotated long
    /// before its actual expiry.
    pub rotation_interval: Duration,

    /// Delay before the first rotation fires. Usually equal
    /// to `rotation_interval`; set to zero to rotate
    /// immediately after spawn (useful in tests).
    pub initial_delay: Duration,
}

impl CertRotationPolicy {
    /// Build a policy from one interval used for both
    /// initial delay and steady-state period.
    pub fn every(interval: Duration) -> Self {
        Self {
            rotation_interval: interval,
            initial_delay: interval,
        }
    }

    /// Builder: override the initial delay independently.
    pub fn with_initial_delay(mut self, initial_delay: Duration) -> Self {
        self.initial_delay = initial_delay;
        self
    }
}

impl Default for CertRotationPolicy {
    fn default() -> Self {
        // 30 days. Reasonable default for a device whose
        // leaf TTL is 90 days; the leaf rotates three times
        // over its lifetime, well before expiry.
        const THIRTY_DAYS: Duration = Duration::from_secs(60 * 60 * 24 * 30);
        Self {
            rotation_interval: THIRTY_DAYS,
            initial_delay: THIRTY_DAYS,
        }
    }
}

/// Callback the rotator invokes on each successful
/// issuance. Returns `Ok(())` if the new bundle was
/// installed; an error if the swap failed (the rotator
/// logs and continues).
///
/// In production this wraps
/// `evo_runtime_http::HotReloadCertResolver::swap`; in
/// tests it can capture the bundle into a shared cell so
/// the test can observe rotations directly.
pub type SwapFn =
    Arc<dyn Fn(&CertBundle) -> Result<(), CertError> + Send + Sync + 'static>;

/// Substrate that owns the rotation loop.
///
/// Construct with [`Self::new`], then call [`Self::spawn`]
/// to start the background task. The task runs until the
/// supplied `shutdown` notifier fires.
pub struct AutoRotator {
    ca: Arc<GeneratedCa>,
    leaf_config: LeafConfig,
    lifecycle: Arc<CertLifecycle>,
    swap: SwapFn,
    policy: CertRotationPolicy,
}

impl AutoRotator {
    /// Build a rotator.
    pub fn new(
        ca: Arc<GeneratedCa>,
        leaf_config: LeafConfig,
        lifecycle: Arc<CertLifecycle>,
        swap: SwapFn,
        policy: CertRotationPolicy,
    ) -> Self {
        Self {
            ca,
            leaf_config,
            lifecycle,
            swap,
            policy,
        }
    }

    /// Spawn the rotation loop on the current tokio
    /// runtime. Returns a [`JoinHandle`] the caller can
    /// `await` after firing `shutdown`.
    pub fn spawn(self, shutdown: Arc<Notify>) -> JoinHandle<RotationStats> {
        tokio::spawn(async move { self.run(shutdown).await })
    }

    async fn run(self, shutdown: Arc<Notify>) -> RotationStats {
        let mut stats = RotationStats::default();

        // Honour the initial delay before the first
        // rotation. Operators usually set it equal to the
        // rotation_interval; tests set it to zero or a
        // small value to drive rotations quickly.
        if !self.policy.initial_delay.is_zero() {
            tokio::select! {
                _ = shutdown.notified() => return stats,
                _ = tokio::time::sleep(self.policy.initial_delay) => {}
            }
        }

        loop {
            self.rotate_once(&mut stats).await;
            tokio::select! {
                _ = shutdown.notified() => return stats,
                _ = tokio::time::sleep(self.policy.rotation_interval) => {}
            }
        }
    }

    async fn rotate_once(&self, stats: &mut RotationStats) {
        let bundle = match self
            .lifecycle
            .issue_leaf(&self.ca, &self.leaf_config)
        {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "cert auto-rotation: issuance failed");
                stats.failures += 1;
                return;
            }
        };
        let new_chain_bytes = bundle.chain.as_str().len();
        match (self.swap)(&bundle) {
            Ok(()) => {
                let old_chain_bytes =
                    stats.last_chain_bytes.unwrap_or(new_chain_bytes);
                self.lifecycle
                    .record_rotation(old_chain_bytes, new_chain_bytes);
                stats.successes += 1;
                stats.last_chain_bytes = Some(new_chain_bytes);
            }
            Err(e) => {
                tracing::warn!(error = %e, "cert auto-rotation: swap failed");
                stats.failures += 1;
            }
        }
    }
}

/// Statistics returned by the rotation task when shutdown
/// is signalled. Test harnesses + operator surfaces consume
/// this to confirm the rotator behaved as expected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RotationStats {
    /// Number of successful rotations (issuance + swap).
    pub successes: u64,
    /// Number of rotation attempts that failed either at
    /// issuance or swap.
    pub failures: u64,
    /// Byte length of the chain produced by the most-recent
    /// successful rotation, when any. Carried so the next
    /// `record_rotation` knows the "old" size.
    pub last_chain_bytes: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_ca::{generate_device_ca, DeviceCaConfig};
    use evo_observatory::{Observatory, ObservatoryConfig};
    use std::sync::Mutex;

    type RotatorHarness = (
        AutoRotator,
        Arc<Mutex<Vec<CertBundle>>>,
        Arc<Observatory>,
        Arc<Notify>,
    );

    fn build_rotator() -> RotatorHarness {
        let observatory =
            Arc::new(Observatory::new(ObservatoryConfig::small()));
        let lifecycle =
            Arc::new(CertLifecycle::with_observatory(Arc::clone(&observatory)));
        let ca =
            Arc::new(generate_device_ca(&DeviceCaConfig::default()).unwrap());
        let leaf_config =
            LeafConfig::for_hostnames(vec!["device.local".to_string()]);

        let captured: Arc<Mutex<Vec<CertBundle>>> =
            Arc::new(Mutex::new(vec![]));
        let captured_for_swap = Arc::clone(&captured);
        let swap: SwapFn = Arc::new(move |bundle: &CertBundle| {
            captured_for_swap
                .lock()
                .expect("test mutex poisoned")
                .push(bundle.clone());
            Ok(())
        });

        let policy = CertRotationPolicy {
            rotation_interval: Duration::from_millis(50),
            initial_delay: Duration::from_millis(0),
        };

        let rotator =
            AutoRotator::new(ca, leaf_config, lifecycle, swap, policy);
        let shutdown = Arc::new(Notify::new());
        (rotator, captured, observatory, shutdown)
    }

    #[tokio::test]
    async fn rotator_fires_on_first_tick_when_initial_delay_is_zero() {
        let (rotator, captured, observatory, shutdown) = build_rotator();
        let handle = rotator.spawn(Arc::clone(&shutdown));
        tokio::time::sleep(Duration::from_millis(30)).await;
        shutdown.notify_waiters();
        let stats = handle.await.unwrap();
        assert!(stats.successes >= 1, "must rotate at least once");
        assert_eq!(captured.lock().unwrap().len(), stats.successes as usize);

        let snap = observatory.snapshot();
        let rotations = snap
            .iter()
            .filter(|o| {
                o.kind == evo_observatory::ObservationKind::TlsBundleRotated
            })
            .count();
        assert!(rotations >= 1, "TlsBundleRotated must surface");
    }

    #[tokio::test]
    async fn rotator_keeps_rotating_until_shutdown() {
        let (rotator, captured, _observatory, shutdown) = build_rotator();
        let handle = rotator.spawn(Arc::clone(&shutdown));
        tokio::time::sleep(Duration::from_millis(180)).await;
        shutdown.notify_waiters();
        let stats = handle.await.unwrap();
        // 180ms with 50ms interval and 0ms initial delay
        // should produce 3-4 rotations.
        assert!(stats.successes >= 3, "got {} successes", stats.successes);
        assert!(captured.lock().unwrap().len() >= 3);
    }

    #[tokio::test]
    async fn rotator_produces_distinct_leaves_each_rotation() {
        let (rotator, captured, _observatory, shutdown) = build_rotator();
        let handle = rotator.spawn(Arc::clone(&shutdown));
        tokio::time::sleep(Duration::from_millis(180)).await;
        shutdown.notify_waiters();
        let _ = handle.await.unwrap();
        let bundles = captured.lock().unwrap().clone();
        assert!(bundles.len() >= 2);
        // Each leaf has a freshly-generated private key, so
        // any two rotations produce distinct keys.
        for pair in bundles.windows(2) {
            assert_ne!(pair[0].private_key, pair[1].private_key);
        }
    }

    #[tokio::test]
    async fn rotator_respects_shutdown_during_initial_delay() {
        let observatory =
            Arc::new(Observatory::new(ObservatoryConfig::small()));
        let lifecycle =
            Arc::new(CertLifecycle::with_observatory(Arc::clone(&observatory)));
        let ca =
            Arc::new(generate_device_ca(&DeviceCaConfig::default()).unwrap());
        let leaf_config =
            LeafConfig::for_hostnames(vec!["device.local".to_string()]);
        let swap: SwapFn = Arc::new(|_| Ok(()));
        let policy = CertRotationPolicy {
            rotation_interval: Duration::from_secs(60),
            initial_delay: Duration::from_secs(60),
        };
        let rotator =
            AutoRotator::new(ca, leaf_config, lifecycle, swap, policy);
        let shutdown = Arc::new(Notify::new());
        let handle = rotator.spawn(Arc::clone(&shutdown));
        // Yield until the spawned task is scheduled and has
        // reached its `shutdown.notified().await` registration.
        // Without this, `Notify::notify_waiters` only wakes
        // currently-registered waiters; a notify that fires
        // before the task is scheduled produces no effect
        // and the test would hang for 60s.
        tokio::time::sleep(Duration::from_millis(20)).await;
        shutdown.notify_waiters();
        let stats = handle.await.unwrap();
        assert_eq!(stats.successes, 0);
        assert_eq!(stats.failures, 0);
    }

    #[tokio::test]
    async fn rotator_counts_swap_failures_and_continues() {
        let observatory =
            Arc::new(Observatory::new(ObservatoryConfig::small()));
        let lifecycle =
            Arc::new(CertLifecycle::with_observatory(Arc::clone(&observatory)));
        let ca =
            Arc::new(generate_device_ca(&DeviceCaConfig::default()).unwrap());
        let leaf_config =
            LeafConfig::for_hostnames(vec!["device.local".to_string()]);
        // Swap that always fails — simulates a broken
        // downstream resolver. The rotator must still
        // continue.
        let swap: SwapFn = Arc::new(|_| {
            Err(CertError::Generation("test-induced swap failure".into()))
        });
        let policy = CertRotationPolicy {
            rotation_interval: Duration::from_millis(40),
            initial_delay: Duration::from_millis(0),
        };
        let rotator =
            AutoRotator::new(ca, leaf_config, lifecycle, swap, policy);
        let shutdown = Arc::new(Notify::new());
        let handle = rotator.spawn(Arc::clone(&shutdown));
        tokio::time::sleep(Duration::from_millis(150)).await;
        shutdown.notify_waiters();
        let stats = handle.await.unwrap();
        assert_eq!(stats.successes, 0);
        assert!(stats.failures >= 3);
    }

    #[test]
    fn rotation_policy_default_is_thirty_days() {
        let p = CertRotationPolicy::default();
        assert_eq!(p.rotation_interval, Duration::from_secs(60 * 60 * 24 * 30));
        assert_eq!(p.initial_delay, p.rotation_interval);
    }

    #[test]
    fn rotation_policy_every_synchronises_initial_and_interval() {
        let p = CertRotationPolicy::every(Duration::from_secs(60));
        assert_eq!(p.rotation_interval, p.initial_delay);
    }

    #[test]
    fn rotation_policy_builder_overrides_initial_delay() {
        let p = CertRotationPolicy::every(Duration::from_secs(60))
            .with_initial_delay(Duration::from_secs(5));
        assert_eq!(p.initial_delay, Duration::from_secs(5));
        assert_eq!(p.rotation_interval, Duration::from_secs(60));
    }
}
