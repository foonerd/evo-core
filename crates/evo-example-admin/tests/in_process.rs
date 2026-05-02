//! In-process integration tests for `evo-example-admin`.
//!
//! Exercise the full admission + dispatch flow:
//!
//! - Admit a "victim" plugin that claims addressings/relations, plus
//!   the admin plugin.
//! - Admit the admin plugin (under various manifest flags / trust
//!   classes) to verify the admission-layer gate.
//! - Dispatch admin requests and observe that the registry and graph
//!   reflect the cross-plugin retract.
//!
//! These tests live in a dedicated crate rather than in evo's own
//! tests module so they exercise the public admission + SDK surface
//! the same way a third-party admin plugin would.

use std::path::PathBuf;
use std::sync::Arc;

use evo::admin::AdminLedger;
use evo::admission::AdmissionEngine;
use evo::catalogue::Catalogue;
use evo::config::PluginsSecurityConfig;
use evo::custody::CustodyLedger;
use evo::error::StewardError;
use evo::happenings::HappeningBus;
use evo::persistence::MemoryPersistenceStore;
use evo::relations::RelationGraph;
use evo::state::StewardState;
use evo::subjects::SubjectRegistry;
use evo_example_admin::{
    manifest as admin_manifest, AddressingPayload, AdminExamplePlugin,
    AdminMergeRequest, AdminRetractAddressingRequest, AdminRetractClaimRequest,
    AdminSplitRequest, AdminSuppressRequest, AdminUnsuppressRequest,
    SplitStrategyPayload, REQ_MERGE, REQ_RETRACT_ADDRESSING, REQ_RETRACT_CLAIM,
    REQ_SPLIT, REQ_SUPPRESS, REQ_UNSUPPRESS,
};
use evo_plugin_sdk::contract::{
    BuildInfo, ExternalAddressing, HealthReport, HealthStatus, LoadContext,
    Plugin, PluginDescription, PluginError, PluginIdentity, RelationAssertion,
    Request, Respondent, Response, RuntimeCapabilities, SubjectAnnouncement,
};
use evo_plugin_sdk::Manifest;

// ---------------------------------------------------------------------
// Test catalogue.
//
// Declares both the administration rack (for the admin plugin) and
// an example rack plus the `album_of` predicate so a victim plugin
// can assert relations between tracks and albums.
// ---------------------------------------------------------------------

/// Build an `AdmissionEngine` over a fresh `StewardState` carrying
/// the supplied catalogue and default-constructed stores.
fn engine_with_catalogue(catalogue: Arc<Catalogue>) -> AdmissionEngine {
    let state = StewardState::builder()
        .catalogue(catalogue)
        .subjects(Arc::new(SubjectRegistry::new()))
        .relations(Arc::new(RelationGraph::new()))
        .custody(Arc::new(CustodyLedger::new()))
        .bus(Arc::new(HappeningBus::new()))
        .admin(Arc::new(AdminLedger::new()))
        .persistence(Arc::new(MemoryPersistenceStore::new()))
        .claimant_issuer(Arc::new(evo::claimant::ClaimantTokenIssuer::new(
            "test-instance",
        )))
        .build()
        .expect("steward state must build");
    AdmissionEngine::new(
        state,
        PathBuf::from("/tmp/evo-admin-in-process-test-data-root"),
        std::path::PathBuf::new(),
        None,
        PluginsSecurityConfig::default(),
    )
}

fn test_catalogue() -> Arc<Catalogue> {
    Arc::new(
        Catalogue::from_toml(
            r#"
schema_version = 1

[[racks]]
name = "administration"
family = "meta"
charter = "Framework-correction primitives"

[[racks.shelves]]
name = "subjects"
shape = 1
description = "Privileged cross-plugin subject administration"

[[racks]]
name = "example"
family = "domain"
charter = "Example rack for integration tests"

[[racks.shelves]]
name = "victim"
shape = 1
description = "Shelf stocked by a victim plugin in admin tests"

[[subjects]]
name = "track"

[[subjects]]
name = "album"

[[relation]]
predicate = "album_of"
source_type = "track"
target_type = "album"
"#,
        )
        .expect("test catalogue must parse"),
    )
}

// ---------------------------------------------------------------------
// Victim plugin that announces a track and an album and asserts a
// relation between them. Used to seed subjects / relations that the
// admin plugin then force-retracts.
// ---------------------------------------------------------------------

struct VictimPlugin {
    name: String,
    loaded: bool,
}

impl Plugin for VictimPlugin {
    fn describe(
        &self,
    ) -> impl std::future::Future<Output = PluginDescription> + Send + '_ {
        let name = self.name.clone();
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name,
                    version: semver::Version::new(0, 1, 0),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec!["noop".into()],
                    accepts_custody: false,
                    flags: Default::default(),
                },
                build_info: BuildInfo {
                    plugin_build: "test".into(),
                    sdk_version: evo_plugin_sdk::VERSION.to_string(),
                    rustc_version: None,
                    built_at: None,
                },
            }
        }
    }

    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> impl std::future::Future<Output = Result<(), PluginError>> + Send + 'a
    {
        async move {
            ctx.subject_announcer
                .announce(SubjectAnnouncement::new(
                    "track",
                    vec![
                        ExternalAddressing::new("mpd-path", "/a.flac"),
                        ExternalAddressing::new("mbid", "track-abc"),
                    ],
                ))
                .await
                .map_err(|e| {
                    PluginError::Permanent(format!("announce track: {e}"))
                })?;
            ctx.subject_announcer
                .announce(SubjectAnnouncement::new(
                    "album",
                    vec![ExternalAddressing::new("mbid", "album-x")],
                ))
                .await
                .map_err(|e| {
                    PluginError::Permanent(format!("announce album: {e}"))
                })?;
            ctx.relation_announcer
                .assert(RelationAssertion::new(
                    ExternalAddressing::new("mpd-path", "/a.flac"),
                    "album_of",
                    ExternalAddressing::new("mbid", "album-x"),
                ))
                .await
                .map_err(|e| {
                    PluginError::Permanent(format!("assert album_of: {e}"))
                })?;
            self.loaded = true;
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl std::future::Future<Output = Result<(), PluginError>> + Send + '_
    {
        async move {
            self.loaded = false;
            Ok(())
        }
    }

    fn health_check(
        &self,
    ) -> impl std::future::Future<Output = HealthReport> + Send + '_ {
        async move { HealthReport::healthy() }
    }
}

impl Respondent for VictimPlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl std::future::Future<Output = Result<Response, PluginError>> + Send + 'a
    {
        async move { Ok(Response::for_request(req, Vec::new())) }
    }
}

fn victim_manifest(name: &str) -> Manifest {
    let toml = format!(
        r#"
[plugin]
name = "{name}"
version = "0.1.0"
contract = 1

[target]
shelf = "example.victim"
shape = 1

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "in-process"
exec = "<compiled-in>"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities.respondent]
request_types = ["noop"]
response_budget_ms = 1000
"#
    );
    Manifest::from_toml(&toml).unwrap()
}

// ---------------------------------------------------------------------
// Integration tests.
// ---------------------------------------------------------------------

#[tokio::test]
async fn admin_forced_retract_addressing_removes_stale_entry() {
    let catalogue = test_catalogue();
    let mut engine = engine_with_catalogue(Arc::clone(&catalogue));

    // Victim claims a track with two addressings, on shelf
    // example.victim.
    engine
        .admit_singleton_respondent(
            VictimPlugin {
                name: "org.test.victim".into(),
                loaded: false,
            },
            victim_manifest("org.test.victim"),
        )
        .await
        .expect("victim must admit");
    // Victim announced 3 addressings total: 2 on the track
    // (mpd-path + mbid) plus 1 on the album (mbid), across 2
    // subjects.
    assert_eq!(engine.registry().addressing_count(), 3);
    assert_eq!(engine.registry().subject_count(), 2);

    // Admit the admin plugin. Its manifest declares
    // capabilities.admin = true and trust = privileged, so
    // admission accepts it and populates subject_admin /
    // relation_admin in its LoadContext.
    engine
        .admit_singleton_respondent(AdminExamplePlugin::new(), admin_manifest())
        .await
        .expect("admin must admit");

    // Dispatch a retract_addressing request against victim's
    // mpd-path addressing.
    let body = AdminRetractAddressingRequest {
        target_plugin: "org.test.victim".into(),
        scheme: "mpd-path".into(),
        value: "/a.flac".into(),
        reason: Some("operator sweep".into()),
    };
    let req = Request {
        request_type: REQ_RETRACT_ADDRESSING.into(),
        payload: serde_json::to_vec(&body).unwrap(),
        correlation_id: 1,
        deadline: None,

        instance_id: None,
    };
    let resp = engine
        .router()
        .handle_request("administration.subjects", req)
        .await
        .expect("admin request must succeed");
    assert!(resp.payload.is_empty());

    // The victim's mpd-path addressing is gone; the track
    // survives with its mbid addressing, and the album is
    // untouched. Addressings: 1 on track + 1 on album = 2.
    // Subjects: both track and album survive = 2. Ledger has
    // one entry for the cross-plugin retract.
    assert_eq!(engine.registry().addressing_count(), 2);
    assert_eq!(engine.registry().subject_count(), 2);
    assert_eq!(engine.admin_ledger().count(), 1);
}

#[tokio::test]
async fn admin_forced_retract_claim_removes_other_plugin_claim() {
    let catalogue = test_catalogue();
    let mut engine = engine_with_catalogue(Arc::clone(&catalogue));

    engine
        .admit_singleton_respondent(
            VictimPlugin {
                name: "org.test.victim".into(),
                loaded: false,
            },
            victim_manifest("org.test.victim"),
        )
        .await
        .unwrap();
    engine
        .admit_singleton_respondent(AdminExamplePlugin::new(), admin_manifest())
        .await
        .unwrap();
    assert_eq!(engine.relation_graph().claim_count(), 1);

    let body = AdminRetractClaimRequest {
        target_plugin: "org.test.victim".into(),
        source: AddressingPayload {
            scheme: "mpd-path".into(),
            value: "/a.flac".into(),
        },
        predicate: "album_of".into(),
        target: AddressingPayload {
            scheme: "mbid".into(),
            value: "album-x".into(),
        },
        reason: None,
    };
    let req = Request {
        request_type: REQ_RETRACT_CLAIM.into(),
        payload: serde_json::to_vec(&body).unwrap(),
        correlation_id: 2,
        deadline: None,

        instance_id: None,
    };
    engine
        .router()
        .handle_request("administration.subjects", req)
        .await
        .expect("admin relation retract must succeed");

    // Victim was the only claimant: relation is forgotten.
    assert_eq!(engine.relation_graph().relation_count(), 0);
    assert_eq!(engine.admin_ledger().count(), 1);
}

#[tokio::test]
async fn admin_refused_without_admin_capability_manifest() {
    // A manifest that declares the same identity but omits
    // capabilities.admin. The admin plugin's load will unwrap
    // ctx.subject_admin and fail with Permanent because non-admin
    // manifests leave those fields None. Surface: admission fails
    // with StewardError::Admission ("load failed: ...").
    let catalogue = test_catalogue();
    let mut engine = engine_with_catalogue(Arc::clone(&catalogue));

    // Build a manifest matching evo-example-admin's identity but
    // without the admin flag.
    let toml_no_admin = r#"
[plugin]
name = "org.evo.example.admin"
version = "0.1.0"
contract = 1

[target]
shelf = "administration.subjects"
shape = 1

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "in-process"
exec = "<compiled-in>"

[trust]
class = "privileged"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"

[capabilities.respondent]
request_types = ["admin.subject.retract_addressing"]
response_budget_ms = 1000
"#;
    let manifest = Manifest::from_toml(toml_no_admin).unwrap();
    assert!(
        !manifest.capabilities.admin,
        "fixture must not declare admin = true"
    );

    let r = engine
        .admit_singleton_respondent(AdminExamplePlugin::new(), manifest)
        .await;
    match r {
        Err(StewardError::Admission(msg)) => {
            // Admission surfaces the plugin's load error.
            assert!(
                msg.contains("subject_admin") || msg.contains("load failed"),
                "expected diagnostic mentioning subject_admin/load, got {msg:?}"
            );
        }
        other => {
            panic!("expected Admission error from failed load, got {other:?}")
        }
    }
    assert_eq!(engine.len(), 0);
}

#[tokio::test]
async fn admin_admitted_at_platform_trust() {
    // Platform trust qualifies for the admin gate (Platform is more
    // privileged than Privileged in the ord). Fixture modifies the
    // embedded manifest's trust class and confirms admission still
    // succeeds.
    let catalogue = test_catalogue();
    let mut engine = engine_with_catalogue(Arc::clone(&catalogue));

    let mut manifest = admin_manifest();
    manifest.trust.class = evo_plugin_sdk::manifest::TrustClass::Platform;

    engine
        .admit_singleton_respondent(AdminExamplePlugin::new(), manifest)
        .await
        .expect("platform-class admin plugin must admit");
    assert_eq!(engine.len(), 1);
}

#[tokio::test]
async fn admin_refused_at_standard_trust() {
    // Standard trust is below the admin minimum (Privileged), so
    // the admin-trust gate must refuse the plugin. StewardError's
    // AdminTrustTooLow variant is the signal.
    let catalogue = test_catalogue();
    let mut engine = engine_with_catalogue(Arc::clone(&catalogue));

    let mut manifest = admin_manifest();
    manifest.trust.class = evo_plugin_sdk::manifest::TrustClass::Standard;

    let r = engine
        .admit_singleton_respondent(AdminExamplePlugin::new(), manifest)
        .await;
    match r {
        Err(StewardError::AdminTrustTooLow {
            plugin_name,
            effective,
            minimum,
        }) => {
            assert_eq!(plugin_name, "org.evo.example.admin");
            assert_eq!(
                effective,
                evo_plugin_sdk::manifest::TrustClass::Standard
            );
            assert_eq!(
                minimum,
                evo_plugin_sdk::manifest::TrustClass::Privileged
            );
        }
        other => panic!("expected AdminTrustTooLow, got {other:?}"),
    }
    assert_eq!(engine.len(), 0);

    // Guard against unused warnings: HealthStatus is imported for
    // documentation parity with other example-plugin tests even
    // though this test does not exercise health paths directly.
    let _ = HealthStatus::Healthy;
}

// ---------------------------------------------------------------------
// Merge / split / suppress / unsuppress integration tests.
//
// These exercise the four request types added on top of the
// forced-retract pair. Each test admits the victim plugin
// (announcing a track + album + album_of relation), admits the
// admin plugin, then dispatches one or more admin requests and
// observes the resulting registry / graph / ledger state.
// ---------------------------------------------------------------------

#[tokio::test]
async fn admin_merge_collapses_two_tracks() {
    let catalogue = test_catalogue();
    let mut engine = engine_with_catalogue(Arc::clone(&catalogue));

    engine
        .admit_singleton_respondent(
            VictimPlugin {
                name: "org.test.victim".into(),
                loaded: false,
            },
            victim_manifest("org.test.victim"),
        )
        .await
        .unwrap();
    engine
        .admit_singleton_respondent(AdminExamplePlugin::new(), admin_manifest())
        .await
        .unwrap();

    // Announce a second track via the registry directly under a
    // different plugin name. This mirrors what a second victim
    // plugin would do without requiring another fixture struct.
    engine
        .registry()
        .announce(
            &SubjectAnnouncement::new(
                "track",
                vec![ExternalAddressing::new("mbid", "track-secondary")],
            ),
            "org.test.secondary",
        )
        .unwrap();

    // Pre-merge: victim's track + album + secondary track = 3.
    assert_eq!(engine.registry().subject_count(), 3);

    let body = AdminMergeRequest {
        target_a: AddressingPayload {
            scheme: "mpd-path".into(),
            value: "/a.flac".into(),
        },
        target_b: AddressingPayload {
            scheme: "mbid".into(),
            value: "track-secondary".into(),
        },
        reason: Some("operator confirmed identity".into()),
    };
    let req = Request {
        request_type: REQ_MERGE.into(),
        payload: serde_json::to_vec(&body).unwrap(),
        correlation_id: 3,
        deadline: None,

        instance_id: None,
    };
    engine
        .router()
        .handle_request("administration.subjects", req)
        .await
        .expect("admin merge must succeed");

    // Post-merge: merged track + album = 2.
    assert_eq!(engine.registry().subject_count(), 2);
    assert_eq!(engine.admin_ledger().count(), 1);

    // Both pre-merge addressings now resolve to the same new ID.
    let from_mpd = engine
        .registry()
        .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
        .expect("mpd-path must resolve");
    let from_secondary = engine
        .registry()
        .resolve(&ExternalAddressing::new("mbid", "track-secondary"))
        .expect("secondary mbid must resolve");
    assert_eq!(from_mpd, from_secondary);
}

#[tokio::test]
async fn admin_split_creates_new_subjects() {
    let catalogue = test_catalogue();
    let mut engine = engine_with_catalogue(Arc::clone(&catalogue));

    engine
        .admit_singleton_respondent(
            VictimPlugin {
                name: "org.test.victim".into(),
                loaded: false,
            },
            victim_manifest("org.test.victim"),
        )
        .await
        .unwrap();
    engine
        .admit_singleton_respondent(AdminExamplePlugin::new(), admin_manifest())
        .await
        .unwrap();

    // Pre-split: victim's track (with 2 addressings) + album = 2.
    assert_eq!(engine.registry().subject_count(), 2);

    let body = AdminSplitRequest {
        source: AddressingPayload {
            scheme: "mpd-path".into(),
            value: "/a.flac".into(),
        },
        partition: vec![
            vec![AddressingPayload {
                scheme: "mpd-path".into(),
                value: "/a.flac".into(),
            }],
            vec![AddressingPayload {
                scheme: "mbid".into(),
                value: "track-abc".into(),
            }],
        ],
        strategy: SplitStrategyPayload::ToBoth,
        explicit_assignments: vec![],
        reason: Some("not the same track".into()),
    };
    let req = Request {
        request_type: REQ_SPLIT.into(),
        payload: serde_json::to_vec(&body).unwrap(),
        correlation_id: 4,
        deadline: None,

        instance_id: None,
    };
    engine
        .router()
        .handle_request("administration.subjects", req)
        .await
        .expect("admin split must succeed");

    // Post-split: two new tracks + album = 3.
    assert_eq!(engine.registry().subject_count(), 3);
    assert_eq!(engine.admin_ledger().count(), 1);
}

#[tokio::test]
async fn admin_suppress_hides_relation() {
    let catalogue = test_catalogue();
    let mut engine = engine_with_catalogue(Arc::clone(&catalogue));

    engine
        .admit_singleton_respondent(
            VictimPlugin {
                name: "org.test.victim".into(),
                loaded: false,
            },
            victim_manifest("org.test.victim"),
        )
        .await
        .unwrap();
    engine
        .admit_singleton_respondent(AdminExamplePlugin::new(), admin_manifest())
        .await
        .unwrap();

    assert_eq!(engine.relation_graph().relation_count(), 1);

    let body = AdminSuppressRequest {
        source: AddressingPayload {
            scheme: "mpd-path".into(),
            value: "/a.flac".into(),
        },
        predicate: "album_of".into(),
        target: AddressingPayload {
            scheme: "mbid".into(),
            value: "album-x".into(),
        },
        reason: Some("disputed".into()),
    };
    let req = Request {
        request_type: REQ_SUPPRESS.into(),
        payload: serde_json::to_vec(&body).unwrap(),
        correlation_id: 5,
        deadline: None,

        instance_id: None,
    };
    engine
        .router()
        .handle_request("administration.subjects", req)
        .await
        .expect("admin suppress must succeed");

    // Relation still exists; suppression hides it from neighbour
    // queries via the is_suppressed flag.
    assert_eq!(engine.relation_graph().relation_count(), 1);
    let source_id = engine
        .registry()
        .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
        .unwrap();
    let target_id = engine
        .registry()
        .resolve(&ExternalAddressing::new("mbid", "album-x"))
        .unwrap();
    assert!(engine
        .relation_graph()
        .is_suppressed(&source_id, "album_of", &target_id));
    assert_eq!(engine.admin_ledger().count(), 1);
}

#[tokio::test]
async fn admin_unsuppress_restores_relation() {
    let catalogue = test_catalogue();
    let mut engine = engine_with_catalogue(Arc::clone(&catalogue));

    engine
        .admit_singleton_respondent(
            VictimPlugin {
                name: "org.test.victim".into(),
                loaded: false,
            },
            victim_manifest("org.test.victim"),
        )
        .await
        .unwrap();
    engine
        .admit_singleton_respondent(AdminExamplePlugin::new(), admin_manifest())
        .await
        .unwrap();

    let suppress = AdminSuppressRequest {
        source: AddressingPayload {
            scheme: "mpd-path".into(),
            value: "/a.flac".into(),
        },
        predicate: "album_of".into(),
        target: AddressingPayload {
            scheme: "mbid".into(),
            value: "album-x".into(),
        },
        reason: None,
    };
    engine
        .router()
        .handle_request(
            "administration.subjects",
            Request {
                request_type: REQ_SUPPRESS.into(),
                payload: serde_json::to_vec(&suppress).unwrap(),
                correlation_id: 6,
                deadline: None,

                instance_id: None,
            },
        )
        .await
        .unwrap();

    let source_id = engine
        .registry()
        .resolve(&ExternalAddressing::new("mpd-path", "/a.flac"))
        .unwrap();
    let target_id = engine
        .registry()
        .resolve(&ExternalAddressing::new("mbid", "album-x"))
        .unwrap();
    assert!(engine
        .relation_graph()
        .is_suppressed(&source_id, "album_of", &target_id));

    let unsuppress = AdminUnsuppressRequest {
        source: AddressingPayload {
            scheme: "mpd-path".into(),
            value: "/a.flac".into(),
        },
        predicate: "album_of".into(),
        target: AddressingPayload {
            scheme: "mbid".into(),
            value: "album-x".into(),
        },
    };
    engine
        .router()
        .handle_request(
            "administration.subjects",
            Request {
                request_type: REQ_UNSUPPRESS.into(),
                payload: serde_json::to_vec(&unsuppress).unwrap(),
                correlation_id: 7,
                deadline: None,

                instance_id: None,
            },
        )
        .await
        .unwrap();

    assert!(!engine
        .relation_graph()
        .is_suppressed(&source_id, "album_of", &target_id));
    // Two ledger entries: suppress, then unsuppress.
    assert_eq!(engine.admin_ledger().count(), 2);
}
