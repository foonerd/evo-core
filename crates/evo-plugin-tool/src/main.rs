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
    Check {
        /// Path to a privileges.yaml file or to a directory
        /// containing one.
        path: PathBuf,
        /// Skip host-prerequisite probes; only validate the schema.
        #[arg(long)]
        schema_only: bool,
        /// Skip running verification commands. Useful when the
        /// declared service identity does not yet exist on this
        /// host (admission gate enforcement rides v0.1.13).
        #[arg(long)]
        skip_verification: bool,
        /// Treat advisory warnings as failures.
        #[arg(long)]
        strict: bool,
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
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
    },
    /// Wipe the named plugin's `state/` and `credentials/`
    /// directories without removing the bundle. Used for
    /// "factory reset" of a misbehaving plugin while preserving
    /// the installed code.
    PurgeState {
        plugin: String,
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
        #[arg(long, value_name = "PATH", default_value = admin::DEFAULT_SOCKET_PATH)]
        socket: PathBuf,
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
            } => privileges_cmd::check(
                &path,
                schema_only,
                skip_verification,
                strict,
            ),
        },
    }
}

fn run_admin(sub: AdminSub) -> Result<(), anyhow::Error> {
    match sub {
        AdminSub::Enable {
            plugin,
            reason,
            socket,
        } => admin::enable(&socket, &plugin, reason.as_deref()),
        AdminSub::Disable {
            plugin,
            reason,
            socket,
        } => admin::disable(&socket, &plugin, reason.as_deref()),
        AdminSub::Uninstall {
            plugin,
            reason,
            purge_state,
            socket,
        } => admin::uninstall(&socket, &plugin, reason.as_deref(), purge_state),
        AdminSub::PurgeState { plugin, socket } => {
            admin::purge_state(&socket, &plugin)
        }
        AdminSub::Diagnose { plugin, socket } => {
            admin::diagnose(&socket, &plugin)
        }
        AdminSub::DescribeCapabilities { socket } => {
            admin::describe_capabilities(&socket)
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
        AdminSub::ReloadPlugin { plugin, socket } => {
            admin::reload_plugin(&socket, &plugin)
        }
        AdminSub::Reload { sub } => match sub {
            AdminReloadSub::Catalogue {
                inline,
                path,
                dry_run,
                socket,
            } => {
                let source = resolve_reload_source(inline, path)?;
                admin::reload_catalogue(&socket, source, dry_run)
            }
            AdminReloadSub::Manifest {
                plugin,
                inline,
                path,
                dry_run,
                socket,
            } => {
                let source = resolve_reload_source(inline, path)?;
                admin::reload_manifest(&socket, &plugin, source, dry_run)
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
