//! Evo steward binary entrypoint.
//!
//! A thin wrapper around the `evo` library: parses CLI flags, loads
//! config, initialises logging, loads the catalogue, admits the v0
//! hardcoded echo plugin, runs the server until a shutdown signal, and
//! drains cleanly.
//!
//! Tests do not touch this file; anything testable lives in the library.

#![forbid(unsafe_code)]
#![allow(missing_docs)]

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio::sync::Mutex;

use evo::admission::AdmissionEngine;
use evo::catalogue::Catalogue;
use evo::cli::Args;
use evo::config::StewardConfig;
use evo::projections::ProjectionEngine;
use evo::server::Server;
use evo::shutdown::wait_for_signal;

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

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "evo starting"
    );

    // Load the catalogue.
    let catalogue = Catalogue::load(&catalogue_path)?;
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

    // Construct the admission engine and admit the hardcoded v0 plugins.
    let mut engine = AdmissionEngine::new();
    admit_v0_plugins(&mut engine, &catalogue).await?;
    tracing::info!(plugins = engine.len(), "admission complete");

    // Construct a projection engine sharing the admission engine's
    // subject registry and relation graph. Plugins announce into the
    // same stores the projection engine reads from.
    let projections = Arc::new(ProjectionEngine::new(
        engine.registry(),
        engine.relation_graph(),
    ));

    // Wrap engine for shared access between the server and the final
    // drain.
    let engine = Arc::new(Mutex::new(engine));

    // Start the server.
    let server = Server::new(
        socket_path.clone(),
        Arc::clone(&engine),
        Arc::clone(&projections),
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

    // Drain: unload every admitted plugin.
    tracing::info!("draining admission engine");
    if let Err(e) = engine.lock().await.shutdown().await {
        tracing::error!(error = %e, "drain encountered errors");
    }

    tracing::warn!("evo exited");
    Ok(())
}

/// Admit the v0 hardcoded plugin set.
///
/// For v0 this is just the echo example plugin. Once dynamic plugin
/// discovery exists, this function goes away and the admission engine is
/// populated by walking `/var/lib/evo/plugins/` and `/opt/evo/plugins/`.
async fn admit_v0_plugins(
    engine: &mut AdmissionEngine,
    catalogue: &Catalogue,
) -> anyhow::Result<()> {
    let echo_plugin = evo_example_echo::EchoPlugin::new();
    let echo_manifest = evo_example_echo::manifest();
    engine
        .admit_singleton_respondent(echo_plugin, echo_manifest, catalogue)
        .await?;
    Ok(())
}
