// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! `evo-plugin-tool` - `lint`, `sign`, `verify`, `pack`, `install` (see `docs/engineering/PLUGIN_TOOL.md`).

mod admin;
mod archive;
mod bundle;
mod catalogue_cmd;
mod exit_code;
mod install;
mod lint;
mod pack_cmd;
mod paths;
mod privileges_cmd;
mod shelf_schema;
mod sign;
mod verify_cmd;

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context as _;
use clap::{Parser, Subcommand};

const MAX_URL_DEFAULT: u64 = paths::DEFAULT_MAX_URL_BYTES;

/// Exit: 0 ok, 1 usage/manifest, 2 trust, 3 io, 4 network
#[derive(Parser)]
#[command(
    name = "evo-plugin-tool",
    about = "Plugin author CLI: lint, sign, verify, pack, install",
    version
)]
struct Cli {
    #[command(subcommand)]
    sub: Sub,
}

#[derive(Subcommand)]
enum Sub {
    /// Validate `manifest.toml` and the artefact path
    Lint { plugin_dir: PathBuf },
    /// Operations on catalogue documents.
    Catalogue {
        #[command(subcommand)]
        sub: CatalogueSub,
    },
    /// Write `manifest.sig` (ed25519 over `signing_message`, per evo_trust)
    Sign {
        plugin_dir: PathBuf,
        /// PKCS#8 PEM private key (cleartext)
        #[arg(long, value_name = "PEMFILE")]
        key: PathBuf,
    },
    /// Check signature and authorisation (loads trust from opt + etc and revocations)
    Verify {
        plugin_dir: PathBuf,
        /// Admit unsigned bundles (at `sandbox` trust class only). Off by default.
        #[arg(long)]
        allow_unsigned: bool,
        /// Refuse admission when the manifest declares a trust class stronger
        /// than the signing key's `max_trust_class`. Default behaviour is to
        /// degrade to the key's maximum; this flag opts in to strict refusal.
        #[arg(long)]
        strict_trust: bool,
        #[arg(long, value_name = "DIR", default_value = "/opt/evo/trust")]
        trust_dir_opt: PathBuf,
        #[arg(long, value_name = "DIR", default_value = "/etc/evo/trust.d")]
        trust_dir_etc: PathBuf,
        #[arg(
            long,
            value_name = "FILE",
            default_value = "/etc/evo/revocations.toml"
        )]
        revocations: PathBuf,
        /// Optional path to a JSON file containing the plugin's
        /// `RuntimeCapabilities` as returned by `Plugin::describe()`.
        /// When provided, verify additionally checks that the
        /// manifest's declared verb sets match the runtime
        /// capabilities; mismatch refuses verification with a
        /// structured drift report. Plugin authors generate the
        /// JSON in their build pipeline by running the plugin in
        /// a test harness and serialising `describe().runtime_capabilities`.
        #[arg(long, value_name = "FILE")]
        describe_json: Option<PathBuf>,
    },
    /// Create `.tar.gz` (default) / `.tar.xz` / `.zip` of the bundle
    Pack {
        plugin_dir: PathBuf,
        /// Output file (format from extension) or use `<n>-<ver>.<ext>` in cwd
        #[arg(long, short)]
        out: Option<PathBuf>,
        /// Archive format. One of `tar-gz`, `tar-xz`, `zip`. Default `tar-gz`.
        #[arg(long, value_enum)]
        format: Option<PackFormatArg>,
    },
    /// Unpack/accept a local directory, file, or URL, verify, install under --to
    Install {
        /// Path to bundle, archive, or `http(s)://` URL
        source: String,
        #[arg(
            long,
            default_value = paths::DEFAULT_SEARCH_ROOT,
            value_name = "DIR"
        )]
        to: PathBuf,
        /// Same semantics as `verify --allow-unsigned`.
        #[arg(long)]
        allow_unsigned: bool,
        /// Same semantics as `verify --strict-trust`.
        #[arg(long)]
        strict_trust: bool,
        #[arg(long, value_name = "DIR", default_value = "/opt/evo/trust")]
        trust_dir_opt: PathBuf,
        #[arg(long, value_name = "DIR", default_value = "/etc/evo/trust.d")]
        trust_dir_etc: PathBuf,
        #[arg(
            long,
            value_name = "FILE",
            default_value = "/etc/evo/revocations.toml"
        )]
        revocations: PathBuf,
        /// chown(1) argument (e.g. `nobody:audio`); runs `chown -R` on the installed path (Unix)
        #[arg(long, value_name = "USER:GROUP")]
        chown: Option<String>,
        #[arg(long, default_value_t = MAX_URL_DEFAULT, value_name = "N")]
        max_url_bytes: u64,
    },
    /// Operator-issued plugin lifecycle and reload verbs. Each
    /// subcommand opens a Unix-socket connection to the running
    /// steward, negotiates the `plugins_admin` capability, and
    /// dispatches the corresponding wire op.
    Admin {
        #[command(subcommand)]
        sub: AdminSub,
    },
    /// Read-only inspection and host-prerequisites verification of
    /// a `privileges.yaml` record. Plugin authors run these locally
    /// during development; operators run `check` on a target host
    /// before admission.
    Privileges {
        #[command(subcommand)]
        sub: PrivilegesSub,
    },
    /// Operator-issued plan management verbs. Each subcommand
    /// opens a Unix-socket connection to the running steward,
    /// negotiates the `plans_admin` capability, and dispatches
    /// the corresponding op.
    Plan {
        #[command(subcommand)]
        sub: PlanSub,
    },
}

#[derive(Subcommand)]
enum PlanSub {
    /// Fire a registered plan now. Drives the steward's
    /// `fire_plan` op which dispatches with
    /// `FireSource::UserCommand`. The fire is asynchronous: the
    /// command returns once the fire is scheduled. Track
    /// execution progress via the happenings bus
    /// (`evo-plugin-tool admin subscribe-happenings`).
    Fire {
        /// Stable plan id (filename stem of
        /// `<plans-storage-root>/<id>.toml`).
        plan_id: String,
        /// Path to the steward's Unix socket.
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

#[derive(Subcommand)]
enum PrivilegesSub {
    /// Parse a privileges.yaml record and print it as a human-
    /// readable report (or JSON with `--format=json`).
    Describe {
        /// Path to a privileges.yaml file or to a directory
        /// containing one.
        path: PathBuf,
        #[arg(long, value_enum, default_value_t = privileges_cmd::DescribeFormat::Text)]
        format: privileges_cmd::DescribeFormat,
    },
    /// Validate a privileges.yaml record and verify the host
    /// satisfies its prerequisites: required binaries on PATH,
    /// kernel modules loadable, system services reachable,
    /// verification commands exit 0. Schema validation always
    /// runs; `--schema-only` skips host probes (useful off-target).
    ///
    /// Blocking failures render alongside a structured remediation
    /// list — package-install hints (`apt install <pkg>` etc.),
    /// kernel-module load hints (`modprobe <mod>`), systemd unit
    /// hints (`systemctl enable --now <unit>`). The distribution
    /// family is auto-detected from `/etc/os-release`; `--distro`
    /// forces a specific family for off-target authoring.
    Check {
        /// Path to a privileges.yaml file or to a directory
        /// containing one.
        path: PathBuf,
        /// Skip host-prerequisite probes; only validate the schema.
        #[arg(long)]
        schema_only: bool,
        /// Skip running verification commands. Useful when the
        /// declared service identity does not yet exist on this
        /// host (admission gate enforcement is steward-side).
        #[arg(long)]
        skip_verification: bool,
        /// Treat advisory warnings as failures.
        #[arg(long)]
        strict: bool,
        /// Distribution family for remediation-hint rendering.
        /// Omit to auto-detect from `/etc/os-release`.
        #[arg(long, value_enum)]
        distro: Option<privileges_cmd::DistroFamilyArg>,
    },
    /// Emit a starter privileges.yaml for a common plugin
    /// archetype. Each template captures the ~80% case for its
    /// archetype; the author specialises the domain-specific
    /// fields (canonical plugin id, owner, verification
    /// commands, per-distribution host_provisioning) before
    /// shipping. Templates are embedded at compile time — no
    /// on-disk lookup, no environment dependencies.
    Template {
        /// Which archetype to emit.
        #[arg(value_enum)]
        archetype: privileges_cmd::Archetype,
        /// Where to write the template. Omit to print to stdout
        /// (the common `> privileges.yaml` redirect usage).
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum AdminSub {
    /// Persist `enabled = true` for the named plugin and record
    /// the operator-supplied reason in the audit row. Inline
    /// re-admission of a currently-unloaded plugin is staged
    /// behind the next discovery boundary today.
    Enable {
        /// Canonical plugin name (reverse-DNS).
        plugin: String,
        /// Operator-readable reason for the audit row.
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
        /// Step-up authentication token from a prior
        /// `step-up verify` invocation. Required when the
        /// running steward has an `AuthService` configured;
        /// optional when the steward runs without one.
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        /// Path to the steward's Unix socket.
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Drain the running plugin (if admitted) and persist
    /// `enabled = false`. Refuses with a structured
    /// `essential_plugin` subclass when the plugin's shelf is
    /// declared `required = true`.
    Disable {
        plugin: String,
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
        /// Step-up authentication token; see `Enable` for
        /// gate semantics.
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        /// When set and the target plugin has admitted
        /// dependents declaring it as required, the framework
        /// disables the transitive dependent set first then
        /// the target. Without the flag, the call refuses with
        /// a structured `dependents_present` subclass listing
        /// the dependents.
        #[arg(long)]
        cascade_dependents: bool,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Read-only preview of the plugin set whose admission
    /// would break if the target plugin were disabled. Useful
    /// before `admin disable --cascade-dependents` to see the
    /// full impact graph.
    Dependents {
        /// Canonical name of the plugin whose dependents are
        /// being previewed.
        plugin: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Read-only aggregate plugin-health snapshot. Returns
    /// admitted/enabled/disabled counts plus per-plugin
    /// health and resource detail (the latter populated
    /// when the per-plugin health and resource-accounting
    /// primitives compose).
    Health {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Drain, remove the plugin's bundle directory from disk,
    /// and forget the `installed_plugins` row. Refuses on
    /// essential shelves. With `--purge-state`, also wipes the
    /// per-plugin state and credentials directories.
    Uninstall {
        plugin: String,
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
        #[arg(long)]
        purge_state: bool,
        /// Step-up authentication token; see `Enable` for
        /// gate semantics.
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Wipe the named plugin's `state/` and `credentials/`
    /// directories without removing the bundle. Used for
    /// "factory reset" of a misbehaving plugin while preserving
    /// the installed code.
    PurgeState {
        plugin: String,
        /// Step-up authentication token; see `Enable` for
        /// gate semantics.
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Aggregate diagnostic view per plugin: admission state,
    /// shelf, interaction kind. Recent-events aggregation rides
    /// a follow-up.
    Diagnose {
        plugin: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Print the steward's `describe_capabilities` response: wire
    /// version, supported ops, named features, catalogue source
    /// (`configured` / `lkg` / `builtin`), wall-clock trust state
    /// (`trusted` / `untrusted` / `stale` / `adjusting`), and
    /// whether the device has a battery-backed RTC. Read-only;
    /// no capability negotiation. The clock-trust field is the
    /// canonical operator-facing surface for time-trust state.
    DescribeCapabilities {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Describe the framework's admitted UI stockings.
    /// Returns one entry per (plugin, stocking) pair across
    /// every admitted plugin. Optional `--shelf` filter
    /// narrows to one shelf id. Read-only; no capability
    /// negotiation.
    DescribeUiStockings {
        /// Optional shelf id filter (e.g. `library.sources`,
        /// `system.diagnostics`). When omitted, every
        /// admitted stocking is returned.
        #[arg(long, value_name = "ID")]
        shelf: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Activate (or clear) the active theme. Pass a plugin
    /// name to activate that theme; pass `--clear` to
    /// deactivate any currently-active theme. Capability-
    /// gated by `plugins_admin` and step-up-aware. Refuses
    /// with `theme_not_admitted` when the named plugin is
    /// not currently in the framework's theme registry.
    ActivateTheme {
        /// Canonical plugin name of the theme to activate.
        /// Required unless `--clear` is set.
        #[arg(long, value_name = "NAME", conflicts_with = "clear")]
        plugin: Option<String>,
        /// Clear the active theme slot. Mutually exclusive
        /// with `--plugin`.
        #[arg(long, conflicts_with = "plugin")]
        clear: bool,
        /// Privileged-session token issued by `step-up
        /// verify`. Required when the steward's auth posture
        /// demands step-up; framework-level auth-disabled
        /// runs admit without one.
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Activate (or clear) the active UI shell. Mirror of
    /// `activate-theme` for the `ui_shell` slot.
    ActivateUiShell {
        /// Canonical plugin name of the UI shell to
        /// activate. Required unless `--clear` is set.
        #[arg(long, value_name = "NAME", conflicts_with = "clear")]
        plugin: Option<String>,
        /// Clear the active UI shell slot.
        #[arg(long, conflicts_with = "plugin")]
        clear: bool,
        /// Privileged-session token.
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Read the active UI selection. Returns the active
    /// theme + active UI shell plugin names (each absent
    /// when the slot is unset). Read-only; no capability
    /// negotiation.
    DescribeActiveUiSelection {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Subscribe to the steward's happenings bus and stream every
    /// matching event to stdout, one happening per line as JSON.
    /// Promotes the connection to streaming mode; exits after
    /// `--max-count` events received OR `--duration-secs` elapsed,
    /// whichever comes first. Default bounds: 100 events / 30s.
    SubscribeHappenings {
        /// Maximum events to print before exiting.
        #[arg(long, value_name = "N", default_value_t = 100)]
        max_count: u64,
        /// Maximum seconds to stream before exiting.
        #[arg(long, value_name = "SECS", default_value_t = 30)]
        duration_secs: u64,
        /// Optional cursor for replay. Only happenings with seq
        /// strictly greater than `since` are streamed.
        #[arg(long, value_name = "SEQ")]
        since: Option<u64>,
        /// Optional comma-separated variant whitelist (e.g.
        /// `appointment_fired,watch_fired`). Empty = no filter.
        #[arg(long, value_name = "VARIANTS")]
        variants: Option<String>,
        /// Optional comma-separated plugin whitelist. Empty = no
        /// filter.
        #[arg(long, value_name = "PLUGINS")]
        plugins: Option<String>,
        /// Optional comma-separated shelf whitelist. Empty = no
        /// filter.
        #[arg(long, value_name = "SHELVES")]
        shelves: Option<String>,
        /// Optional comma-separated coalesce labels. Same-label
        /// happenings within the window collapse into one delivered
        /// envelope per the framework's selection rule. Common
        /// shapes: `variant,plugin`; `variant,plugin,sensor_id`.
        /// Absent = firehose delivery.
        #[arg(long, value_name = "LABELS")]
        coalesce_labels: Option<String>,
        /// Coalesce window in milliseconds. Same-key happenings
        /// within the window collapse. Defaults to the framework
        /// default when omitted; only consulted when
        /// --coalesce-labels is set.
        #[arg(long, value_name = "MS")]
        coalesce_window_ms: Option<u32>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Operator-issued plugin-registry registration.
    /// Records a registry's HTTPS manifest URL + signature URL +
    /// pinned signing-key fingerprint; the framework polls and
    /// caches the manifest, surfacing its plugin listings to
    /// metadata-aware operator surfaces.
    RegisterRegistry {
        /// Stable filesystem-safe slug for the registry.
        slug: String,
        /// HTTPS URL of the manifest TOML.
        #[arg(long, value_name = "URL")]
        manifest_url: String,
        /// HTTPS URL of the detached signature.
        #[arg(long, value_name = "URL")]
        signature_url: String,
        /// SHA-256 fingerprint the signing key must present.
        #[arg(long, value_name = "FINGERPRINT")]
        public_key_fingerprint: String,
        /// Optional per-registry polling interval (seconds).
        #[arg(long, value_name = "SECS")]
        poll_interval_secs: Option<u64>,
        /// Step-up authentication token; see `Enable` for
        /// gate semantics.
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        /// Path to the steward's Unix socket.
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Operator-issued unregistration of a registry.
    UnregisterRegistry {
        /// Slug to forget.
        slug: String,
        /// Step-up authentication token; see `Enable` for
        /// gate semantics.
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        /// Path to the steward's Unix socket.
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// List registered plugin registries with their cached
    /// manifest counts and last-refresh timestamps.
    ListRegistries {
        /// Path to the steward's Unix socket.
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Refresh a registered registry now (bypasses the polling
    /// cadence). Useful when the operator knows the upstream
    /// manifest has updated and wants the cached copy
    /// immediately.
    RefreshRegistry {
        /// Slug to refresh.
        slug: String,
        /// Step-up authentication token; see `Enable` for
        /// gate semantics.
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        /// Path to the steward's Unix socket.
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Grant operator trust to a publisher whose signing key
    /// is not shipped in the vendor distribution. The grant
    /// materialises by writing the publisher's public key
    /// into the operator trust directory; admission picks it
    /// up on next signature verify.
    GrantPublisherTrust {
        /// Publisher's signing-key SHA-256 fingerprint
        /// (lowercase hex).
        publisher_id: String,
        /// Operator-readable display name.
        #[arg(long, value_name = "NAME")]
        display_name: String,
        /// Path to the publisher's PEM-encoded public key.
        #[arg(long, value_name = "FILE")]
        public_key_pem: PathBuf,
        /// Optional comma-separated list of plugin canonical
        /// names this grant covers. Absent grants AllPlugins.
        #[arg(long, value_name = "NAMES")]
        scope_per_plugin: Option<String>,
        /// Step-up authentication token; see `Enable` for
        /// gate semantics.
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        /// Path to the steward's Unix socket.
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Revoke a previously-granted publisher trust.
    RevokePublisherTrust {
        /// Publisher fingerprint to revoke.
        publisher_id: String,
        /// Step-up authentication token; see `Enable` for
        /// gate semantics.
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        /// Path to the steward's Unix socket.
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// List recorded publisher trust grants and revocations.
    ListPublisherTrust {
        /// Path to the steward's Unix socket.
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Operator-issued install-from-URL through the steward.
    /// Distinct from the top-level `install` subcommand: the
    /// top-level command runs locally on the dev box and
    /// admits in-place. This admin verb dispatches the wire op
    /// `install_plugin_from_url`; the running steward fetches
    /// over HTTPS, drops the bundle into its stage directory,
    /// and the stage watcher admits on the next polling tick.
    /// The two paths exist for different operator workflows:
    /// `install` for scripted dev-box deploys, this admin verb
    /// for in-product UI invocation against a running steward.
    InstallFromUrl {
        /// HTTPS URL of the bundle archive
        /// (`.tar.gz` / `.tar.xz` / `.zip`).
        url: String,
        /// Optional fingerprint pin recording an out-of-band-
        /// approved signing key. Recorded in the audit ledger;
        /// the trust-state primitive consumes it for trust
        /// elevation.
        #[arg(long, value_name = "SHA256_HEX")]
        signature_pin: Option<String>,
        /// Step-up authentication token; see `Enable` for
        /// gate semantics.
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        /// Path to the steward's Unix socket.
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Reload one plugin. Drives the steward's reload_plugin op,
    /// which dispatches to Live or Restart mode per the plugin's
    /// manifest `lifecycle.hot_reload`. For OOP Live, the
    /// framework calls prepare_for_live_reload on the running
    /// instance, spawns a successor from the recorded bundle
    /// directory, and calls load_with_state on the successor with
    /// the blob the prior instance returned.
    ReloadPlugin {
        /// Canonical name of the plugin to reload.
        plugin: String,
        /// Step-up authentication token; see `Enable` for
        /// gate semantics.
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Operator-gestured plugin reload via the lifecycle
    /// coordinator. Distinct from `reload-plugin`: this routes
    /// through the new per-plugin `lifecycle.mode` surface
    /// (`reactive-only` / `reload-cleanable` / `frozen`),
    /// emitting an OperatorGesture reload request the
    /// admission-engine integration consumes. The legacy
    /// `reload-plugin` continues to serve the
    /// `lifecycle.hot_reload` Live / Restart mechanisms.
    PluginReload {
        /// Canonical plugin name.
        plugin: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Operator-gestured recovery of a degraded plugin.
    /// Clears the plugin's degraded slot and resets failure
    /// counters in the plugin-degraded registry; the
    /// admission-engine integration's re-admit-from-defaults
    /// path consumes the clear and re-admits the plugin.
    /// Capability-gated by `plugins_admin` and step-up-aware.
    PluginRestore {
        /// Canonical plugin name.
        plugin: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Privileged step-up authentication. `verify` reads a
    /// password from /dev/tty without echo and exchanges it
    /// for a short-lived session token; `revoke` invalidates a
    /// previously-issued token. Tokens are bound to the
    /// verifying peer's UID and not portable to other shells
    /// without the same UID.
    StepUp {
        #[command(subcommand)]
        sub: AdminStepUpSub,
    },
    /// Operator bearer-token mint surface for the HTTPS / WS
    /// projection. `mint-bearer-token` is the SSH-required
    /// developer / recovery path of the framework's trust
    /// substrate: dispatches the `mint_bearer_token` wire op,
    /// prints the signed token to stdout for the operator to
    /// paste into a browser session header. Audit-logged via the
    /// steward's observatory and the journal. The consumer-grade
    /// flow (first-boot wizard + QR cert install + WS-upgrade
    /// session mint) lands in a follow-on release alongside the
    /// per-domain CA primitive.
    Auth {
        #[command(subcommand)]
        sub: AdminAuthSub,
    },
    /// Operator-issued read/write of the update-channel
    /// preference per target. `set` requires the
    /// `plugins_admin` capability and step-up auth; `list` is
    /// read-only. Allowed channels: `alpha` / `test` /
    /// `production`. Allowed targets: `core` / `plugins`.
    UpdateChannel {
        #[command(subcommand)]
        sub: AdminUpdateChannelSub,
    },
    /// Plugin profile management — named plugin sets with
    /// per-plugin enabled/disabled state. `list` and `get` are
    /// read-only; `put` / `delete` / `activate` are
    /// `plugins_admin` capability-gated and step-up-aware.
    /// `activate --dry-run` returns the per-plugin transition
    /// plan without dispatching.
    Profile {
        #[command(subcommand)]
        sub: AdminProfileSub,
    },
    /// Bulk plugin lifecycle ops over a typed filter. The
    /// filter is a JSON expression — see the
    /// `crate::plugin_filter::PluginFilter` enum for the
    /// supported variants. `list-where` is read-only;
    /// `enable-where` / `disable-where` are step-up-aware
    /// transactional bulk ops.
    Bulk {
        #[command(subcommand)]
        sub: AdminBulkSub,
    },
    /// Plugin tag management — operator-applied metadata
    /// consumed by the bulk-op filter language's `by_tag`
    /// matcher. `add` / `remove` are step-up-aware; `list` is
    /// read-only.
    Tag {
        #[command(subcommand)]
        sub: AdminTagSub,
    },
    /// Admission policy management — operator-defined rule
    /// sets. `list` / `get` / `audit` are read-only;
    /// `put` / `delete` / `activate` are step-up-aware.
    /// `audit <id>` walks every known plugin and returns
    /// per-plugin policy violations without transitioning any
    /// plugin (continuous-review surface).
    Policy {
        #[command(subcommand)]
        sub: AdminPolicySub,
    },
    /// Per-capability grant revocation. Records and lists
    /// operator-issued revocations of individual capabilities a
    /// plugin's manifest declares (`outbound_network`,
    /// `filesystem_unrestricted`, `appointments`, `watches`,
    /// ...). `list` / `list-all` are read-only; `revoke` /
    /// `unrevoke` are step-up-aware.
    Capability {
        #[command(subcommand)]
        sub: AdminCapabilitySub,
    },
    /// Cross-device operator-configuration migration bundle.
    /// `export` writes a single TOML document capturing every
    /// section of the operator-curated configuration substrate
    /// (update channels, plugin tags, plugin profiles + active
    /// selection, admission policies + active selection,
    /// per-capability revocations); `import` applies a bundle
    /// against a target device, replacing each covered section
    /// atomically. The plugin set itself and per-device runtime
    /// state are NOT in the bundle.
    Migration {
        #[command(subcommand)]
        sub: AdminMigrationSub,
    },
    /// Audio data plane operator surface. `profile-override`
    /// is the one substrate the framework owns persistently
    /// for the four-source hardware-profile composer; the
    /// other three layers (probed-live / manifest-declared /
    /// vendor-database) are computed on demand by the topology
    /// scorer and live at the delivery plugin / vendor
    /// distribution.
    Audio {
        #[command(subcommand)]
        sub: AdminAudioSub,
    },
    /// Reload catalogue or manifest declarations.
    Reload {
        #[command(subcommand)]
        sub: AdminReloadSub,
    },
    /// Inspect and operate on the per-pair reconciliation
    /// loop. `list` and `project` are read-only; `now` requires
    /// the `reconciliation_admin` capability.
    Reconcile {
        #[command(subcommand)]
        sub: AdminReconcileSub,
    },
    /// Per-class hardware flight-mode control. Sugar over
    /// `op = "request"` against shelves of the
    /// distribution's `flight_mode` rack. The framework owns
    /// no flight-mode taxonomy; the available classes come from
    /// the distribution's catalogue declaration.
    Flight {
        #[command(subcommand)]
        sub: AdminFlightSub,
    },
    /// Operator-issued subject-grammar migration controls.
    /// Sugar over `list_grammar_orphans` /
    /// `accept_grammar_orphans` / `migrate_grammar_orphans`.
    Grammar {
        #[command(subcommand)]
        sub: AdminGrammarSub,
    },
    /// Operator-side responder for plugin-issued user-interaction
    /// prompts. List open prompts, answer them, or cancel them.
    /// Each verb requires the `user_interaction_responder`
    /// capability the steward enforces server-side.
    Prompt {
        #[command(subcommand)]
        sub: AdminPromptSub,
    },
    /// Operator-side warden custody surface. Take custody, issue
    /// course-corrections, release custody. The framework's
    /// `course_correct_verbs` manifest gate refuses verbs not in
    /// the warden's declared set; the verb-gate is the
    /// operator-visible refusal point.
    Warden {
        #[command(subcommand)]
        sub: AdminWardenSub,
    },
    /// Persistent device identity — singleton substrate
    /// generated at first boot and durable across reinstall.
    /// Multi-room discovery, group membership, source-host
    /// election, and ledger primitives identify the local node
    /// by this id. The canonical id is immutable; only the
    /// operator-editable display name may change.
    Device {
        #[command(subcommand)]
        sub: AdminDeviceSub,
    },
    /// Multi-room group entities — typed multi-device sets.
    /// A group is one logical playback target spanning one
    /// or more devices. Operators construct groups to issue
    /// verbs at the room-level rather than per-device.
    Group {
        #[command(subcommand)]
        sub: AdminGroupSub,
    },
    /// Multi-room runtime state — per-device role substrate
    /// (Source / Receiver / Auto) operator gestures land in
    /// here. The multi-room plugin observes the substrate via
    /// subscription and reconfigures DAC, capture, and
    /// audio-plane connections in place without plugin
    /// reload.
    Multiroom {
        #[command(subcommand)]
        sub: AdminMultiroomSub,
    },
    /// Domain trust ledger — list / admit / revoke peers in
    /// this device's domain. A domain is the trust unit the
    /// operator administers; admit gestures from a domain
    /// member's UI grow the roster; soft revoke retains the
    /// row for re-admit.
    Domain {
        #[command(subcommand)]
        sub: AdminDomainSub,
    },
    /// Three-channel update model — inspect inventory,
    /// trigger checks, apply updates, manage per-source
    /// auto-apply policy. Update sources (plugins / core /
    /// OS) admit as gateway plugins and register with the
    /// framework; the substrate ships with no default
    /// sources — vendor distributions carry the
    /// implementations.
    Updates {
        #[command(subcommand)]
        sub: AdminUpdatesSub,
    },
    /// Steward process control — graceful restart that
    /// preserves durable substrate across `execve`.
    Steward {
        #[command(subcommand)]
        sub: AdminStewardSub,
    },
}

#[derive(Subcommand)]
enum AdminStewardSub {
    /// Initiate a graceful steward restart. The framework
    /// emits a `StewardRestarting` happening, drains
    /// briefly, and `execve`s the target binary. The new
    /// steward boot path rehydrates from durable
    /// substrate.
    Restart {
        /// Operator-supplied free-form reason recorded in
        /// the audit trail.
        reason: String,
        /// Optional target binary path. When omitted, the
        /// steward restarts in place using
        /// `std::env::current_exe()`.
        #[arg(long, value_name = "PATH")]
        target_binary: Option<String>,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

#[derive(Subcommand)]
enum AdminUpdatesSub {
    /// List the aggregated update inventory across every
    /// registered source.
    List {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Trigger a check across every registered source, or
    /// one named source.
    CheckNow {
        /// Source id (e.g. `plugins`, `core`, `os`). When
        /// absent, all sources are checked.
        #[arg(long, value_name = "ID")]
        source_id: Option<String>,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Apply one specific update.
    Apply {
        /// Source id.
        source_id: String,
        /// Source-defined update id.
        update_id: String,
        /// Run the source's reversible apply path without
        /// committing.
        #[arg(long)]
        dry_run: bool,
        /// Optional operator principal to record in the
        /// audit trail.
        #[arg(long, value_name = "PRINCIPAL")]
        approved_by: Option<String>,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Per-source auto-apply policy surface.
    AutoApply {
        #[command(subcommand)]
        sub: AdminUpdatesAutoApplySub,
    },
}

#[derive(Subcommand)]
enum AdminUpdatesAutoApplySub {
    /// Show every recorded auto-apply policy entry.
    Show {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Set the auto-apply policy for one source.
    Set {
        /// Source id.
        source_id: String,
        /// Enable or disable auto-apply. Accepts
        /// `true` / `false` / `yes` / `no` / `on` / `off`
        /// / `1` / `0`.
        enabled: String,
        /// Severity threshold — one of `routine` /
        /// `recommended` / `security` / `critical`.
        severity_threshold: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

#[derive(Subcommand)]
enum AdminGroupSub {
    /// Create a new group with the supplied display name and
    /// member device ids. Capability-gated by `plugins_admin`
    /// and step-up-aware. Refuses empty membership; duplicates
    /// are de-duped before persistence.
    Create {
        /// Operator-supplied display name.
        display_name: String,
        /// Member device ids. At least one is required.
        device_ids: Vec<String>,
        /// Step-up authentication token.
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// List every recorded group with full membership.
    /// Capability-gated by `plugins_admin`.
    List {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Show one group with full membership. Capability-gated
    /// by `plugins_admin`.
    Show {
        /// Canonical group id.
        group_id: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Rename one group's display name. Capability-gated by
    /// `plugins_admin` and step-up-aware.
    Rename {
        /// Canonical group id.
        group_id: String,
        /// New display name.
        new_name: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Set the per-group multi-room latency budget in
    /// milliseconds. The multi-room plugin reads the value
    /// every render frame; changes take effect on the next
    /// frame without restart. Validates the new value (range
    /// 10..=5000 ms). Capability-gated by `plugins_admin` and
    /// step-up-aware.
    SetLeaderMs {
        /// Canonical group id.
        group_id: String,
        /// New per-group latency budget in milliseconds.
        leader_ms: u32,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Add a device to an existing group. Idempotent on
    /// already-present devices. Capability-gated by
    /// `plugins_admin` and step-up-aware.
    AddMember {
        /// Canonical group id.
        group_id: String,
        /// Device id to add.
        device_id: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Remove a device from a group. Refuses removal of the
    /// last member (use `delete` instead). Capability-gated
    /// by `plugins_admin` and step-up-aware.
    RemoveMember {
        /// Canonical group id.
        group_id: String,
        /// Device id to remove.
        device_id: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Move a device atomically from one group to another.
    /// No visible solo intermediate. When the moved device is
    /// the source group's leader AND post-move source count
    /// would be \u{2265} 2, the first invocation returns
    /// `LeaderSuccessorRequired` with eligible successor ids;
    /// retry with `--successor <device-id>` to commit the
    /// atomic pin + remove + insert. When post-move source
    /// count drops below 2, the source group auto-dissolves
    /// and the move proceeds in a single round-trip.
    /// Capability-gated by `plugins_admin` and step-up-aware.
    MoveMember {
        /// Source group id.
        from_group_id: String,
        /// Target group id.
        to_group_id: String,
        /// Canonical id of the device to move.
        device_id: String,
        /// Successor for the leader-of-source case. Pass on
        /// the second invocation after seeing
        /// `LeaderSuccessorRequired`.
        #[arg(long, value_name = "DEVICE_ID")]
        successor: Option<String>,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Delete a group (cascades membership). Idempotent on
    /// absent ids. Capability-gated by `plugins_admin` and
    /// step-up-aware.
    Delete {
        /// Canonical group id.
        group_id: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Pin a specific device as the source-host (leader) for
    /// a multi-room group. Operator override of the
    /// framework's canonical-min election rule. The election
    /// runtime respects the pin while the pinned device
    /// remains a live group member. Capability-gated by
    /// `plugins_admin` and step-up-aware.
    PinSourceHost {
        /// Canonical group id.
        group_id: String,
        /// Canonical id of the device to pin as source-host.
        device_id: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Clear the source-host pin for a multi-room group.
    /// Election resumes its standard canonical-min rule on
    /// the next sweep. Capability-gated by `plugins_admin`
    /// and step-up-aware.
    UnpinSourceHost {
        /// Canonical group id.
        group_id: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Inspect the local node's last-known source-host
    /// election for a group, or list every recorded
    /// election. Read-only; capability-gated by
    /// `plugins_admin`.
    SourceHost {
        #[command(subcommand)]
        sub: AdminGroupSourceHostSub,
    },
    /// Inspect the local node's clock-sync state for a
    /// group, or list every recorded state. Read-only;
    /// capability-gated by `plugins_admin`.
    Clock {
        #[command(subcommand)]
        sub: AdminGroupClockSub,
    },
    /// Inspect the local node's audio-plane peer
    /// connections (TCP control + data channel between
    /// this node and other multi-room peers). Read-only;
    /// capability-gated by `plugins_admin`.
    Network {
        #[command(subcommand)]
        sub: AdminGroupNetworkSub,
    },
    /// Resolve where a verb targeting one multi-room group
    /// would dispatch — identifies the elected source-host
    /// and the audio-plane connection state. Read-only;
    /// observational substrate. Capability-gated by
    /// `plugins_admin`.
    Dispatch {
        /// Canonical group id.
        group_id: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Inspect the composite active topology snapshot for a
    /// group, or list every recorded group's snapshot.
    /// Read-only; capability-gated by `plugins_admin`.
    Topology {
        #[command(subcommand)]
        sub: AdminGroupTopologySub,
    },
    /// Inspect admitted gateway plugins (plugins that
    /// bridge between an external ecosystem and the multi-
    /// room native protocol). Read-only; capability-gated
    /// by `plugins_admin`.
    Gateways {
        #[command(subcommand)]
        sub: AdminGroupGatewaysSub,
    },
    /// Per-frame audible-time trace snapshot for the
    /// multi-room native protocol. Reads the rolling-window
    /// snapshot from the admitted `audio.multiroom` shelf's
    /// `audio.multiroom.frame_trace.snapshot` wire-op and
    /// renders it for operator consumption. Read-only;
    /// capability-gated by `plugins_admin`.
    FrameTrace {
        /// Emit the snapshot as JSON (one envelope object
        /// with `records`) rather than the human-readable
        /// table form. Useful for piping into `jq` or other
        /// analysis tooling.
        #[arg(long)]
        json: bool,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

/// Subcommands under `admin multiroom`. Operator gestures
/// for the per-device multi-room role substrate.
#[derive(Subcommand)]
enum AdminMultiroomSub {
    /// Set the per-device multi-room role
    /// (`source` / `receiver` / `auto`). The multi-room
    /// plugin observes the substrate via subscription and
    /// reconfigures DAC, capture, and audio-plane
    /// connections in place. Idempotent on unchanged value.
    /// Capability-gated by `plugins_admin` and step-up-aware.
    SetRole {
        /// Canonical device id.
        device_id: String,
        /// Role to set (`source` / `receiver` / `auto`).
        role: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Read the operator-declared role for a device. Returns
    /// `auto` for devices with no explicit gesture
    /// (substrate-empty default). Read-only; capability-gated
    /// by `plugins_admin`.
    GetRole {
        /// Canonical device id.
        device_id: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// List every device with an explicit operator-gestured
    /// role. Devices in the substrate-empty / `auto` default
    /// are NOT enumerated. Read-only; capability-gated by
    /// `plugins_admin`.
    ListRoles {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Clear the operator-declared role for a device. The
    /// device returns to the substrate-empty `auto` default.
    /// Idempotent on devices already at default.
    /// Capability-gated by `plugins_admin` and step-up-aware.
    ClearRole {
        /// Canonical device id.
        device_id: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Operator-gestured reconnect storm against a peer that
    /// is currently unreachable. Runs the 5-carrier reconnect
    /// sequence (cached-endpoint dial / subnet sweep /
    /// mDNS-SD targeted query / UDP broadcast wake /
    /// audio-plane hello) in parallel with a 30 s deadline
    /// and 2 s retry cadence; emits the storm outcome
    /// (winning carrier + elapsed time, or exhaustion).
    /// Capability-gated by `plugins_admin` and step-up-aware.
    Reconnect {
        /// Canonical device id of the peer to reconnect.
        device_id: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

#[derive(Subcommand)]
enum AdminGroupTopologySub {
    /// Show the topology snapshot for one group.
    Show {
        /// Canonical group id.
        group_id: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// List the topology snapshot for every group.
    List {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

#[derive(Subcommand)]
enum AdminGroupGatewaysSub {
    /// List every registered gateway plugin.
    List {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

#[derive(Subcommand)]
enum AdminGroupNetworkSub {
    /// List every active audio-plane peer connection.
    Connections {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Manually establish an outbound audio-plane connection
    /// to a peer when auto-discovery does not surface a
    /// dialable address.
    Dial {
        /// Peer's audio-plane listener address in `host:port`
        /// form. Accepts IPv4 dotted-quad or IPv6 bracketed.
        addr: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

#[derive(Subcommand)]
enum AdminGroupClockSub {
    /// Show the clock-sync state for one group.
    Show {
        /// Canonical group id.
        group_id: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// List the clock-sync state for every group.
    List {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

#[derive(Subcommand)]
enum AdminGroupSourceHostSub {
    /// Show the elected source-host for one group.
    Show {
        /// Canonical group id.
        group_id: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// List the elected source-host for every group.
    List {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

#[derive(Subcommand)]
enum AdminDomainSub {
    /// List every domain-membership row in the local trust
    /// ledger composed with live discovery state and the
    /// persistent display-name cache. Read-only;
    /// capability-gated by `plugins_admin`.
    List {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Admit a device to the local domain trust ledger.
    /// Operator gesture; the admitting device id is captured
    /// from the local identity. When `display_name` is
    /// omitted, the framework resolves the peer's
    /// currently-observed mDNS-SD advert display_name from
    /// `discovered_peers`; admission refuses when the peer has
    /// not yet been seen. Capability-gated by
    /// `plugins_admin` and step-up-aware.
    Admit {
        /// Canonical device id of the peer being admitted.
        device_id: String,
        /// Optional explicit display name. When omitted, the
        /// framework auto-resolves from the peer's last-observed
        /// mDNS-SD advert.
        display_name: Option<String>,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Revoke a device's domain admission (soft revoke —
    /// the row is retained, `revoked_at_ms` is set).
    /// Capability-gated by `plugins_admin` and step-up-aware.
    /// **Superseded by `Discard`** — new code should use
    /// the irreversible chain-substrate discard.
    Revoke {
        /// Canonical device id of the peer being revoked.
        device_id: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Discard a device from the domain. Operator-explicit,
    /// irreversible — appends a signed entry to the domain
    /// witness chain. Capability-gated by `plugins_admin`
    /// and step-up-aware.
    Discard {
        /// Canonical device id of the peer being discarded.
        device_id: String,
        /// Optional operator-supplied rationale, retained
        /// in the chain entry as audit material.
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Read the current domain witness chain head hash +
    /// chain length. The freshness oracle for chain-
    /// substrate consumers. Read-only.
    ChainHead {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Replay recent domain witness chain entries in
    /// chronological order. Read-only.
    History {
        /// Maximum number of recent entries to return.
        /// Default 50; clamped to [1, 1000].
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Fire an operator-gestured reconnect storm against an
    /// absent peer. Every available carrier is tried in
    /// parallel until one responds or the storm window
    /// expires. Capability-gated by `plugins_admin` and
    /// step-up-aware.
    Reconnect {
        /// Canonical device id of the peer to reconnect.
        device_id: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Found a new multi-room domain on this device. Signs the
    /// genesis chain entry (self-admit) so the local device
    /// becomes the first member of a new chain. Refuses when
    /// the chain already contains any entry — re-founding
    /// requires `leave` or `factory-reset` first. Capability-
    /// gated by `plugins_admin` and step-up-aware.
    Bootstrap {
        /// Optional operator-supplied display name for the
        /// founder admission. When unset, the framework uses
        /// the device identity record's display name.
        #[arg(long, value_name = "TEXT")]
        display_name: Option<String>,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Mark the local device as looking to join an existing
    /// domain. With `--endpoint`, the device dials the named
    /// peer directly and requests the chain tail; without, the
    /// device waits for announces. Refuses when the chain
    /// already contains any entry. Capability-gated by
    /// `plugins_admin`.
    Join {
        /// Optional `host:port` of an admitted peer to dial
        /// directly. When omitted, the device waits for
        /// announce-driven discovery.
        #[arg(long, value_name = "HOST:PORT")]
        endpoint: Option<String>,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Soft-leave the current domain. Resets the chain log
    /// and projection cache in-process so the device becomes
    /// domain-less. The per-device signing key persists so a
    /// subsequent `join` or `bootstrap` reuses the same
    /// identity. Capability-gated by `plugins_admin` and
    /// step-up-aware.
    Leave {
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Hard-reset the device's domain state. Resets the chain
    /// log in-process AND removes the per-device signing key
    /// on disk so the device returns to a fresh-from-factory
    /// posture; a steward restart materialises a fresh
    /// signing key on next boot. Capability-gated by
    /// `plugins_admin` and step-up-aware.
    FactoryReset {
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

#[derive(Subcommand)]
enum AdminDeviceSub {
    /// Persistent device identity surface — show the canonical
    /// id + display name + optional vendor + creation timestamp,
    /// or rename the operator-editable display name.
    Identity {
        #[command(subcommand)]
        sub: AdminDeviceIdentitySub,
    },
    /// Multi-room peer discovery surface — list peers
    /// currently observed by the mDNS-SD discovery runtime.
    Peers {
        #[command(subcommand)]
        sub: AdminDevicePeersSub,
    },
}

#[derive(Subcommand)]
enum AdminDevicePeersSub {
    /// List multi-room peers currently observed on the local
    /// broadcast domain. Capability-gated by `plugins_admin`.
    List {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

#[derive(Subcommand)]
enum AdminDeviceIdentitySub {
    /// Read the device identity record. Capability-gated by
    /// `plugins_admin`.
    Show {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Rename the operator-editable display name. The canonical
    /// device id is immutable. Capability-gated by
    /// `plugins_admin` and step-up-aware.
    Rename {
        /// New display name. Trimmed; must be non-empty after
        /// trim and at most 128 chars.
        new_name: String,
        /// Step-up authentication token; see `Enable` for gate
        /// semantics.
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Reset the display name to its default — re-seeded from
    /// the OS hostname when sane, or the `evo-<short>` fallback
    /// otherwise. Clears any prior operator override;
    /// `name_source` returns to `Auto` so the collision resolver
    /// is once again free to rewrite. Capability-gated by
    /// `plugins_admin` and step-up-aware.
    Reset {
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

#[derive(Subcommand)]
enum AdminWardenSub {
    /// One-shot bundled custody flow: take custody, issue a
    /// single course-correction, release custody. Exit code is
    /// the course-correction's outcome — 0 on accepted, non-zero
    /// on the framework's structured refusal (e.g. verb not in
    /// `course_correct_verbs`). Take and release are best-effort
    /// (logged on failure but do not affect exit code).
    CourseCorrect {
        /// Shelf the warden is admitted on (e.g. `playback.mpd`).
        #[arg(long, value_name = "SHELF")]
        shelf: String,
        /// Custody type discriminator declared by the shelf shape.
        #[arg(long, value_name = "TYPE")]
        custody_type: String,
        /// Verb name. Refused if not in the warden's
        /// `capabilities.warden.course_correct_verbs`.
        #[arg(long, value_name = "VERB")]
        verb: String,
        /// Optional base64 payload for take_custody. Default
        /// empty.
        #[arg(long, value_name = "B64", default_value = "")]
        custody_payload_b64: String,
        /// Optional base64 payload for course_correct. Default
        /// empty.
        #[arg(long, value_name = "B64", default_value = "")]
        payload_b64: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Take custody on a warden and print the resulting handle
    /// id + started_at_ms to stdout (one `key=value` per line)
    /// so a script can capture them and feed them to a follow-up
    /// `fast-path-dispatch` or `release-custody` call. The
    /// bundled `course-correct` verb hides the handle inside
    /// one round-trip; this verb exposes it for the held-handle
    /// flows fast-path and multi-step custody scenarios need.
    TakeCustody {
        /// Shelf the warden is admitted on.
        #[arg(long, value_name = "SHELF")]
        shelf: String,
        /// Custody type discriminator declared by the shelf shape.
        #[arg(long, value_name = "TYPE")]
        custody_type: String,
        /// Optional base64 payload for take_custody. Default
        /// empty.
        #[arg(long, value_name = "B64", default_value = "")]
        payload_b64: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Release a previously-taken custody by handle id +
    /// started_at_ms. Counterpart to `take-custody`. The
    /// bundled `course-correct` verb releases automatically;
    /// scripts driving the held-handle flow use this verb to
    /// release at the chosen point.
    ReleaseCustody {
        /// Shelf the warden is admitted on.
        #[arg(long, value_name = "SHELF")]
        shelf: String,
        /// Custody handle id (from a prior take_custody).
        #[arg(long, value_name = "ID")]
        handle_id: String,
        /// Custody handle started_at, milliseconds since UNIX
        /// epoch (from a prior take_custody response).
        #[arg(long, value_name = "MS")]
        handle_started_at_ms: u64,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Fast-path dispatch against an open custody. Drives the
    /// steward's `/run/evo/fast.sock` channel directly with a
    /// length-prefixed CBOR `FastPathRequest::Dispatch`. The
    /// warden's manifest `fast_path_verbs` gate refuses verbs
    /// not in the declared set; the budget gate refuses calls
    /// exceeding `fast_path_budget_ms`. Distinct from
    /// `course-correct` (slow path): this is the latency-bounded
    /// channel.
    FastPathDispatch {
        /// Shelf the warden is admitted on.
        #[arg(long, value_name = "SHELF")]
        shelf: String,
        /// Verb name. Must be in the warden's
        /// `capabilities.warden.fast_path_verbs`.
        #[arg(long, value_name = "VERB")]
        verb: String,
        /// Custody handle id (from a prior take_custody).
        #[arg(long, value_name = "ID")]
        handle_id: String,
        /// Custody handle started_at, milliseconds since UNIX
        /// epoch (from a prior take_custody response).
        #[arg(long, value_name = "MS")]
        handle_started_at_ms: u64,
        /// Optional base64 payload. Default empty.
        #[arg(long, value_name = "B64", default_value = "")]
        payload_b64: String,
        /// Optional per-frame deadline override in ms.
        #[arg(long, value_name = "MS")]
        deadline_ms: Option<u32>,
        /// Path to the steward's Fast Path socket.
        #[arg(long, value_name = "PATH", default_value = "/run/evo/fast.sock")]
        socket: PathBuf,
    },
}

#[derive(Subcommand)]
enum AdminPromptSub {
    /// List every open user-interaction prompt currently held
    /// by the steward's prompt ledger. One line per prompt.
    List {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Answer an open prompt with a `Text` response. Sub-shells
    /// for non-text prompt types (Select / Confirm / MultiField)
    /// land alongside the scenarios that need them.
    AnswerText {
        /// Canonical name of the plugin that issued the prompt.
        #[arg(long, value_name = "NAME")]
        plugin: String,
        /// Plugin-chosen prompt id.
        #[arg(long, value_name = "ID")]
        prompt_id: String,
        /// The text value to send back as the response.
        #[arg(long, value_name = "TEXT")]
        value: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Cancel an open prompt. The plugin's awaiting future
    /// resolves with `PromptOutcome::Cancelled { by: Consumer }`.
    Cancel {
        /// Canonical name of the plugin that issued the prompt.
        #[arg(long, value_name = "NAME")]
        plugin: String,
        /// Plugin-chosen prompt id.
        #[arg(long, value_name = "ID")]
        prompt_id: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

#[derive(Subcommand)]
enum AdminGrammarSub {
    /// Read-only enumeration of every row in
    /// `pending_grammar_orphans`. One row per orphaned
    /// `subject_type` with its current state.
    List {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Plan a migration without mutating state. Equivalent to
    /// `migrate --dry-run` but rendered as a human-readable
    /// plan including target-type breakdown and first/last
    /// sample IDs.
    Plan {
        /// Orphaned `subject_type` to migrate.
        #[arg(long)]
        from_type: String,
        /// Post-migration `subject_type` (Rename strategy).
        #[arg(long)]
        to_type: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Migrate every orphan of `from_type` to `to_type` per
    /// the Rename strategy. Foreground; the call returns when
    /// every batch has committed. Per-batch boundary is
    /// configurable via `--batch-size`; the per-call cap via
    /// `--max-subjects`.
    Migrate {
        /// Orphaned `subject_type` to migrate.
        #[arg(long)]
        from_type: String,
        /// Post-migration `subject_type`.
        #[arg(long)]
        to_type: String,
        /// Operator-supplied reason recorded with the migration.
        #[arg(long)]
        reason: Option<String>,
        /// Per-batch transaction boundary.
        #[arg(long)]
        batch_size: Option<u32>,
        /// Cap subjects per call. Subsequent calls resume.
        #[arg(long)]
        max_subjects: Option<u32>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Record the deliberate decision to leave the orphans of
    /// a type un-migrated. Suppresses the boot-time diagnostic
    /// warning while the row stays `accepted`.
    Accept {
        /// Orphaned `subject_type` to accept.
        #[arg(long)]
        from_type: String,
        /// Operator-supplied reason for accepting.
        #[arg(long)]
        reason: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

#[derive(Subcommand)]
enum AdminFlightSub {
    /// List every shelf in the `flight_mode` rack and its
    /// current state (one `flight_mode.query` per shelf).
    List {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Set one class's flight-mode state.
    /// Class is the shelf name within the `flight_mode` rack
    /// (e.g. `wireless.bluetooth`); the tool prepends the rack
    /// name automatically when forming the wire request.
    Set {
        /// Class identifier (shelf name within `flight_mode`).
        class: String,
        /// `on` to activate flight mode (radio off); `off` to
        /// clear (radio on).
        state: AdminFlightState,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Set every class's flight-mode state in catalogue order.
    /// Per-class failures are reported but do not abort the walk;
    /// the operator can re-run for restartable bulk control.
    All {
        /// `on` to activate flight mode on every class; `off`
        /// to clear.
        state: AdminFlightState,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

/// Two-state argument for [`AdminFlightSub::Set`] and
/// [`AdminFlightSub::All`]. Lifted out of the parent enum so
/// clap renders it as a positional `on`/`off` value rather than
/// a flag.
#[derive(Clone, Copy, clap::ValueEnum)]
enum AdminFlightState {
    /// Flight mode active (radio off).
    On,
    /// Flight mode cleared (radio on).
    Off,
}

impl AdminFlightState {
    fn as_bool(self) -> bool {
        matches!(self, Self::On)
    }
}

#[derive(Subcommand)]
enum AdminReconcileSub {
    /// Read-only enumeration of every active reconciliation
    /// pair: pair id, composer / warden shelves, generation,
    /// last applied wall-clock millisecond timestamp.
    List {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Read-only single-pair projection: generation + warden-
    /// emitted applied state.
    Project {
        /// Operator-visible pair identifier.
        #[arg(long, value_name = "ID")]
        pair: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Operator-issued manual trigger: bypass the per-pair
    /// debounce window and run one compose-and-apply cycle
    /// immediately. Requires `reconciliation_admin`.
    Now {
        /// Operator-visible pair identifier.
        #[arg(long, value_name = "ID")]
        pair: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

#[derive(Subcommand)]
enum AdminReloadSub {
    /// Reload the catalogue. Source defaults to inline-from-stdin
    /// when neither `--inline` nor `--path` is supplied.
    Catalogue {
        /// Inline TOML body. Mutually exclusive with `--path`.
        #[arg(long, value_name = "TOML")]
        inline: Option<String>,
        /// Path to the catalogue TOML to load. Mutually
        /// exclusive with `--inline`.
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,
        /// Validate only; do not mutate.
        #[arg(long)]
        dry_run: bool,
        /// Step-up authentication token; see `admin enable` for
        /// gate semantics.
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Reload one plugin's manifest. Source defaults to
    /// inline-from-stdin when neither `--inline` nor `--path`
    /// is supplied.
    Manifest {
        /// Canonical plugin name.
        #[arg(long, value_name = "NAME")]
        plugin: String,
        #[arg(long, value_name = "TOML")]
        inline: Option<String>,
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
        /// Step-up authentication token; see `admin enable` for
        /// gate semantics.
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

/// Sub-subcommands for `admin policy`.
#[derive(Subcommand)]
enum AdminPolicySub {
    /// List recorded admission policies (metadata only).
    List {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Print one policy by id (full body).
    Get {
        policy_id: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Upsert one policy from a TOML document. Pass `-` to
    /// read from stdin.
    Put {
        path: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Delete one policy by id.
    Delete {
        policy_id: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Activate one policy (or `--clear` to clear).
    Activate {
        #[arg(long, value_name = "ID")]
        policy_id: Option<String>,
        #[arg(long)]
        clear: bool,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Walk every known plugin and report per-plugin
    /// violations against the policy without transitioning
    /// any plugin.
    Audit {
        policy_id: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

/// Sub-subcommands for `admin capability`.
#[derive(Subcommand)]
enum AdminCapabilitySub {
    /// Revoke one capability on one plugin. Idempotent on the
    /// `(plugin, capability)` pair — re-revoking advances the
    /// recorded principal / reason without duplicating the row.
    Revoke {
        /// Plugin canonical name.
        plugin_name: String,
        /// Capability token (e.g. `outbound_network`,
        /// `filesystem_unrestricted`, `appointments`, `watches`).
        capability: String,
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Un-revoke a previously-recorded `(plugin, capability)`
    /// revocation. Idempotent on absent pairs.
    Unrevoke {
        plugin_name: String,
        capability: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// List every revocation recorded against one plugin.
    List {
        plugin_name: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// List every recorded revocation across all plugins.
    ListAll {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

/// Sub-subcommands for `admin audio`.
#[derive(Subcommand)]
enum AdminAudioSub {
    /// Operator-authored hardware-profile overrides per
    /// delivery target. Sub-subcommands: `put` (TOML
    /// document), `get <key>`, `list`, `clear <key>`.
    ProfileOverride {
        #[command(subcommand)]
        sub: AdminAudioProfileOverrideSub,
    },
    /// Operator-authored audio policy per delivery target —
    /// Auto / StrictBitPerfect / Pinned. Sub-subcommands:
    /// `set`, `get`, `list`, `clear`. Framework default when
    /// no row exists for a target: Auto.
    Policy {
        #[command(subcommand)]
        sub: AdminAudioPolicySub,
    },
    /// Operator-authored volume mode per delivery target —
    /// `software` / `hardware` / `none`. Sub-subcommands:
    /// `set`, `get`, `list`, `clear`. Framework default when
    /// no row exists for a target: `software`.
    VolumeMode {
        #[command(subcommand)]
        sub: AdminAudioVolumeModeSub,
    },
    /// Active audio topology snapshots per delivery target.
    /// The framework owns the publish primitive plus
    /// persistence and propagation; the vendor distribution
    /// drives the chain decision and pushes complete snapshots
    /// through `publish`. `get` / `list` are read-only;
    /// `clear` removes a target's snapshot and clears the
    /// per-stage AudioRouting handles.
    Topology {
        #[command(subcommand)]
        sub: AdminAudioTopologySub,
    },
}

/// Sub-sub-subcommands for `admin audio topology`.
#[derive(Subcommand)]
enum AdminAudioTopologySub {
    /// Publish an active audio topology snapshot for one
    /// delivery target. The TOML document carries the typed
    /// `ActiveAudioTopology` shape (target_key, display_name,
    /// chain array, volume, score, warnings). Pass `-` to
    /// read from stdin.
    Publish {
        path: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Read the active topology for one delivery target.
    Get {
        target_key: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// List every published active topology.
    List {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Clear the active topology for one delivery target.
    /// Idempotent on absent targets.
    Clear {
        target_key: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Render the active topology for one delivery target (or
    /// every published topology when `target_key` is omitted)
    /// in operator-readable form — chain stages with format
    /// at each stage, volume + bit-perfect verdict + score
    /// breakdown + warnings. The `get` and `list` variants
    /// dump the same data as raw JSON; `show` is the
    /// human-consumption rendering.
    Show {
        target_key: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

/// Sub-sub-subcommands for `admin audio policy`.
#[derive(Subcommand)]
enum AdminAudioPolicySub {
    /// Set the operator policy for one delivery target. The
    /// `--policy-json` argument carries the typed
    /// `OperatorPolicy` JSON shape (`{"kind":"auto"}`,
    /// `{"kind":"strict_bit_perfect"}`, or
    /// `{"kind":"pinned","source_plugin":"...","delivery_plugin":"...","composition_plugin":"..."}`).
    Set {
        target_key: String,
        #[arg(long, value_name = "JSON")]
        policy_json: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Read the operator policy for one delivery target.
    Get {
        target_key: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// List every recorded operator policy.
    List {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Clear the operator policy for one delivery target
    /// (reverts to the framework default `Auto`).
    Clear {
        target_key: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

/// Sub-sub-subcommands for `admin audio volume-mode`.
#[derive(Subcommand)]
enum AdminAudioVolumeModeSub {
    /// Set the volume mode for one delivery target. `mode` is
    /// one of `software` / `hardware` / `none`.
    Set {
        target_key: String,
        mode: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Read the volume mode for one delivery target.
    Get {
        target_key: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// List every recorded volume mode.
    List {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Clear the volume mode for one delivery target (reverts
    /// to the framework default `software`).
    Clear {
        target_key: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

/// Sub-sub-subcommands for `admin audio profile-override`.
#[derive(Subcommand)]
enum AdminAudioProfileOverrideSub {
    /// Record an override. The TOML document carries the
    /// `[identity]` and `[override]` tables. Pass `-` to read
    /// from stdin. Refuses an empty `[override]` table.
    Put {
        /// Source path of the override TOML, or `-` for
        /// stdin.
        path: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Read one override by canonical identity key
    /// (`usb:vid=...,pid=...` / `hat:<sig>` / `hdmi:<sink>` /
    /// `alsa:<card>`).
    Get {
        key: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// List every recorded override.
    List {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Clear one override by canonical identity key.
    Clear {
        key: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

/// Sub-subcommands for `admin migration`.
#[derive(Subcommand)]
enum AdminMigrationSub {
    /// Export the device's configuration bundle as a TOML
    /// document. Pass `-` for stdout, otherwise a target file
    /// path. Read-only.
    Export {
        /// Target path for the exported bundle, or `-` for
        /// stdout.
        path: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Import a configuration bundle from a TOML document. Pass
    /// `-` to read from stdin. Replaces every covered section of
    /// the substrate atomically.
    Import {
        /// Source path of the bundle to import, or `-` for
        /// stdin.
        path: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

/// Sub-subcommands for `admin bulk`.
#[derive(Subcommand)]
#[allow(clippy::enum_variant_names)]
enum AdminBulkSub {
    /// Read-only preview: list every plugin matching the filter.
    ListWhere {
        /// JSON filter expression. See `PluginFilter` enum.
        #[arg(long, value_name = "JSON")]
        filter_json: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Disable every plugin matching the filter.
    DisableWhere {
        #[arg(long, value_name = "JSON")]
        filter_json: String,
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Enable every plugin matching the filter.
    EnableWhere {
        #[arg(long, value_name = "JSON")]
        filter_json: String,
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

/// Sub-subcommands for `admin tag`.
#[derive(Subcommand)]
enum AdminTagSub {
    /// Add a tag to a plugin.
    Add {
        plugin_name: String,
        tag: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Remove a tag from a plugin.
    Remove {
        plugin_name: String,
        tag: String,
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// List every tag applied to a plugin.
    List {
        plugin_name: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

/// Sub-subcommands for `admin profile`.
#[derive(Subcommand)]
enum AdminProfileSub {
    /// List recorded profiles (metadata only).
    List {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Print one profile (with its full entry list).
    Get {
        /// Profile id to fetch.
        profile_id: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Upsert one profile from a TOML document. Pass `-` to
    /// read from stdin.
    Put {
        /// Path to the TOML profile document, or `-` for stdin.
        path: String,
        /// Step-up authentication token.
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Delete one profile by id.
    Delete {
        profile_id: String,
        /// Step-up authentication token.
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Activate a profile (or pass `--clear` to clear the
    /// active flag without activating another).
    Activate {
        /// Profile id to activate. Mutually exclusive with
        /// `--clear`.
        #[arg(long, value_name = "ID")]
        profile_id: Option<String>,
        /// Clear the active profile (no profile becomes
        /// active afterward).
        #[arg(long)]
        clear: bool,
        /// Compute and print the per-plugin transition plan
        /// without dispatching.
        #[arg(long)]
        dry_run: bool,
        /// Step-up authentication token.
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

/// Sub-subcommands for `admin update-channel`.
#[derive(Subcommand)]
enum AdminUpdateChannelSub {
    /// Set the update-channel preference for one target.
    /// Capability-gated by `plugins_admin` and step-up-aware.
    Set {
        /// Update target (`core` or `plugins`).
        #[arg(long, value_name = "TARGET")]
        target: String,
        /// Channel (`alpha`, `test`, or `production`).
        #[arg(long, value_name = "CHANNEL")]
        channel: String,
        /// Step-up authentication token; see `admin enable` for
        /// gate semantics.
        #[arg(long, value_name = "TOKEN")]
        step_up_token: Option<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// List recorded update-channel preferences. Read-only.
    List {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

/// Sub-subcommands for `admin step-up`.
#[derive(Subcommand)]
enum AdminStepUpSub {
    /// Verify the operator's credentials and obtain a privileged
    /// session token. Reads the password from /dev/tty without
    /// echo. The token is printed to stdout on success.
    Verify {
        /// Username to verify.
        username: String,
        /// Optional TTL override in seconds. Capped at the
        /// framework ceiling regardless of the requested value.
        #[arg(long, value_name = "SECS")]
        ttl_seconds: Option<u64>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Revoke a previously-issued step-up token. Idempotent —
    /// revoking an unknown / already-expired token reports
    /// `revoked = false` without erroring.
    Revoke {
        /// Token to revoke.
        token: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

/// Sub-subcommands for `admin auth`.
#[derive(Subcommand)]
enum AdminAuthSub {
    /// Mint a fresh long-lived operator bearer token through the
    /// steward's per-device signing key. The encoded token is
    /// printed to stdout on success; treat it as sensitive
    /// (anyone in possession can act as the operator until
    /// expiry). The SSH-required developer / recovery path of
    /// the trust substrate; the consumer-grade flow mints
    /// tokens through the first-boot wizard ceremony instead
    /// in a follow-on release.
    MintBearerToken {
        /// Operator-supplied free-form reason recorded in the
        /// audit ledger. Must be non-empty after trim. The
        /// framework refuses blank-reason mints by design;
        /// reviewers correlating audit entries across surfaces
        /// rely on this field.
        #[arg(long, value_name = "TEXT")]
        reason: String,
        /// Optional TTL override in seconds. Capped at the
        /// framework `MAX_TOKEN_TTL_MS` ceiling regardless of
        /// the requested value. Omit to use the framework
        /// default (`DEFAULT_TOKEN_TTL_MS`).
        #[arg(long, value_name = "SECS")]
        ttl_seconds: Option<u64>,
        /// Narrow the issued token's capability set to only the
        /// listed scope names. Repeatable; each `--scope NAME`
        /// keeps the matching capability from the operator
        /// bootstrap set (with its bootstrap-declared rank
        /// preserved). A scope name that does not appear in the
        /// bootstrap set is silently dropped; if the resulting
        /// intersection is empty the framework refuses the mint
        /// with `empty_capability_intersection`. Absent means
        /// today's full bootstrap set (unchanged pre-scoping
        /// behaviour).
        ///
        /// Example: `--scope user_interaction_responder` for a
        /// browser session that only needs to answer plugin-
        /// initiated prompts and nothing else.
        #[arg(long = "scope", value_name = "NAME")]
        scopes: Vec<String>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Per-credential mint: create a named bearer credential
    /// with operator-set scopes and operator-set expiry
    /// policy. The credential record is persisted to the
    /// operator inventory so it shows up in
    /// `list-bearer-tokens` and can be revoked through
    /// `revoke-bearer-token`.
    CreateBearerToken {
        /// Operator-friendly label, e.g. `home-assistant` or
        /// `esp8266-doorbell`. Surfaces in the inventory.
        #[arg(long, value_name = "TEXT")]
        name: String,
        /// Free-form reason recorded in the audit observation.
        #[arg(long, value_name = "TEXT")]
        reason: String,
        /// Repeatable `<kind>:<scope>` flag — e.g.
        /// `--scope read:audio --scope write:multiroom`.
        /// Omit to grant the full operator scope set.
        #[arg(long = "scope", value_name = "KIND:SCOPE")]
        scopes: Vec<String>,
        /// Expiry in seconds from creation. Omit (or pass
        /// `0`) for `Never` — the default for IoT-platform
        /// consumers.
        #[arg(long, value_name = "SECS")]
        expires_in_seconds: Option<u64>,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// List the operator-managed bearer credential
    /// inventory. Returns metadata only — token bytes are
    /// never persisted, never returned post-mint.
    ListBearerTokens {
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Revoke a previously-minted bearer credential by token
    /// id. Revocation persists across steward restarts.
    RevokeBearerToken {
        /// The token id surfaced by `list-bearer-tokens`.
        #[arg(long, value_name = "ID")]
        token_id: String,
        /// Free-form reason recorded in the audit observation.
        #[arg(long, value_name = "TEXT")]
        reason: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Recovery gesture: purge every credential record + every
    /// revocation entry and flip the steward to Open tier on
    /// next boot. Used when the operator is locked out of the
    /// device (lost token, every credential expired).
    /// Requires step_up:system_admin.
    ResetCredentialsToOpen {
        /// Free-form reason recorded in the audit observation.
        #[arg(long, value_name = "TEXT")]
        reason: String,
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
}

#[derive(Subcommand)]
enum CatalogueSub {
    /// Parse and validate a catalogue document. Surfaces parser
    /// errors (missing required fields, schema_version out of range)
    /// as a non-zero exit. With `--schema-version N` additionally
    /// pins the document's `schema_version` to N exactly.
    Lint {
        /// Path to the catalogue TOML document.
        path: PathBuf,
        /// If set, additionally require the document to declare
        /// `schema_version = <N>`. Useful at distribution-author
        /// time to catch a fixture-update slip-through.
        #[arg(long, value_name = "N")]
        schema_version: Option<u32>,
    },
    /// Validate every per-shelf schema file (`<rack>/<shelf>.v<N>.toml`)
    /// under a schemas tree. Resolves the path via a cascade:
    /// `--schemas-path` flag, `$EVO_SCHEMAS_DIR`, the
    /// distribution-installed `/usr/share/evo-catalogue-schemas/`.
    /// Walks the tree, parses every `*.toml` it finds, and
    /// reports per-file pass/fail with a final aggregate count.
    /// Non-zero exit when any file fails.
    ValidateShelfSchema {
        /// Override the resolution cascade and use this path
        /// directly.
        #[arg(long, value_name = "PATH")]
        schemas_path: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum PackFormatArg {
    TarGz,
    TarXz,
    Zip,
}

impl From<PackFormatArg> for archive::PackFormat {
    fn from(v: PackFormatArg) -> Self {
        match v {
            PackFormatArg::TarGz => Self::TarGz,
            PackFormatArg::TarXz => Self::TarXz,
            PackFormatArg::Zip => Self::Zip,
        }
    }
}

fn run(cli: Cli) -> Result<(), anyhow::Error> {
    match cli.sub {
        Sub::Lint { plugin_dir } => lint::run(&plugin_dir),
        Sub::Catalogue { sub } => match sub {
            CatalogueSub::Lint {
                path,
                schema_version,
            } => catalogue_cmd::lint(&path, schema_version),
            CatalogueSub::ValidateShelfSchema { schemas_path } => {
                shelf_schema::validate(schemas_path.as_deref())
            }
        },
        Sub::Sign { plugin_dir, key } => sign::run(&plugin_dir, &key),
        Sub::Verify {
            plugin_dir,
            allow_unsigned,
            strict_trust,
            trust_dir_opt,
            trust_dir_etc,
            revocations,
            describe_json,
        } => verify_cmd::run(
            &plugin_dir,
            &verify_cmd::VerifyArgs {
                allow_unsigned,
                degrade_trust: !strict_trust,
                trust_dir_opt,
                trust_dir_etc,
                revocations_path: revocations,
                describe_json,
            },
        ),
        Sub::Pack {
            plugin_dir,
            out,
            format,
        } => pack_cmd::run(&plugin_dir, out.as_deref(), format.map(Into::into)),
        Sub::Install {
            source,
            to,
            allow_unsigned,
            strict_trust,
            trust_dir_opt,
            trust_dir_etc,
            revocations,
            chown,
            max_url_bytes,
        } => install::run(
            &source,
            &to,
            &verify_cmd::VerifyArgs {
                allow_unsigned,
                degrade_trust: !strict_trust,
                trust_dir_opt,
                trust_dir_etc,
                revocations_path: revocations,
                // The install path does not have access to a
                // pre-extracted describe() JSON; drift checking
                // happens at admit time on the device. Plugin
                // authors run `verify --describe-json` directly
                // in their build pipeline.
                describe_json: None,
            },
            chown.as_deref(),
            max_url_bytes,
        ),
        Sub::Admin { sub } => run_admin(sub),
        Sub::Privileges { sub } => match sub {
            PrivilegesSub::Describe { path, format } => {
                privileges_cmd::describe(&path, format)
            }
            PrivilegesSub::Check {
                path,
                schema_only,
                skip_verification,
                strict,
                distro,
            } => privileges_cmd::check(
                &path,
                schema_only,
                skip_verification,
                strict,
                privileges_cmd::resolve_distro(distro),
            ),
            PrivilegesSub::Template { archetype, out } => {
                privileges_cmd::template(archetype, out.as_deref())
            }
        },
        Sub::Plan { sub } => run_plan(sub),
    }
}

fn run_plan(sub: PlanSub) -> Result<(), anyhow::Error> {
    match sub {
        PlanSub::Fire { plan_id, socket } => {
            admin::fire_plan(&socket, &plan_id)
        }
    }
}

fn run_admin(sub: AdminSub) -> Result<(), anyhow::Error> {
    match sub {
        AdminSub::Enable {
            plugin,
            reason,
            step_up_token,
            socket,
        } => admin::enable(
            &socket,
            &plugin,
            reason.as_deref(),
            step_up_token.as_deref(),
        ),
        AdminSub::Disable {
            plugin,
            reason,
            step_up_token,
            cascade_dependents,
            socket,
        } => admin::disable(
            &socket,
            &plugin,
            reason.as_deref(),
            step_up_token.as_deref(),
            cascade_dependents,
        ),
        AdminSub::Dependents { plugin, socket } => {
            admin::preview_dependents(&socket, &plugin)
        }
        AdminSub::Health { socket } => admin::health_get(&socket),
        AdminSub::Uninstall {
            plugin,
            reason,
            purge_state,
            step_up_token,
            socket,
        } => admin::uninstall(
            &socket,
            &plugin,
            reason.as_deref(),
            purge_state,
            step_up_token.as_deref(),
        ),
        AdminSub::PurgeState {
            plugin,
            step_up_token,
            socket,
        } => admin::purge_state(&socket, &plugin, step_up_token.as_deref()),
        AdminSub::Diagnose { plugin, socket } => {
            admin::diagnose(&socket, &plugin)
        }
        AdminSub::DescribeCapabilities { socket } => {
            admin::describe_capabilities(&socket)
        }
        AdminSub::DescribeUiStockings { shelf, socket } => {
            admin::describe_ui_stockings(&socket, shelf.as_deref())
        }
        AdminSub::ActivateTheme {
            plugin,
            clear,
            step_up_token,
            socket,
        } => admin::activate_theme(
            &socket,
            plugin.as_deref(),
            clear,
            step_up_token.as_deref(),
        ),
        AdminSub::ActivateUiShell {
            plugin,
            clear,
            step_up_token,
            socket,
        } => admin::activate_ui_shell(
            &socket,
            plugin.as_deref(),
            clear,
            step_up_token.as_deref(),
        ),
        AdminSub::DescribeActiveUiSelection { socket } => {
            admin::describe_active_ui_selection(&socket)
        }
        AdminSub::SubscribeHappenings {
            max_count,
            duration_secs,
            since,
            variants,
            plugins,
            shelves,
            coalesce_labels,
            coalesce_window_ms,
            socket,
        } => admin::subscribe_happenings(
            &socket,
            max_count,
            duration_secs,
            since,
            variants.as_deref(),
            plugins.as_deref(),
            shelves.as_deref(),
            coalesce_labels.as_deref(),
            coalesce_window_ms,
        ),
        AdminSub::ReloadPlugin {
            plugin,
            step_up_token,
            socket,
        } => admin::reload_plugin(&socket, &plugin, step_up_token.as_deref()),
        AdminSub::PluginReload {
            plugin,
            step_up_token,
            socket,
        } => admin::plugin_reload_coord(
            &socket,
            &plugin,
            step_up_token.as_deref(),
        ),
        AdminSub::PluginRestore {
            plugin,
            step_up_token,
            socket,
        } => admin::plugin_restore(&socket, &plugin, step_up_token.as_deref()),
        AdminSub::StepUp { sub } => match sub {
            AdminStepUpSub::Verify {
                username,
                ttl_seconds,
                socket,
            } => admin::step_up_verify(&socket, &username, ttl_seconds),
            AdminStepUpSub::Revoke { token, socket } => {
                admin::step_up_revoke(&socket, &token)
            }
        },
        AdminSub::Auth { sub } => match sub {
            AdminAuthSub::MintBearerToken {
                reason,
                ttl_seconds,
                scopes,
                socket,
            } => {
                admin::mint_bearer_token(&socket, &reason, ttl_seconds, scopes)
            }
            AdminAuthSub::CreateBearerToken {
                name,
                reason,
                scopes,
                expires_in_seconds,
                socket,
            } => admin::create_bearer_token(
                &socket,
                &name,
                &reason,
                &scopes,
                expires_in_seconds,
            ),
            AdminAuthSub::ListBearerTokens { socket } => {
                admin::list_bearer_tokens(&socket)
            }
            AdminAuthSub::RevokeBearerToken {
                token_id,
                reason,
                socket,
            } => admin::revoke_bearer_token(&socket, &token_id, &reason),
            AdminAuthSub::ResetCredentialsToOpen { reason, socket } => {
                admin::reset_credentials_to_open(&socket, &reason)
            }
        },
        AdminSub::UpdateChannel { sub } => match sub {
            AdminUpdateChannelSub::Set {
                target,
                channel,
                step_up_token,
                socket,
            } => admin::update_channel_set(
                &socket,
                &target,
                &channel,
                step_up_token.as_deref(),
            ),
            AdminUpdateChannelSub::List { socket } => {
                admin::update_channel_list(&socket)
            }
        },
        AdminSub::Bulk { sub } => match sub {
            AdminBulkSub::ListWhere {
                filter_json,
                socket,
            } => admin::bulk_list_where(&socket, &filter_json),
            AdminBulkSub::DisableWhere {
                filter_json,
                reason,
                step_up_token,
                socket,
            } => admin::bulk_disable_where(
                &socket,
                &filter_json,
                reason.as_deref(),
                step_up_token.as_deref(),
            ),
            AdminBulkSub::EnableWhere {
                filter_json,
                reason,
                step_up_token,
                socket,
            } => admin::bulk_enable_where(
                &socket,
                &filter_json,
                reason.as_deref(),
                step_up_token.as_deref(),
            ),
        },
        AdminSub::Policy { sub } => match sub {
            AdminPolicySub::List { socket } => admin::policy_list(&socket),
            AdminPolicySub::Get { policy_id, socket } => {
                admin::policy_get(&socket, &policy_id)
            }
            AdminPolicySub::Put {
                path,
                step_up_token,
                socket,
            } => admin::policy_put(&socket, &path, step_up_token.as_deref()),
            AdminPolicySub::Delete {
                policy_id,
                step_up_token,
                socket,
            } => admin::policy_delete(
                &socket,
                &policy_id,
                step_up_token.as_deref(),
            ),
            AdminPolicySub::Activate {
                policy_id,
                clear,
                step_up_token,
                socket,
            } => {
                if clear && policy_id.is_some() {
                    return Err(anyhow::anyhow!(
                        "--clear and --policy-id are mutually exclusive"
                    ));
                }
                let id = if clear { None } else { policy_id };
                admin::policy_activate(
                    &socket,
                    id.as_deref(),
                    step_up_token.as_deref(),
                )
            }
            AdminPolicySub::Audit { policy_id, socket } => {
                admin::policy_audit(&socket, &policy_id)
            }
        },
        AdminSub::Capability { sub } => match sub {
            AdminCapabilitySub::Revoke {
                plugin_name,
                capability,
                reason,
                step_up_token,
                socket,
            } => admin::capability_revoke(
                &socket,
                &plugin_name,
                &capability,
                reason.as_deref(),
                step_up_token.as_deref(),
            ),
            AdminCapabilitySub::Unrevoke {
                plugin_name,
                capability,
                step_up_token,
                socket,
            } => admin::capability_unrevoke(
                &socket,
                &plugin_name,
                &capability,
                step_up_token.as_deref(),
            ),
            AdminCapabilitySub::List {
                plugin_name,
                socket,
            } => admin::capability_list_for_plugin(&socket, &plugin_name),
            AdminCapabilitySub::ListAll { socket } => {
                admin::capability_list_all(&socket)
            }
        },
        AdminSub::Migration { sub } => match sub {
            AdminMigrationSub::Export { path, socket } => {
                admin::migration_export(&socket, &path)
            }
            AdminMigrationSub::Import {
                path,
                step_up_token,
                socket,
            } => admin::migration_import(
                &socket,
                &path,
                step_up_token.as_deref(),
            ),
        },
        AdminSub::Audio { sub } => match sub {
            AdminAudioSub::ProfileOverride { sub } => match sub {
                AdminAudioProfileOverrideSub::Put {
                    path,
                    step_up_token,
                    socket,
                } => admin::audio_profile_override_put(
                    &socket,
                    &path,
                    step_up_token.as_deref(),
                ),
                AdminAudioProfileOverrideSub::Get { key, socket } => {
                    admin::audio_profile_override_get(&socket, &key)
                }
                AdminAudioProfileOverrideSub::List { socket } => {
                    admin::audio_profile_override_list(&socket)
                }
                AdminAudioProfileOverrideSub::Clear {
                    key,
                    step_up_token,
                    socket,
                } => admin::audio_profile_override_clear(
                    &socket,
                    &key,
                    step_up_token.as_deref(),
                ),
            },
            AdminAudioSub::Policy { sub } => match sub {
                AdminAudioPolicySub::Set {
                    target_key,
                    policy_json,
                    step_up_token,
                    socket,
                } => admin::audio_policy_set(
                    &socket,
                    &target_key,
                    &policy_json,
                    step_up_token.as_deref(),
                ),
                AdminAudioPolicySub::Get { target_key, socket } => {
                    admin::audio_policy_get(&socket, &target_key)
                }
                AdminAudioPolicySub::List { socket } => {
                    admin::audio_policy_list(&socket)
                }
                AdminAudioPolicySub::Clear {
                    target_key,
                    step_up_token,
                    socket,
                } => admin::audio_policy_clear(
                    &socket,
                    &target_key,
                    step_up_token.as_deref(),
                ),
            },
            AdminAudioSub::VolumeMode { sub } => match sub {
                AdminAudioVolumeModeSub::Set {
                    target_key,
                    mode,
                    step_up_token,
                    socket,
                } => admin::audio_volume_mode_set(
                    &socket,
                    &target_key,
                    &mode,
                    step_up_token.as_deref(),
                ),
                AdminAudioVolumeModeSub::Get { target_key, socket } => {
                    admin::audio_volume_mode_get(&socket, &target_key)
                }
                AdminAudioVolumeModeSub::List { socket } => {
                    admin::audio_volume_mode_list(&socket)
                }
                AdminAudioVolumeModeSub::Clear {
                    target_key,
                    step_up_token,
                    socket,
                } => admin::audio_volume_mode_clear(
                    &socket,
                    &target_key,
                    step_up_token.as_deref(),
                ),
            },
            AdminAudioSub::Topology { sub } => match sub {
                AdminAudioTopologySub::Publish {
                    path,
                    step_up_token,
                    socket,
                } => admin::audio_topology_publish(
                    &socket,
                    &path,
                    step_up_token.as_deref(),
                ),
                AdminAudioTopologySub::Get { target_key, socket } => {
                    admin::audio_topology_get(&socket, &target_key)
                }
                AdminAudioTopologySub::List { socket } => {
                    admin::audio_topology_list(&socket)
                }
                AdminAudioTopologySub::Clear {
                    target_key,
                    step_up_token,
                    socket,
                } => admin::audio_topology_clear(
                    &socket,
                    &target_key,
                    step_up_token.as_deref(),
                ),
                AdminAudioTopologySub::Show { target_key, socket } => {
                    admin::audio_topology_show(&socket, target_key.as_deref())
                }
            },
        },
        AdminSub::Tag { sub } => match sub {
            AdminTagSub::Add {
                plugin_name,
                tag,
                step_up_token,
                socket,
            } => admin::tag_add(
                &socket,
                &plugin_name,
                &tag,
                step_up_token.as_deref(),
            ),
            AdminTagSub::Remove {
                plugin_name,
                tag,
                step_up_token,
                socket,
            } => admin::tag_remove(
                &socket,
                &plugin_name,
                &tag,
                step_up_token.as_deref(),
            ),
            AdminTagSub::List {
                plugin_name,
                socket,
            } => admin::tag_list(&socket, &plugin_name),
        },
        AdminSub::Profile { sub } => match sub {
            AdminProfileSub::List { socket } => admin::profile_list(&socket),
            AdminProfileSub::Get { profile_id, socket } => {
                admin::profile_get(&socket, &profile_id)
            }
            AdminProfileSub::Put {
                path,
                step_up_token,
                socket,
            } => admin::profile_put(&socket, &path, step_up_token.as_deref()),
            AdminProfileSub::Delete {
                profile_id,
                step_up_token,
                socket,
            } => admin::profile_delete(
                &socket,
                &profile_id,
                step_up_token.as_deref(),
            ),
            AdminProfileSub::Activate {
                profile_id,
                clear,
                dry_run,
                step_up_token,
                socket,
            } => {
                if clear && profile_id.is_some() {
                    return Err(anyhow::anyhow!(
                        "--clear and --profile-id are mutually exclusive"
                    ));
                }
                let id = if clear { None } else { profile_id };
                admin::profile_activate(
                    &socket,
                    id.as_deref(),
                    dry_run,
                    step_up_token.as_deref(),
                )
            }
        },
        AdminSub::InstallFromUrl {
            url,
            signature_pin,
            step_up_token,
            socket,
        } => admin::install_plugin_from_url(
            &socket,
            &url,
            signature_pin.as_deref(),
            step_up_token.as_deref(),
        ),
        AdminSub::RegisterRegistry {
            slug,
            manifest_url,
            signature_url,
            public_key_fingerprint,
            poll_interval_secs,
            step_up_token,
            socket,
        } => admin::register_plugin_registry(
            &socket,
            &slug,
            &manifest_url,
            &signature_url,
            &public_key_fingerprint,
            poll_interval_secs,
            step_up_token.as_deref(),
        ),
        AdminSub::UnregisterRegistry {
            slug,
            step_up_token,
            socket,
        } => admin::unregister_plugin_registry(
            &socket,
            &slug,
            step_up_token.as_deref(),
        ),
        AdminSub::ListRegistries { socket } => {
            admin::list_plugin_registries(&socket)
        }
        AdminSub::RefreshRegistry {
            slug,
            step_up_token,
            socket,
        } => admin::refresh_plugin_registry(
            &socket,
            &slug,
            step_up_token.as_deref(),
        ),
        AdminSub::GrantPublisherTrust {
            publisher_id,
            display_name,
            public_key_pem,
            scope_per_plugin,
            step_up_token,
            socket,
        } => {
            let pem_body = std::fs::read_to_string(&public_key_pem)
                .with_context(|| {
                    format!("reading {}", public_key_pem.display())
                })?;
            let scope_vec: Vec<String> = scope_per_plugin
                .map(|s| {
                    s.split(',')
                        .map(|p| p.trim().to_string())
                        .filter(|p| !p.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            admin::grant_publisher_trust(
                &socket,
                &publisher_id,
                &display_name,
                &pem_body,
                if scope_vec.is_empty() {
                    None
                } else {
                    Some(&scope_vec)
                },
                step_up_token.as_deref(),
            )
        }
        AdminSub::RevokePublisherTrust {
            publisher_id,
            step_up_token,
            socket,
        } => admin::revoke_publisher_trust(
            &socket,
            &publisher_id,
            step_up_token.as_deref(),
        ),
        AdminSub::ListPublisherTrust { socket } => {
            admin::list_publisher_trust(&socket)
        }
        AdminSub::Reload { sub } => match sub {
            AdminReloadSub::Catalogue {
                inline,
                path,
                dry_run,
                step_up_token,
                socket,
            } => {
                let source = resolve_reload_source(inline, path)?;
                admin::reload_catalogue(
                    &socket,
                    source,
                    dry_run,
                    step_up_token.as_deref(),
                )
            }
            AdminReloadSub::Manifest {
                plugin,
                inline,
                path,
                dry_run,
                step_up_token,
                socket,
            } => {
                let source = resolve_reload_source(inline, path)?;
                admin::reload_manifest(
                    &socket,
                    &plugin,
                    source,
                    dry_run,
                    step_up_token.as_deref(),
                )
            }
        },
        AdminSub::Reconcile { sub } => match sub {
            AdminReconcileSub::List { socket } => {
                admin::reconcile_list(&socket)
            }
            AdminReconcileSub::Project { pair, socket } => {
                admin::reconcile_project(&socket, &pair)
            }
            AdminReconcileSub::Now { pair, socket } => {
                admin::reconcile_now(&socket, &pair)
            }
        },
        AdminSub::Flight { sub } => match sub {
            AdminFlightSub::List { socket } => admin::flight_list(&socket),
            AdminFlightSub::Set {
                class,
                state,
                socket,
            } => admin::flight_set(&socket, &class, state.as_bool()),
            AdminFlightSub::All { state, socket } => {
                admin::flight_all(&socket, state.as_bool())
            }
        },
        AdminSub::Grammar { sub } => match sub {
            AdminGrammarSub::List { socket } => admin::grammar_list(&socket),
            AdminGrammarSub::Plan {
                from_type,
                to_type,
                socket,
            } => admin::grammar_plan(&socket, &from_type, &to_type),
            AdminGrammarSub::Migrate {
                from_type,
                to_type,
                reason,
                batch_size,
                max_subjects,
                socket,
            } => admin::grammar_migrate(
                &socket,
                &from_type,
                &to_type,
                reason.as_deref(),
                batch_size,
                max_subjects,
            ),
            AdminGrammarSub::Accept {
                from_type,
                reason,
                socket,
            } => admin::grammar_accept(&socket, &from_type, &reason),
        },
        AdminSub::Warden { sub } => match sub {
            AdminWardenSub::CourseCorrect {
                shelf,
                custody_type,
                verb,
                custody_payload_b64,
                payload_b64,
                socket,
            } => admin::warden_course_correct(
                &socket,
                &shelf,
                &custody_type,
                &custody_payload_b64,
                &verb,
                &payload_b64,
            ),
            AdminWardenSub::FastPathDispatch {
                shelf,
                verb,
                handle_id,
                handle_started_at_ms,
                payload_b64,
                deadline_ms,
                socket,
            } => admin::warden_fast_path_dispatch(
                &socket,
                &shelf,
                &verb,
                &handle_id,
                handle_started_at_ms,
                &payload_b64,
                deadline_ms,
            ),
            AdminWardenSub::TakeCustody {
                shelf,
                custody_type,
                payload_b64,
                socket,
            } => admin::warden_take_custody(
                &socket,
                &shelf,
                &custody_type,
                &payload_b64,
            ),
            AdminWardenSub::ReleaseCustody {
                shelf,
                handle_id,
                handle_started_at_ms,
                socket,
            } => admin::warden_release_custody(
                &socket,
                &shelf,
                &handle_id,
                handle_started_at_ms,
            ),
        },
        AdminSub::Prompt { sub } => match sub {
            AdminPromptSub::List { socket } => admin::prompt_list(&socket),
            AdminPromptSub::AnswerText {
                plugin,
                prompt_id,
                value,
                socket,
            } => {
                admin::prompt_answer_text(&socket, &plugin, &prompt_id, &value)
            }
            AdminPromptSub::Cancel {
                plugin,
                prompt_id,
                socket,
            } => admin::prompt_cancel(&socket, &plugin, &prompt_id),
        },
        AdminSub::Device { sub } => match sub {
            AdminDeviceSub::Identity { sub } => match sub {
                AdminDeviceIdentitySub::Show { socket } => {
                    admin::device_identity_show(&socket)
                }
                AdminDeviceIdentitySub::Rename {
                    new_name,
                    step_up_token,
                    socket,
                } => admin::device_identity_rename(
                    &socket,
                    &new_name,
                    step_up_token.as_deref(),
                ),
                AdminDeviceIdentitySub::Reset {
                    step_up_token,
                    socket,
                } => admin::device_identity_reset(
                    &socket,
                    step_up_token.as_deref(),
                ),
            },
            AdminDeviceSub::Peers { sub } => match sub {
                AdminDevicePeersSub::List { socket } => {
                    admin::device_peers_list(&socket)
                }
            },
        },
        AdminSub::Group { sub } => match sub {
            AdminGroupSub::Create {
                display_name,
                device_ids,
                step_up_token,
                socket,
            } => admin::group_create(
                &socket,
                &display_name,
                &device_ids,
                step_up_token.as_deref(),
            ),
            AdminGroupSub::List { socket } => admin::group_list(&socket),
            AdminGroupSub::Show { group_id, socket } => {
                admin::group_show(&socket, &group_id)
            }
            AdminGroupSub::Rename {
                group_id,
                new_name,
                step_up_token,
                socket,
            } => admin::group_rename(
                &socket,
                &group_id,
                &new_name,
                step_up_token.as_deref(),
            ),
            AdminGroupSub::SetLeaderMs {
                group_id,
                leader_ms,
                step_up_token,
                socket,
            } => admin::group_set_leader_ms(
                &socket,
                &group_id,
                leader_ms,
                step_up_token.as_deref(),
            ),
            AdminGroupSub::AddMember {
                group_id,
                device_id,
                step_up_token,
                socket,
            } => admin::group_add_member(
                &socket,
                &group_id,
                &device_id,
                step_up_token.as_deref(),
            ),
            AdminGroupSub::RemoveMember {
                group_id,
                device_id,
                step_up_token,
                socket,
            } => admin::group_remove_member(
                &socket,
                &group_id,
                &device_id,
                step_up_token.as_deref(),
            ),
            AdminGroupSub::MoveMember {
                from_group_id,
                to_group_id,
                device_id,
                successor,
                step_up_token,
                socket,
            } => admin::group_move_member(
                &socket,
                &from_group_id,
                &to_group_id,
                &device_id,
                successor.as_deref(),
                step_up_token.as_deref(),
            ),
            AdminGroupSub::Delete {
                group_id,
                step_up_token,
                socket,
            } => admin::group_delete(
                &socket,
                &group_id,
                step_up_token.as_deref(),
            ),
            AdminGroupSub::PinSourceHost {
                group_id,
                device_id,
                step_up_token,
                socket,
            } => admin::group_pin_source_host(
                &socket,
                &group_id,
                &device_id,
                step_up_token.as_deref(),
            ),
            AdminGroupSub::UnpinSourceHost {
                group_id,
                step_up_token,
                socket,
            } => admin::group_unpin_source_host(
                &socket,
                &group_id,
                step_up_token.as_deref(),
            ),
            AdminGroupSub::SourceHost { sub } => match sub {
                AdminGroupSourceHostSub::Show { group_id, socket } => {
                    admin::group_source_host_show(&socket, &group_id)
                }
                AdminGroupSourceHostSub::List { socket } => {
                    admin::group_source_host_list(&socket)
                }
            },
            AdminGroupSub::Clock { sub } => match sub {
                AdminGroupClockSub::Show { group_id, socket } => {
                    admin::group_clock_show(&socket, &group_id)
                }
                AdminGroupClockSub::List { socket } => {
                    admin::group_clock_list(&socket)
                }
            },
            AdminGroupSub::Network { sub } => match sub {
                AdminGroupNetworkSub::Connections { socket } => {
                    admin::group_network_connections(&socket)
                }
                AdminGroupNetworkSub::Dial { addr, socket } => {
                    admin::group_network_dial(&socket, &addr)
                }
            },
            AdminGroupSub::Dispatch { group_id, socket } => {
                admin::group_dispatch(&socket, &group_id)
            }
            AdminGroupSub::Topology { sub } => match sub {
                AdminGroupTopologySub::Show { group_id, socket } => {
                    admin::group_topology_show(&socket, &group_id)
                }
                AdminGroupTopologySub::List { socket } => {
                    admin::group_topology_list(&socket)
                }
            },
            AdminGroupSub::Gateways { sub } => match sub {
                AdminGroupGatewaysSub::List { socket } => {
                    admin::group_gateways_list(&socket)
                }
            },
            AdminGroupSub::FrameTrace { json, socket } => {
                admin::group_frame_trace(&socket, json)
            }
        },
        AdminSub::Multiroom { sub } => match sub {
            AdminMultiroomSub::SetRole {
                device_id,
                role,
                step_up_token,
                socket,
            } => admin::multiroom_set_role(
                &socket,
                &device_id,
                &role,
                step_up_token.as_deref(),
            ),
            AdminMultiroomSub::GetRole { device_id, socket } => {
                admin::multiroom_get_role(&socket, &device_id)
            }
            AdminMultiroomSub::ListRoles { socket } => {
                admin::multiroom_list_roles(&socket)
            }
            AdminMultiroomSub::ClearRole {
                device_id,
                step_up_token,
                socket,
            } => admin::multiroom_clear_role(
                &socket,
                &device_id,
                step_up_token.as_deref(),
            ),
            AdminMultiroomSub::Reconnect {
                device_id,
                step_up_token,
                socket,
            } => admin::multiroom_reconnect(
                &socket,
                &device_id,
                step_up_token.as_deref(),
            ),
        },
        AdminSub::Domain { sub } => match sub {
            AdminDomainSub::List { socket } => admin::domain_list(&socket),
            AdminDomainSub::Admit {
                device_id,
                display_name,
                step_up_token,
                socket,
            } => admin::domain_admit(
                &socket,
                &device_id,
                display_name.as_deref(),
                step_up_token.as_deref(),
            ),
            AdminDomainSub::Revoke {
                device_id,
                step_up_token,
                socket,
            } => admin::domain_revoke(
                &socket,
                &device_id,
                step_up_token.as_deref(),
            ),
            AdminDomainSub::Discard {
                device_id,
                reason,
                step_up_token,
                socket,
            } => admin::domain_discard(
                &socket,
                &device_id,
                reason.as_deref(),
                step_up_token.as_deref(),
            ),
            AdminDomainSub::ChainHead { socket } => {
                admin::domain_chain_head(&socket)
            }
            AdminDomainSub::History { limit, socket } => {
                admin::domain_history(&socket, limit)
            }
            AdminDomainSub::Reconnect {
                device_id,
                step_up_token,
                socket,
            } => admin::domain_reconnect(
                &socket,
                &device_id,
                step_up_token.as_deref(),
            ),
            AdminDomainSub::Bootstrap {
                display_name,
                step_up_token,
                socket,
            } => admin::domain_bootstrap(
                &socket,
                display_name.as_deref(),
                step_up_token.as_deref(),
            ),
            AdminDomainSub::Join {
                endpoint,
                step_up_token,
                socket,
            } => admin::domain_join(
                &socket,
                endpoint.as_deref(),
                step_up_token.as_deref(),
            ),
            AdminDomainSub::Leave {
                step_up_token,
                socket,
            } => admin::domain_leave(&socket, step_up_token.as_deref()),
            AdminDomainSub::FactoryReset {
                step_up_token,
                socket,
            } => admin::domain_factory_reset(&socket, step_up_token.as_deref()),
        },
        AdminSub::Updates { sub } => match sub {
            AdminUpdatesSub::List { socket } => admin::updates_list(&socket),
            AdminUpdatesSub::CheckNow {
                source_id,
                step_up_token,
                socket,
            } => admin::updates_check_now(
                &socket,
                source_id.as_deref(),
                step_up_token.as_deref(),
            ),
            AdminUpdatesSub::Apply {
                source_id,
                update_id,
                dry_run,
                approved_by,
                step_up_token,
                socket,
            } => admin::updates_apply(
                &socket,
                &source_id,
                &update_id,
                dry_run,
                approved_by.as_deref(),
                step_up_token.as_deref(),
            ),
            AdminUpdatesSub::AutoApply { sub } => match sub {
                AdminUpdatesAutoApplySub::Show { socket } => {
                    admin::updates_auto_apply_show(&socket)
                }
                AdminUpdatesAutoApplySub::Set {
                    source_id,
                    enabled,
                    severity_threshold,
                    step_up_token,
                    socket,
                } => {
                    let enabled_bool =
                        match enabled.to_ascii_lowercase().as_str() {
                            "true" | "yes" | "on" | "1" => true,
                            "false" | "no" | "off" | "0" => false,
                            other => {
                                return Err(anyhow::anyhow!(
                                    "auto-apply set: --enabled must be one of \
                                 true / false / yes / no / on / off / 1 / 0 \
                                 (got {other:?})"
                                ));
                            }
                        };
                    admin::updates_auto_apply_set(
                        &socket,
                        &source_id,
                        enabled_bool,
                        &severity_threshold,
                        step_up_token.as_deref(),
                    )
                }
            },
        },
        AdminSub::Steward { sub } => match sub {
            AdminStewardSub::Restart {
                reason,
                target_binary,
                step_up_token,
                socket,
            } => admin::steward_restart(
                &socket,
                &reason,
                target_binary.as_deref(),
                step_up_token.as_deref(),
            ),
        },
    }
}

/// Pick between `--inline`, `--path`, and stdin for the reload
/// subcommands.
///
/// Resolution order:
///
/// - `--inline=<TOML>` returns `Inline`.
/// - `--path=<FILE>` returns `Path`.
/// - Neither supplied AND stdin is not a terminal: read stdin to
///   end-of-file and return `Inline` with the captured bytes.
///   Useful for `cat manifest.toml | evo-plugin-tool admin reload
///   manifest --plugin=...`, and the only path that works under
///   service-side `PrivateTmp=yes` since no path crosses the
///   sandbox.
/// - Neither supplied AND stdin IS a terminal: refuse with a
///   usage error rather than hanging on a blank prompt.
/// - Both supplied: refuse — `--inline` and `--path` are mutually
///   exclusive (and combining either with a piped stdin is also
///   refused on the principle that one explicit source per
///   invocation beats silent precedence rules).
fn resolve_reload_source(
    inline: Option<String>,
    path: Option<PathBuf>,
) -> Result<admin::ReloadSource, anyhow::Error> {
    match (inline, path) {
        (Some(t), None) => Ok(admin::ReloadSource::Inline(t)),
        (None, Some(p)) => Ok(admin::ReloadSource::Path(p)),
        (Some(_), Some(_)) => Err(anyhow::anyhow!(
            "--inline and --path are mutually exclusive"
        )),
        (None, None) => read_reload_source_from_stdin(),
    }
}

/// Read a reload TOML body from stdin. Refuses when stdin is a
/// terminal (no piped body) and when the captured body is empty.
fn read_reload_source_from_stdin() -> Result<admin::ReloadSource, anyhow::Error>
{
    let mut stdin = std::io::stdin().lock();
    if stdin.is_terminal() {
        return Err(anyhow::anyhow!(
            "no source supplied: provide --inline=<TOML>, \
             --path=<FILE>, or pipe the TOML body on stdin"
        ));
    }
    read_reload_source_from_reader(&mut stdin)
}

/// Read a reload TOML body from any reader. Extracted so the
/// stdin-handling semantics (empty-input refusal, error wrapping)
/// can be unit-tested without touching real stdin.
fn read_reload_source_from_reader<R: Read>(
    r: &mut R,
) -> Result<admin::ReloadSource, anyhow::Error> {
    let mut buf = String::new();
    r.read_to_string(&mut buf)
        .context("reading reload TOML body from stdin")?;
    if buf.trim().is_empty() {
        return Err(anyhow::anyhow!(
            "stdin produced an empty body; supply non-empty TOML"
        ));
    }
    Ok(admin::ReloadSource::Inline(buf))
}

#[cfg(test)]
mod resolve_reload_source_tests {
    use super::*;
    use std::io::Cursor;
    use std::path::PathBuf;

    #[test]
    fn inline_wins_when_only_inline_supplied() {
        let r = resolve_reload_source(Some("schema_version = 1".into()), None)
            .expect("inline source resolves");
        match r {
            admin::ReloadSource::Inline(t) => {
                assert_eq!(t, "schema_version = 1");
            }
            admin::ReloadSource::Path(_) => panic!("expected Inline"),
        }
    }

    #[test]
    fn path_wins_when_only_path_supplied() {
        let r =
            resolve_reload_source(None, Some(PathBuf::from("/etc/foo.toml")))
                .expect("path source resolves");
        match r {
            admin::ReloadSource::Path(p) => {
                assert_eq!(p, PathBuf::from("/etc/foo.toml"));
            }
            admin::ReloadSource::Inline(_) => panic!("expected Path"),
        }
    }

    #[test]
    fn both_supplied_is_refused() {
        let e = resolve_reload_source(
            Some("x = 1".into()),
            Some(PathBuf::from("/etc/foo.toml")),
        )
        .unwrap_err();
        assert!(
            e.to_string().contains("mutually exclusive"),
            "expected mutual-exclusion message, got: {e}"
        );
    }

    #[test]
    fn reader_returns_captured_body_as_inline() {
        let mut cur = Cursor::new(b"schema_version = 1\nrack = []\n".to_vec());
        let r = read_reload_source_from_reader(&mut cur)
            .expect("reader source resolves");
        match r {
            admin::ReloadSource::Inline(t) => {
                assert_eq!(t, "schema_version = 1\nrack = []\n");
            }
            admin::ReloadSource::Path(_) => panic!("expected Inline"),
        }
    }

    #[test]
    fn reader_refuses_empty_body() {
        let mut cur = Cursor::new(b"".to_vec());
        let e = read_reload_source_from_reader(&mut cur).unwrap_err();
        assert!(
            e.to_string().contains("empty body"),
            "expected empty-body message, got: {e}"
        );
    }

    #[test]
    fn reader_refuses_whitespace_only_body() {
        let mut cur = Cursor::new(b"   \n\t\n".to_vec());
        let e = read_reload_source_from_reader(&mut cur).unwrap_err();
        assert!(
            e.to_string().contains("empty body"),
            "expected empty-body message, got: {e}"
        );
    }
}

fn main() -> ExitCode {
    // Route clap diagnostics through our documented exit-code contract
    // (PLUGIN_TOOL.md section 8): 0 for help/version, 1 for every other
    // CLI-usage error. Clap's own default is 2 for usage errors, which
    // would collide with our documented "trust / signature" exit code.
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            let _ = e.print();
            let code: u8 = match e.kind() {
                clap::error::ErrorKind::DisplayHelp
                | clap::error::ErrorKind::DisplayVersion
                | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => 0,
                _ => 1,
            };
            return ExitCode::from(code);
        }
    };
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e:#}");
            ExitCode::from(exit_code::code_from_error(&e))
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_paths_are_documented() {
        use crate::paths;
        assert_eq!(paths::DEFAULT_SEARCH_ROOT, "/var/lib/evo/plugins");
    }
}
