//! Out-of-process wire wrapper for the synthetic reconciliation delivery warden.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{anyhow, Result};
use evo_acceptance_synthetic::SyntheticDeliveryWardenPlugin;
use evo_plugin_sdk::host::{run_oop_warden, HostConfig};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

const PLUGIN_NAME: &str =
    "org.evoframework.acceptance.synthetic-delivery-warden";

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    init_logging();

    let socket_path = parse_args()?;
    tracing::info!(
        socket = %socket_path.display(),
        plugin = PLUGIN_NAME,
        "synthetic-delivery-warden-wire starting"
    );

    let plugin = SyntheticDeliveryWardenPlugin::new();
    let config = HostConfig::new(PLUGIN_NAME);
    run_oop_warden(plugin, config, &socket_path).await?;
    tracing::info!(
        "synthetic-delivery-warden-wire: steward disconnected, exiting"
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
        anyhow!("usage: synthetic-delivery-warden-wire <socket-path>")
    })?;
    if args.next().is_some() {
        return Err(anyhow!(
            "usage: synthetic-delivery-warden-wire <socket-path> \
             (too many arguments)"
        ));
    }
    Ok(PathBuf::from(path))
}
