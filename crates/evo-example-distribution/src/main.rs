//! # evo-example-distribution
//!
//! Reference skeleton for an `evo-device-<vendor>` distribution.
//!
//! A distribution composing its own steward binary on top of evo-core
//! follows this shape: a thin `main.rs` that calls [`evo::run`] with
//! a custom [`evo::AdmissionSetup`] admitting the distribution's
//! plugin set. Everything the steward does (CLI, config, logging,
//! catalogue, persistence, instance-id, happenings bus, janitor,
//! subject-rehydration, shared state, server, drain, shutdown,
//! WAL checkpoint) is encapsulated by [`evo::run`]; the
//! distribution adds only:
//!
//! 1. **The plugin set.** This skeleton admits `evo-example-echo`
//!    (a respondent) and `evo-example-warden` (a warden) in-process.
//!    A real distribution substitutes its own plugins here.
//! 2. **The manifests.** Every admitted plugin needs a
//!    [`Manifest`]. This skeleton embeds
//!    the manifests as TOML constants for self-containedness; a
//!    larger distribution typically loads them from on-disk
//!    `manifest.toml` files inside its bundle directory.
//! 3. **The catalogue.** Distributions ship a catalogue declaring
//!    racks and shelves. This skeleton's catalogue is at
//!    `dist/catalogue/v0-skeleton.toml` in the evo-core repository
//!    root; the steward reads it via `evo.toml`'s `[catalogue]`
//!    section.
//!
//! Below is the entire `main.rs` a working distribution needs. It
//! is intentionally short: every framework concern is delegated to
//! [`evo::run`] and the SDK admission API.
//!
//! See `docs/engineering/SHOWCASE.md` for the binary-distribution
//! policy and `RELEASE_PLANE.md` for how a distribution's binaries
//! reach an operator.

#![forbid(unsafe_code)]
#![allow(missing_docs)]

use clap::Parser as _;

use evo::admission::AdmissionEngine;
use evo::config::StewardConfig;
use evo::AdmissionSetup;
use evo_example_echo::EchoPlugin;
use evo_example_warden::WardenPlugin;
use evo_plugin_sdk::Manifest;

/// Manifest for the in-process echo respondent.
///
/// In a real distribution this typically lives at
/// `<plugin_bundle>/manifest.toml` and is read at admission time.
/// Embedding it here keeps the skeleton self-contained.
const ECHO_MANIFEST_TOML: &str = r#"
[plugin]
name = "org.evo.example.echo"
version = "0.1.1"
contract = 1

[target]
shelf = "example.echo"
shape = 1

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "in-process"
exec = "<compiled-in>"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"
outbound_network = false
filesystem_scopes = []

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"
autostart = true
restart_on_crash = true
restart_budget = 5

[capabilities.respondent]
request_types = ["echo"]
response_budget_ms = 1000
"#;

/// Manifest for the in-process warden.
const WARDEN_MANIFEST_TOML: &str = r#"
[plugin]
name = "org.evo.example.warden"
version = "0.1.0"
contract = 1

[target]
shelf = "example.custody"
shape = 1

[kind]
instance = "singleton"
interaction = "warden"

[transport]
type = "in-process"
exec = "<compiled-in>"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "any"
outbound_network = false
filesystem_scopes = []

[resources]
max_memory_mb = 16
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"
autostart = true
restart_on_crash = true
restart_budget = 5

[capabilities.warden]
custody_domain = "playback"
custody_exclusive = false
course_correction_budget_ms = 1000
custody_failure_mode = "abort"
"#;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = evo::cli::Args::parse();
    evo::run(evo::RunOptions::new(args, distribution_admission())).await
}

/// Build the admission strategy this distribution uses.
///
/// Pulled out into its own function so the shape is readable and
/// the manifests' parse is in one place. A larger distribution
/// extends this with as many `engine.admit_singleton_*` calls as
/// it has plugins, optionally chained with
/// [`evo::plugin_discovery::discover_and_admit`] to also admit
/// out-of-process plugins discovered from the configured
/// `plugins.search_roots`.
fn distribution_admission() -> AdmissionSetup {
    Box::new(|engine: &mut AdmissionEngine, _config: &StewardConfig| {
        Box::pin(async move {
            // 1. Echo respondent (in-process).
            let echo_manifest = Manifest::from_toml(ECHO_MANIFEST_TOML)
                .map_err(|e| anyhow::anyhow!("parsing echo manifest: {e}"))?;
            engine
                .admit_singleton_respondent(EchoPlugin::new(), echo_manifest)
                .await
                .map_err(|e| {
                    anyhow::anyhow!("admitting echo respondent: {e}")
                })?;

            // 2. Example warden (in-process).
            let warden_manifest = Manifest::from_toml(WARDEN_MANIFEST_TOML)
                .map_err(|e| anyhow::anyhow!("parsing warden manifest: {e}"))?;
            engine
                .admit_singleton_warden(WardenPlugin::new(), warden_manifest)
                .await
                .map_err(|e| anyhow::anyhow!("admitting warden: {e}"))?;

            // 3. (Optional, commented out for the skeleton.) Run the
            //    in-tree discovery pass so out-of-process plugins
            //    dropped under `/opt/evo/plugins/` or
            //    `/var/lib/evo/plugins/` also admit. Real
            //    distributions usually want both: programmatic
            //    in-process admission for first-party plugins,
            //    discovery for third-party operator-installed
            //    plugins.
            //
            // evo::plugin_discovery::discover_and_admit(engine, _config)
            //     .await
            //     .map_err(anyhow::Error::from)?;

            Ok(())
        })
    })
}
