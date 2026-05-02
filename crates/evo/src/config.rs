//! Steward configuration.
//!
//! The steward reads `/etc/evo/evo.toml` at startup. All fields have
//! sensible defaults; the file may be absent entirely.
//!
//! Schema:
//!
//! ```toml
//! [steward]
//! log_level = "warn"
//! socket_path = "/var/run/evo/evo.sock"
//!
//! [catalogue]
//! path = "/opt/evo/catalogue/default.toml"
//!
//! [plugins]
//! allow_unsigned = false
//! # plugin_data_root, runtime_dir, search_roots,
//! # trust_dir_opt, trust_dir_etc, revocations_path, degrade_trust, security:
//! # see CONFIG.md section 3 / SCHEMAS.md section 3.3.
//! ```

use std::collections::BTreeMap;

use crate::error::StewardError;
use evo_plugin_sdk::manifest::TrustClass;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Default location of the steward's config file.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/evo/evo.toml";

/// Default location of the catalogue.
pub const DEFAULT_CATALOGUE_PATH: &str = "/opt/evo/catalogue/default.toml";

/// Default location of the steward-managed last-known-good catalogue
/// shadow. Mirror-written atomically after every successful steady-
/// state catalogue load; consulted as the second tier of the
/// resilience chain when the configured catalogue fails to parse or
/// validate. See `catalogue::Catalogue::load_with_fallback`.
pub const DEFAULT_CATALOGUE_LKG_PATH: &str =
    "/var/lib/evo/state/catalogue.lkg.toml";

/// Default location of the steward's client-facing socket.
pub const DEFAULT_SOCKET_PATH: &str = "/var/run/evo/evo.sock";

/// Default path of the steward's durable state database. The
/// persistence layer treats this as the canonical SQLite file;
/// WAL and SHM sidecars live alongside it under the same
/// directory.
pub const DEFAULT_PERSISTENCE_PATH: &str = "/var/lib/evo/state/evo.db";

/// Default per-plugin data root: `<this>/<plugin_name>/state` and
/// `credentials/` (see `PLUGIN_PACKAGING.md`).
pub const DEFAULT_PLUGIN_DATA_ROOT: &str = "/var/lib/evo/plugins";

/// Default runtime directory for out-of-process plugin sockets.
///
/// Aligns with FHS: runtime state (sockets, pidfiles) lives under
/// `/var/run/` (symlink to `/run/` on merged-`/usr` systems),
/// alongside the steward's own client socket at
/// `/var/run/evo/evo.sock`. Distributions using modern systemd expose
/// this via `RuntimeDirectory=evo/plugins`.
pub const DEFAULT_PLUGIN_RUNTIME_DIR: &str = "/var/run/evo/plugins";

/// Default per-plugin operator config drop-in directory.
///
/// The steward looks for `<this>/<plugin_name>.toml` at admission time
/// and merges its contents into the plugin's `LoadContext.config`. A
/// missing file is not an error (the plugin sees an empty table); a
/// malformed file aborts admission of that plugin so the operator
/// catches the typo at boot. See `PLUGIN_PACKAGING.md` section 3 and
/// the `LoadContext.config` documentation in
/// `evo-plugin-sdk::contract::LoadContext`.
pub const DEFAULT_PLUGINS_CONFIG_DIR: &str = "/etc/evo/plugins.d";

/// Default out-of-process plugin search order: read-only system tree
/// first, then `/var` (same plugin name in a later root replaces an
/// earlier one).
const DEFAULT_PLUGIN_SEARCH_ROOTS: [&str; 2] =
    ["/opt/evo/plugins", "/var/lib/evo/plugins"];

/// Package-shipped trust directory (`/opt/evo/trust/*.pem`). See
/// `PLUGIN_PACKAGING.md` section 5.
pub const DEFAULT_TRUST_DIR_OPT: &str = "/opt/evo/trust";

/// Operator-installed trust drop-in directory (`/etc/evo/trust.d/*.pem`).
pub const DEFAULT_TRUST_DIR_ETC: &str = "/etc/evo/trust.d";

/// Revocation list path.
pub const DEFAULT_REVOCATIONS_PATH: &str = "/etc/evo/revocations.toml";

/// Default for [`PluginsSection::degrade_trust`]: admit at the signing
/// key's `max_trust_class` when the manifest declares a stronger class,
/// instead of refusing. See `PLUGIN_PACKAGING.md` section 5.
pub const DEFAULT_DEGRADE_TRUST: bool = true;

/// Default in-memory broadcast capacity for the happenings bus. The
/// bus also retains durable rows in `happenings_log`; the broadcast
/// ring is the live-stream backpressure surface and the value here
/// caps how far a slow subscriber can fall behind before the bus
/// emits a structured lagged signal.
pub const DEFAULT_HAPPENINGS_RETENTION_CAPACITY: usize = 1024;

/// Default minimum durable retention window expressed in seconds.
/// Distributions size this against expected reconnect intervals
/// (transient drop, steward restart). The default of 30 minutes
/// covers brief outages without growing the durable footprint
/// disproportionately.
pub const DEFAULT_HAPPENINGS_RETENTION_WINDOW_SECS: u64 = 30 * 60;

/// Default interval between write-side janitor passes that trim
/// `happenings_log` according to the retention policy. Sized so the
/// trim work is amortised across operator-visible time but the table
/// never grows uncontrolled between passes.
pub const DEFAULT_HAPPENINGS_JANITOR_INTERVAL_SECS: u64 = 60;

/// Root of the steward's configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct StewardConfig {
    /// Core steward settings.
    #[serde(default)]
    pub steward: StewardSection,
    /// Catalogue location.
    #[serde(default)]
    pub catalogue: CatalogueSection,
    /// Plugin admission policy.
    #[serde(default)]
    pub plugins: PluginsSection,
    /// Durable-state persistence. Tunes where the steward's SQLite
    /// database lives.
    #[serde(default)]
    pub persistence: PersistenceSection,
    /// Happenings bus retention. Tunes the in-memory broadcast ring
    /// and the minimum durable replay window operators rely on for
    /// reconnect-resume.
    #[serde(default)]
    pub happenings: HappeningsSection,
    /// Wall-clock trust signal — distribution-declared hardware
    /// reality plus poll cadence.
    #[serde(default)]
    pub time_trust: TimeTrustSection,
}

impl StewardConfig {
    /// Load the config from the default path, falling back to built-in
    /// defaults if the file is absent.
    ///
    /// Returns an error only if the file exists but is malformed. A
    /// missing file is treated as "use defaults" with a note written to
    /// the caller's log via `tracing`.
    pub fn load() -> Result<Self, StewardError> {
        Self::load_from(Path::new(DEFAULT_CONFIG_PATH))
    }

    /// Load the config from a specified path.
    pub fn load_from(path: &Path) -> Result<Self, StewardError> {
        match std::fs::read_to_string(path) {
            Ok(content) => {
                let cfg: Self = toml::from_str(&content).map_err(|e| {
                    StewardError::toml(format!("{}", path.display()), e)
                })?;
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(Self::default())
            }
            Err(e) => Err(StewardError::io(
                format!("reading config {}", path.display()),
                e,
            )),
        }
    }

    /// Load the config from a specified path. Errors if the file does
    /// not exist.
    ///
    /// Use this when the path came from an explicit CLI flag: the user
    /// asked for that specific file, so a missing file is a real error
    /// rather than an invitation to fall back to defaults.
    pub fn load_from_required(path: &Path) -> Result<Self, StewardError> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            StewardError::io(format!("reading config {}", path.display()), e)
        })?;
        let cfg: Self = toml::from_str(&content).map_err(|e| {
            StewardError::toml(format!("{}", path.display()), e)
        })?;
        Ok(cfg)
    }

    /// Parse a config from a TOML string. Intended for tests.
    pub fn from_toml(input: &str) -> Result<Self, StewardError> {
        let cfg: Self = toml::from_str(input)
            .map_err(|e| StewardError::toml("inline config", e))?;
        Ok(cfg)
    }
}

/// `[steward]` section.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StewardSection {
    /// Log level filter. One of `error`, `warn`, `info`, `debug`, `trace`.
    /// Overridden by `RUST_LOG` if set. Default: `warn`.
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// Path to the Unix domain socket the steward listens on for client
    /// requests.
    #[serde(default = "default_socket_path")]
    pub socket_path: PathBuf,
}

impl Default for StewardSection {
    fn default() -> Self {
        Self {
            log_level: default_log_level(),
            socket_path: default_socket_path(),
        }
    }
}

fn default_log_level() -> String {
    "warn".to_string()
}

fn default_socket_path() -> PathBuf {
    PathBuf::from(DEFAULT_SOCKET_PATH)
}

/// `[persistence]` section.
///
/// Distributions override `path` to relocate the steward's state
/// directory. The default suits FHS systems; embedded targets
/// pointing at a writeable partition supply their own path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistenceSection {
    /// Path to the SQLite database file. Defaults to
    /// [`DEFAULT_PERSISTENCE_PATH`].
    #[serde(default = "default_persistence_path")]
    pub path: PathBuf,
}

impl Default for PersistenceSection {
    fn default() -> Self {
        Self {
            path: default_persistence_path(),
        }
    }
}

fn default_persistence_path() -> PathBuf {
    PathBuf::from(DEFAULT_PERSISTENCE_PATH)
}

/// `[happenings]` section.
///
/// Controls how much backlog the steward keeps for live and
/// reconnecting subscribers. The two knobs are independent: the
/// broadcast capacity caps live-stream lag tolerance; the window
/// seconds value documents the minimum durable retention operators
/// rely on for cross-restart and cross-disconnect resume.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HappeningsSection {
    /// In-memory broadcast ring capacity, in events. Controls how
    /// many unconsumed live happenings the bus buffers per
    /// subscriber before a slow consumer receives a structured
    /// lagged signal. Default:
    /// [`DEFAULT_HAPPENINGS_RETENTION_CAPACITY`].
    #[serde(default = "default_retention_capacity")]
    pub retention_capacity: usize,
    /// Minimum durable retention window the steward guarantees, in
    /// seconds. Operators size this against expected reconnect
    /// intervals; the steward enforces the durable replay surface
    /// against this window and surfaces a structured
    /// `replay_window_exceeded` response when a consumer asks for a
    /// cursor older than the oldest retained event. Default:
    /// [`DEFAULT_HAPPENINGS_RETENTION_WINDOW_SECS`].
    #[serde(default = "default_retention_window_secs")]
    pub retention_window_secs: u64,
    /// Interval between write-side janitor passes that trim
    /// `happenings_log` according to the retention policy. Default:
    /// [`DEFAULT_HAPPENINGS_JANITOR_INTERVAL_SECS`]. Operators with
    /// very high happening throughput may shorten this; idle
    /// stewards can lengthen it. The minimum effective interval is
    /// one second.
    #[serde(default = "default_janitor_interval_secs")]
    pub janitor_interval_secs: u64,
}

impl Default for HappeningsSection {
    fn default() -> Self {
        Self {
            retention_capacity: default_retention_capacity(),
            retention_window_secs: default_retention_window_secs(),
            janitor_interval_secs: default_janitor_interval_secs(),
        }
    }
}

fn default_retention_capacity() -> usize {
    DEFAULT_HAPPENINGS_RETENTION_CAPACITY
}

fn default_retention_window_secs() -> u64 {
    DEFAULT_HAPPENINGS_RETENTION_WINDOW_SECS
}

fn default_janitor_interval_secs() -> u64 {
    DEFAULT_HAPPENINGS_JANITOR_INTERVAL_SECS
}

/// `[catalogue]` section.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogueSection {
    /// Path to the catalogue TOML file.
    #[serde(default = "default_catalogue_path")]
    pub path: PathBuf,
    /// Path to the last-known-good catalogue shadow. Mirror-written
    /// atomically after every successful steady-state catalogue
    /// load; consulted as the second tier of the resilience chain
    /// when the configured catalogue fails to parse or validate.
    #[serde(default = "default_catalogue_lkg_path")]
    pub lkg_path: PathBuf,
}

impl Default for CatalogueSection {
    fn default() -> Self {
        Self {
            path: default_catalogue_path(),
            lkg_path: default_catalogue_lkg_path(),
        }
    }
}

fn default_catalogue_path() -> PathBuf {
    PathBuf::from(DEFAULT_CATALOGUE_PATH)
}

fn default_catalogue_lkg_path() -> PathBuf {
    PathBuf::from(DEFAULT_CATALOGUE_LKG_PATH)
}

/// `[time_trust]` section. Distribution declares the device's
/// hardware time-keeping reality and the framework's poll cadence
/// for the kernel's NTP synchronisation state. The framework
/// itself does NOT run an NTP / PTP / GPS client; it consumes the
/// state the OS daemon (`systemd-timesyncd`, `chrony`, `ptp4l`,
/// vendor agents) has already negotiated.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimeTrustSection {
    /// Whether the device has a battery-backed real-time clock.
    /// On `false`, the framework treats every boot and post-suspend
    /// wake as `Untrusted` until the OS time-sync daemon completes
    /// its first synchronisation. Default: `false` (conservative).
    #[serde(default)]
    pub has_battery_rtc: bool,
    /// Maximum tolerated staleness of the kernel's last-observed
    /// synchronisation. After this much elapsed time without a
    /// fresh sync observation, `Trusted` transitions to `Stale`.
    /// Default: 24 hours (86 400 000 ms).
    #[serde(default = "default_max_acceptable_staleness_ms")]
    pub max_acceptable_staleness_ms: u64,
    /// How often the framework polls the kernel's NTP state.
    /// Default: 5 seconds.
    #[serde(default = "default_time_trust_poll_interval_secs")]
    pub poll_interval_secs: u64,
}

impl Default for TimeTrustSection {
    fn default() -> Self {
        Self {
            has_battery_rtc: false,
            max_acceptable_staleness_ms: default_max_acceptable_staleness_ms(),
            poll_interval_secs: default_time_trust_poll_interval_secs(),
        }
    }
}

fn default_max_acceptable_staleness_ms() -> u64 {
    crate::time_trust::DEFAULT_MAX_ACCEPTABLE_STALENESS_MS
}

fn default_time_trust_poll_interval_secs() -> u64 {
    crate::time_trust::DEFAULT_POLL_INTERVAL_SECS
}

/// `[plugins]` section.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginsSection {
    /// Whether unsigned plugins are admitted. Per `PLUGIN_PACKAGING.md`
    /// section 5: unsigned plugins run only at `sandbox` class, only if
    /// this flag is true. Default: false.
    #[serde(default)]
    pub allow_unsigned: bool,
    /// Root for each plugin’s `state/` and `credentials/` subdirectories
    /// (`<plugin_data_root>/<name>/...`). Default:
    /// [`DEFAULT_PLUGIN_DATA_ROOT`].
    #[serde(default = "default_plugin_data_root_path")]
    pub plugin_data_root: PathBuf,
    /// Directory in which the steward places Unix domain sockets
    /// `<runtime_dir>/<plugin_name>.sock` for out-of-process plugins.
    /// Must exist and be writable before admission; the binary creates
    /// it at startup if missing. Default:
    /// [`DEFAULT_PLUGIN_RUNTIME_DIR`] (`/var/run/evo/plugins`),
    /// paralleling the steward's own socket at `/var/run/evo/evo.sock`
    /// per FHS.
    #[serde(default = "default_plugin_runtime_dir")]
    pub runtime_dir: PathBuf,
    /// Per-plugin operator config drop-in directory. The steward
    /// looks for `<this>/<plugin_name>.toml` at admission time and
    /// merges its contents into the plugin's `LoadContext.config`.
    /// A missing file is not an error (empty table); a malformed
    /// file aborts admission of that plugin. Default:
    /// [`DEFAULT_PLUGINS_CONFIG_DIR`] (`/etc/evo/plugins.d`).
    #[serde(default = "default_plugins_config_dir")]
    pub config_dir: PathBuf,
    /// Directories to scan for plugin bundles (`manifest.toml` in each
    /// bundle). Processed in order: a duplicate `plugin.name` in a
    /// later root replaces the earlier path. Default: `/opt/evo/plugins`
    /// then [`DEFAULT_PLUGIN_DATA_ROOT`].
    #[serde(default = "default_plugin_search_roots")]
    pub search_roots: Vec<PathBuf>,
    /// Directory of `*.pem` public keys (package-shipped; union with
    /// [`Self::trust_dir_etc`]). See `PLUGIN_PACKAGING.md` section 5.
    #[serde(default = "default_trust_dir_opt")]
    pub trust_dir_opt: PathBuf,
    /// Directory of `*.pem` public keys (operator `trust.d`). Each
    /// `foo.pem` must have a `foo.meta.toml` sidecar in the same directory.
    #[serde(default = "default_trust_dir_etc")]
    pub trust_dir_etc: PathBuf,
    /// Install-digest revocations (`[[revoke]]` in TOML).
    #[serde(default = "default_revocations_path")]
    pub revocations_path: PathBuf,
    /// If the manifest's trust class is above what the signing key may
    /// authorise, admit at the key's `max_trust_class` instead of
    /// refusing. If `false`, that situation is a hard error.
    #[serde(default = "default_degrade_trust")]
    pub degrade_trust: bool,
    /// Optional per-trust-class Unix identity for out-of-process plugin
    /// spawns. See [`PluginsSecurityConfig`]. Default: disabled; all
    /// plugins run as the steward user.
    #[serde(default)]
    pub security: PluginsSecurityConfig,
    /// Grace window in seconds before persisted factory-instance
    /// subjects whose owning plugin has not re-announced are forgotten.
    /// At boot, persisted subjects under the
    /// `evo-factory-instance` addressing scheme that were minted by a
    /// factory plugin in a previous run carry over in the durable
    /// store; if the plugin re-admits and re-announces them, they
    /// remain. If the plugin does not re-admit, or admits but does
    /// not re-announce a particular instance, the steward retracts
    /// the orphan after this grace window expires. Default:
    /// [`DEFAULT_FACTORY_ORPHAN_GRACE_SECS`] (30 seconds). Set to `0`
    /// to disable the scrub entirely (orphans then accumulate
    /// indefinitely).
    #[serde(default = "default_factory_orphan_grace_secs")]
    pub factory_orphan_grace_secs: u64,
}

fn default_plugin_data_root_path() -> PathBuf {
    PathBuf::from(DEFAULT_PLUGIN_DATA_ROOT)
}

fn default_plugin_runtime_dir() -> PathBuf {
    PathBuf::from(DEFAULT_PLUGIN_RUNTIME_DIR)
}

fn default_plugins_config_dir() -> PathBuf {
    PathBuf::from(DEFAULT_PLUGINS_CONFIG_DIR)
}

fn default_plugin_search_roots() -> Vec<PathBuf> {
    DEFAULT_PLUGIN_SEARCH_ROOTS
        .iter()
        .map(|s| PathBuf::from(*s))
        .collect()
}

/// Default factory-orphan grace window in seconds.
pub const DEFAULT_FACTORY_ORPHAN_GRACE_SECS: u64 = 30;

fn default_factory_orphan_grace_secs() -> u64 {
    DEFAULT_FACTORY_ORPHAN_GRACE_SECS
}

fn default_trust_dir_opt() -> PathBuf {
    PathBuf::from(DEFAULT_TRUST_DIR_OPT)
}

fn default_trust_dir_etc() -> PathBuf {
    PathBuf::from(DEFAULT_TRUST_DIR_ETC)
}

fn default_revocations_path() -> PathBuf {
    PathBuf::from(DEFAULT_REVOCATIONS_PATH)
}

fn default_degrade_trust() -> bool {
    DEFAULT_DEGRADE_TRUST
}

impl Default for PluginsSection {
    fn default() -> Self {
        Self {
            allow_unsigned: false,
            plugin_data_root: default_plugin_data_root_path(),
            runtime_dir: default_plugin_runtime_dir(),
            config_dir: default_plugins_config_dir(),
            search_roots: default_plugin_search_roots(),
            trust_dir_opt: default_trust_dir_opt(),
            trust_dir_etc: default_trust_dir_etc(),
            revocations_path: default_revocations_path(),
            degrade_trust: default_degrade_trust(),
            security: PluginsSecurityConfig::default(),
            factory_orphan_grace_secs: default_factory_orphan_grace_secs(),
        }
    }
}

/// Configures optional per-trust-class OS identity for out-of-process
/// plugin processes (`[plugins.security]` in `evo.toml`). When disabled
/// (the default), the steward’s behaviour is unchanged: every plugin
/// process runs as the same user as the steward. When enabled, classes
/// listed in [`Self::uid`] (and optional [`Self::gid`]) are applied on
/// Unix with `setgid` / `setuid` before `exec` (see
/// `std::os::unix::process::CommandExt`). Distributions that enable this
/// must create the corresponding system users and file/socket DAC as
/// appropriate; the steward does not implement seccomp, capabilities, or
/// namespace isolation. See `PLUGIN_PACKAGING.md` section 5.
#[derive(Default, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginsSecurityConfig {
    /// When `true`, spawns that have a `uid` entry for the effective
    /// trust class use that identity. When `false` (the default), `uid`
    /// and `gid` are ignored.
    #[serde(default)]
    pub enable: bool,
    /// Per effective [`TrustClass`], the Unix user ID of the child
    /// process. TOML keys are lowercase: `platform`, `privileged`, etc.
    /// Classes not present here spawn as the steward.
    #[serde(default)]
    pub uid: BTreeMap<TrustClass, u32>,
    /// Per-class GID. If a class is absent, its UID (from `uid`) is
    /// used as the GID.
    #[serde(default)]
    pub gid: BTreeMap<TrustClass, u32>,
}

impl PluginsSecurityConfig {
    /// Returns the `(uid, gid)` to apply to the child when [`Self::enable`]
    /// is true and `uid` contains an entry for `class`. GID defaults to
    /// the same as UID when not overridden in `gid`. Returns `None` when
    /// identity mapping is disabled or this class is not listed in `uid`.
    pub fn uid_gid_for_class(&self, class: TrustClass) -> Option<(u32, u32)> {
        if !self.enable {
            return None;
        }
        let uid = self.uid.get(&class).copied()?;
        let gid = self.gid.get(&class).copied().unwrap_or(uid);
        Some((uid, gid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_expected_paths() {
        let cfg = StewardConfig::default();
        assert_eq!(cfg.steward.log_level, "warn");
        assert_eq!(cfg.steward.socket_path, PathBuf::from(DEFAULT_SOCKET_PATH));
        assert_eq!(cfg.catalogue.path, PathBuf::from(DEFAULT_CATALOGUE_PATH));
        assert_eq!(
            cfg.persistence.path,
            PathBuf::from(DEFAULT_PERSISTENCE_PATH)
        );
        assert!(!cfg.plugins.allow_unsigned);
        assert_eq!(
            cfg.plugins.plugin_data_root,
            default_plugin_data_root_path()
        );
        assert_eq!(
            cfg.plugins.runtime_dir,
            PathBuf::from(DEFAULT_PLUGIN_RUNTIME_DIR)
        );
        assert_eq!(cfg.plugins.search_roots, default_plugin_search_roots());
        assert_eq!(cfg.plugins.trust_dir_opt, default_trust_dir_opt());
        assert_eq!(cfg.plugins.trust_dir_etc, default_trust_dir_etc());
        assert_eq!(cfg.plugins.revocations_path, default_revocations_path());
        assert!(cfg.plugins.degrade_trust);
        assert!(!cfg.plugins.security.enable);
        assert!(cfg.plugins.security.uid.is_empty());
    }

    #[test]
    fn empty_toml_is_all_defaults() {
        let cfg = StewardConfig::from_toml("").unwrap();
        assert_eq!(cfg, StewardConfig::default());
    }

    #[test]
    fn partial_toml_fills_defaults() {
        let cfg = StewardConfig::from_toml(
            r#"
[steward]
log_level = "info"
"#,
        )
        .unwrap();
        assert_eq!(cfg.steward.log_level, "info");
        assert_eq!(cfg.steward.socket_path, default_socket_path());
    }

    #[test]
    fn full_toml_round_trips() {
        let cfg = StewardConfig::from_toml(
            r#"
[steward]
log_level = "debug"
socket_path = "/tmp/evo.sock"

[catalogue]
path = "/etc/evo/catalogue.toml"

[plugins]
allow_unsigned = true
"#,
        )
        .unwrap();
        assert_eq!(cfg.steward.log_level, "debug");
        assert_eq!(cfg.steward.socket_path, PathBuf::from("/tmp/evo.sock"));
        assert_eq!(
            cfg.catalogue.path,
            PathBuf::from("/etc/evo/catalogue.toml")
        );
        assert!(cfg.plugins.allow_unsigned);
    }

    #[test]
    fn happenings_section_defaults_match_constants() {
        let cfg = StewardConfig::default();
        assert_eq!(
            cfg.happenings.retention_capacity,
            DEFAULT_HAPPENINGS_RETENTION_CAPACITY
        );
        assert_eq!(
            cfg.happenings.retention_window_secs,
            DEFAULT_HAPPENINGS_RETENTION_WINDOW_SECS
        );
        assert_eq!(
            cfg.happenings.janitor_interval_secs,
            DEFAULT_HAPPENINGS_JANITOR_INTERVAL_SECS
        );
    }

    #[test]
    fn happenings_section_overrides_parse() {
        let cfg = StewardConfig::from_toml(
            r#"
[happenings]
retention_capacity = 4096
retention_window_secs = 7200
janitor_interval_secs = 30
"#,
        )
        .unwrap();
        assert_eq!(cfg.happenings.retention_capacity, 4096);
        assert_eq!(cfg.happenings.retention_window_secs, 7200);
        assert_eq!(cfg.happenings.janitor_interval_secs, 30);
    }

    #[test]
    fn persistence_section_overrides_default_path() {
        let cfg = StewardConfig::from_toml(
            r#"
[persistence]
path = "/tmp/state/evo.db"
"#,
        )
        .unwrap();
        assert_eq!(cfg.persistence.path, PathBuf::from("/tmp/state/evo.db"));
    }

    #[test]
    fn plugins_security_toml_parses() {
        let cfg = StewardConfig::from_toml(
            r#"
[plugins.security]
enable = true

[plugins.security.uid]
standard = 2001
sandbox = 2002

[plugins.security.gid]
sandbox = 2003
"#,
        )
        .unwrap();
        assert!(cfg.plugins.security.enable);
        assert_eq!(
            *cfg.plugins.security.uid.get(&TrustClass::Standard).unwrap(),
            2001
        );
        assert_eq!(
            cfg.plugins
                .security
                .uid_gid_for_class(TrustClass::Standard)
                .unwrap(),
            (2001, 2001)
        );
        assert_eq!(
            cfg.plugins
                .security
                .uid_gid_for_class(TrustClass::Sandbox)
                .unwrap(),
            (2002, 2003)
        );
    }

    #[test]
    fn plugins_security_uid_gid_for_class_respects_enable() {
        let mut s = PluginsSecurityConfig {
            enable: false,
            uid: BTreeMap::from([(TrustClass::Platform, 99)]),
            ..Default::default()
        };
        assert_eq!(s.uid_gid_for_class(TrustClass::Platform), None);
        s.enable = true;
        assert_eq!(s.uid_gid_for_class(TrustClass::Platform), Some((99, 99)));
    }

    #[test]
    fn malformed_toml_errors() {
        let r = StewardConfig::from_toml("this is not [valid toml");
        assert!(matches!(r, Err(StewardError::Toml { .. })));
    }

    #[test]
    fn missing_file_returns_defaults() {
        let path =
            std::path::Path::new("/nonexistent/evo-test-never-exists.toml");
        let cfg = StewardConfig::load_from(path).unwrap();
        assert_eq!(cfg, StewardConfig::default());
    }

    #[test]
    fn load_from_required_errors_on_missing_file() {
        let path =
            std::path::Path::new("/nonexistent/evo-test-never-exists.toml");
        let r = StewardConfig::load_from_required(path);
        assert!(matches!(r, Err(StewardError::Io { .. })));
    }
}
