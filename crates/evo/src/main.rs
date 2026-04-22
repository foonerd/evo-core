//! Evo steward binary entrypoint.
//!
//! A thin wrapper around the `evo` library: loads config, initialises
//! logging, loads the catalogue, admits the v0 hardcoded echo plugin,
//! runs the server until a shutdown signal, and drains cleanly.
//!
//! Tests do not touch this file; anything testable lives in the library.

#![forbid(unsafe_code)]
#![allow(missing_docs)]

use std::sync::Arc;
use tokio::sync::Mutex;

use evo::admission::AdmissionEngine;
use evo::catalogue::Catalogue;
use evo::config::StewardConfig;
use evo::server::Server;
use evo::shutdown::wait_for_signal;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load config first. Logging is not yet initialised here; any
    // errors go to stderr as anyhow messages.
    let config = StewardConfig::load()?;

    // Initialise logging per LOGGING.md. From here on, use `tracing`.
    evo::logging::init(&config)?;

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "evo starting"
    );

    // Load the catalogue.
    let catalogue = Catalogue::load(&config.catalogue.path)?;
    tracing::info!(
        racks = catalogue.racks.len(),
        shelves = catalogue
            .racks
            .iter()
            .map(|r| r.shelves.len())
            .sum::<usize>(),
        path = %config.catalogue.path.display(),
        "catalogue loaded"
    );

    // Construct the admission engine and admit the hardcoded v0 plugins.
    let mut engine = AdmissionEngine::new();
    admit_v0_plugins(&mut engine, &catalogue).await?;
    tracing::info!(plugins = engine.len(), "admission complete");

    // Wrap engine for shared access between the server and the final
    // drain.
    let engine = Arc::new(Mutex::new(engine));

    // Start the server.
    let server = Server::new(
        config.steward.socket_path.clone(),
        Arc::clone(&engine),
    );

    tracing::warn!(
        socket = %config.steward.socket_path.display(),
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
