//! # evo-acceptance-synthetic
//!
//! Synthetic plugins purpose-built for the evo acceptance test plan.
//! Each plugin in this crate exists to exercise one specific
//! framework behaviour from the acceptance harness; production
//! builds never embed these plugins, production trust roots never
//! sign them, and they have no value outside the
//! `acceptance/plans/<release>.toml` runs that target them.
//!
//! Each binary in `src/bin/` is a small OOP wrapper over a
//! `Plugin + (Respondent | Warden | …)` implementation defined in
//! this crate's library. The matching manifest lives under
//! `manifests/<plugin-name>/manifest.oop.toml` and is staged into
//! the bundle layout the steward expects on disk
//! (`manifest.toml + plugin.bin + manifest.sig`, packed as
//! `<plugin-name>-<version>-<target>.tar.gz`).
//!
//! ## Plugins shipped
//!
//! - **`drift_respondent`** — declares `request_types = ["drift_x"]`
//!   in its manifest and returns `["drift_x", "drift_y"]` from
//!   `Plugin::describe()`. Used by `T2.manifest-drift-refused` to
//!   verify the steward refuses admission of a plugin whose runtime
//!   surface drifts from its declared manifest.
//! - **`appointment_plugin`** — registers a OneShot appointment
//!   five seconds into the future on every load and records each
//!   fire to `{state_dir}/appointment-fires.log`. Used by
//!   `T2.appointment-fire` to verify the framework's appointment
//!   dispatch path end-to-end.
//!
//! Future plugins in this crate land alongside their acceptance
//! scenarios; each addition follows the canonical pattern set by
//! `drift_respondent` (lib type, OOP wire wrapper, manifest under
//! `manifests/`).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod appointment_past_due_plugin;
pub mod appointment_periodic_plugin;
pub mod appointment_plugin;
pub mod appointment_wake_plugin;
pub mod drift_respondent;
pub mod fast_path_warden;
pub mod flight_rack_plugin;
pub mod prompt_multistage_plugin;
pub mod prompt_plugin;
pub mod reload_plugin;
pub mod skew_plugin;
pub mod synthetic_coalescing_source;
pub mod synthetic_composer;
pub mod synthetic_delivery_warden;
pub mod synthetic_grammar_source;
pub mod synthetic_warden;
pub mod watch_cooldown_burst_plugin;
pub mod watch_hysteresis_plugin;
pub mod watch_plugin;
pub mod watch_subject_state_plugin;

pub use appointment_past_due_plugin::AppointmentPastDuePlugin;
pub use appointment_periodic_plugin::AppointmentPeriodicPlugin;
pub use appointment_plugin::AppointmentPlugin;
pub use appointment_wake_plugin::AppointmentWakePlugin;
pub use drift_respondent::DriftRespondent;
pub use fast_path_warden::FastPathWardenPlugin;
pub use flight_rack_plugin::FlightRackPlugin;
pub use prompt_multistage_plugin::PromptMultistagePlugin;
pub use prompt_plugin::PromptPlugin;
pub use reload_plugin::ReloadPlugin;
pub use skew_plugin::SkewPlugin;
pub use synthetic_coalescing_source::SyntheticCoalescingSourcePlugin;
pub use synthetic_composer::SyntheticComposerPlugin;
pub use synthetic_delivery_warden::SyntheticDeliveryWardenPlugin;
pub use synthetic_grammar_source::SyntheticGrammarSourcePlugin;
pub use synthetic_warden::SyntheticWardenPlugin;
pub use watch_cooldown_burst_plugin::WatchCooldownBurstPlugin;
pub use watch_hysteresis_plugin::WatchHysteresisPlugin;
pub use watch_plugin::WatchPlugin;
pub use watch_subject_state_plugin::WatchSubjectStatePlugin;
