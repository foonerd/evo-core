//! Property tests pinning the [`PluginRouter`]'s table-state
//! invariants and the lookup-clone-drop discipline at the
//! public-surface level.
//!
//! These tests exercise the actual router (not the
//! [`evo::sync::RouterTable`] shape model used in
//! `tests/loom_router.rs`). They run as part of the standard
//! `cargo test` invocation; loom-style exhaustive interleaving search
//! is out of scope here. The properties exercised are:
//!
//! 1. Insert N entries, then look up: every inserted shelf returns its
//!    entry; never-inserted shelves return None.
//! 2. Insert N entries, then drain: drain returns every entry exactly
//!    once in reverse admission order, and the router is empty
//!    afterward.
//! 3. The Arc<PluginEntry> obtained via lookup remains valid after the
//!    router itself is dropped (clone-out-of-guard correctness).
//!
//! The `proptest` strategy combinators randomise the operation order
//! so the property holds across many shapes of admission/drain
//! sequence rather than a single canned scenario.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;

use proptest::prelude::*;

use evo::admin::AdminLedger;
use evo::admission::{AdmittedHandle, ErasedRespondent, RespondentAdapter};
use evo::catalogue::Catalogue;
use evo::custody::CustodyLedger;
use evo::happenings::HappeningBus;
use evo::persistence::MemoryPersistenceStore;
use evo::relations::RelationGraph;
use evo::router::{PluginEntry, PluginRouter};
use evo::state::StewardState;
use evo::subjects::SubjectRegistry;
use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};

fn fresh_state() -> Arc<StewardState> {
    StewardState::builder()
        .catalogue(Arc::new(Catalogue::default()))
        .subjects(Arc::new(SubjectRegistry::new()))
        .relations(Arc::new(RelationGraph::new()))
        .custody(Arc::new(CustodyLedger::new()))
        .bus(Arc::new(HappeningBus::new()))
        .admin(Arc::new(AdminLedger::new()))
        .persistence(Arc::new(MemoryPersistenceStore::new()))
        .build()
        .expect("steward state must build")
}

/// Trivial respondent used to populate router entries.
#[derive(Default)]
struct ProbeRespondent {
    name: String,
}

impl Plugin for ProbeRespondent {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: self.name.clone(),
                    version: semver::Version::new(0, 1, 0),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec!["ping".into()],
                    accepts_custody: false,
                    flags: Default::default(),
                },
                build_info: BuildInfo {
                    plugin_build: "test".into(),
                    sdk_version: "0.1.0".into(),
                    rustc_version: None,
                    built_at: None,
                },
            }
        }
    }

    fn load<'a>(
        &'a mut self,
        _ctx: &'a LoadContext,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move { Ok(()) }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move { Ok(()) }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move { HealthReport::healthy() }
    }
}

impl Respondent for ProbeRespondent {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move { Ok(Response::for_request(req, req.payload.clone())) }
    }
}

fn entry_for(name: &str, shelf: &str) -> Arc<PluginEntry> {
    let r: Box<dyn ErasedRespondent> =
        Box::new(RespondentAdapter::new(ProbeRespondent {
            name: name.into(),
        }));
    let handle = AdmittedHandle::Respondent(r);
    Arc::new(PluginEntry::new(name.into(), shelf.into(), handle))
}

/// Strategy: a vector of unique shelf names of length 0..=N.
fn unique_shelves(max_len: usize) -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(0u32..1024, 0..=max_len).prop_map(|raw| {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for v in raw {
            let shelf = format!("test.shelf{v}");
            if seen.insert(shelf.clone()) {
                out.push(shelf);
            }
        }
        out
    })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        .. ProptestConfig::default()
    })]

    /// After N inserts, every inserted shelf is reachable via lookup
    /// and reports the same `Arc<PluginEntry>` that was inserted.
    /// Shelves that were never inserted are not reachable.
    #[test]
    fn insert_then_lookup_is_total_for_inserted_shelves(
        shelves in unique_shelves(16),
        probe in 0u32..1024,
    ) {
        let router = PluginRouter::new(fresh_state());

        // Insert every shelf in `shelves`.
        let mut inserted: Vec<Arc<PluginEntry>> = Vec::new();
        for s in &shelves {
            let e = entry_for(s, s);
            router.insert(Arc::clone(&e)).expect("insert");
            inserted.push(e);
        }

        prop_assert_eq!(router.len(), shelves.len());

        // Every inserted shelf round-trips to the same Arc.
        for (s, e) in shelves.iter().zip(inserted.iter()) {
            let got = router
                .lookup(s)
                .expect("inserted shelf must be present");
            prop_assert!(Arc::ptr_eq(&got, e));
        }

        // A probe shelf that was not inserted must not be reachable.
        let probe_shelf = format!("test.absent{probe}");
        let absent = !shelves.iter().any(|s| s == &probe_shelf);
        if absent {
            prop_assert!(router.lookup(&probe_shelf).is_none());
        }
    }

    /// Drain returns every inserted entry exactly once, in reverse
    /// admission order, and leaves the router empty.
    #[test]
    fn drain_is_lifo_and_total(shelves in unique_shelves(16)) {
        let router = PluginRouter::new(fresh_state());

        let mut inserted_names = Vec::new();
        for s in &shelves {
            router.insert(entry_for(s, s)).expect("insert");
            inserted_names.push(s.clone());
        }

        let drained = router.drain_in_reverse_admission_order();
        prop_assert_eq!(drained.len(), shelves.len());
        prop_assert_eq!(router.len(), 0);
        prop_assert!(router.is_empty());

        // Reverse admission order: drained[i].shelf == shelves[len-1-i].
        for (i, e) in drained.iter().enumerate() {
            let expected = &shelves[shelves.len() - 1 - i];
            prop_assert_eq!(&e.shelf, expected);
        }

        // After drain, every previously inserted shelf is gone.
        for s in &inserted_names {
            prop_assert!(router.lookup(s).is_none());
        }
    }

    /// An `Arc<PluginEntry>` cloned out of `lookup` continues to keep
    /// the entry alive after the router is dropped. This pins the
    /// clone-out-of-guard discipline at the type-system level: the
    /// Arc's strong count holds even when the router that originally
    /// owned the Arc is gone.
    #[test]
    fn arc_outlives_router_drop(shelves in unique_shelves(8)) {
        // At least one shelf so we have something to look up.
        prop_assume!(!shelves.is_empty());

        let router = PluginRouter::new(fresh_state());
        for s in &shelves {
            router.insert(entry_for(s, s)).expect("insert");
        }

        let target = &shelves[0];
        let cloned = router
            .lookup(target)
            .expect("target shelf must be reachable");
        let strong_before_drop = Arc::strong_count(&cloned);
        prop_assert!(strong_before_drop >= 2);

        // Drop the router: the routing table and its internal Arc
        // references go away. The cloned Arc must still be valid.
        drop(router);

        let strong_after_drop = Arc::strong_count(&cloned);
        prop_assert_eq!(strong_after_drop, 1);
        // The entry data is still readable through the cloned Arc.
        prop_assert_eq!(&cloned.shelf, target);
    }

    /// Inserting a duplicate shelf is rejected; the router's table
    /// remains in the same shape it was in before the duplicate
    /// attempt.
    #[test]
    fn duplicate_insert_is_rejected_and_table_unchanged(
        shelves in unique_shelves(8),
    ) {
        prop_assume!(!shelves.is_empty());

        let router = PluginRouter::new(fresh_state());
        for s in &shelves {
            router.insert(entry_for(s, s)).expect("insert");
        }

        let len_before = router.len();
        let target = &shelves[0];
        let dup = router.insert(entry_for(target, target));
        prop_assert!(dup.is_err());
        prop_assert_eq!(router.len(), len_before);

        // Original entry is still reachable.
        prop_assert!(router.lookup(target).is_some());
    }
}
