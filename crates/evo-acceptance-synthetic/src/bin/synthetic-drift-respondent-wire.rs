//! Out-of-process wire wrapper for the synthetic drift respondent.
//!
//! Listens on the Unix socket given as its sole positional argument
//! and serves the connection via the plugin SDK's
//! [`evo_plugin_sdk::host::run_oop`] helper. The plugin itself never
//! handles a request — admission is expected to refuse it — but the
//! wire wrapper still binds + accepts, because the steward issues
//! `describe()` (and therefore opens the connection) before drift
//! detection runs.
//!
//! ## Usage
//!
//! ```text
//! synthetic-drift-respondent-wire <socket-path>
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{anyhow, Result};
use evo_acceptance_synthetic::DriftRespondent;
use evo_plugin_sdk::host::{run_oop, HostConfig};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

const PLUGIN_NAME: &str = "org.evoframework.acceptance.drift-respondent";

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    init_logging();

    let socket_path = parse_args()?;
    tracing::info!(
        socket = %socket_path.display(),
        plugin = PLUGIN_NAME,
        "synthetic-drift-respondent-wire starting"
    );

    let plugin = DriftRespondent::new();
    let config = HostConfig::new(PLUGIN_NAME);
    run_oop(plugin, config, &socket_path).await?;
    tracing::info!(
        "synthetic-drift-respondent-wire: steward disconnected, exiting"
    );
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
    let path = args.next().ok_or_else(|| {
        anyhow!("usage: synthetic-drift-respondent-wire <socket-path>")
    })?;
    if args.next().is_some() {
        return Err(anyhow!(
            "usage: synthetic-drift-respondent-wire <socket-path> \
             (too many arguments)"
        ));
    }
    Ok(PathBuf::from(path))
}
