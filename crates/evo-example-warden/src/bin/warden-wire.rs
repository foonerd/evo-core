//! # warden-wire
//!
//! Out-of-process reference implementation of the evo example warden
//! plugin.
//!
//! Listens on a Unix socket given as its sole positional argument,
//! accepts exactly one connection, serves that connection via the
//! plugin SDK's [`evo_plugin_sdk::host::run_oop_warden`] helper, and
//! exits when the steward disconnects.
//!
//! Mirrors `echo-wire` (the respondent reference); third-party
//! warden authors copy this skeleton and substitute their own
//! `Plugin + Warden` implementation.
//!
//! ## Usage
//!
//! ```text
//! warden-wire <socket-path>
//! ```
//!
//! The socket path must be writable. If a file exists at that path
//! it is removed before binding (so a stale socket from a previous
//! run does not block startup).
//!
//! Logging goes to stderr. The log filter can be overridden via the
//! `RUST_LOG` environment variable; the default is `warn`.
//!
//! ## Lifecycle and exit codes
//!
//! * `0` — steward disconnected cleanly, [`run_oop_warden`]
//!   returned `Ok`.
//! * `1` — argument parsing, socket binding, accept, or
//!   [`run_oop_warden`] errored.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{anyhow, Result};
use evo_example_warden::WardenPlugin;
use evo_plugin_sdk::host::{run_oop_warden, HostConfig};
use std::path::PathBuf;
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

    let plugin = WardenPlugin::new();
    let config = HostConfig::new(PLUGIN_NAME);
    run_oop_warden(plugin, config, &socket_path).await?;
    tracing::info!("warden-wire: steward disconnected, exiting");
    Ok(())
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
