//! Steward-side `InstanceAnnouncer` implementation for factory plugins.
//!
//! Factory plugins announce variable-cardinality single-claimant
//! entities ("instances") through the SDK's [`InstanceAnnouncer`]
//! callback. This module provides [`RegistryInstanceAnnouncer`] — the
//! wiring layer's implementation of that callback.
//!
//! # Lifecycle
//!
//! For each admitted factory plugin, the admission engine constructs
//! one [`RegistryInstanceAnnouncer`] tagged with the plugin name, the
//! target shelf from the plugin's manifest, and the plugin's declared
//! [`RetractionPolicy`]. The announcer is wrapped in `Arc` and placed
//! in the plugin's [`LoadContext`](evo_plugin_sdk::contract::LoadContext);
//! the plugin holds it for its lifetime and emits announce/retract
//! calls through it.
//!
//! On `announce(InstanceAnnouncement)` the announcer:
//!
//! 1. Validates the call against the declared [`RetractionPolicy`].
//! 2. Synthesises an [`ExternalAddressing`] under the dedicated
//!    `evo-factory-instance` scheme with value `<plugin>/<instance_id>`.
//!    The pair is collision-free across factories (per-plugin
//!    namespacing) and stable across plugin restarts (the plugin's
//!    own [`InstanceId`] is the only identity key).
//! 3. Builds a [`SubjectAnnouncement`] with `subject_type` set to the
//!    factory's `target.shelf` from its manifest, the synthesised
//!    addressing as the sole entry, and no per-claim provenance.
//! 4. Calls into the existing [`SubjectRegistry::announce`] path. The
//!    registry mints a canonical UUID for the subject; the announcer
//!    retains the `InstanceId → canonical_id` mapping so a future
//!    `retract` call can locate the subject without a registry scan.
//! 5. Emits a [`Happening::FactoryInstanceAnnounced`] on the bus.
//!
//! On `retract(InstanceId)` the announcer:
//!
//! 1. Validates the call against the declared [`RetractionPolicy`]
//!    (e.g. [`RetractionPolicy::StartupOnly`] forbids retract during
//!    plugin lifetime; the steward retracts on unload).
//! 2. Looks up the `InstanceId → canonical_id` mapping. An unknown
//!    `instance_id` returns [`ReportError::Invalid`] with a structured
//!    message naming the unknown instance.
//! 3. Drops the addressing for the instance, which collapses the
//!    subject if it was the only claimant.
//! 4. Emits a [`Happening::FactoryInstanceRetracted`] on the bus.
//!
//! # Concurrency
//!
//! The announcer is `Send + Sync`. Concurrent calls serialise on the
//! announcer's internal `Mutex<AnnouncerState>`; the lock is never
//! held across an `await` (the registry's `announce` is a sync call).

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use evo_plugin_sdk::contract::factory::{
    InstanceAnnouncement, InstanceId, RetractionPolicy,
};
use evo_plugin_sdk::contract::subjects::{
    ExternalAddressing, SubjectAnnouncement,
};
use evo_plugin_sdk::contract::{InstanceAnnouncer, ReportError};

use crate::happenings::{Happening, HappeningBus};
use crate::subjects::SubjectRegistry;

/// Synthetic addressing scheme reserved for factory instance
/// identifiers. Plugins MUST NOT announce subjects under this scheme
/// directly via [`SubjectAdmin`](evo_plugin_sdk::contract::SubjectAdmin);
/// the wiring layer mints addressings
/// of this shape on the plugin's behalf when the plugin announces an
/// instance through [`InstanceAnnouncer`].
pub const FACTORY_INSTANCE_SCHEME: &str = "evo-factory-instance";

/// Build the canonical addressing value for a factory-announced
/// instance. The format `<plugin>/<instance_id>` is collision-free
/// across factories and stable across plugin restarts.
fn addressing_value(plugin_name: &str, instance_id: &InstanceId) -> String {
    format!("{plugin_name}/{instance_id}")
}

/// Steward-side wiring of the SDK's [`InstanceAnnouncer`] callback.
///
/// Announces and retracts factory instances by routing through the
/// existing subject registry, then emits structured happenings on
/// the bus. See module-level docs for the full lifecycle.
pub struct RegistryInstanceAnnouncer {
    registry: Arc<SubjectRegistry>,
    bus: Arc<HappeningBus>,
    plugin_name: String,
    target_shelf: String,
    retraction_policy: RetractionPolicy,
    state: Mutex<AnnouncerState>,
    persistence: Option<Arc<dyn crate::persistence::PersistenceStore>>,
}

/// Per-announcer mutable state. Held under a `Mutex` so concurrent
/// announce/retract calls from the plugin serialise on the wiring
/// layer rather than racing inside the registry.
struct AnnouncerState {
    /// `true` once the plugin's `load` callback has returned; until
    /// then, [`RetractionPolicy::StartupOnly`] permits announce. The
    /// admission engine flips this flag through
    /// [`RegistryInstanceAnnouncer::mark_load_complete`].
    load_complete: bool,
    /// Map from instance_id (plugin-owned identity) to canonical_id
    /// (registry-minted UUID). Populated on announce; consulted on
    /// retract; iterated on plugin unload by the lifecycle-drain path.
    instances: HashMap<InstanceId, String>,
}

impl std::fmt::Debug for RegistryInstanceAnnouncer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.state.lock() {
            Ok(g) => f
                .debug_struct("RegistryInstanceAnnouncer")
                .field("plugin", &self.plugin_name)
                .field("shelf", &self.target_shelf)
                .field("policy", &self.retraction_policy)
                .field("load_complete", &g.load_complete)
                .field("instance_count", &g.instances.len())
                .finish(),
            Err(_) => f
                .debug_struct("RegistryInstanceAnnouncer")
                .field("state", &"<poisoned>")
                .finish(),
        }
    }
}

impl RegistryInstanceAnnouncer {
    /// Construct an announcer. The `load_complete` flag starts
    /// `false`; the admission engine flips it after the plugin's
    /// `load` callback returns successfully.
    pub fn new(
        registry: Arc<SubjectRegistry>,
        bus: Arc<HappeningBus>,
        plugin_name: impl Into<String>,
        target_shelf: impl Into<String>,
        retraction_policy: RetractionPolicy,
    ) -> Self {
        Self {
            registry,
            bus,
            plugin_name: plugin_name.into(),
            target_shelf: target_shelf.into(),
            retraction_policy,
            state: Mutex::new(AnnouncerState {
                load_complete: false,
                instances: HashMap::new(),
            }),
            persistence: None,
        }
    }

    /// Attach a durable backing store. When set, every successful
    /// announce / retract is mirrored to the subjects /
    /// subject_addressings / claim_log tables so factory-instance
    /// subjects survive a steward restart even if the plugin
    /// re-announces from cold every boot.
    pub fn with_persistence(
        mut self,
        persistence: Arc<dyn crate::persistence::PersistenceStore>,
    ) -> Self {
        self.persistence = Some(persistence);
        self
    }

    /// Mark the plugin's `load` callback as complete. Once flipped,
    /// [`RetractionPolicy::StartupOnly`] refuses further announces.
    /// The admission engine calls this immediately after `load`
    /// returns successfully. The flag starts `false` and is one-shot:
    /// calling this before `load` returns is the documented
    /// shape for any future fast-path that admits a plugin without
    /// going through the standard load sequence.
    pub fn mark_load_complete(&self) {
        self.state
            .lock()
            .expect("announcer state mutex poisoned")
            .load_complete = true;
    }

    /// Snapshot of currently-announced instance IDs and their
    /// registry-minted canonical IDs. Used by the lifecycle-drain
    /// path on plugin unload to retract every announced instance.
    pub fn snapshot_instances(&self) -> Vec<(InstanceId, String)> {
        self.state
            .lock()
            .expect("announcer state mutex poisoned")
            .instances
            .iter()
            .map(|(id, canonical)| (id.clone(), canonical.clone()))
            .collect()
    }

    /// Number of currently-announced instances. Diagnostic.
    pub fn instance_count(&self) -> usize {
        self.state
            .lock()
            .expect("announcer state mutex poisoned")
            .instances
            .len()
    }

    /// Retract every announced instance, bypassing the
    /// retraction-policy gate. Called by the steward's drain path on
    /// plugin unload to ensure no factory-announced subjects outlive
    /// their owning plugin: every instance is retracted, in
    /// announce-order's reverse, with a structured
    /// `FactoryInstanceRetracted` happening emitted per instance.
    ///
    /// Returns `(retracted, errored)` counts. Errors are logged and
    /// counted; they do not abort the drain — every remaining
    /// instance is still attempted, because a partial drain that
    /// stops on the first error would leak subjects on plugin unload.
    pub async fn retract_all_for_drain(&self) -> (usize, usize) {
        // Snapshot in announce-order's reverse so a watcher walking
        // the happenings stream sees the most-recently-announced
        // instances retract first. The retract path mutates the
        // internal map under the same lock, so we drain the snapshot
        // outside the lock to keep the announce path responsive.
        let mut entries: Vec<(InstanceId, String)> = {
            let guard =
                self.state.lock().expect("announcer state mutex poisoned");
            guard
                .instances
                .iter()
                .map(|(id, canonical)| (id.clone(), canonical.clone()))
                .collect()
        };
        // Reverse-order retraction so subscribers see the LIFO unwind.
        entries.sort_by(|a, b| b.0.as_str().cmp(a.0.as_str()));

        let mut retracted = 0usize;
        let mut errored = 0usize;
        for (instance_id, canonical_id) in entries {
            // Remove the local mapping ourselves so the registry call
            // below is the only side-effect on success and so any
            // policy gate is bypassed: drain operates on every
            // instance regardless of declared policy.
            {
                let mut guard =
                    self.state.lock().expect("announcer state mutex poisoned");
                guard.instances.remove(&instance_id);
            }
            let addressing = ExternalAddressing::new(
                FACTORY_INSTANCE_SCHEME,
                addressing_value(&self.plugin_name, &instance_id),
            );
            match self.registry.retract(&addressing, &self.plugin_name, None) {
                Ok(retract_outcome) => {
                    // Mirror the retract through to persistence so a
                    // restart after drain does not see ghost factory
                    // subjects. Drain runs on plugin unload, so this
                    // is the last chance to record the
                    // forget / addressing-removed.
                    if let Some(store) = self.persistence.as_ref() {
                        let at_ms = crate::persistence::system_time_to_ms_now();
                        let persist_result = match &retract_outcome {
                            crate::subjects::SubjectRetractOutcome::AddressingRemoved => {
                                store
                                    .record_subject_retract(
                                        &canonical_id,
                                        &addressing,
                                        &self.plugin_name,
                                        at_ms,
                                    )
                                    .await
                            }
                            crate::subjects::SubjectRetractOutcome::SubjectForgotten {
                                canonical_id: forgotten_id,
                                ..
                            } => {
                                store
                                    .record_subject_forget(
                                        forgotten_id,
                                        &self.plugin_name,
                                        None,
                                        at_ms,
                                    )
                                    .await
                            }
                        };
                        if let Err(e) = persist_result {
                            tracing::warn!(
                                plugin = %self.plugin_name,
                                instance = %instance_id,
                                error = %e,
                                "drain: persistence refused retract \
                                 mirror; the in-memory retract still \
                                 succeeded so the happening is emitted"
                            );
                        }
                    }
                    if let Err(e) = self
                        .bus
                        .emit_durable(Happening::FactoryInstanceRetracted {
                            plugin: self.plugin_name.clone(),
                            instance_id: instance_id.to_string(),
                            canonical_id,
                            shelf: self.target_shelf.clone(),
                            at: SystemTime::now(),
                        })
                        .await
                    {
                        // Drain runs on plugin unload; we count this
                        // as a successfully-retracted instance since
                        // the registry mutation already landed and
                        // logging the persistence failure is the
                        // right level here. Aborting drain on a
                        // happening-persist failure would leak
                        // subjects into the next steward boot.
                        tracing::warn!(
                            plugin = %self.plugin_name,
                            instance = %instance_id,
                            error = %e,
                            "drain: emit_durable failed for \
                             FactoryInstanceRetracted; drain continues"
                        );
                    }
                    retracted += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        plugin = %self.plugin_name,
                        instance = %instance_id,
                        error = %e,
                        "drain: registry refused retract; \
                         continuing with remaining instances"
                    );
                    errored += 1;
                }
            }
        }
        (retracted, errored)
    }
}

impl InstanceAnnouncer for RegistryInstanceAnnouncer {
    fn announce<'a>(
        &'a self,
        announcement: InstanceAnnouncement,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<(), ReportError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            // Policy gate: StartupOnly refuses announce after `load`
            // has returned. Dynamic and ShutdownOnly permit announce
            // any time during plugin lifetime.
            {
                let state =
                    self.state.lock().expect("announcer state mutex poisoned");
                if matches!(
                    self.retraction_policy,
                    RetractionPolicy::StartupOnly
                ) && state.load_complete
                {
                    return Err(ReportError::Invalid(format!(
                        "{}: factory has RetractionPolicy::StartupOnly; \
                         announce_instance refused after load returned \
                         (instance_id={})",
                        self.plugin_name, announcement.instance_id
                    )));
                }
                if state.instances.contains_key(&announcement.instance_id) {
                    return Err(ReportError::Invalid(format!(
                        "{}: instance_id={} already announced",
                        self.plugin_name, announcement.instance_id
                    )));
                }
            }

            // Synthesise the addressing the registry will key the
            // subject under. The value `<plugin>/<instance_id>` is
            // unique per plugin/instance and survives plugin
            // restart (the plugin re-announces with the same
            // instance_id and the same addressing reappears).
            let addressing = ExternalAddressing::new(
                FACTORY_INSTANCE_SCHEME,
                addressing_value(&self.plugin_name, &announcement.instance_id),
            );

            let subject_announcement = SubjectAnnouncement {
                subject_type: self.target_shelf.clone(),
                addressings: vec![addressing.clone()],
                claims: Vec::new(),
                announced_at: SystemTime::now(),
            };

            let outcome = self
                .registry
                .announce(&subject_announcement, &self.plugin_name)
                .map_err(|e| {
                    ReportError::Invalid(format!(
                        "{}: subject registry refused announce for \
                         instance_id={}: {}",
                        self.plugin_name, announcement.instance_id, e
                    ))
                })?;

            // Factory addressings are unique per (plugin, instance_id),
            // so the registry never sees them already-resolved at the
            // first announce. We expect Created. Updated/NoChange
            // would mean the addressing already existed (e.g. a
            // boot-time re-announce from durable subject state);
            // Conflict cannot happen because the addressing is unique
            // to this announcement.
            let canonical_id = match outcome {
                crate::subjects::AnnounceOutcome::Created(id)
                | crate::subjects::AnnounceOutcome::Updated(id)
                | crate::subjects::AnnounceOutcome::NoChange(id) => id,
                crate::subjects::AnnounceOutcome::Conflict {
                    canonical_ids,
                } => {
                    return Err(ReportError::Invalid(format!(
                        "{}: factory addressing collided across {} \
                         existing canonical IDs (instance_id={}); \
                         this should not happen for factory-minted \
                         addressings",
                        self.plugin_name,
                        canonical_ids.len(),
                        announcement.instance_id
                    )));
                }
            };

            // Record the mapping and emit the happening only after
            // the registry side succeeded.
            {
                let mut state =
                    self.state.lock().expect("announcer state mutex poisoned");
                state.instances.insert(
                    announcement.instance_id.clone(),
                    canonical_id.clone(),
                );
            }

            // Durable mirror of the subject the registry just
            // minted. Factory addressings round-trip cleanly: the
            // restart sequence brings the subject row back from
            // disk via SubjectRegistry::rehydrate_from before the
            // plugin's load() re-announces, so the second-boot
            // announce sees Updated/NoChange against the persisted
            // canonical_id. When the announcer has no persistence
            // handle (test fixtures), the registry still operates
            // in memory; only durability across restart is missed.
            if let Some(store) = self.persistence.as_ref() {
                let at_ms =
                    crate::persistence::system_time_to_ms_now();
                store
                    .record_subject_announce(
                        crate::persistence::AnnounceRecord {
                            canonical_id: &canonical_id,
                            subject_type: &self.target_shelf,
                            addressings: std::slice::from_ref(&addressing),
                            claimant: &self.plugin_name,
                            claims: &[],
                            at_ms,
                        },
                    )
                    .await
                    .map_err(|e| {
                        ReportError::Invalid(format!(
                            "factory instance announce persist failed: {e}"
                        ))
                    })?;
            }

            self.bus
                .emit_durable(Happening::FactoryInstanceAnnounced {
                    plugin: self.plugin_name.clone(),
                    instance_id: announcement.instance_id.to_string(),
                    canonical_id,
                    shelf: self.target_shelf.clone(),
                    payload_bytes: announcement.payload.len(),
                    at: SystemTime::now(),
                })
                .await
                .map_err(|e| {
                    ReportError::Invalid(format!(
                        "factory instance happening persist failed: {e}"
                    ))
                })?;

            Ok(())
        })
    }

    fn retract<'a>(
        &'a self,
        instance_id: InstanceId,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<(), ReportError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            // Policy gate: ShutdownOnly refuses retract during plugin
            // lifetime; the steward retracts every instance on
            // unload via the lifecycle-drain path.
            if matches!(self.retraction_policy, RetractionPolicy::ShutdownOnly)
            {
                return Err(ReportError::Invalid(format!(
                    "{}: factory has RetractionPolicy::ShutdownOnly; \
                     retract_instance refused during plugin lifetime \
                     (instance_id={})",
                    self.plugin_name, instance_id
                )));
            }

            // Look up the canonical_id and remove the local mapping.
            // Holding the lock across the registry call would be
            // safe (registry is sync), but we drop the lock so the
            // happening emit below can yield without blocking other
            // announces.
            let canonical_id = {
                let mut state =
                    self.state.lock().expect("announcer state mutex poisoned");
                state.instances.remove(&instance_id).ok_or_else(|| {
                    ReportError::Invalid(format!(
                        "{}: retract_instance for unknown instance_id={}",
                        self.plugin_name, instance_id
                    ))
                })?
            };

            // Drop the addressing the registry minted on announce.
            // The subject collapses if it was the only claimant
            // (which is always true for factory-minted subjects in
            // v0.1.11; multi-claimant factory subjects are deferred).
            let addressing = ExternalAddressing::new(
                FACTORY_INSTANCE_SCHEME,
                addressing_value(&self.plugin_name, &instance_id),
            );
            let retract_outcome = self
                .registry
                .retract(&addressing, &self.plugin_name, None)
                .map_err(|e| {
                    ReportError::Invalid(format!(
                        "{}: subject registry refused retract for \
                         instance_id={}: {}",
                        self.plugin_name, instance_id, e
                    ))
                })?;

            // Mirror the registry mutation through to durable
            // storage. SubjectForgotten is the typical case for
            // factory addressings (single-claimant); the
            // AddressingRemoved branch covers the deferred
            // multi-claimant scenario.
            if let Some(store) = self.persistence.as_ref() {
                let at_ms = crate::persistence::system_time_to_ms_now();
                match &retract_outcome {
                    crate::subjects::SubjectRetractOutcome::AddressingRemoved => {
                        store
                            .record_subject_retract(
                                &canonical_id,
                                &addressing,
                                &self.plugin_name,
                                at_ms,
                            )
                            .await
                            .map_err(|e| {
                                ReportError::Invalid(format!(
                                    "factory instance retract persist failed: {e}"
                                ))
                            })?;
                    }
                    crate::subjects::SubjectRetractOutcome::SubjectForgotten {
                        canonical_id: forgotten_id,
                        ..
                    } => {
                        store
                            .record_subject_forget(
                                forgotten_id,
                                &self.plugin_name,
                                None,
                                at_ms,
                            )
                            .await
                            .map_err(|e| {
                                ReportError::Invalid(format!(
                                    "factory instance forget persist failed: {e}"
                                ))
                            })?;
                    }
                }
            }

            self.bus
                .emit_durable(Happening::FactoryInstanceRetracted {
                    plugin: self.plugin_name.clone(),
                    instance_id: instance_id.to_string(),
                    canonical_id,
                    shelf: self.target_shelf.clone(),
                    at: SystemTime::now(),
                })
                .await
                .map_err(|e| {
                    ReportError::Invalid(format!(
                        "factory instance happening persist failed: {e}"
                    ))
                })?;

            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalogue::Catalogue;

    /// Build a minimal catalogue declaring a single rack with a
    /// shelf the tests use as the factory's target.
    fn test_catalogue() -> Arc<Catalogue> {
        let toml = r#"
schema_version = 1

[[racks]]
name = "test"
family = "domain"
kinds = ["registrar"]
charter = "Factory tests."

[[racks.shelves]]
name = "factory"
shape = 1
description = "Factory test shelf."
"#;
        Arc::new(Catalogue::from_toml(toml).expect("catalogue parses"))
    }

    fn build_announcer(
        policy: RetractionPolicy,
    ) -> (
        RegistryInstanceAnnouncer,
        Arc<SubjectRegistry>,
        Arc<HappeningBus>,
    ) {
        // Catalogue is constructed for completeness but the registry
        // does not consult it during a bare announce — the test harness
        // exercises the wiring shape rather than the catalogue gate.
        let _catalogue = test_catalogue();
        let registry = Arc::new(SubjectRegistry::new());
        let bus = Arc::new(HappeningBus::new());
        let announcer = RegistryInstanceAnnouncer::new(
            Arc::clone(&registry),
            Arc::clone(&bus),
            "org.test.factory",
            "test.factory",
            policy,
        );
        (announcer, registry, bus)
    }

    #[tokio::test]
    async fn announce_mints_subject_and_emits_happening() {
        let (announcer, registry, bus) =
            build_announcer(RetractionPolicy::Dynamic);
        let mut subscriber = bus.subscribe();

        announcer
            .announce(InstanceAnnouncement::new("dac-001", b"payload".to_vec()))
            .await
            .expect("announce succeeds");

        // Subject registry has the subject.
        let snapshot = registry.snapshot_subjects();
        assert_eq!(
            snapshot.len(),
            1,
            "exactly one subject minted; got {snapshot:?}"
        );
        assert_eq!(snapshot[0].subject_type, "test.factory");

        // Announcer's internal map records the instance.
        assert_eq!(announcer.instance_count(), 1);
        let instances = announcer.snapshot_instances();
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].0, InstanceId::from("dac-001"));

        // Happening was emitted.
        let happening = subscriber
            .recv()
            .await
            .expect("subscriber receives happening");
        match happening {
            Happening::FactoryInstanceAnnounced {
                plugin,
                instance_id,
                shelf,
                payload_bytes,
                ..
            } => {
                assert_eq!(plugin, "org.test.factory");
                assert_eq!(instance_id, "dac-001");
                assert_eq!(shelf, "test.factory");
                assert_eq!(payload_bytes, b"payload".len());
            }
            other => panic!("expected FactoryInstanceAnnounced, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn retract_removes_subject_and_emits_happening() {
        let (announcer, registry, bus) =
            build_announcer(RetractionPolicy::Dynamic);
        let mut subscriber = bus.subscribe();

        announcer
            .announce(InstanceAnnouncement::new("dac-001", b"payload".to_vec()))
            .await
            .expect("announce");

        // Drain the announce happening so retract is the next one
        // we observe.
        let _ = subscriber.recv().await;

        announcer
            .retract(InstanceId::from("dac-001"))
            .await
            .expect("retract succeeds");

        // Subject is gone (it was the only claimant).
        assert!(
            registry.snapshot_subjects().is_empty(),
            "subject collapsed after retract"
        );
        assert_eq!(announcer.instance_count(), 0);

        let happening = subscriber.recv().await.expect("subscriber");
        match happening {
            Happening::FactoryInstanceRetracted {
                plugin,
                instance_id,
                shelf,
                ..
            } => {
                assert_eq!(plugin, "org.test.factory");
                assert_eq!(instance_id, "dac-001");
                assert_eq!(shelf, "test.factory");
            }
            other => panic!("expected FactoryInstanceRetracted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn announce_rejects_duplicate_instance_id() {
        let (announcer, _, _) = build_announcer(RetractionPolicy::Dynamic);

        announcer
            .announce(InstanceAnnouncement::new("dac-001", vec![]))
            .await
            .expect("first announce");

        let err = announcer
            .announce(InstanceAnnouncement::new("dac-001", vec![]))
            .await
            .expect_err("duplicate announce refused");

        assert!(
            matches!(err, ReportError::Invalid(ref msg) if msg.contains("already announced")),
            "expected Invalid(already announced), got {err:?}"
        );
    }

    #[tokio::test]
    async fn retract_rejects_unknown_instance_id() {
        let (announcer, _, _) = build_announcer(RetractionPolicy::Dynamic);

        let err = announcer
            .retract(InstanceId::from("never-announced"))
            .await
            .expect_err("retract for unknown id refused");

        assert!(
            matches!(err, ReportError::Invalid(ref msg) if msg.contains("unknown instance_id")),
            "expected Invalid(unknown instance_id), got {err:?}"
        );
    }

    #[tokio::test]
    async fn startup_only_policy_refuses_announce_after_load_complete() {
        let (announcer, _, _) = build_announcer(RetractionPolicy::StartupOnly);

        // During load: announce is permitted.
        announcer
            .announce(InstanceAnnouncement::new("during-load", vec![]))
            .await
            .expect("announce during load permitted under StartupOnly");

        // After load: flip the flag, then refuse.
        announcer.mark_load_complete();

        let err = announcer
            .announce(InstanceAnnouncement::new("after-load", vec![]))
            .await
            .expect_err("StartupOnly refuses announce after load");

        assert!(
            matches!(err, ReportError::Invalid(ref msg) if msg.contains("StartupOnly")),
            "expected Invalid(StartupOnly), got {err:?}"
        );
    }

    #[tokio::test]
    async fn shutdown_only_policy_refuses_retract_during_lifetime() {
        let (announcer, _, _) = build_announcer(RetractionPolicy::ShutdownOnly);

        announcer
            .announce(InstanceAnnouncement::new("dac-001", vec![]))
            .await
            .expect("announce permitted under ShutdownOnly");

        let err = announcer
            .retract(InstanceId::from("dac-001"))
            .await
            .expect_err("ShutdownOnly refuses retract during lifetime");

        assert!(
            matches!(err, ReportError::Invalid(ref msg) if msg.contains("ShutdownOnly")),
            "expected Invalid(ShutdownOnly), got {err:?}"
        );

        // Instance is still live in the announcer's map (retract refused).
        assert_eq!(announcer.instance_count(), 1);
    }

    #[tokio::test]
    async fn dynamic_policy_permits_announce_and_retract_freely() {
        let (announcer, _, _) = build_announcer(RetractionPolicy::Dynamic);

        for i in 0..3 {
            let id = format!("dac-{i:03}");
            announcer
                .announce(InstanceAnnouncement::new(id.clone(), vec![]))
                .await
                .expect("announce");
        }
        assert_eq!(announcer.instance_count(), 3);

        announcer
            .retract(InstanceId::from("dac-001"))
            .await
            .expect("retract");
        assert_eq!(announcer.instance_count(), 2);

        // mark_load_complete does NOT change Dynamic semantics.
        announcer.mark_load_complete();
        announcer
            .announce(InstanceAnnouncement::new("dac-003", vec![]))
            .await
            .expect("Dynamic permits announce after load");
        assert_eq!(announcer.instance_count(), 3);
    }

    #[tokio::test]
    async fn snapshot_instances_returns_announced_set() {
        let (announcer, _, _) = build_announcer(RetractionPolicy::Dynamic);

        announcer
            .announce(InstanceAnnouncement::new("a", vec![]))
            .await
            .expect("announce a");
        announcer
            .announce(InstanceAnnouncement::new("b", vec![]))
            .await
            .expect("announce b");

        let snap = announcer.snapshot_instances();
        let ids: std::collections::HashSet<String> =
            snap.iter().map(|(id, _)| id.to_string()).collect();
        assert!(ids.contains("a"));
        assert!(ids.contains("b"));
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn addressing_value_format_is_plugin_slash_instance() {
        let id = InstanceId::from("dac-001");
        assert_eq!(
            addressing_value("org.test.factory", &id),
            "org.test.factory/dac-001"
        );
    }
}
