//! Out-of-process wire wrapper for the synthetic live-reload plugin.
//!
//! ## Usage
//!
//! ```text
//! synthetic-reload-plugin-wire <socket-path>
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{anyhow, Result};
use evo_acceptance_synthetic::ReloadPlugin;
use evo_plugin_sdk::host::{run_oop, HostConfig};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

const DEFAULT_PLUGIN_NAME: &str = "org.evoframework.acceptance.reload-plugin";
const ENV_PLUGIN_NAME: &str = "EVO_ACCEPTANCE_RELOAD_PLUGIN_NAME";

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    init_logging();

    let socket_path = parse_args()?;
    // Allow scenarios to point this binary at the raised-cap
    // manifest variant by overriding the wire-host identity. The
    // SDK Plugin::describe() reads the same env, so the spawned
    // process keeps Hello-handshake identity and describe()
    // identity in lockstep. Synthetic-acceptance only —
    // production plugins hard-code their identity.
    let plugin_name = std::env::var(ENV_PLUGIN_NAME)
        .unwrap_or_else(|_| DEFAULT_PLUGIN_NAME.to_string());
    tracing::info!(
        socket = %socket_path.display(),
        plugin = %plugin_name,
        "synthetic-reload-plugin-wire starting"
    );

    let plugin = ReloadPlugin::new();
    let config = HostConfig::new(&plugin_name);
    run_oop(plugin, config, &socket_path).await?;
    tracing::info!(
        "synthetic-reload-plugin-wire: steward disconnected, exiting"
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
        anyhow!("usage: synthetic-reload-plugin-wire <socket-path>")
    })?;
    if args.next().is_some() {
        return Err(anyhow!(
            "usage: synthetic-reload-plugin-wire <socket-path> \
             (too many arguments)"
        ));
    }
    Ok(PathBuf::from(path))
}
