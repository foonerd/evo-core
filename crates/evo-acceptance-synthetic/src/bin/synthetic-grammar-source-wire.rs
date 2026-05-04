//! Out-of-process wire wrapper for the synthetic grammar-source plugin.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{anyhow, Result};
use evo_acceptance_synthetic::SyntheticGrammarSourcePlugin;
use evo_plugin_sdk::host::{run_oop, HostConfig};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

const PLUGIN_NAME: &str =
    "org.evoframework.acceptance.synthetic-grammar-source";

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    init_logging();
    let socket_path = parse_args()?;
    tracing::info!(
        socket = %socket_path.display(),
        plugin = PLUGIN_NAME,
        "synthetic-grammar-source-wire starting"
    );
    let plugin = SyntheticGrammarSourcePlugin::new();
    let config = HostConfig::new(PLUGIN_NAME);
    run_oop(plugin, config, &socket_path).await?;
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
        anyhow!("usage: synthetic-grammar-source-wire <socket-path>")
    })?;
    if args.next().is_some() {
        return Err(anyhow!(
            "usage: synthetic-grammar-source-wire <socket-path>"
        ));
    }
    Ok(PathBuf::from(path))
}
