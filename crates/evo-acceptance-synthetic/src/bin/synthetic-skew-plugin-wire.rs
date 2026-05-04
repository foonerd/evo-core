//! Out-of-process wire wrapper for the synthetic skew plugin.
//!
//! ## Usage
//!
//! ```text
//! synthetic-skew-plugin-wire <socket-path>
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{anyhow, Result};
use evo_acceptance_synthetic::SkewPlugin;
use evo_plugin_sdk::host::{run_oop, HostConfig};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

const DEFAULT_PLUGIN_NAME: &str = "org.evoframework.acceptance.skew-plugin";
const ENV_PLUGIN_NAME: &str = "EVO_ACCEPTANCE_SKEW_PLUGIN_NAME";

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    init_logging();

    let socket_path = parse_args()?;
    // Same env override as the SDK Plugin::describe() reads; both
    // must agree for the framework's Hello-handshake identity
    // check to pass. Synthetic-acceptance only.
    let plugin_name = std::env::var(ENV_PLUGIN_NAME)
        .unwrap_or_else(|_| DEFAULT_PLUGIN_NAME.to_string());
    tracing::info!(
        socket = %socket_path.display(),
        plugin = %plugin_name,
        "synthetic-skew-plugin-wire starting"
    );

    let plugin = SkewPlugin::new();
    let config = HostConfig::new(&plugin_name);
    run_oop(plugin, config, &socket_path).await?;
    tracing::info!("synthetic-skew-plugin-wire: steward disconnected, exiting");
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
        anyhow!("usage: synthetic-skew-plugin-wire <socket-path>")
    })?;
    if args.next().is_some() {
        return Err(anyhow!(
            "usage: synthetic-skew-plugin-wire <socket-path> \
             (too many arguments)"
        ));
    }
    Ok(PathBuf::from(path))
}
