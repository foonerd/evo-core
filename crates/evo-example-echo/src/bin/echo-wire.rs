//! # echo-wire
//!
//! Out-of-process reference implementation of the evo echo plugin.
//!
//! Listens on a Unix socket given as its sole positional argument,
//! accepts exactly one connection, serves that connection via the
//! plugin SDK's [`evo_plugin_sdk::host::serve`] function, and exits
//! when the steward disconnects.
//!
//! This is the minimal binary shape of an out-of-process plugin. It
//! exists for two reasons:
//!
//! 1. As a working reference implementation: any third-party plugin
//!    binary that wants to speak the evo wire protocol can follow this
//!    skeleton.
//! 2. As a fixture for the steward's out-of-process admission
//!    integration test in `tests/out_of_process.rs`, which spawns this
//!    binary and drives it from an in-process `AdmissionEngine`.
//!
//! ## Usage
//!
//! ```text
//! echo-wire <socket-path>
//! ```
//!
//! The socket path must be writable. If a file exists at that path it
//! is removed before binding.
//!
//! Logging goes to stderr. The log filter can be overridden via the
//! `RUST_LOG` environment variable; the default is `warn`.
//!
//! ## Lifecycle and exit codes
//!
//! * `0` - steward disconnected cleanly, `serve` returned `Ok`.
//! * `1` - argument parsing, socket binding, or `serve` errored.
//!
//! This binary does not daemonise and does not re-accept after the
//! first connection returns. A production plugin wrapping this
//! skeleton would likely loop on `listener.accept()`; v0 keeps things
//! linear to match the integration test's expectations and to keep
//! shutdown semantics crisp.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{anyhow, Context, Result};
use evo_example_echo::EchoPlugin;
use evo_plugin_sdk::host::{serve, HostConfig};
use std::path::{Path, PathBuf};
use tokio::net::UnixListener;
use tracing_subscriber::EnvFilter;

/// Canonical plugin name, must match the embedded manifest.
const PLUGIN_NAME: &str = "org.evo.example.echo";

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    init_logging();

    let socket_path = parse_args()?;
    tracing::info!(
        socket = %socket_path.display(),
        plugin = PLUGIN_NAME,
        "echo-wire starting"
    );

    let listener = bind_listener(&socket_path).with_context(|| {
        format!("binding Unix socket at {}", socket_path.display())
    })?;

    // Accept exactly one connection. For a v0 reference implementation
    // this is sufficient; production plugins would typically loop.
    let (stream, _addr) = listener
        .accept()
        .await
        .context("accepting connection on Unix socket")?;
    tracing::info!("echo-wire: accepted connection");

    // Into_split gives us genuinely independent owned halves - no
    // BiLock, no waker-registration surprises under multi-threaded
    // runtimes. This matches the split strategy documented in
    // evo::wire_client.
    let (reader, writer) = stream.into_split();

    let plugin = EchoPlugin::new();
    let config = HostConfig::new(PLUGIN_NAME);

    match serve(plugin, config, reader, writer).await {
        Ok(()) => {
            tracing::info!("echo-wire: steward disconnected, exiting");
            cleanup_socket(&socket_path);
            Ok(())
        }
        Err(e) => {
            tracing::error!(error = %e, "echo-wire: serve failed");
            cleanup_socket(&socket_path);
            Err(anyhow!("serve failed: {e}"))
        }
    }
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}

fn parse_args() -> Result<PathBuf> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or_else(|| anyhow!("usage: echo-wire <socket-path>"))?;
    if args.next().is_some() {
        return Err(anyhow!(
            "usage: echo-wire <socket-path> (too many arguments)"
        ));
    }
    Ok(PathBuf::from(path))
}

fn bind_listener(path: &Path) -> std::io::Result<UnixListener> {
    if path.exists() {
        // The socket path already exists. Remove it so bind() can
        // create a fresh one. Errors here are propagated; if we
        // cannot clean up, we cannot bind.
        std::fs::remove_file(path)?;
    }
    UnixListener::bind(path)
}

/// Best-effort removal of the socket file on exit.
///
/// Failures are logged but not propagated: the process is already
/// exiting and there is nothing useful to do with a cleanup error.
fn cleanup_socket(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        tracing::warn!(
            error = %e,
            socket = %path.display(),
            "echo-wire: failed to remove socket file on exit"
        );
    }
}
