//! Acceptance-only distribution.
//!
//! Identical to the production steward in every observable
//! behaviour except one: the synthetic
//! [`evo_acceptance_synthetic::ReloadPlugin`] is admitted
//! in-process before the OOP discovery pass runs. This gives
//! `T2.hot-reload-live-inproc` a Live-reload-capable plugin on the
//! in-process path — distinct from the OOP path covered by
//! `T2.hot-reload-live-oop` and `T2.hot-reload-blob-size-cap`.
//!
//! The two paths exercise different framework code: in-process
//! Live skips the spawn / `prepare_for_live_reload` over-the-wire /
//! successor-socket sequence and passes the [`StateBlob`] directly
//! between calls on the same Plugin trait object. That's the
//! validation gap T2.hot-reload-live-inproc closes.
//!
//! Used only by acceptance scenarios. Production builds never ship
//! this binary.

#![forbid(unsafe_code)]
#![allow(missing_docs)]

use clap::Parser as _;

use evo::admission::AdmissionEngine;
use evo::appointments::RtcWakeCallback;
use evo::config::StewardConfig;
use evo::AdmissionSetup;
use evo_acceptance_synthetic::ReloadPlugin;
use evo_plugin_sdk::Manifest;
use std::path::PathBuf;
use std::sync::Arc;

/// Recording RTC-wake adapter. Logs every `program_wake(at)`
/// call to a file the acceptance scenarios can grep. Used by
/// `T2.appointment-wake-device` and `T3.appt-time-wake` to
/// validate the framework's must-wake-device contract: the
/// framework programs the OS RTC wake at `(scheduled_fire_ms -
/// wake_pre_arm_ms)` for each must-wake appointment, re-programs
/// when the next-pending appointment changes, and clears
/// (`None`) when no must-wake appointment is pending.
///
/// Synthetic-acceptance only. Production distributions ship an
/// adapter that actually drives the kernel's RTC alarm
/// (`/sys/class/rtc/rtc0/wakealarm` on Linux); this one only
/// records.
struct RecordingRtcWakeCallback {
    log_path: PathBuf,
}

impl RecordingRtcWakeCallback {
    fn new(log_path: PathBuf) -> Self {
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Truncate / create on construction so a fresh boot starts
        // from an empty log; the test asserts on what THIS boot
        // recorded, not stale entries from a prior run.
        let _ = std::fs::write(&log_path, "");
        Self { log_path }
    }
}

impl RtcWakeCallback for RecordingRtcWakeCallback {
    fn program_wake(&self, at_ms_utc: Option<u64>) {
        use std::io::Write;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let line = match at_ms_utc {
            Some(at) => {
                format!("program_wake at_ms={at} recorded_at_ms={now_ms}\n")
            }
            None => format!("program_wake clear recorded_at_ms={now_ms}\n"),
        };
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

const ENV_RTC_WAKE_LOG: &str = "EVO_ACCEPTANCE_RTC_WAKE_LOG";

/// Manifest for the in-process reload plugin admitted by this
/// acceptance distribution. Differs from the OOP manifest at
/// `crates/evo-acceptance-synthetic/manifests/synthetic-reload-plugin/`
/// only in `[transport].type = "in-process"` (vs `out-of-process`)
/// and the absence of `exec` / signing fields the in-process path
/// does not need.
const RELOAD_PLUGIN_INPROC_MANIFEST_TOML: &str = r#"
[plugin]
name = "org.evoframework.acceptance.reload-plugin-inproc"
version = "0.1.0"
contract = 1

[target]
shelf = "acceptance.reload-inproc"
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
hot_reload = "live"
autostart = true
restart_on_crash = false
restart_budget = 0

[capabilities.respondent]
request_types = ["never_called"]
response_budget_ms = 1000
"#;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = evo::cli::Args::parse();
    let mut opts = evo::RunOptions::new(args, distribution_admission());
    if let Ok(path) = std::env::var(ENV_RTC_WAKE_LOG) {
        let cb: Arc<dyn RtcWakeCallback> =
            Arc::new(RecordingRtcWakeCallback::new(PathBuf::from(path)));
        opts = opts.with_rtc_wake(cb);
    }
    evo::run(opts).await
}

fn distribution_admission() -> AdmissionSetup {
    Box::new(|engine: &mut AdmissionEngine, config: &StewardConfig| {
        Box::pin(async move {
            // 1. In-process synthetic reload plugin. Same code as
            //    the OOP `synthetic-reload-plugin-wire`, just
            //    admitted directly without a wire transport.
            let manifest =
                Manifest::from_toml(RELOAD_PLUGIN_INPROC_MANIFEST_TOML)
                    .map_err(|e| {
                        anyhow::anyhow!("parsing inproc reload manifest: {e}")
                    })?;
            // The plugin reads EVO_ACCEPTANCE_RELOAD_PLUGIN_NAME on
            // describe(); set it to match the inproc manifest's
            // identity so admission verification passes.
            std::env::set_var(
                "EVO_ACCEPTANCE_RELOAD_PLUGIN_NAME",
                "org.evoframework.acceptance.reload-plugin-inproc",
            );
            engine
                .admit_singleton_respondent(ReloadPlugin::new(), manifest)
                .await
                .map_err(|e| {
                    anyhow::anyhow!("admitting inproc reload plugin: {e}")
                })?;

            // 2. Run the standard OOP discovery pass so OOP plugins
            //    operators have installed under
            //    `plugins.search_roots` still admit. Any prior
            //    test's bundles stay reachable; the in-process
            //    plugin co-exists with them on a different shelf.
            evo::plugin_discovery::discover_and_admit(engine, config)
                .await
                .map_err(anyhow::Error::from)?;

            Ok(())
        })
    })
}
