//! # evo
//!
//! The evo steward. Administers a catalogue, admits plugins, emits
//! projections to any consumer that looks.
//!
//! This crate is both a library and a binary. The binary
//! (`src/main.rs`) is a thin wrapper that calls [`run`] with the
//! default admission strategy ([`discover_plugins`]). Distributions
//! that want to admit their own plugin set programmatically build
//! their own `main.rs` that calls [`run`] with a custom
//! [`AdmissionSetup`].
//!
//! ## Library boot
//!
//! A distribution that wants to compose its own steward binary calls
//! [`run`]:
//!
//! ```no_run
//! use clap::Parser as _;
//! # async fn example() -> anyhow::Result<()> {
//! let args = evo::cli::Args::parse();
//! evo::run(evo::RunOptions::from_args(args)).await
//! # }
//! ```
//!
//! That uses the default admission strategy: walk the configured
//! `plugins.search_roots` and admit out-of-process singletons per
//! [`plugin_discovery`]. To admit a programmatic plugin set instead
//! (or in addition):
//!
//! ```no_run
//! use clap::Parser as _;
//! # async fn example() -> anyhow::Result<()> {
//! let args = evo::cli::Args::parse();
//! let admission: evo::AdmissionSetup = Box::new(|engine, config| {
//!     Box::pin(async move {
//!         // Admit distribution-supplied plugins here, e.g.
//!         // `engine.admit_in_process_respondent(...)`. Optionally
//!         // run discovery as well.
//!         evo::plugin_discovery::discover_and_admit(engine, config)
//!             .await
//!             .map_err(anyhow::Error::from)
//!     })
//! });
//! evo::run(evo::RunOptions { args, admission }).await
//! # }
//! ```
//!
//! Tests do not need [`run`]; they construct the admission engine
//! directly and exercise it through the library types.
//!
//! ## Module map
//!
//! - [`config`]: steward configuration ([`config::StewardConfig`]), loaded
//!   from `/etc/evo/evo.toml`.
//! - [`cli`]: command-line argument parsing (clap derive).
//! - [`catalogue`]: the rack/shelf catalogue the steward administers.
//! - [`admission`]: the admission engine that runs plugin lifecycles.
//! - [`subjects`]: the subject registry, implementing `SUBJECTS.md`.
//! - [`relations`]: the relation graph, implementing `RELATIONS.md`.
//! - [`router`]: the per-request plugin router holding the routing
//!   table behind a finer-grained synchronisation primitive than
//!   the engine mutex. Receives admitted entries from
//!   [`admission`] and dispatches lookups via the
//!   lookup-clone-drop pattern.
//! - [`context`]: concrete implementations of the SDK callback traits
//!   supplied to plugins in their [`LoadContext`].
//! - [`custody`]: the custody ledger, tracking every custody the
//!   steward has handed to a warden.
//! - [`happenings`]: streamed notification surface for fabric
//!   transitions. Subscribers observe custody (and, later,
//!   other) transitions without polling.
//! - [`server`]: the client-facing Unix socket server.
//! - [`shutdown`]: graceful shutdown on SIGTERM / SIGINT / Ctrl-C.
//! - [`state`]: immutable handle bag of shared steward stores
//!   ([`state::StewardState`]). Built once at boot; consumed by
//!   future passes that decouple dispatch from the per-engine mutex.
//! - [`sync`]: shape model of the router's table-of-`Arc`s
//!   synchronisation core. Hosts the [`sync::RouterTable`] type used
//!   by the property tests in `tests/router_proptest.rs`. Mirrored
//!   one-to-one (against loom's instrumented primitives) by the
//!   stand-alone `evo-loom` crate's loom model-checking test.
//! - [`persistence`]: durable storage for the steward's fabric.
//!   Defines the schema-aware [`persistence::PersistenceStore`]
//!   trait and ships an SQLite-backed implementation alongside an
//!   in-memory mock for tests. Subject registry, custody ledger,
//!   relation graph, admin ledger, and happenings cursor all write
//!   through this trait; boot-time replay rehydrates each store.
//! - [`logging`]: tracing subscriber setup per the LOGGING contract.
//! - [`wire_client`]: steward-side client for out-of-process plugins
//!   speaking the wire protocol from `PLUGIN_CONTRACT.md` sections 6
//!   through 11.
//! - [`projections`]: pull-projection engine per `PROJECTIONS.md`. v0
//!   supports federated (subject-keyed) queries with one-hop relation
//!   traversal.
//! - [`resolution`]: audit ledger for `resolve_claimants` queries
//!   on the client API. One entry per call (granted or refused),
//!   carrying the peer's UID/GID and the request's request-and-
//!   resolve counts.
//! - [`error`]: the steward's error type.
//! - [`plugin_discovery`]: optional scan of configured search roots and
//!   admission of out-of-process plugins (used by the shipped binary).
//!
//! This crate implements the v0 skeleton: singleton respondents and
//! wardens, plugin discovery for out-of-process bundles, and a minimal
//! socket protocol. The engineering layer documents in
//! `docs/engineering/` are the source of truth for where this is going.
//!
//! [`LoadContext`]: evo_plugin_sdk::contract::LoadContext

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod admin;
pub mod admission;
pub mod catalogue;
pub mod claimant;
pub mod cli;
pub mod client_acl;
pub mod config;
pub mod context;
pub mod custody;
pub mod error;
pub mod error_taxonomy;
pub mod factory;
pub mod happenings;
pub mod logging;
pub mod persistence;
pub mod plugin_discovery;
pub mod plugin_trust;
pub mod projections;
pub mod relations;
pub mod resolution;
pub mod router;
pub mod server;
pub mod shutdown;
pub mod state;
pub mod subjects;
pub mod sync;
pub mod wire_client;

pub use error::StewardError;
pub use state::{StewardState, StewardStateBuildError, StewardStateBuilder};

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::Mutex;

/// Plugin admission strategy invoked once during boot, after the
/// admission engine is constructed and before the server starts.
///
/// The default strategy ([`discover_plugins`]) runs the in-tree
/// [`plugin_discovery::discover_and_admit`] pass over the
/// configured `plugins.search_roots`. A distribution that prefers
/// programmatic admission (calling
/// [`admission::AdmissionEngine::admit_out_of_process_from_directory`]
/// or the in-process admission entries directly) supplies its own.
///
/// The closure receives `&mut AdmissionEngine` and `&StewardConfig`
/// so a distribution can both inspect operator settings and admit
/// plugins in the same call. Return `Ok(())` on success; any error
/// aborts boot before the server starts.
pub type AdmissionSetup = Box<
    dyn for<'a> FnOnce(
            &'a mut admission::AdmissionEngine,
            &'a config::StewardConfig,
        ) -> Pin<
            Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>,
        > + Send,
>;

/// Default admission strategy: run [`plugin_discovery::discover_and_admit`]
/// against the configured `plugins.search_roots`. Out-of-process
/// singletons are admitted; factory and in-process bundles are
/// skipped with a warning.
///
/// The shipped `evo` binary uses this by default
/// ([`RunOptions::from_args`]). Distributions may compose this
/// with programmatic admission inside their own [`AdmissionSetup`].
pub fn discover_plugins() -> AdmissionSetup {
    Box::new(|engine, config| {
        Box::pin(async move {
            plugin_discovery::discover_and_admit(engine, config)
                .await
                .map_err(anyhow::Error::from)
        })
    })
}

/// Runtime options for [`run`].
///
/// Carries parsed CLI [`cli::Args`] plus the admission strategy
/// the steward applies during boot. Construct via
/// [`Self::from_args`] for the default discovery-based strategy or
/// directly for a programmatic admission set.
pub struct RunOptions {
    /// Parsed CLI arguments. Pass `Args::parse()` to honour the
    /// process command line, or construct directly for embedded /
    /// test builds.
    pub args: cli::Args,
    /// Plugin admission strategy. Default: [`discover_plugins`].
    pub admission: AdmissionSetup,
}

impl RunOptions {
    /// Construct [`RunOptions`] using the default admission strategy
    /// ([`discover_plugins`]). The shipped `evo` binary uses this
    /// shape; distributions composing their own steward binary call
    /// the [`Self::new`] constructor.
    pub fn from_args(args: cli::Args) -> Self {
        Self {
            args,
            admission: discover_plugins(),
        }
    }

    /// Construct [`RunOptions`] with an explicit admission strategy.
    pub fn new(args: cli::Args, admission: AdmissionSetup) -> Self {
        Self { args, admission }
    }
}

/// Boot, run, and drain the steward.
///
/// Encapsulates the full v0.1.x boot sequence: CLI / config / logging
/// / catalogue / persistence / instance-id / catalogue-orphan diagnostic
/// / happenings bus / janitor / subject-registry rehydration / shared
/// state / admission-engine construction / plugin admission
/// (per [`RunOptions::admission`]) / projection engine / client ACL /
/// resolution ledger / server bind / shutdown wait / signal forwarding
/// / janitor join / admission drain / WAL checkpoint.
///
/// Distributions that want a steward with their own plugin set call
/// this function from their own `main`. The shipped `evo` binary
/// itself is a 12-line wrapper around `run(RunOptions::from_args(...))`.
pub async fn run(opts: RunOptions) -> anyhow::Result<()> {
    let RunOptions { args, admission } = opts;

    // Load the config. If --config was given, missing file is an
    // error; otherwise a missing default config silently falls back.
    let config = match &args.config {
        Some(path) => config::StewardConfig::load_from_required(path)?,
        None => config::StewardConfig::load()?,
    };

    // Resolve effective paths. CLI flags override config values.
    let catalogue_path: PathBuf = args
        .catalogue
        .clone()
        .unwrap_or_else(|| config.catalogue.path.clone());
    let socket_path: PathBuf = args
        .socket
        .clone()
        .unwrap_or_else(|| config.steward.socket_path.clone());

    // Initialise logging. CLI --log-level takes precedence over
    // RUST_LOG and over config.steward.log_level.
    logging::init(&config, args.log_level.as_deref())?;

    tracing::info!(version = env!("CARGO_PKG_VERSION"), "evo starting");

    // Load the catalogue. Wrap in Arc so the relation announcer can
    // hold a shared handle alongside the subject registry and
    // relation graph for predicate-existence validation at assertion.
    let catalogue = Arc::new(catalogue::Catalogue::load(&catalogue_path)?);
    tracing::info!(
        racks = catalogue.racks.len(),
        shelves = catalogue
            .racks
            .iter()
            .map(|r| r.shelves.len())
            .sum::<usize>(),
        path = %catalogue_path.display(),
        "catalogue loaded"
    );

    // Open the durable persistence store. The file is created if
    // absent; pragmas are applied to every pooled connection;
    // pending migrations are run before the handle is returned.
    let persistence_path = config.persistence.path.clone();
    let persistence: Arc<dyn persistence::PersistenceStore> = Arc::new(
        persistence::SqlitePersistenceStore::open(persistence_path.clone())
            .map_err(|e| {
                anyhow::anyhow!(
                    "opening persistence store at {}: {e}",
                    persistence_path.display()
                )
            })?,
    );
    tracing::info!(
        path = %persistence_path.display(),
        "persistence store opened"
    );

    // Load the steward instance ID from persistence. Pinned at first
    // boot, persisted forever; anchors the per-deployment
    // unlinkability of claimant tokens.
    let instance_id = persistence
        .load_instance_id()
        .await
        .map_err(|e| anyhow::anyhow!("loading instance_id: {e}"))?;
    tracing::info!(instance_id = %instance_id, "steward instance identified");

    // Catalogue-orphan diagnostic. Boot-time scan of every persisted
    // subject_type against the loaded catalogue's declared types: a
    // type that appears in storage but not in the catalogue is an
    // orphan. The diagnostic does not refuse boot or modify state.
    let declared_types: std::collections::HashSet<String> =
        catalogue.subjects.iter().map(|s| s.name.clone()).collect();
    match persistence.count_subjects_by_type().await {
        Ok(persisted) => {
            let mut orphan_types = 0usize;
            let mut orphan_rows: u64 = 0;
            for (subject_type, count) in &persisted {
                if !declared_types.contains(subject_type) {
                    tracing::warn!(
                        subject_type = %subject_type,
                        count = *count,
                        "catalogue orphan: persisted subjects of this type \
                         remain in storage but the loaded catalogue no longer \
                         declares the type; these subjects are read-only via \
                         existing queries and cannot be re-announced or \
                         re-stated until a migration verb lands"
                    );
                    orphan_types += 1;
                    orphan_rows += count;
                }
            }
            if orphan_types == 0 {
                tracing::info!(
                    declared_types = declared_types.len(),
                    persisted_types = persisted.len(),
                    "catalogue-orphan scan: no orphans"
                );
            } else {
                tracing::warn!(
                    orphan_types,
                    orphan_rows,
                    "catalogue-orphan scan: {} orphaned subject_type(s) \
                     covering {} row(s); operator action recommended",
                    orphan_types,
                    orphan_rows
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "catalogue-orphan scan: persistence accessor failed; orphan \
                 detection skipped this boot"
            );
        }
    }
    let claimant_issuer =
        Arc::new(claimant::ClaimantTokenIssuer::new(instance_id));

    // Construct the happenings bus with persistence write-through
    // and operator-tunable retention.
    let bus = Arc::new(
        happenings::HappeningBus::with_persistence_capacity_and_window(
            Arc::clone(&persistence),
            config.happenings.retention_capacity,
            config.happenings.retention_window_secs,
        )
        .await
        .map_err(|e| anyhow::anyhow!("seeding happenings bus: {e}"))?,
    );

    // Spawn the happenings_log janitor.
    let janitor_shutdown = Arc::new(tokio::sync::Notify::new());
    let janitor_persistence = Arc::clone(&persistence);
    let janitor_capacity = config.happenings.retention_capacity as u64;
    let janitor_window = config.happenings.retention_window_secs;
    let janitor_interval = config.happenings.janitor_interval_secs;
    let janitor_shutdown_for_task = Arc::clone(&janitor_shutdown);
    let janitor_task = tokio::spawn(async move {
        happenings::run_happenings_janitor(
            janitor_persistence,
            janitor_window,
            janitor_capacity,
            janitor_interval,
            janitor_shutdown_for_task,
        )
        .await;
    });

    // Construct the in-memory conflict index.
    let conflict_index = Arc::new(projections::SubjectConflictIndex::new());

    // Construct the subject registry and rehydrate it from the
    // durable store BEFORE the admission engine is built.
    let subjects = Arc::new(subjects::SubjectRegistry::new());
    match subjects.rehydrate_from(persistence.as_ref()).await {
        Ok(report) => {
            tracing::info!(
                live_subjects = report.live_subjects_loaded,
                live_addressings = report.live_addressings_loaded,
                forgotten_subjects_skipped = report.forgotten_subjects_seen,
                merged_aliases = report.merged_aliases_loaded,
                split_aliases = report.split_aliases_loaded,
                tombstone_aliases = report.tombstone_aliases_loaded,
                "subject registry rehydrated from persistence"
            );
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                "subject registry rehydration failed; aborting boot to \
                 avoid serving an inconsistent in-memory view of durable \
                 state"
            );
            return Err(anyhow::anyhow!(
                "subject registry rehydration failed: {e}"
            ));
        }
    }

    // Construct the admin ledger with persistence write-through and
    // rehydrate the in-memory mirror from the `admin_log` table so a
    // restarting steward presents the same audit trail it had before.
    // Boot aborts on rehydrate failure for the same reason subject
    // registry rehydration aborts: serving an inconsistent view of
    // durable state is more dangerous than refusing to boot.
    let admin_ledger =
        Arc::new(admin::AdminLedger::with_persistence(Arc::clone(&persistence)));
    if let Err(e) = admin_ledger.rehydrate_from(persistence.as_ref()).await {
        tracing::error!(
            error = %e,
            "admin ledger rehydration failed; aborting boot to avoid \
             serving an inconsistent in-memory view of durable state"
        );
        return Err(anyhow::anyhow!(
            "admin ledger rehydration failed: {e}"
        ));
    }

    // Same shape for the custody ledger: durable write-through plus
    // boot rehydration so active custodies survive restart.
    let custody_ledger = Arc::new(custody::CustodyLedger::with_persistence(
        Arc::clone(&persistence),
    ));
    if let Err(e) = custody_ledger.rehydrate_from(persistence.as_ref()).await {
        tracing::error!(
            error = %e,
            "custody ledger rehydration failed; aborting boot to avoid \
             serving an inconsistent in-memory view of durable state"
        );
        return Err(anyhow::anyhow!(
            "custody ledger rehydration failed: {e}"
        ));
    }

    // Relation graph rehydration: load every (relation, claimants)
    // pair the persistence layer has and rebuild the in-memory
    // graph (relations, claimants, suppression markers, forward /
    // inverse indices). Aborts boot on rehydrate failure for the
    // same reason the other ledgers do.
    let relations_graph = Arc::new(relations::RelationGraph::new());
    if let Err(e) = relations_graph.rehydrate_from(persistence.as_ref()).await {
        tracing::error!(
            error = %e,
            "relation graph rehydration failed; aborting boot to avoid \
             serving an inconsistent in-memory view of durable state"
        );
        return Err(anyhow::anyhow!(
            "relation graph rehydration failed: {e}"
        ));
    }

    // Build the shared steward state once.
    let state = StewardState::builder()
        .catalogue(Arc::clone(&catalogue))
        .subjects(subjects)
        .relations(relations_graph)
        .custody(custody_ledger)
        .bus(bus)
        .admin(admin_ledger)
        .persistence(Arc::clone(&persistence))
        .claimant_issuer(Arc::clone(&claimant_issuer))
        .conflict_index(Arc::clone(&conflict_index))
        .build()?;

    // Construct the admission engine and run the distribution-
    // supplied admission setup (default: plugin discovery).
    let trust = plugin_trust::load_plugin_trust_arc(&config)?;
    let mut engine = admission::AdmissionEngine::new(
        Arc::clone(&state),
        config.plugins.plugin_data_root.clone(),
        config.plugins.config_dir.clone(),
        Some(trust),
        config.plugins.security.clone(),
    );
    admission(&mut engine, &config).await?;

    // Construct a projection engine.
    let projections = Arc::new(
        projections::ProjectionEngine::new(
            Arc::clone(&state.subjects),
            Arc::clone(&state.relations),
        )
        .with_conflict_index(Arc::clone(&conflict_index)),
    );

    // Clone an Arc to the router so the server can dispatch directly
    // through it without acquiring the admission-engine mutex.
    let router = Arc::clone(engine.router());

    // Wrap engine for shared access between any future admission-side
    // mutations and the final drain.
    let engine = Arc::new(Mutex::new(engine));

    // Spawn the factory-orphan scrub task. After the operator-
    // configured grace window expires, the task walks every persisted
    // factory-instance subject and forgets any whose owning plugin
    // has not re-announced it since boot. Disabled when
    // `factory_orphan_grace_secs = 0`.
    let factory_orphan_grace_secs = config.plugins.factory_orphan_grace_secs;
    if factory_orphan_grace_secs > 0 {
        let engine_for_scrub = Arc::clone(&engine);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(
                factory_orphan_grace_secs,
            ))
            .await;
            let report =
                engine_for_scrub.lock().await.scrub_factory_orphans().await;
            if report.forgotten > 0 || report.errored > 0 {
                tracing::info!(
                    forgotten = report.forgotten,
                    errored = report.errored,
                    grace_secs = factory_orphan_grace_secs,
                    "factory orphan scrub complete"
                );
            }
        });
    }

    // Load the operator-controlled client-API ACL.
    let client_acl = Arc::new(client_acl::ClientAcl::load()?);
    if let Some(src) = client_acl.source() {
        tracing::info!(
            path = %src.display(),
            "client capability ACL loaded"
        );
    } else {
        tracing::info!(
            "client capability ACL: default policy (no file present)"
        );
    }

    // Capture the steward's own identity once at boot.
    let steward_identity = client_acl::StewardIdentity::current();

    // Construct the audit ledger that records every
    // `resolve_claimants` call.
    let resolution_ledger = Arc::new(resolution::ResolutionLedger::new());

    // Start the server.
    let server = server::Server::with_acl(
        socket_path.clone(),
        router,
        Arc::clone(&state),
        Arc::clone(&projections),
        Arc::clone(&client_acl),
        steward_identity,
        Arc::clone(&resolution_ledger),
    );

    tracing::warn!(socket = %socket_path.display(), "evo ready");

    // Wait for either the server to exit (unlikely) or a shutdown signal.
    let shutdown_fut = shutdown::wait_for_signal();
    tokio::pin!(shutdown_fut);

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let signal_forwarder = async move {
        let _ = shutdown_fut.as_mut().await;
        let _ = tx.send(());
    };

    let server_task = tokio::spawn(async move {
        server
            .run(async move {
                let _ = rx.await;
            })
            .await
    });

    signal_forwarder.await;

    match server_task.await {
        Ok(Ok(())) => {
            tracing::info!("server loop exited cleanly");
        }
        Ok(Err(e)) => {
            tracing::error!(error = %e, "server loop returned an error");
        }
        Err(e) => {
            tracing::error!(error = %e, "server task panicked or was cancelled");
        }
    }

    // Notify the happenings_log janitor and join its task.
    janitor_shutdown.notify_waiters();
    if let Err(e) = janitor_task.await {
        tracing::warn!(error = %e, "happenings_log janitor task did not join cleanly");
    }

    // Drain: unload every admitted plugin under a global deadline.
    tracing::info!("draining admission engine");
    let report = engine
        .lock()
        .await
        .shutdown_with_config(admission::ShutdownConfig::default())
        .await;
    tracing::info!(
        plugins_total = report.plugins_total,
        plugins_unloaded_cleanly = report.plugins_unloaded_cleanly.len(),
        plugins_killed_after_deadline =
            report.plugins_killed_after_deadline.len(),
        custody_drained = report.custody_drained.len(),
        custody_abandoned = report.custody_abandoned.len(),
        elapsed_ms = report.elapsed.as_millis() as u64,
        "shutdown report"
    );
    for name in &report.plugins_unloaded_cleanly {
        tracing::info!(plugin = %name, "plugin unloaded cleanly during drain");
    }
    for name in &report.plugins_killed_after_deadline {
        tracing::warn!(
            plugin = %name,
            "plugin missed shutdown deadline; SIGKILL was sent"
        );
    }
    for c in &report.custody_abandoned {
        tracing::warn!(
            plugin = %c.plugin,
            handle_id = %c.handle_id,
            shelf = ?c.shelf,
            "custody not released cleanly within drain window"
        );
    }

    // Final WAL checkpoint before the persistence pool drops.
    match persistence.checkpoint_wal().await {
        Ok(()) => {
            tracing::info!("WAL checkpoint truncated on shutdown");
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "WAL checkpoint at shutdown failed; rows are still durable in \
                 the WAL but not yet folded into the main database file"
            );
        }
    }

    tracing::warn!("evo exited");
    Ok(())
}
