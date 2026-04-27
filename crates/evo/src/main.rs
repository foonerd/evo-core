//! Evo steward binary entrypoint.
//!
//! A thin wrapper around the `evo` library: parses CLI flags, loads
//! config, initialises logging, loads the catalogue, constructs an
//! empty admission engine, runs the server until a shutdown signal,
//! and drains cleanly.
//!
//! Plugins are admitted by a discovery pass over configured search
//! roots (see the `plugin_discovery` module) or, in tests, by integration
//! harnesses that construct an [`AdmissionEngine`] directly and call its
//! `admit_*` methods.
//!
//! Tests do not touch this file; anything testable lives in the library.

#![forbid(unsafe_code)]
#![allow(missing_docs)]

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio::sync::Mutex;

use evo::admin::AdminLedger;
use evo::admission::AdmissionEngine;
use evo::catalogue::Catalogue;
use evo::cli::Args;
use evo::client_acl::{ClientAcl, StewardIdentity};
use evo::config::StewardConfig;
use evo::custody::CustodyLedger;
use evo::happenings::HappeningBus;
use evo::persistence::{PersistenceStore, SqlitePersistenceStore};
use evo::plugin_discovery;
use evo::plugin_trust::load_plugin_trust_arc;
use evo::projections::{ProjectionEngine, SubjectConflictIndex};
use evo::relations::RelationGraph;
use evo::resolution::ResolutionLedger;
use evo::server::Server;
use evo::shutdown::wait_for_signal;
use evo::state::StewardState;
use evo::subjects::SubjectRegistry;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse CLI first. Clap handles --version and --help by printing and
    // exiting with status 0; invalid flags exit with status 2.
    let args = Args::parse();

    // Load the config. If --config was given, missing file is an error;
    // otherwise a missing default config silently falls back.
    let config = match &args.config {
        Some(path) => StewardConfig::load_from_required(path)?,
        None => StewardConfig::load()?,
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

    // Initialise logging. CLI --log-level takes precedence over RUST_LOG
    // and over config.steward.log_level; see logging::resolve_filter.
    evo::logging::init(&config, args.log_level.as_deref())?;

    tracing::info!(version = env!("CARGO_PKG_VERSION"), "evo starting");

    // Load the catalogue. Wrap in Arc so the relation announcer
    // can hold a shared handle alongside the subject registry and
    // relation graph for predicate-existence validation at
    // assertion.
    let catalogue = Arc::new(Catalogue::load(&catalogue_path)?);
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
    // absent; pragmas (synchronous = FULL, journal_mode = WAL,
    // foreign_keys = ON, etc.) are applied to every pooled
    // connection; pending migrations are run before the handle is
    // returned. The store is unintegrated in Phase 1; Phase 2
    // wires the subject-registry write path through it.
    let persistence_path = config.persistence.path.clone();
    let persistence: Arc<dyn PersistenceStore> = Arc::new(
        SqlitePersistenceStore::open(persistence_path.clone()).map_err(
            |e| {
                anyhow::anyhow!(
                    "opening persistence store at {}: {e}",
                    persistence_path.display()
                )
            },
        )?,
    );
    tracing::info!(
        path = %persistence_path.display(),
        "persistence store opened"
    );

    // Load the steward instance ID from persistence (migration 003).
    // Pinned at first boot, persisted forever; anchors the per-
    // deployment unlinkability of claimant tokens.
    let instance_id = persistence
        .load_instance_id()
        .await
        .map_err(|e| anyhow::anyhow!("loading instance_id: {e}"))?;
    tracing::info!(instance_id = %instance_id, "steward instance identified");

    // Catalogue-orphan diagnostic. Boot-time scan of every persisted
    // subject_type against the loaded catalogue's declared types: a
    // type that appears in storage but not in the catalogue is an
    // orphan — a subject persisted under a vocabulary entry the
    // current catalogue no longer admits. This emits an
    // operator-visible warning per orphaned type with the row count
    // so the operator can scope a deliberate migration. The diagnostic
    // does not refuse boot or modify state; orphans continue to be
    // readable via existing query paths and announce of new subjects
    // of the orphaned type fails at the wiring layer with the same
    // structured error any unknown-type announcement raises.
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
        Arc::new(evo::claimant::ClaimantTokenIssuer::new(instance_id));

    // Construct the happenings bus with persistence write-through
    // and operator-tunable retention. The bus seeds its monotonic
    // seq counter from the on-disk maximum so cursors continue to
    // grow across restart; the broadcast capacity sets the live
    // backpressure ceiling and the retention window is advertised
    // for observability.
    let bus = Arc::new(
        HappeningBus::with_persistence_capacity_and_window(
            Arc::clone(&persistence),
            config.happenings.retention_capacity,
            config.happenings.retention_window_secs,
        )
        .await
        .map_err(|e| anyhow::anyhow!("seeding happenings bus: {e}"))?,
    );

    // Spawn the happenings_log janitor. It periodically calls the
    // store's `trim_happenings_log` to evict rows that have aged
    // past the retention window OR fallen out of the retention
    // capacity tail. Shutdown notification fires on signal so the
    // janitor exits cleanly alongside the server.
    let janitor_shutdown = Arc::new(tokio::sync::Notify::new());
    let janitor_persistence = Arc::clone(&persistence);
    let janitor_capacity = config.happenings.retention_capacity as u64;
    let janitor_window = config.happenings.retention_window_secs;
    let janitor_interval = config.happenings.janitor_interval_secs;
    let janitor_shutdown_for_task = Arc::clone(&janitor_shutdown);
    let janitor_task = tokio::spawn(async move {
        evo::happenings::run_happenings_janitor(
            janitor_persistence,
            janitor_window,
            janitor_capacity,
            janitor_interval,
            janitor_shutdown_for_task,
        )
        .await;
    });

    // Construct the in-memory conflict index. The wiring layer's
    // announce path records detected multi-subject conflicts into
    // it, the admin layer's merge path resolves them, and the
    // projection engine consults it at composition time so a
    // subject sitting in an unresolved conflict carries the
    // structured degraded reason. Held centrally on the steward
    // state so every consumer reads the same instance.
    let conflict_index = Arc::new(SubjectConflictIndex::new());

    // Construct the subject registry and rehydrate it from the
    // durable store BEFORE the admission engine is built. Every
    // successful announce / merge / split / forget writes through
    // to SQL on the running steward; on restart the registry is
    // reconstructed from those rows so a consumer querying the
    // freshly-booted steward sees the same state it had before
    // the restart, rather than waiting for plugins to re-announce.
    // Forgotten subjects are excluded from the live maps; their
    // tombstone alias rows populate the alias chain so a query
    // for a forgotten canonical id returns the documented
    // tombstone record rather than a bare not-found.
    let subjects = Arc::new(SubjectRegistry::new());
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

    // Build the shared steward state once: catalogue plus
    // freshly-allocated stores. The same `Arc<StewardState>` is
    // handed to the admission engine and the server so dispatch
    // does not have to lock the engine to read shared stores.
    let state = StewardState::builder()
        .catalogue(Arc::clone(&catalogue))
        .subjects(subjects)
        .relations(Arc::new(RelationGraph::new()))
        .custody(Arc::new(CustodyLedger::new()))
        .bus(bus)
        .admin(Arc::new(AdminLedger::new()))
        .persistence(Arc::clone(&persistence))
        .claimant_issuer(Arc::clone(&claimant_issuer))
        .conflict_index(Arc::clone(&conflict_index))
        .build()?;

    // Construct the admission engine and run plugin discovery
    // (out-of-process singletons only; see `plugin_discovery`).
    let trust = load_plugin_trust_arc(&config)?;
    let mut engine = AdmissionEngine::new(
        Arc::clone(&state),
        config.plugins.plugin_data_root.clone(),
        Some(trust),
        config.plugins.security.clone(),
    );
    plugin_discovery::discover_and_admit(&mut engine, &config).await?;

    // Construct a projection engine sharing the steward's subject
    // registry and relation graph. Plugins announce into the same
    // stores the projection engine reads from. Attach the shared
    // conflict index so projections name every unresolved conflict
    // a subject participates in via a structured degraded entry.
    let projections = Arc::new(
        ProjectionEngine::new(
            Arc::clone(&state.subjects),
            Arc::clone(&state.relations),
        )
        .with_conflict_index(Arc::clone(&conflict_index)),
    );

    // Clone an Arc to the router so the server can dispatch directly
    // through it without acquiring the admission-engine mutex. The
    // engine still owns the router; the server only borrows it.
    let router = Arc::clone(engine.router());

    // Wrap engine for shared access between any future admission-side
    // mutations and the final drain. The server no longer needs the
    // engine handle: it routes through the router and reads the bus /
    // ledger directly off the steward state bag.
    let engine = Arc::new(Mutex::new(engine));

    // Load the operator-controlled client-API ACL. Missing file
    // yields the default-deny posture (local steward UID only);
    // malformed file is a hard boot-time error so the operator
    // catches the typo before any consumer connects.
    let client_acl = Arc::new(ClientAcl::load()?);
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

    // Capture the steward's own identity once at boot. The default
    // ACL grants the local-UID branch only when the peer's UID
    // matches this value; resolving it once here avoids a per-
    // connection metadata read.
    let steward_identity = StewardIdentity::current();

    // Construct the audit ledger that records every
    // `resolve_claimants` call (granted or refused). One ledger per
    // steward instance; held by the server and exposed to operator
    // tooling via [`Server::resolution_ledger`].
    let resolution_ledger = Arc::new(ResolutionLedger::new());

    // Start the server. The server clones the state Arc directly so
    // it can serve bus subscriptions and ledger snapshots without
    // taking the engine mutex; dispatch flows through the router.
    let server = Server::with_acl(
        socket_path.clone(),
        router,
        Arc::clone(&state),
        Arc::clone(&projections),
        Arc::clone(&client_acl),
        steward_identity,
        Arc::clone(&resolution_ledger),
    );

    tracing::warn!(
        socket = %socket_path.display(),
        "evo ready"
    );

    // Wait for either the server to exit (unlikely) or a shutdown
    // signal. wait_for_signal() logs a warn-level event identifying
    // which signal fired.
    let shutdown_fut = wait_for_signal();
    tokio::pin!(shutdown_fut);

    // The server run takes ownership of a shutdown future that completes
    // when we say. We drive that from the signal future below.
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

    // Drive signal forwarding on the main task. When the signal future
    // completes, the oneshot fires, the server's accept loop exits,
    // and the server_task join completes.
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

    // Notify the happenings_log janitor and join its task. The
    // janitor's loop observes the notify on its next sleep boundary
    // and exits cleanly. Joining here keeps the boot/shutdown
    // symmetry: every spawned task is awaited before drop.
    janitor_shutdown.notify_waiters();
    if let Err(e) = janitor_task.await {
        tracing::warn!(error = %e, "happenings_log janitor task did not join cleanly");
    }

    // Drain: unload every admitted plugin under a global deadline.
    // The report carries which plugins released cleanly and which
    // missed the deadline; stage 4 SIGKILL'd the holdouts before
    // shutdown_with_config returned.
    tracing::info!("draining admission engine");
    let report = engine
        .lock()
        .await
        .shutdown_with_config(evo::admission::ShutdownConfig::default())
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

    // Final WAL checkpoint before the persistence pool drops. Flushes
    // the write-ahead log into the main database file and truncates
    // it to zero so an operator backing up the database alone (without
    // the `-wal`/`-shm` siblings) captures every committed row.
    // Failure here is non-fatal: the rows are still on disk in the
    // WAL and recoverable by SQLite on next open; we log a warning
    // so the operator notices.
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
