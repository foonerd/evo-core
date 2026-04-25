//! # warden-wire
//!
//! Out-of-process reference implementation of the evo example warden
//! plugin.
//!
//! Listens on a Unix socket given as its sole positional argument,
//! accepts exactly one connection, serves that connection via the
//! plugin SDK's [`evo_plugin_sdk::host::serve_warden`] function, and
//! exits when the steward disconnects.
//!
//! This is the minimal binary shape of an out-of-process warden. It
//! parallels `echo-wire` (the respondent reference) and exists for
//! the same two reasons:
//!
//! 1. As a working reference: any third-party warden binary that
//!    wants to speak the evo wire protocol can follow this skeleton.
//! 2. As a fixture for the steward's out-of-process warden admission
//!    integration tests in `tests/out_of_process.rs` and
//!    `tests/from_directory.rs`, which spawn this binary and drive
//!    it from an in-process `AdmissionEngine`.
//!
//! ## Usage
//!
//! ```text
//! warden-wire <socket-path>
//! ```
//!
//! The socket path must be writable. If a file exists at that path
//! it is removed before binding.
//!
//! Logging goes to stderr. The log filter can be overridden via the
//! `RUST_LOG` environment variable; the default is `warn`.
//!
//! ## Lifecycle and exit codes
//!
//! * `0` - steward disconnected cleanly, `serve_warden` returned
//!   `Ok`.
//! * `1` - argument parsing, socket binding, or `serve_warden`
//!   errored.
//!
//! This binary does not daemonise and does not re-accept after the
//! first connection returns. A production warden wrapping this
//! skeleton would likely loop on `listener.accept()`; v0 keeps
//! things linear to match the integration tests' expectations and
//! to keep shutdown semantics crisp.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{anyhow, Context, Result};
use evo_example_warden::WardenPlugin;
use evo_plugin_sdk::host::{serve_warden, HostConfig};
use std::path::{Path, PathBuf};
use tokio::net::UnixListener;
use tracing_subscriber::EnvFilter;

/// Canonical plugin name, must match the embedded manifest.
const PLUGIN_NAME: &str = "org.evo.example.warden";

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    init_logging();

    let socket_path = parse_args()?;
    tracing::info!(
        socket = %socket_path.display(),
        plugin = PLUGIN_NAME,
        "warden-wire starting"
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
    tracing::info!("warden-wire: accepted connection");

    // into_split gives us genuinely independent owned halves - no
    // BiLock, no waker-registration surprises under multi-threaded
    // runtimes. This matches the split strategy documented in
    // evo::wire_client.
    let (reader, writer) = stream.into_split();

    let plugin = WardenPlugin::new();
    let config = HostConfig::new(PLUGIN_NAME);

    match serve_warden(plugin, config, reader, writer).await {
        Ok(()) => {
            tracing::info!("warden-wire: steward disconnected, exiting");
            cleanup_socket(&socket_path);
            Ok(())
        }
        Err(e) => {
            tracing::error!(error = %e, "warden-wire: serve_warden failed");
            cleanup_socket(&socket_path);
            Err(anyhow!("serve_warden failed: {e}"))
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
        .ok_or_else(|| anyhow!("usage: warden-wire <socket-path>"))?;
    if args.next().is_some() {
        return Err(anyhow!(
            "usage: warden-wire <socket-path> (too many arguments)"
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
            "warden-wire: failed to remove socket file on exit"
        );
    }
}
