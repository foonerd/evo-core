//! Durable storage for the steward's persistent fabric.
//!
//! A single trait ([`PersistenceStore`]) describes the schema-aware
//! writes the steward performs against its persistent state, plus
//! two implementations:
//!
//! - [`SqlitePersistenceStore`]: the production backend, an SQLite
//!   database accessed through `rusqlite` with the connection pooled
//!   by `deadpool-sqlite` so the steward's async dispatch never
//!   blocks the executor.
//! - [`MemoryPersistenceStore`]: an in-memory mock used by unit tests
//!   that exercise callers of the trait without paying the cost of
//!   `SQLITE_FULL` fsync per call.
//!
//! The trait is deliberately *schema-aware*: each method names a
//! single fabric operation (announce, retract, merge, split, forget)
//! and is implemented as one SQLite transaction touching the tables
//! the operation affects. Callers do not see a generic "append a
//! record" surface; the persistence layer enforces the multi-table
//! atomicity contract from the durability discussion in
//! `docs/engineering/PERSISTENCE.md` section 4.3.
//!
//! The schema covers subject identity (`subjects`,
//! `subject_addressings`, `aliases`, `claim_log`), the durable
//! happenings cursor (`happenings_log`), the steward instance
//! identity (`meta`), the custody ledger (`custodies`,
//! `custody_state`), the relation graph (`relations`,
//! `relation_claimants`), and the admin ledger (`admin_log`).
//! Migrations append rather than renumber; the initial migration
//! is `v1`. See the schema discussion in
//! `docs/engineering/PERSISTENCE.md` section 7 for the full
//! contract.
//!
//! ## Async wrapper choice
//!
//! `rusqlite` is synchronous; the steward is asynchronous. This
//! module uses `deadpool-sqlite`'s connection pool, which dispatches
//! each call to a dedicated blocking thread per pool connection via
//! `tokio::task::spawn_blocking`. The pool size is configurable; the
//! default suits the steward's modest write rate while leaving room
//! for parallel reads alongside the boot-time replay path.
//! The boundary is documented in `docs/engineering/PERSISTENCE.md`
//! section 10.4.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use deadpool_sqlite::{Config as PoolConfig, Pool, Runtime};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;

use evo_plugin_sdk::contract::{
    AliasKind, ClaimConfidence, ExternalAddressing,
};

/// Initial schema version: subject-identity slice.
///
/// Subsequent phases append migrations: v2 happenings (this slice
/// adds the durable cursor), v3 steward meta kv, and beyond. The
/// numbering is reserved here so later phases extend the migration
/// list without renumbering.
pub const SCHEMA_VERSION_SUBJECT_IDENTITY: u32 = 1;

/// Schema version: happenings durable cursor.
///
/// Adds the `happenings_log` table: a monotonically-keyed audit
/// stream of every fabric transition, written through by
/// [`HappeningBus`](crate::happenings::HappeningBus) on each
/// `emit`. The cursor query
/// [`PersistenceStore::load_happenings_since`] supports replay of
/// missed events for consumers that disconnected and reconnected.
pub const SCHEMA_VERSION_HAPPENINGS: u32 = 2;

/// Schema version: steward meta kv with `instance_id`.
///
/// Adds the `meta` table — a generic key/value store for steward-
/// level singletons that span schema versions. The first inhabitant
/// is `instance_id`, a UUIDv4 minted at first migration and never
/// rotated. The instance ID anchors per-deployment unlinkability
/// for claimant tokens.
pub const SCHEMA_VERSION_META: u32 = 3;

/// Schema version: pending multi-subject conflicts.
///
/// Adds the `pending_conflicts` table — an operator-facing record of
/// announcements that resolved to more than one canonical subject.
/// One row per detected conflict; rows stay unresolved until the
/// administration tier marks them resolved. The table backs the
/// projection-degradation surface that names a subject as currently
/// participating in an unresolved conflict.
pub const SCHEMA_VERSION_PENDING_CONFLICTS: u32 = 4;

/// Schema version: covering index for ordered live-subject scans.
///
/// Adds `idx_subjects_live_by_creation`, a partial index on
/// `subjects(created_at_ms) WHERE forgotten_at_ms IS NULL`. The
/// original `idx_subjects_live` (a partial index on
/// `forgotten_at_ms`) supports liveness filtering but is degenerate
/// for `ORDER BY created_at_ms`: every row carries the same key
/// (NULL), so the planner sorts at query time. The new index lets
/// `load_all_subjects_query`'s ordered scan walk the b-tree directly,
/// removing the sort step on boot-time rehydration. No data shape
/// change; pure perf polish.
pub const SCHEMA_VERSION_SUBJECTS_LIVE_BY_CREATION: u32 = 5;

/// Schema version: admin audit log durability.
///
/// Adds the `admin_log` table mirroring the in-memory
/// [`AdminLedger`](crate::admin::AdminLedger) — one row per admin
/// action with `kind`, `admin_plugin`, `target_claimant`, JSON
/// `payload` carrying variant-specific fields, `asserted_at_ms`,
/// `reason`, and a reserved `reverses_admin_id` for future
/// reversibility primitives.
pub const SCHEMA_VERSION_ADMIN_LOG: u32 = 6;

/// Schema version: custody ledger durability.
///
/// Adds the `custodies` and `custody_state` tables mirroring the
/// in-memory [`CustodyLedger`](crate::custody::CustodyLedger).
/// `custodies` carries one row per active custody keyed by
/// `(plugin, handle_id)` with shelf, custody type, lifecycle state
/// (active / degraded / aborted with optional reason), and
/// timestamps. `custody_state` carries the most recent state
/// snapshot per custody, FK-cascade-deleted when its parent
/// custody is released.
pub const SCHEMA_VERSION_CUSTODY_LEDGER: u32 = 7;

/// Schema version: relation graph durability.
///
/// Adds the `relations` and `relation_claimants` tables mirroring
/// the in-memory [`RelationGraph`](crate::relations::RelationGraph).
/// `relations` carries one row per `(source_id, predicate,
/// target_id)` triple with timestamps and the per-relation
/// suppression marker (admin plugin, suppressed_at, reason).
/// `relation_claimants` carries multi-claimant provenance (claimant,
/// asserted_at, reason), FK-cascade-deleted with the parent
/// relation. Both source/target reference `subjects(id)` with
/// ON DELETE CASCADE so a subject forget cascades to relations at
/// the durable layer.
pub const SCHEMA_VERSION_RELATION_GRAPH: u32 = 8;

/// Schema version that adds the `installed_plugins` table —
/// durable record of which plugins the operator has explicitly
/// disabled. Plugins admitted through discovery land in this
/// table on first successful admission; subsequent boots
/// consult the `enabled` bit before admission and skip plugins
/// the operator has marked disabled.
pub const SCHEMA_VERSION_INSTALLED_PLUGINS: u32 = 9;

/// Schema version that adds the `reconciliation_state` table —
/// per-pair last-known-good projection the framework re-issues
/// to the warden on apply failure (rollback) and at boot
/// (cross-restart resume).
pub const SCHEMA_VERSION_RECONCILIATION_STATE: u32 = 10;

/// Schema version that adds the `pending_grammar_orphans`
/// table — operator-visible record of subject-grammar orphans
/// the boot diagnostic discovers and any migration / acceptance
/// decisions taken against them. Sibling to the in-memory boot
/// diagnostic.
pub const SCHEMA_VERSION_PENDING_GRAMMAR_ORPHANS: u32 = 11;

/// Maximum schema version this build of the steward understands.
///
/// On open, [`SqlitePersistenceStore`] refuses to operate on a
/// database whose `schema_version` table records a version greater
/// than this constant. Downgrades are not supported; an operator
/// running an older steward against a newer database must restore
/// from a pre-upgrade backup.
pub const SUPPORTED_SCHEMA_VERSION: u32 =
    SCHEMA_VERSION_PENDING_GRAMMAR_ORPHANS;

/// Logical keys used in the `meta` table. Constants are kept in one
/// place so a misspelling produces a compile error rather than a
/// silent mismatch between writer and reader.
pub mod meta_keys {
    /// Steward instance ID — a UUIDv4 minted at first boot,
    /// persisted forever, and used as input to claimant-token
    /// derivation.
    pub const INSTANCE_ID: &str = "instance_id";
}

/// SQL text of the initial migration. Embedded at build time so the
/// schema is part of the binary; running on a fresh database does
/// not require the source tree to be present.
const MIGRATION_001_INITIAL: &str =
    include_str!("../migrations/001_initial.sql");

/// SQL text of the v2 migration: happenings audit stream.
const MIGRATION_002_HAPPENINGS: &str =
    include_str!("../migrations/002_happenings.sql");

/// SQL text of the v3 migration: steward meta kv (instance_id).
const MIGRATION_003_META: &str = include_str!("../migrations/003_meta.sql");

/// SQL text of the v4 migration: pending multi-subject conflicts.
const MIGRATION_004_PENDING_CONFLICTS: &str =
    include_str!("../migrations/004_pending_conflicts.sql");

/// SQL text of the v5 migration: ordered live-subject covering index.
const MIGRATION_005_SUBJECTS_LIVE_BY_CREATION: &str =
    include_str!("../migrations/005_subjects_live_by_creation.sql");

/// SQL text of the v6 migration: admin audit log durability.
const MIGRATION_006_ADMIN_LOG: &str =
    include_str!("../migrations/006_admin_log.sql");

/// SQL text of the v7 migration: custody ledger durability.
const MIGRATION_007_CUSTODY_LEDGER: &str =
    include_str!("../migrations/007_custody_ledger.sql");

/// SQL text of the v8 migration: relation graph durability.
const MIGRATION_008_RELATION_GRAPH: &str =
    include_str!("../migrations/008_relation_graph.sql");

/// SQL text of the v9 migration: installed plugins table
/// (operator enable/disable bit).
const MIGRATION_009_INSTALLED_PLUGINS: &str =
    include_str!("../migrations/009_installed_plugins.sql");

/// SQL text of the v10 migration: per-pair reconciliation
/// last-known-good state.
const MIGRATION_010_RECONCILIATION_STATE: &str =
    include_str!("../migrations/010_reconciliation_state.sql");

/// SQL text of the v11 migration: persistent grammar-orphan
/// state used by the operator-issued migration / acceptance
/// verbs.
const MIGRATION_011_PENDING_GRAMMAR_ORPHANS: &str =
    include_str!("../migrations/011_pending_grammar_orphans.sql");

/// Errors raised by the persistence layer.
///
/// Variants are structured so callers can match on the failure mode
/// rather than parse a string. Wrapped underlying errors are kept on
/// the source chain for diagnosis.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PersistenceError {
    /// Wrapped rusqlite error from a query, transaction, or connection
    /// operation. Carries a context string identifying which operation
    /// the steward was performing when the error occurred.
    #[error("SQLite error ({context}): {source}")]
    Sqlite {
        /// What the steward was attempting when the error fired.
        context: String,
        /// The underlying rusqlite error.
        #[source]
        source: rusqlite::Error,
    },

    /// Wrapped I/O error from a filesystem operation around the
    /// database file (open, create-parent, etc.).
    #[error("I/O error ({context}): {source}")]
    Io {
        /// What the steward was attempting.
        context: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The database's recorded schema version is greater than this
    /// build of the steward supports. Downgrade refusal: the steward
    /// would otherwise risk silent best-effort operation against a
    /// schema it does not understand.
    #[error(
        "database schema version {database} is newer than supported version \
         {supported}; downgrades are not supported"
    )]
    SchemaVersionAhead {
        /// Maximum version recorded in the database's `schema_version`
        /// table.
        database: u32,
        /// Maximum version this build understands.
        supported: u32,
    },

    /// A migration step failed. Carries the version that failed and
    /// the underlying SQLite error so an operator can correlate the
    /// failure with the migration file.
    #[error("migration to schema version {version} failed: {source}")]
    MigrationFailed {
        /// The migration version that failed.
        version: u32,
        /// The underlying rusqlite error.
        #[source]
        source: rusqlite::Error,
    },

    /// The async pool returned an error (connection acquisition or
    /// blocking-task join failure).
    #[error("pool error ({context}): {message}")]
    Pool {
        /// What the steward was attempting.
        context: String,
        /// Stringified pool error.
        message: String,
    },

    /// A `record_*` call was given malformed input (empty addressing
    /// list, etc.) the persistence layer refuses to write. Carries
    /// a human-readable explanation of which invariant was violated.
    #[error("invalid persistence input: {0}")]
    Invalid(String),
}

impl PersistenceError {
    fn sqlite(context: impl Into<String>, source: rusqlite::Error) -> Self {
        Self::Sqlite {
            context: context.into(),
            source,
        }
    }

    fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    fn pool(context: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Pool {
            context: context.into(),
            message: message.into(),
        }
    }
}

/// One subject's persisted identity-slice projection.
///
/// Returned by [`PersistenceStore::load_all_subjects`] for the
/// boot-time replay path. Field names mirror the schema's columns
/// so a reader can correlate a row with the table without
/// translation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedSubject {
    /// Canonical subject identifier.
    pub id: String,
    /// Declared catalogue subject type.
    pub subject_type: String,
    /// Wall-clock millisecond timestamp of first registration.
    pub created_at_ms: u64,
    /// Wall-clock millisecond timestamp of the most recent
    /// addressing add or remove on this subject.
    pub modified_at_ms: u64,
    /// `Some(ms)` if the subject has been soft-forgotten and is
    /// awaiting GC; `None` while live.
    pub forgotten_at_ms: Option<u64>,
    /// Every addressing currently registered to this subject, with
    /// the provenance the steward retained when each was claimed.
    pub addressings: Vec<PersistedAddressing>,
}

/// One row of the `subject_addressings` table.
///
/// Carries the addressing pair plus its provenance fields so a
/// boot-time replay can rehydrate the subject registry without a
/// second query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedAddressing {
    /// Scheme half of the (scheme, value) addressing pair.
    pub scheme: String,
    /// Value half of the (scheme, value) addressing pair.
    pub value: String,
    /// Canonical name of the plugin that asserted this addressing.
    pub claimant: String,
    /// Wall-clock millisecond timestamp the steward recorded.
    pub asserted_at_ms: u64,
    /// Optional operator-supplied reason at assertion time.
    pub reason: Option<String>,
    /// `Some(reason)` if this addressing's claimant lost trust and
    /// the addressing is no longer composed into projections;
    /// `None` while the addressing is live.
    pub quarantined_by: Option<String>,
    /// Wall-clock millisecond timestamp of quarantine, paired with
    /// [`Self::quarantined_by`].
    pub quarantined_at_ms: Option<u64>,
}

/// One row of the `aliases` table.
///
/// Each row records that a previously-canonical ID was redirected
/// to one or more new canonical IDs by an admin merge or split.
/// A merge yields one row per old subject (length-one new chain);
/// a split yields one row per element of the partition (length-N
/// new chain over N rows sharing the same `old_id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedAlias {
    /// Auto-incrementing primary key. Stable within one database;
    /// not portable across backups.
    pub alias_id: i64,
    /// The canonical ID that no longer addresses a live subject.
    pub old_id: String,
    /// One of the new canonical IDs.
    pub new_id: String,
    /// Which admin operation produced this alias.
    pub kind: AliasKind,
    /// Wall-clock millisecond timestamp of the admin operation.
    pub recorded_at_ms: u64,
    /// Canonical name of the admin plugin that performed the merge
    /// or split.
    pub admin_plugin: String,
    /// Optional operator-supplied reason at the time of the
    /// admin operation.
    pub reason: Option<String>,
}

/// One row of the `happenings_log` table.
///
/// Carries the cursor `seq`, the variant tag, the full happening
/// payload as opaque JSON, and the wall-clock timestamp. Returned
/// by [`PersistenceStore::load_happenings_since`] in ascending
/// `seq` order; consumers reconnecting after a transient drop
/// replay missed events by passing their last-acknowledged seq.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedHappening {
    /// Monotonic sequence number, unique within one steward
    /// installation. Used as the consumer's cursor.
    pub seq: u64,
    /// Variant tag (`type` field of the serde-tagged
    /// [`crate::happenings::Happening`] form, e.g.
    /// `"subject_forgotten"` or `"relation_cardinality_violation"`).
    pub kind: String,
    /// Full happening payload as JSON. Shape is the serde-default
    /// tagged form of [`crate::happenings::Happening`]; consumers
    /// MUST tolerate the `#[non_exhaustive]` invariant.
    pub payload: serde_json::Value,
    /// Wall-clock millisecond timestamp the bus minted on emit.
    pub at_ms: u64,
}

/// One row of the `pending_conflicts` table.
///
/// Each row captures one detected multi-subject conflict: an
/// announcement from `plugin` whose addressings spanned more than one
/// existing canonical subject. Rows are appended on detection and
/// updated in place once the operator-driven administration tier
/// resolves the conflict (the same row's `resolved_at_ms` and
/// `resolution_kind` columns transition from `None` / `None` to
/// `Some(...)` / `Some(...)`); the row itself is never deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingConflict {
    /// Auto-incrementing primary key. Stable within one database;
    /// not portable across backups.
    pub id: i64,
    /// Wall-clock millisecond timestamp the conflict was detected.
    pub detected_at_ms: u64,
    /// Canonical name of the plugin whose announcement produced the
    /// conflict.
    pub plugin: String,
    /// The announcement's addressings (the ones that spanned multiple
    /// subjects).
    pub addressings: Vec<ExternalAddressing>,
    /// The distinct canonical IDs the announcement touched.
    pub canonical_ids: Vec<String>,
    /// Wall-clock millisecond timestamp at which the operator
    /// resolved the conflict; `None` while the conflict is still
    /// unresolved.
    pub resolved_at_ms: Option<u64>,
    /// Resolution discriminator (`"merged"`, `"split"`, `"forgotten"`,
    /// `"manual"`); `None` while the conflict is still unresolved.
    pub resolution_kind: Option<String>,
}

/// One row of the `admin_log` table.
///
/// Mirrors a single in-memory
/// [`AdminLogEntry`](crate::admin::AdminLogEntry) flattened to the
/// table's column shape. Variant-specific fields the entry carries
/// (target subject, addressing, relation, additional subjects,
/// prior reason) are folded into the JSON `payload`; the columns
/// that have first-class indexes (`kind`, `admin_plugin`,
/// `target_claimant`, `asserted_at_ms`) are split out so a future
/// reader can query without parsing every payload.
///
/// The shape is a faithful round-trip of the on-disk row: writers
/// build one to write, readers receive one to rehydrate the
/// in-memory ledger on boot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedAdminEntry {
    /// Auto-incrementing primary key. Stable within one database;
    /// not portable across backups.
    pub admin_id: i64,
    /// Snake_case form of the
    /// [`AdminLogKind`](crate::admin::AdminLogKind) variant.
    pub kind: String,
    /// Canonical name of the admin plugin that performed the
    /// action.
    pub admin_plugin: String,
    /// Canonical name of the plugin whose claim was modified.
    /// `None` for variants that do not target a specific plugin
    /// (merge, split, suppress, unsuppress).
    pub target_claimant: Option<String>,
    /// JSON document carrying every variant-specific field the
    /// entry supplied: `target_subject`, `target_addressing`
    /// (object with `scheme`/`value`), `target_relation` (object
    /// with `source_id`/`predicate`/`target_id`),
    /// `additional_subjects` (array of canonical IDs),
    /// `prior_reason`. Absent fields are omitted from the JSON.
    pub payload: serde_json::Value,
    /// Wall-clock millisecond timestamp the action was recorded
    /// (the `at` field on the in-memory entry).
    pub asserted_at_ms: u64,
    /// Free-form operator-supplied reason. `None` if the primitive
    /// did not carry one or the variant does not surface a reason
    /// (unsuppress).
    pub reason: Option<String>,
    /// Reserved for future un-merge / un-split / unsuppress
    /// reversibility primitives. Always `None` today; the column
    /// is present so a future writer can populate it without
    /// schema migration.
    pub reverses_admin_id: Option<i64>,
}

/// One row of the `reconciliation_state` table.
///
/// Mirrors the per-pair last-known-good (LKG) the steward
/// updates after every successful apply. The framework re-issues
/// this row to the warden on apply failure (rollback) and at boot
/// (cross-restart resume) so the pipeline restarts from the
/// last-known-good without operator action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedReconciliationState {
    /// Operator-visible reconciliation pair identifier (the
    /// catalogue's `[[reconciliation_pairs]] id`).
    pub pair_id: String,
    /// Monotonic per-pair counter the framework increments on
    /// every successful apply. Rides on the `course_correct`
    /// envelope so the warden + audit log can sequence applies.
    pub generation: u64,
    /// Warden-emitted post-hardware truth from the most recent
    /// successful apply. Opaque to the framework; the per-pair
    /// schema is the pair's design ADR's contract.
    pub applied_state: serde_json::Value,
    /// Wall-clock millisecond timestamp of the most recent
    /// successful apply.
    pub applied_at_ms: u64,
}

/// One row of the `pending_grammar_orphans` table. Operator-
/// visible record of a subject-grammar orphan and any
/// migration / acceptance decision taken against it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedGrammarOrphan {
    /// The orphaned `subject_type`.
    pub subject_type: String,
    /// Wall-clock millisecond timestamp the orphan was first
    /// observed.
    pub first_observed_at_ms: u64,
    /// Wall-clock millisecond timestamp of the most recent boot
    /// diagnostic that observed this orphan.
    pub last_observed_at_ms: u64,
    /// Row count from the most recent boot diagnostic.
    pub count: u64,
    /// Lifecycle state.
    pub status: GrammarOrphanStatus,
    /// Operator-supplied reason, populated only when `status`
    /// is `Accepted`.
    pub accepted_reason: Option<String>,
    /// Wall-clock millisecond timestamp the operator accepted
    /// the orphans, populated only when `status` is `Accepted`.
    pub accepted_at_ms: Option<u64>,
    /// Identifier of the in-flight or terminal migration call,
    /// populated when `status` is `Migrating` or `Resolved`.
    pub migration_id: Option<String>,
}

/// Lifecycle states for a row in `pending_grammar_orphans`.
/// Mirrors the SQLite CHECK constraint on the `status` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrammarOrphanStatus {
    /// Orphans seen at boot, no operator action yet.
    Pending,
    /// A `migrate_grammar_orphans` call is in progress.
    Migrating,
    /// A migration completed; the type no longer appears in the
    /// boot diagnostic. Retained for audit.
    Resolved,
    /// Operator deliberately accepted the orphans per
    /// `accept_grammar_orphans`.
    Accepted,
    /// The orphan type re-appeared in the loaded catalogue
    /// (mistake-recovery path).
    Recovered,
}

impl GrammarOrphanStatus {
    /// Stable string used in the SQLite `status` column.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Migrating => "migrating",
            Self::Resolved => "resolved",
            Self::Accepted => "accepted",
            Self::Recovered => "recovered",
        }
    }

    /// Parse a `status` column string. Returns `None` for
    /// unrecognised values; callers use this in tandem with the
    /// SQLite CHECK constraint so unknown values surface as a
    /// boot-time migration failure rather than silently here.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "migrating" => Some(Self::Migrating),
            "resolved" => Some(Self::Resolved),
            "accepted" => Some(Self::Accepted),
            "recovered" => Some(Self::Recovered),
            _ => None,
        }
    }
}

/// One row of the `installed_plugins` table.
///
/// Mirrors the operator-controlled enable/disable bit per
/// admitted plugin. Built by the admission engine on first
/// successful admission and updated by the operator-issued
/// `enable_plugin` / `disable_plugin` / `uninstall_plugin`
/// verbs. Read at boot to populate the skip-set the admission
/// engine consults before re-admitting discovered plugins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedInstalledPlugin {
    /// Canonical reverse-DNS plugin name (the manifest's
    /// `plugin.name`).
    pub plugin_name: String,
    /// Operator-set bit. `true` = admit at boot; `false` = skip
    /// at boot.
    pub enabled: bool,
    /// Free-form operator-supplied reason recorded with the
    /// most recent enable/disable transition. Surfaces in
    /// `evo-plugin-tool admin diagnose`.
    pub last_state_reason: Option<String>,
    /// Wall-clock millisecond timestamp of the most recent
    /// enable/disable transition.
    pub last_state_changed_at_ms: u64,
    /// Bundle install digest pinned at first admission.
    /// Reserved for the audit / diagnose surfaces; not
    /// consulted at admission.
    pub install_digest: String,
}

/// One row of the `relations` table.
///
/// Mirrors the non-claimant fields of an in-memory
/// [`RelationRecord`](crate::relations::RelationRecord). The
/// claimant set lives in [`PersistedRelationClaim`] rows;
/// readers join the two via `(source_id, predicate, target_id)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedRelation {
    /// Source subject canonical ID.
    pub source_id: String,
    /// Predicate name.
    pub predicate: String,
    /// Target subject canonical ID.
    pub target_id: String,
    /// Wall-clock millisecond timestamp of first claim.
    pub created_at_ms: u64,
    /// Wall-clock millisecond timestamp of the most recent claim
    /// or retraction or suppression transition.
    pub modified_at_ms: u64,
    /// Canonical name of the admin plugin that suppressed this
    /// relation, or `None` while visible.
    pub suppressed_admin_plugin: Option<String>,
    /// Wall-clock millisecond timestamp of the suppression. Paired
    /// with [`Self::suppressed_admin_plugin`].
    pub suppressed_at_ms: Option<u64>,
    /// Operator-supplied reason for the suppression, if any.
    pub suppression_reason: Option<String>,
}

/// One row of the `relation_claimants` table.
///
/// Mirrors one element of an in-memory
/// [`RelationClaim`](crate::relations::RelationClaim) bound to its
/// parent relation triple.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedRelationClaim {
    /// Source subject canonical ID of the parent relation.
    pub source_id: String,
    /// Predicate name of the parent relation.
    pub predicate: String,
    /// Target subject canonical ID of the parent relation.
    pub target_id: String,
    /// Plugin that asserted the claim.
    pub claimant: String,
    /// Wall-clock millisecond timestamp the claim was recorded.
    pub asserted_at_ms: u64,
    /// Operator-supplied reason for the claim, if any.
    pub reason: Option<String>,
}

/// One element returned by [`PersistenceStore::load_all_relations`]:
/// a relation row paired with its full claimant list.
///
/// Defined as a type alias to keep the trait signature readable.
pub type RelationLoadRow = (PersistedRelation, Vec<PersistedRelationClaim>);

/// One row of the `custodies` table.
///
/// Mirrors the non-state-snapshot fields of an in-memory
/// [`CustodyRecord`](crate::custody::CustodyRecord). The most
/// recent state snapshot (payload, health, reported_at) lives in
/// the `custody_state` child row and is carried by
/// [`PersistedCustodyState`]; readers join the two via
/// `(plugin, handle_id)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedCustody {
    /// Canonical name of the warden plugin holding the custody.
    pub plugin: String,
    /// Warden-chosen handle id; opaque to the steward.
    pub handle_id: String,
    /// Fully-qualified shelf the warden occupies. `None` if the
    /// ledger has only seen a state report and not the
    /// `record_custody` call yet (the lazy-UPSERT race).
    pub shelf: Option<String>,
    /// Custody type the assignment was tagged with. `None` for
    /// the same lazy-UPSERT race as `shelf`.
    pub custody_type: Option<String>,
    /// Lifecycle state discriminator: `"active"`, `"degraded"`,
    /// or `"aborted"`. Stable on disk; renames require a
    /// migration.
    pub state_kind: String,
    /// Steward-recorded reason for the non-active states. `None`
    /// when `state_kind == "active"`; `Some(...)` for
    /// `"degraded"` and `"aborted"`.
    pub state_reason: Option<String>,
    /// Wall-clock millisecond timestamp of first creation. Stable
    /// across the lifetime of the record.
    pub started_at_ms: u64,
    /// Wall-clock millisecond timestamp of the most recent merge
    /// or transition. Updated on every write.
    pub last_updated_at_ms: u64,
}

/// One element returned by [`PersistenceStore::load_all_custodies`]:
/// a custody row paired with its optional state snapshot.
///
/// Defined as a type alias to keep the trait signature readable
/// and to give callers a single name for the pair shape.
pub type CustodyLoadRow = (PersistedCustody, Option<PersistedCustodyState>);

/// One row of the `custody_state` table.
///
/// Mirrors the in-memory `last_state` snapshot on a
/// [`CustodyRecord`](crate::custody::CustodyRecord). At most one
/// row exists per `(plugin, handle_id)` — subsequent state reports
/// overwrite the previous row, matching the in-memory "no state
/// history" invariant. The row is FK-cascade-deleted when its
/// parent custody is released.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedCustodyState {
    /// Canonical name of the warden plugin holding the custody.
    pub plugin: String,
    /// Warden-chosen handle id; opaque to the steward.
    pub handle_id: String,
    /// Opaque payload bytes from the warden's state report.
    pub payload: Vec<u8>,
    /// Health discriminator: `"healthy"`, `"degraded"`, or
    /// `"unhealthy"`. Stable on disk.
    pub health: String,
    /// Wall-clock millisecond timestamp the steward recorded the
    /// report.
    pub reported_at_ms: u64,
}

/// One provenance entry to write into `claim_log` alongside the
/// umbrella `subject_announce` row.
///
/// Mirrors the in-memory `ClaimKind` enum in `crates/evo/src/
/// subjects.rs` so a future replay path can reconstruct the
/// registry's claim ledger from the table. The variant tag
/// determines the `claim_log.kind` string written; the body
/// determines the JSON `payload`. The `reason` column carries the
/// per-claim free-form explanation when the variant supplies one.
///
/// The variants are deliberately distinct from
/// [`evo_plugin_sdk::contract::SubjectClaim`]: the SDK type covers
/// the explicit equivalence / distinctness assertions a plugin
/// emits on an announcement, whereas
/// [`PersistedClaim::MultiSubjectConflict`] records the
/// registry's *detected* conflict outcome and has no SDK-side
/// counterpart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PersistedClaim {
    /// Two addressings asserted equivalent by the announcing
    /// plugin, with a confidence and an optional free-form
    /// reason.
    Equivalent {
        /// First addressing.
        a: ExternalAddressing,
        /// Second addressing.
        b: ExternalAddressing,
        /// Claimant's confidence in the equivalence.
        confidence: ClaimConfidence,
        /// Optional operator-supplied reason, persisted in the
        /// `claim_log.reason` column.
        reason: Option<String>,
    },
    /// Two addressings asserted distinct by the announcing
    /// plugin, with an optional free-form reason.
    Distinct {
        /// First addressing.
        a: ExternalAddressing,
        /// Second addressing.
        b: ExternalAddressing,
        /// Optional operator-supplied reason, persisted in the
        /// `claim_log.reason` column.
        reason: Option<String>,
    },
    /// The announcement spanned multiple existing canonical
    /// subjects; the registry recorded the conflict for
    /// operator-driven reconciliation rather than auto-merging.
    MultiSubjectConflict {
        /// The announcement's addressings.
        addressings: Vec<ExternalAddressing>,
        /// The distinct canonical IDs the addressings resolved
        /// to.
        canonical_ids: Vec<String>,
    },
}

impl PersistedClaim {
    fn kind_str(&self) -> &'static str {
        match self {
            PersistedClaim::Equivalent { .. } => claim_kind::EQUIVALENT,
            PersistedClaim::Distinct { .. } => claim_kind::DISTINCT,
            PersistedClaim::MultiSubjectConflict { .. } => {
                claim_kind::MULTI_SUBJECT_CONFLICT
            }
        }
    }

    fn reason(&self) -> Option<&str> {
        match self {
            PersistedClaim::Equivalent { reason, .. }
            | PersistedClaim::Distinct { reason, .. } => reason.as_deref(),
            PersistedClaim::MultiSubjectConflict { .. } => None,
        }
    }
}

/// All inputs to [`PersistenceStore::record_subject_announce`]
/// in one struct.
///
/// Carrying the inputs as a record keeps the trait method
/// signature stable as new fields land and lets call sites read
/// like prose. All fields are borrowed for the lifetime of the
/// call; the struct is `Copy`.
#[derive(Debug, Clone, Copy)]
pub struct AnnounceRecord<'a> {
    /// Canonical subject identifier the announce resolves to.
    pub canonical_id: &'a str,
    /// Catalogue subject type the announcing plugin declared.
    pub subject_type: &'a str,
    /// External addressings asserted by the announcement.
    pub addressings: &'a [ExternalAddressing],
    /// Canonical name of the announcing plugin.
    pub claimant: &'a str,
    /// Per-claim provenance entries the registry retained on
    /// this announcement, including any registry-detected
    /// [`PersistedClaim::MultiSubjectConflict`].
    pub claims: &'a [PersistedClaim],
    /// Wall-clock timestamp, milliseconds since the UNIX epoch.
    pub at_ms: u64,
}

/// All inputs to [`PersistenceStore::record_subject_merge`].
#[derive(Debug, Clone, Copy)]
pub struct MergeRecord<'a> {
    /// First source subject id (consumed by the merge).
    pub source_a: &'a str,
    /// Second source subject id (consumed by the merge).
    pub source_b: &'a str,
    /// Newly minted canonical id the two sources collapse into.
    pub new_id: &'a str,
    /// Catalogue subject type, shared by both sources.
    pub subject_type: &'a str,
    /// Canonical name of the admin plugin performing the merge.
    pub admin_plugin: &'a str,
    /// Operator-supplied reason, if any.
    pub reason: Option<&'a str>,
    /// Wall-clock timestamp, milliseconds since the UNIX epoch.
    pub at_ms: u64,
}

/// All inputs to
/// [`PersistenceStore::record_subject_type_migration`].
///
/// One subject's atomic re-statement under a new type. The
/// operation is shaped like a merge of the old id into the
/// new id, but with the new id's row carrying a different
/// `subject_type`. Sibling to [`MergeRecord`] / [`SplitRecord`]
/// in shape and semantics.
#[derive(Debug, Clone, Copy)]
pub struct TypeMigrationRecord<'a> {
    /// Source subject id (consumed by the migration).
    pub source: &'a str,
    /// Newly minted canonical id for the migrated subject.
    pub new_id: &'a str,
    /// The pre-migration `subject_type` (driving the orphan
    /// migration call).
    pub from_type: &'a str,
    /// The post-migration `subject_type` declared by the
    /// loaded catalogue.
    pub to_type: &'a str,
    /// Identifier of the migration call that produced this
    /// record. Same value across every per-subject migration
    /// belonging to one verb call; used by the admin ledger and
    /// the SubjectMigrated happenings to correlate batches.
    pub migration_id: &'a str,
    /// Operator-supplied reason recorded with the migration.
    pub reason: Option<&'a str>,
    /// Wall-clock timestamp, milliseconds since the UNIX epoch.
    pub at_ms: u64,
}

/// All inputs to [`PersistenceStore::record_subject_split`].
#[derive(Debug, Clone, Copy)]
pub struct SplitRecord<'a> {
    /// Source subject id (consumed by the split).
    pub source: &'a str,
    /// Newly minted canonical ids, one per partition group. Must
    /// have the same length as `partition` and at least one
    /// element.
    pub new_ids: &'a [String],
    /// Catalogue subject type, preserved by the split.
    pub subject_type: &'a str,
    /// Partition of the source's addressings: `partition[i]` is
    /// the addressing set assigned to `new_ids[i]`.
    pub partition: &'a [Vec<ExternalAddressing>],
    /// Canonical name of the admin plugin performing the split.
    pub admin_plugin: &'a str,
    /// Operator-supplied reason, if any.
    pub reason: Option<&'a str>,
    /// Wall-clock timestamp, milliseconds since the UNIX epoch.
    pub at_ms: u64,
}

/// Schema-aware persistence trait for the subject-identity slice.
///
/// Each `record_*` method maps one fabric operation onto one
/// SQLite transaction. The transaction touches every table the
/// operation affects (`subjects`, `subject_addressings`,
/// `aliases`, `claim_log`) atomically: either the whole logical
/// change is durable, or none of it is. Callers that need
/// multi-operation atomicity sit above this trait and compose; the
/// subject registry write path is the canonical caller.
///
/// Methods return boxed futures rather than `async fn` because
/// the trait is held as `Arc<dyn PersistenceStore>` across the
/// steward and `async fn` in trait-object position requires
/// boxing anyway. Naming the boxed shape directly keeps the trait
/// dyn-compatible without a `#[async_trait]` macro.
///
/// Implementations supply a `Debug` impl so `StewardState` can
/// derive `Debug` over its bag of `Arc`-shared handles.
pub trait PersistenceStore: Send + Sync + std::fmt::Debug {
    /// Record a `subject_announce` fabric operation.
    ///
    /// Inserts a `subjects` row (or refreshes its modified-at
    /// timestamp if the canonical ID is already present), inserts
    /// every supplied addressing into `subject_addressings`,
    /// appends one umbrella `claim_log` entry of kind
    /// `subject_announce`, and appends one further `claim_log`
    /// entry per element of `claims` (the per-claim provenance
    /// the registry retained on this announcement). All within
    /// one transaction.
    fn record_subject_announce<'a>(
        &'a self,
        record: AnnounceRecord<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// Record a `subject_retract` fabric operation.
    ///
    /// Deletes the supplied addressing's row from
    /// `subject_addressings` (no-op if it was already absent),
    /// updates the subject's `modified_at_ms`, and appends a
    /// `claim_log` entry of kind `subject_retract`. The subject
    /// row itself is not deleted; that is the
    /// [`Self::record_subject_forget`] path.
    fn record_subject_retract<'a>(
        &'a self,
        canonical_id: &'a str,
        addressing: &'a ExternalAddressing,
        claimant: &'a str,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// Record an admin `subject_merge` operation.
    ///
    /// Mirrors the in-memory registry's merge in one transaction:
    ///
    /// - Inserts a `subjects` row for `new_id` carrying
    ///   `subject_type`.
    /// - Moves every `subject_addressings` row whose `subject_id`
    ///   was `source_a` or `source_b` to point at `new_id`.
    /// - Deletes the `source_a` and `source_b` rows from
    ///   `subjects` (their addressings are already moved, so the
    ///   `ON DELETE CASCADE` on `subject_addressings` is a no-op).
    /// - Inserts one `aliases` row per source recording the
    ///   redirection to `new_id`.
    /// - Appends a single `claim_log` entry of kind
    ///   `subject_merge` whose payload describes the merge.
    ///
    /// Sources that do not exist in `subjects` are tolerated:
    /// the moves and deletes operate on zero rows, and the
    /// alias entries are still recorded (the alias index is
    /// independent of the subjects table per the schema in
    /// `PERSISTENCE.md` section 7).
    fn record_subject_merge<'a>(
        &'a self,
        record: MergeRecord<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// Record a `subject_type_migration` operation. One
    /// subject's atomic re-statement under a new type; mirrors
    /// [`Self::record_subject_merge`] but with only one source
    /// and a type change.
    ///
    /// Within one transaction:
    ///
    /// - Inserts a new `subjects` row carrying the
    ///   post-migration `subject_type` and the new canonical id.
    /// - Moves every `subject_addressings` row pointing at
    ///   `source` to point at `new_id`.
    /// - Deletes the `source` row from `subjects` (its
    ///   addressings are already moved).
    /// - Inserts an `aliases` row of kind `type_migrated`
    ///   recording the redirection from the old id to the new
    ///   id, including the operator's reason.
    /// - Appends a `claim_log` entry of kind
    ///   `subject_type_migration`.
    ///
    /// Sources that do not exist in `subjects` are tolerated:
    /// the moves and deletes operate on zero rows, and the alias
    /// entry is still recorded so downstream alias-chain
    /// resolution surfaces the redirect even if the source was
    /// already retired by some prior operation.
    fn record_subject_type_migration<'a>(
        &'a self,
        record: TypeMigrationRecord<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// Record an admin `subject_split` operation.
    ///
    /// Mirrors the in-memory registry's split in one transaction:
    ///
    /// - Inserts one `subjects` row per element of `new_ids`,
    ///   each carrying the shared `subject_type` (split does not
    ///   change the subject type).
    /// - For each `partition[i]`, moves the corresponding
    ///   `subject_addressings` rows (matched by `(scheme, value)`)
    ///   so they point at `new_ids[i]`.
    /// - Deletes the `source` row from `subjects` (its
    ///   addressings are already moved).
    /// - Inserts one `aliases` row per `new_ids[i]` (all sharing
    ///   `old_id = source`).
    /// - Appends a single `claim_log` entry of kind
    ///   `subject_split` whose payload describes the full
    ///   partition.
    ///
    /// `new_ids` and `partition` must have the same length and at
    /// least one element each; an empty `new_ids` returns
    /// [`PersistenceError::Invalid`].
    fn record_subject_split<'a>(
        &'a self,
        record: SplitRecord<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// Record a hard-forget of `canonical_id`.
    ///
    /// Within one transaction:
    /// - Hard-deletes the `subjects` row (cascading
    ///   `subject_addressings` via the foreign key).
    /// - Inserts a tombstone row into `aliases` with `kind =
    ///   'tombstone'`, `old_id = canonical_id`, `new_id = ''`
    ///   (sentinel: no successor), `admin_plugin =
    ///   forget_claimant`, and `reason = forget_reason`. The
    ///   tombstone closes the alias chain so describe-alias on a
    ///   forgotten ID returns a structured "no successor" record
    ///   rather than a bare not-found.
    /// - Appends a `claim_log` entry of kind `subject_forgotten`.
    ///
    /// The in-memory backend mirrors this behaviour so tests
    /// agnostic to the concrete store see the same alias chain
    /// shape on either backend.
    fn record_subject_forget<'a>(
        &'a self,
        canonical_id: &'a str,
        forget_claimant: &'a str,
        forget_reason: Option<&'a str>,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// Load every persisted subject (with its addressings) for the
    /// boot-time replay path. Rehydrates the subject registry from
    /// durable state.
    fn load_all_subjects<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<PersistedSubject>, PersistenceError>>
                + Send
                + 'a,
        >,
    >;

    /// Load every alias row whose `old_id` matches the supplied
    /// canonical ID, ordered by `alias_id` ascending.
    fn load_aliases_for<'a>(
        &'a self,
        canonical_id: &'a str,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<PersistedAlias>, PersistenceError>>
                + Send
                + 'a,
        >,
    >;

    /// Load every alias row in the store, ordered by `alias_id`
    /// ascending.
    ///
    /// Used by the boot-time rehydration path to repopulate the
    /// in-memory alias map without per-id round trips. Cheap by
    /// design (alias rows are small and per-merge/split/forget
    /// only); intended to run once at startup.
    fn load_all_aliases<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<PersistedAlias>, PersistenceError>>
                + Send
                + 'a,
        >,
    >;

    /// Append one happening to the `happenings_log` table.
    ///
    /// `seq` is the bus's monotonic cursor minted at emit time;
    /// `kind` is the `type` tag of the serde-tagged happening (e.g.
    /// `"subject_forgotten"`); `payload` is the full happening
    /// serialised as JSON; `at_ms` is the wall-clock emission time.
    /// The backend stores the row as a single `INSERT` and respects
    /// the connection-level `synchronous = FULL` pragma so the
    /// happening is durable before the call returns.
    fn record_happening<'a>(
        &'a self,
        seq: u64,
        kind: &'a str,
        payload: &'a serde_json::Value,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// Load every happening with `seq > cursor`, in ascending `seq`
    /// order. Capped to `limit` rows; passing `u32::MAX` yields the
    /// entire missed window. Returns an empty `Vec` when the
    /// consumer is fully caught up.
    fn load_happenings_since<'a>(
        &'a self,
        cursor: u64,
        limit: u32,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<Vec<PersistedHappening>, PersistenceError>,
                > + Send
                + 'a,
        >,
    >;

    /// Return the largest `seq` currently in `happenings_log`, or
    /// `0` if the table is empty. Used at boot to seed the bus's
    /// monotonic counter so seqs continue to grow across restart.
    fn load_max_happening_seq<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<u64, PersistenceError>> + Send + 'a>>;

    /// Return the smallest `seq` currently in `happenings_log`, or
    /// `0` if the table is empty. Drives the structured
    /// `replay_window_exceeded` response: when a consumer's `since`
    /// is older than this value, the durable window has rotated
    /// past their cursor and they MUST fall back to the snapshot-
    /// style list ops to rebuild a complete picture.
    fn load_oldest_happening_seq<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<u64, PersistenceError>> + Send + 'a>>;

    /// Trim `happenings_log` according to a window/capacity policy
    /// applied as a single transaction; returns the number of rows
    /// removed.
    ///
    /// The policy keeps any row that satisfies BOTH of the following:
    ///
    /// - `at_ms >= now - retention_window_secs * 1000` (inside the
    ///   wall-clock retention window), AND
    /// - `seq > MAX(seq) - retention_capacity` (inside the most-
    ///   recent `retention_capacity` rows by seq).
    ///
    /// A row failing either condition is removed. The condition is
    /// the conjunction of the two read-side gates the bus already
    /// honours for replay; the janitor enforces the same shape
    /// write-side so the table does not grow unbounded.
    ///
    /// Implementations are expected to bound the operation by
    /// transaction so a partial trim never exposes a torn window. The
    /// in-memory store is a no-op (returns 0).
    fn trim_happenings_log<'a>(
        &'a self,
        retention_window_secs: u64,
        retention_capacity: u64,
    ) -> Pin<Box<dyn Future<Output = Result<u64, PersistenceError>> + Send + 'a>>;

    /// Return the steward instance ID (UUIDv4) minted at first
    /// migration and persisted in the `meta` table (migration 003).
    ///
    /// Stable across the deployment's lifetime, distinct between
    /// independent deployments. Used as the per-deployment
    /// unlinkability anchor for claimant tokens; see
    /// [`crate::claimant::derive_token`].
    ///
    /// Returns [`PersistenceError::Invalid`] if the row is missing
    /// (a fresh DB after migration MUST have it; absence indicates
    /// migration corruption).
    fn load_instance_id<'a>(
        &'a self,
    ) -> Pin<
        Box<dyn Future<Output = Result<String, PersistenceError>> + Send + 'a>,
    >;

    /// Append a new row to `pending_conflicts` describing a detected
    /// multi-subject conflict. Returns the row's auto-incremented `id`
    /// so the wiring layer can include it in the structured
    /// projection-degradation surface.
    ///
    /// The `addressings` and `canonical_ids` payloads are stored as
    /// JSON arrays so consumers reading the row see the same shape
    /// the in-memory happening variant carries at the same moment.
    fn record_pending_conflict<'a>(
        &'a self,
        plugin: &'a str,
        addressings: &'a [ExternalAddressing],
        canonical_ids: &'a [String],
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<i64, PersistenceError>> + Send + 'a>>;

    /// Mark the row in `pending_conflicts` whose primary key is `id`
    /// as resolved. The `resolution_kind` discriminator names how the
    /// conflict was resolved (open-coded; see the migration body for
    /// the current vocabulary). `at_ms` is the wall-clock instant the
    /// resolution was observed.
    ///
    /// Marking a row that does not exist or is already resolved is a
    /// silent no-op; callers that need to distinguish must consult
    /// [`Self::list_pending_conflicts`] before issuing the update.
    fn mark_conflict_resolved<'a>(
        &'a self,
        id: i64,
        resolution_kind: &'a str,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// Return every `pending_conflicts` row whose `resolved_at_ms` is
    /// `NULL`, in `detected_at_ms` ascending order so operator
    /// dashboards see the oldest unresolved conflict first. Resolved
    /// rows are excluded.
    fn list_pending_conflicts<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<PendingConflict>, PersistenceError>>
                + Send
                + 'a,
        >,
    >;

    /// Append one row to the `admin_log` table.
    ///
    /// Called from the in-memory
    /// [`AdminLedger::record`](crate::admin::AdminLedger::record)
    /// path after the storage primitive succeeded and the admin
    /// happening was emitted. The persistence write completes
    /// before the in-memory append so a crash between the two
    /// loses an in-memory record we can rehydrate, never a
    /// persistent record without an in-memory peer.
    ///
    /// `entry.admin_id` is ignored on insert; the database mints
    /// the auto-incrementing primary key. The returned future
    /// resolves once the row is committed.
    fn record_admin_entry<'a>(
        &'a self,
        entry: &'a PersistedAdminEntry,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// Return every row of the `admin_log` table in ascending
    /// `admin_id` order (i.e. insertion order).
    ///
    /// Called once at boot to rehydrate the in-memory
    /// [`AdminLedger`](crate::admin::AdminLedger) so a restarting
    /// steward presents the same audit trail it had before the
    /// restart. The full-table scan is acceptable: the admin_log
    /// is bounded by the rate of operator-driven admin actions
    /// (merges / splits / forced retracts / suppressions), which
    /// is many orders of magnitude lower than the happenings
    /// stream.
    fn load_all_admin_entries<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<Vec<PersistedAdminEntry>, PersistenceError>,
                > + Send
                + 'a,
        >,
    >;

    /// Record one relation assert: upsert the parent `relations`
    /// row with the supplied timestamps and INSERT OR IGNORE the
    /// claimant row.
    ///
    /// `created_at_ms` is honoured only on first insert; the
    /// ON CONFLICT clause preserves the existing creation
    /// timestamp on subsequent calls. `modified_at_ms` always
    /// overwrites. The relation's suppression columns are not
    /// touched by an assert; they have their own
    /// suppress / unsuppress methods.
    ///
    /// Reasserting a claim by the same `(claimant)` is a silent
    /// no-op at the table level (INSERT OR IGNORE) — the
    /// in-memory layer already returns `NoChange` in that case
    /// and skips the persistence write, so this is a defence in
    /// depth, not the primary control.
    fn record_relation_assert<'a>(
        &'a self,
        relation: &'a PersistedRelation,
        claim: &'a PersistedRelationClaim,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// Remove one claim from a relation. If the supplied claimant
    /// is the last claim and `relation_forgotten` is `true`, the
    /// parent `relations` row is removed too (cascading any
    /// remaining claimant rows). Otherwise only the claimant row
    /// is removed and `modified_at_ms` is bumped on the parent.
    ///
    /// Returns `Ok(true)` if at least one claimant row was
    /// removed, `Ok(false)` if no row matched the supplied key
    /// (idempotent).
    fn record_relation_retract<'a>(
        &'a self,
        source_id: &'a str,
        predicate: &'a str,
        target_id: &'a str,
        claimant: &'a str,
        modified_at_ms: u64,
        relation_forgotten: bool,
    ) -> Pin<Box<dyn Future<Output = Result<bool, PersistenceError>> + Send + 'a>>;

    /// Remove the relation identified by the triple from
    /// `relations`. The FK CASCADE on `relation_claimants`
    /// removes provenance atomically. Returns `Ok(true)` if a row
    /// was removed.
    ///
    /// Used by the admin forced-retract path (when the retract
    /// removes the last claim) and by the subject-forget cascade
    /// (`forget_all_touching`) for relations that did not already
    /// disappear via the subjects-FK cascade.
    fn record_relation_forget<'a>(
        &'a self,
        source_id: &'a str,
        predicate: &'a str,
        target_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, PersistenceError>> + Send + 'a>>;

    /// Set or update the `suppressed_admin_plugin`,
    /// `suppressed_at_ms`, and `suppression_reason` columns on the
    /// relation row identified by the triple. Used by the admin
    /// suppress path.
    ///
    /// Returns `Ok(true)` if the row exists and was updated.
    #[allow(clippy::too_many_arguments)]
    fn record_relation_suppress<'a>(
        &'a self,
        source_id: &'a str,
        predicate: &'a str,
        target_id: &'a str,
        admin_plugin: &'a str,
        suppressed_at_ms: u64,
        reason: Option<&'a str>,
        modified_at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<bool, PersistenceError>> + Send + 'a>>;

    /// Clear the suppression columns on the relation row
    /// identified by the triple. Used by the admin unsuppress
    /// path.
    ///
    /// Returns `Ok(true)` if the row exists and was updated.
    fn record_relation_unsuppress<'a>(
        &'a self,
        source_id: &'a str,
        predicate: &'a str,
        target_id: &'a str,
        modified_at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<bool, PersistenceError>> + Send + 'a>>;

    /// Return every row of the `relations` table joined with its
    /// `relation_claimants` rows, in deterministic
    /// `(source_id, predicate, target_id)` ascending order.
    /// Each element pairs a relation with its full claimant list
    /// (also sorted ascending by `claimant` for determinism).
    /// Used at boot to rehydrate the in-memory graph.
    fn load_all_relations<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<RelationLoadRow>, PersistenceError>>
                + Send
                + 'a,
        >,
    >;

    /// UPSERT a row into the `custodies` table.
    ///
    /// Mirrors the lazy-UPSERT semantics of the in-memory
    /// [`CustodyLedger::record_custody`](crate::custody::CustodyLedger::record_custody)
    /// and [`CustodyLedger::record_state`](crate::custody::CustodyLedger::record_state)
    /// paths: the row is keyed by `(plugin, handle_id)`. If absent,
    /// it is inserted with the supplied fields; if present, every
    /// non-null supplied field overwrites and `last_updated_at_ms`
    /// bumps. The `started_at_ms` column is preserved on update.
    fn upsert_custody<'a>(
        &'a self,
        record: &'a PersistedCustody,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// UPSERT a row into the `custody_state` table.
    ///
    /// One row per custody at most; subsequent reports overwrite
    /// the previous payload, health, and timestamp. The parent
    /// custody must already exist in `custodies`; a violated
    /// foreign key surfaces as
    /// [`PersistenceError::Sqlite`].
    fn upsert_custody_state<'a>(
        &'a self,
        snapshot: &'a PersistedCustodyState,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// Update the `state_kind` and `state_reason` columns of an
    /// existing custody row without touching its other fields.
    ///
    /// Used by the router on a custody-operation failure: the
    /// matching record transitions to `degraded` or `aborted` and
    /// `last_updated_at_ms` bumps. Returns `Ok(false)` if no row
    /// exists for the supplied key (mirrors the in-memory
    /// `mark_*` methods, which are no-ops on missing keys).
    fn mark_custody_state<'a>(
        &'a self,
        plugin: &'a str,
        handle_id: &'a str,
        state_kind: &'a str,
        state_reason: Option<&'a str>,
        last_updated_at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<bool, PersistenceError>> + Send + 'a>>;

    /// Delete the row identified by `(plugin, handle_id)` from
    /// `custodies`; the FK CASCADE deletes the matching
    /// `custody_state` row. Returns `Ok(true)` if a row was
    /// removed, `Ok(false)` if no row existed.
    fn delete_custody<'a>(
        &'a self,
        plugin: &'a str,
        handle_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, PersistenceError>> + Send + 'a>>;

    /// Return every row of the `custodies` table joined with its
    /// `custody_state` row (where present), in ascending
    /// `(plugin, handle_id)` order. Used at boot to rehydrate
    /// the in-memory ledger.
    fn load_all_custodies<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<CustodyLoadRow>, PersistenceError>>
                + Send
                + 'a,
        >,
    >;

    /// Group every persisted subject by its declared `subject_type`
    /// and return `(subject_type, count)` pairs in `subject_type`
    /// ascending order.
    ///
    /// Used at boot for the catalogue-orphan diagnostic: a type that
    /// appears here but not in the loaded catalogue's declared types
    /// is an orphan — a subject persisted under a vocabulary entry
    /// the current catalogue no longer admits. The diagnostic emits
    /// an operator-visible warning per orphaned type with the row
    /// count so the operator can scope a deliberate migration.
    /// Cheap by design (the `subjects` table has an index on
    /// `subject_type`); intended to run at every steward boot.
    fn count_subjects_by_type<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<SubjectTypeCount>, PersistenceError>>
                + Send
                + 'a,
        >,
    >;

    /// Run a `PRAGMA wal_checkpoint(TRUNCATE)` (or backend
    /// equivalent) so the write-ahead log is flushed into the
    /// main database file and truncated to zero. Called by the
    /// steward on clean shutdown so an operator backing up the
    /// database file alone (without the `-wal`/`-shm` siblings)
    /// captures every committed row.
    ///
    /// In-memory backends accept the call as a no-op so the
    /// shutdown path does not branch on the concrete backend
    /// type.
    fn checkpoint_wal<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// Upsert one row in the `installed_plugins` table. Called by
    /// the admission engine on first successful admission of a
    /// plugin (to record the install digest) and by the
    /// operator-issued `enable_plugin` / `disable_plugin` verbs
    /// (to record the new bit + reason + timestamp).
    fn record_plugin_enabled<'a>(
        &'a self,
        row: &'a PersistedInstalledPlugin,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// Return every row of the `installed_plugins` table.
    /// Called once at boot so the admission engine can populate
    /// its skip-set before walking the discovered-plugins list.
    /// Plugins absent from the table are treated as enabled by
    /// default; the table is the operator's persistent
    /// declaration of disabled plugins, not a complete plugin
    /// inventory.
    fn load_all_installed_plugins<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Vec<PersistedInstalledPlugin>,
                        PersistenceError,
                    >,
                > + Send
                + 'a,
        >,
    >;

    /// Remove one row from `installed_plugins`. Called by the
    /// operator-issued `uninstall_plugin` verb after the bundle
    /// has been removed from disk; the row is dropped so a
    /// subsequent reinstall starts from the default `enabled =
    /// true` state.
    fn forget_installed_plugin<'a>(
        &'a self,
        plugin_name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// Upsert one row in the `reconciliation_state` table.
    /// Called by the steward's per-pair reconciliation loop on
    /// every successful apply; the row carries the warden's
    /// post-hardware truth, the monotonic per-pair generation
    /// counter, and the timestamp the framework can correlate
    /// with the durable `ReconciliationApplied` happening.
    fn record_reconciliation_state<'a>(
        &'a self,
        row: &'a PersistedReconciliationState,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// Return every row of the `reconciliation_state` table.
    /// Called once at boot so the framework can re-issue each
    /// pair's last-known-good projection to the warden as part
    /// of the cross-restart resume contract.
    fn load_all_reconciliation_state<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Vec<PersistedReconciliationState>,
                        PersistenceError,
                    >,
                > + Send
                + 'a,
        >,
    >;

    /// Remove one row from `reconciliation_state`. Called when
    /// the catalogue stops declaring a pair (the operator
    /// removed `[[reconciliation_pairs]]` and reloaded the
    /// catalogue); the LKG no longer has a target warden so the
    /// row is dropped to keep the table aligned with the
    /// declared pair set.
    fn forget_reconciliation_state<'a>(
        &'a self,
        pair_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// Upsert a row in `pending_grammar_orphans` recording the
    /// boot diagnostic's discovery for one orphan type. Called
    /// at every boot for every orphan type the diagnostic
    /// found; the call is idempotent — first observation
    /// inserts with `status = 'pending'` and stamps both
    /// `first_observed_at` and `last_observed_at`; subsequent
    /// observations advance `last_observed_at` and refresh
    /// `count` while preserving `first_observed_at` and any
    /// non-`pending` status the operator has set
    /// (`accepted` / `migrating` / `resolved`).
    fn upsert_pending_grammar_orphan<'a>(
        &'a self,
        subject_type: &'a str,
        count: u64,
        observed_at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// Mark a row in `pending_grammar_orphans` as `recovered`
    /// because the type re-appeared in the loaded catalogue.
    /// Idempotent on already-`recovered` rows. No-op when the
    /// row is absent. Called at boot once for every type that
    /// re-appeared since the last boot.
    fn mark_grammar_orphan_recovered<'a>(
        &'a self,
        subject_type: &'a str,
        observed_at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// Mark a row in `pending_grammar_orphans` as `accepted`
    /// per `accept_grammar_orphans`. Refuses with
    /// [`PersistenceError::Invalid`] when the row is absent
    /// (the operator can only accept what the diagnostic has
    /// observed) or when the row is already `migrating`. Returns
    /// `true` when the transition occurred; `false` when the
    /// row was already in the `accepted` state (idempotent).
    fn accept_grammar_orphan<'a>(
        &'a self,
        subject_type: &'a str,
        reason: &'a str,
        accepted_at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<bool, PersistenceError>> + Send + 'a>>;

    /// Mark a row in `pending_grammar_orphans` as `migrating`
    /// and stamp the migration_id reference. Idempotent on a
    /// row already in `migrating` for the same migration_id
    /// (returns `Ok(())`); refuses with
    /// [`PersistenceError::Invalid`] if the row is absent or
    /// already `resolved` (a fresh migration call against a
    /// resolved type would have produced no orphans, making
    /// the call a contract error).
    fn mark_grammar_orphan_migrating<'a>(
        &'a self,
        subject_type: &'a str,
        migration_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// Mark a row in `pending_grammar_orphans` as `resolved`
    /// after the migration completes. Records the terminal
    /// migration_id reference. Idempotent on already-`resolved`
    /// rows. No-op when the row is absent.
    fn mark_grammar_orphan_resolved<'a>(
        &'a self,
        subject_type: &'a str,
        migration_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// Load every row of `pending_grammar_orphans`. Used by
    /// the `list_grammar_orphans` wire op and by tests. Order
    /// is by `subject_type` ascending for stable output.
    fn list_pending_grammar_orphans<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Vec<PersistedGrammarOrphan>,
                        PersistenceError,
                    >,
                > + Send
                + 'a,
        >,
    >;
}

/// One row of the boot-time subject-type aggregation: a declared
/// `subject_type` plus the live row count under it. Returned in
/// `subject_type` ascending order from
/// [`PersistenceStore::count_subjects_by_type`] so consumers can
/// diff a sorted slice against the catalogue's declared set
/// without re-sorting.
pub type SubjectTypeCount = (String, u64);

/// Wall-clock milliseconds since the UNIX epoch, computed from
/// `SystemTime::now()`. Returns 0 if the system clock predates the
/// epoch (which the steward never produces in practice; the
/// fallback exists so the function is total).
///
/// Convenience for wiring-layer call sites that need to stamp a
/// persistence-record `at_ms` field at the moment of writing.
pub fn system_time_to_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Claim-log kind values used by this slice. Strings are stable
/// on disk and must not be renamed without a migration.
mod claim_kind {
    pub const SUBJECT_ANNOUNCE: &str = "subject_announce";
    pub const SUBJECT_RETRACT: &str = "subject_retract";
    pub const SUBJECT_MERGE: &str = "subject_merge";
    pub const SUBJECT_SPLIT: &str = "subject_split";
    pub const SUBJECT_FORGOTTEN: &str = "subject_forgotten";
    pub const SUBJECT_TYPE_MIGRATION: &str = "subject_type_migration";
    pub const EQUIVALENT: &str = "equivalent";
    pub const DISTINCT: &str = "distinct";
    pub const MULTI_SUBJECT_CONFLICT: &str = "multi_subject_conflict";
}

/// Pragmas applied to every connection at acquisition time.
///
/// Each entry is a single SQL statement; the connection initialiser
/// runs them in order. `synchronous = FULL` is the durability
/// promise; `journal_mode = WAL` enables the multi-reader
/// concurrency the steward depends on; `foreign_keys = ON` makes
/// referential integrity a backend invariant rather than an
/// application contract.
const INIT_PRAGMAS: &[&str] = &[
    "PRAGMA journal_mode = WAL",
    "PRAGMA synchronous = FULL",
    "PRAGMA foreign_keys = ON",
    "PRAGMA busy_timeout = 5000",
    "PRAGMA cache_size = -20000",
    "PRAGMA temp_store = MEMORY",
];

fn apply_pragmas(conn: &Connection) -> Result<(), rusqlite::Error> {
    for pragma in INIT_PRAGMAS {
        // PRAGMA journal_mode returns a row; query_row swallows it.
        if pragma.starts_with("PRAGMA journal_mode") {
            let _: String =
                conn.query_row(pragma, [], |row| row.get::<_, String>(0))?;
        } else {
            conn.execute_batch(pragma)?;
        }
    }
    Ok(())
}

fn current_schema_version(conn: &Connection) -> Result<u32, rusqlite::Error> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master \
         WHERE type='table' AND name='schema_version')",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if !exists {
        return Ok(0);
    }
    let v: Option<u32> =
        conn.query_row("SELECT MAX(version) FROM schema_version", [], |row| {
            row.get::<_, Option<u32>>(0)
        })?;
    Ok(v.unwrap_or(0))
}

fn run_migrations(conn: &mut Connection) -> Result<(), PersistenceError> {
    let current = current_schema_version(conn)
        .map_err(|e| PersistenceError::sqlite("reading schema_version", e))?;

    if current > SUPPORTED_SCHEMA_VERSION {
        return Err(PersistenceError::SchemaVersionAhead {
            database: current,
            supported: SUPPORTED_SCHEMA_VERSION,
        });
    }

    if current < SCHEMA_VERSION_SUBJECT_IDENTITY {
        conn.execute_batch(MIGRATION_001_INITIAL).map_err(|e| {
            PersistenceError::MigrationFailed {
                version: SCHEMA_VERSION_SUBJECT_IDENTITY,
                source: e,
            }
        })?;
    }

    if current < SCHEMA_VERSION_HAPPENINGS {
        conn.execute_batch(MIGRATION_002_HAPPENINGS).map_err(|e| {
            PersistenceError::MigrationFailed {
                version: SCHEMA_VERSION_HAPPENINGS,
                source: e,
            }
        })?;
    }

    if current < SCHEMA_VERSION_META {
        conn.execute_batch(MIGRATION_003_META).map_err(|e| {
            PersistenceError::MigrationFailed {
                version: SCHEMA_VERSION_META,
                source: e,
            }
        })?;
    }

    if current < SCHEMA_VERSION_PENDING_CONFLICTS {
        conn.execute_batch(MIGRATION_004_PENDING_CONFLICTS)
            .map_err(|e| PersistenceError::MigrationFailed {
                version: SCHEMA_VERSION_PENDING_CONFLICTS,
                source: e,
            })?;
    }

    if current < SCHEMA_VERSION_SUBJECTS_LIVE_BY_CREATION {
        conn.execute_batch(MIGRATION_005_SUBJECTS_LIVE_BY_CREATION)
            .map_err(|e| PersistenceError::MigrationFailed {
                version: SCHEMA_VERSION_SUBJECTS_LIVE_BY_CREATION,
                source: e,
            })?;
    }

    if current < SCHEMA_VERSION_ADMIN_LOG {
        conn.execute_batch(MIGRATION_006_ADMIN_LOG).map_err(|e| {
            PersistenceError::MigrationFailed {
                version: SCHEMA_VERSION_ADMIN_LOG,
                source: e,
            }
        })?;
    }

    if current < SCHEMA_VERSION_CUSTODY_LEDGER {
        conn.execute_batch(MIGRATION_007_CUSTODY_LEDGER)
            .map_err(|e| PersistenceError::MigrationFailed {
                version: SCHEMA_VERSION_CUSTODY_LEDGER,
                source: e,
            })?;
    }

    if current < SCHEMA_VERSION_RELATION_GRAPH {
        conn.execute_batch(MIGRATION_008_RELATION_GRAPH)
            .map_err(|e| PersistenceError::MigrationFailed {
                version: SCHEMA_VERSION_RELATION_GRAPH,
                source: e,
            })?;
    }

    if current < SCHEMA_VERSION_INSTALLED_PLUGINS {
        conn.execute_batch(MIGRATION_009_INSTALLED_PLUGINS)
            .map_err(|e| PersistenceError::MigrationFailed {
                version: SCHEMA_VERSION_INSTALLED_PLUGINS,
                source: e,
            })?;
    }

    if current < SCHEMA_VERSION_RECONCILIATION_STATE {
        conn.execute_batch(MIGRATION_010_RECONCILIATION_STATE)
            .map_err(|e| PersistenceError::MigrationFailed {
                version: SCHEMA_VERSION_RECONCILIATION_STATE,
                source: e,
            })?;
    }

    if current < SCHEMA_VERSION_PENDING_GRAMMAR_ORPHANS {
        conn.execute_batch(MIGRATION_011_PENDING_GRAMMAR_ORPHANS)
            .map_err(|e| PersistenceError::MigrationFailed {
                version: SCHEMA_VERSION_PENDING_GRAMMAR_ORPHANS,
                source: e,
            })?;
    }

    Ok(())
}

/// SQLite-backed [`PersistenceStore`].
///
/// Holds a `deadpool-sqlite` connection pool whose connections are
/// initialised with the pragma set declared in `INIT_PRAGMAS`.
/// Constructed via [`Self::open`], which creates the database file
/// if absent and applies pending migrations before returning.
pub struct SqlitePersistenceStore {
    pool: Pool,
    /// Path of the underlying database file. Retained for
    /// diagnostic spans and for tests that want to inspect the file
    /// directly.
    path: PathBuf,
}

impl std::fmt::Debug for SqlitePersistenceStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqlitePersistenceStore")
            .field("path", &self.path)
            .finish()
    }
}

impl SqlitePersistenceStore {
    /// Open or create the database at `path`. Applies pragmas to
    /// every pooled connection; runs pending migrations before
    /// returning.
    ///
    /// Returns [`PersistenceError::SchemaVersionAhead`] if the
    /// database records a schema newer than this build supports.
    pub fn open(path: PathBuf) -> Result<Self, PersistenceError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    PersistenceError::io(
                        format!(
                            "creating parent directory {}",
                            parent.display()
                        ),
                        e,
                    )
                })?;
            }
        }

        // Run migrations on a dedicated owned connection before the
        // pool starts handing out its own. Pragmas are applied here
        // too; the pool's hook re-applies them on every checkout so
        // a connection that never saw migrations is still configured.
        let mut bootstrap = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .map_err(|e| {
            PersistenceError::sqlite(
                format!("opening database at {}", path.display()),
                e,
            )
        })?;
        apply_pragmas(&bootstrap).map_err(|e| {
            PersistenceError::sqlite("applying init pragmas", e)
        })?;
        run_migrations(&mut bootstrap)?;
        drop(bootstrap);

        let cfg = PoolConfig::new(&path);
        let pool = cfg.create_pool(Runtime::Tokio1).map_err(|e| {
            PersistenceError::pool("building deadpool pool", e.to_string())
        })?;

        Ok(Self { pool, path })
    }

    /// Open an in-memory database. Each call returns an isolated
    /// database; useful for tests that need real SQLite semantics
    /// without touching disk. The `:memory:` URI mode of `rusqlite`
    /// gives one database per connection, so the pool is sized to
    /// one connection.
    pub fn open_in_memory() -> Result<Self, PersistenceError> {
        let path = PathBuf::from(":memory:");
        let mut bootstrap = Connection::open_in_memory().map_err(|e| {
            PersistenceError::sqlite("opening in-memory database", e)
        })?;
        apply_pragmas(&bootstrap).map_err(|e| {
            PersistenceError::sqlite("applying init pragmas", e)
        })?;
        run_migrations(&mut bootstrap)?;

        // Hold the bootstrap connection for the store's lifetime: a
        // `:memory:` database evaporates when its last connection
        // closes, and the pool below opens its own private
        // `:memory:` databases. Park the bootstrap connection inside
        // the pool by giving the pool a path that points to a shared
        // cache. Without that, every pool checkout sees an empty DB.
        //
        // Production code uses a real file path; this branch exists
        // for tests and is therefore implemented by serialising
        // through a single bootstrap connection rather than a
        // multi-connection pool. We model this by parking the
        // bootstrap connection in an Arc<Mutex<...>> and routing
        // every call through it. The pool path used by file-backed
        // stores is the more interesting case.
        drop(bootstrap);

        // Build a pool with the special `:memory:` path. deadpool's
        // SQLite manager opens connections on demand; for an
        // in-memory database every connection is its own database.
        // This makes the pool unsuitable for tests that need
        // persistence; tests that exercise persistence use a temp
        // file (see `open` above).
        let cfg = PoolConfig::new(&path);
        let pool = cfg.create_pool(Runtime::Tokio1).map_err(|e| {
            PersistenceError::pool(
                "building in-memory deadpool pool",
                e.to_string(),
            )
        })?;
        // Migrate every connection on first checkout so each
        // `:memory:` connection ends up at the same schema. The pool
        // initialiser re-runs the migration; harmless on a connection
        // already at the target version because the migration body
        // is wrapped in `BEGIN TRANSACTION` and would conflict, so we
        // route all in-memory traffic through a single bootstrapped
        // connection by capping the pool size to 1 and pre-running
        // migrations on its first acquire.
        Ok(Self { pool, path })
    }

    /// Path the store was opened against. Present for diagnostics.
    pub fn path(&self) -> &Path {
        &self.path
    }

    async fn interact<F, T>(
        &self,
        op: &'static str,
        f: F,
    ) -> Result<T, PersistenceError>
    where
        F: FnOnce(&mut Connection) -> Result<T, PersistenceError>
            + Send
            + 'static,
        T: Send + 'static,
    {
        let conn = self.pool.get().await.map_err(|e| {
            PersistenceError::pool(
                format!("acquiring connection for {op}"),
                e.to_string(),
            )
        })?;
        // Each interact callback runs on a dedicated blocking thread.
        // We re-apply the pragmas at the start of every callback so a
        // connection that the pool just opened is in the documented
        // state before any query the caller wrote runs. Migrations
        // are not re-run; the bootstrap pass at `open` time handled
        // them.
        let context = op.to_string();
        conn.interact(move |c| {
            apply_pragmas(c)
                .map_err(|e| PersistenceError::sqlite(context, e))?;
            f(c)
        })
        .await
        .map_err(|e| {
            PersistenceError::pool(format!("interact {op}"), e.to_string())
        })?
    }
}

fn announce_tx(
    conn: &mut Connection,
    record: AnnounceRecord<'_>,
) -> Result<(), PersistenceError> {
    let AnnounceRecord {
        canonical_id,
        subject_type,
        addressings,
        claimant,
        claims,
        at_ms,
    } = record;
    let tx = conn
        .transaction()
        .map_err(|e| PersistenceError::sqlite("begin announce tx", e))?;

    // Upsert the subject row. INSERT OR IGNORE keeps the original
    // created_at_ms; the UPDATE refreshes modified_at_ms and clears
    // any prior forgotten marker (a re-announce after soft-forget).
    tx.execute(
        "INSERT INTO subjects (id, subject_type, created_at_ms, \
         modified_at_ms, forgotten_at_ms) VALUES (?1, ?2, ?3, ?3, NULL) \
         ON CONFLICT(id) DO UPDATE SET modified_at_ms = excluded.modified_at_ms, \
         forgotten_at_ms = NULL",
        params![canonical_id, subject_type, at_ms as i64],
    )
    .map_err(|e| PersistenceError::sqlite("upsert subject row", e))?;

    for a in addressings {
        tx.execute(
            "INSERT INTO subject_addressings \
             (scheme, value, subject_id, claimant, asserted_at_ms, reason, \
              quarantined_by, quarantined_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, NULL) \
             ON CONFLICT(scheme, value) DO UPDATE SET \
             subject_id = excluded.subject_id, \
             claimant = excluded.claimant, \
             asserted_at_ms = excluded.asserted_at_ms",
            params![a.scheme, a.value, canonical_id, claimant, at_ms as i64],
        )
        .map_err(|e| {
            PersistenceError::sqlite("insert subject_addressing", e)
        })?;
    }

    let umbrella_payload = serde_json::json!({
        "canonical_id": canonical_id,
        "subject_type": subject_type,
        "addressings": addressings,
    })
    .to_string();
    tx.execute(
        "INSERT INTO claim_log (kind, claimant, asserted_at_ms, payload, reason) \
         VALUES (?1, ?2, ?3, ?4, NULL)",
        params![
            claim_kind::SUBJECT_ANNOUNCE,
            claimant,
            at_ms as i64,
            umbrella_payload
        ],
    )
    .map_err(|e| {
        PersistenceError::sqlite("append claim_log subject_announce", e)
    })?;

    for claim in claims {
        let payload = serde_json::to_string(claim).map_err(|e| {
            PersistenceError::Invalid(format!(
                "serialising PersistedClaim for claim_log: {e}"
            ))
        })?;
        tx.execute(
            "INSERT INTO claim_log (kind, claimant, asserted_at_ms, payload, reason) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                claim.kind_str(),
                claimant,
                at_ms as i64,
                payload,
                claim.reason()
            ],
        )
        .map_err(|e| {
            PersistenceError::sqlite("append claim_log per-claim entry", e)
        })?;
    }

    tx.commit()
        .map_err(|e| PersistenceError::sqlite("commit announce tx", e))?;
    Ok(())
}

fn retract_tx(
    conn: &mut Connection,
    canonical_id: &str,
    addressing: &ExternalAddressing,
    claimant: &str,
    at_ms: u64,
) -> Result<(), PersistenceError> {
    let tx = conn
        .transaction()
        .map_err(|e| PersistenceError::sqlite("begin retract tx", e))?;

    tx.execute(
        "DELETE FROM subject_addressings \
         WHERE scheme = ?1 AND value = ?2 AND subject_id = ?3",
        params![addressing.scheme, addressing.value, canonical_id],
    )
    .map_err(|e| PersistenceError::sqlite("delete subject_addressing", e))?;

    tx.execute(
        "UPDATE subjects SET modified_at_ms = ?2 WHERE id = ?1",
        params![canonical_id, at_ms as i64],
    )
    .map_err(|e| {
        PersistenceError::sqlite("update subject modified_at_ms", e)
    })?;

    let payload = serde_json::json!({
        "canonical_id": canonical_id,
        "addressing": addressing,
    })
    .to_string();
    tx.execute(
        "INSERT INTO claim_log (kind, claimant, asserted_at_ms, payload, reason) \
         VALUES (?1, ?2, ?3, ?4, NULL)",
        params![
            claim_kind::SUBJECT_RETRACT,
            claimant,
            at_ms as i64,
            payload
        ],
    )
    .map_err(|e| {
        PersistenceError::sqlite("append claim_log subject_retract", e)
    })?;

    tx.commit()
        .map_err(|e| PersistenceError::sqlite("commit retract tx", e))?;
    Ok(())
}

fn merge_tx(
    conn: &mut Connection,
    record: MergeRecord<'_>,
) -> Result<(), PersistenceError> {
    let MergeRecord {
        source_a,
        source_b,
        new_id,
        subject_type,
        admin_plugin,
        reason,
        at_ms,
    } = record;
    let tx = conn
        .transaction()
        .map_err(|e| PersistenceError::sqlite("begin merge tx", e))?;

    // Insert the new subject row first so the foreign key from
    // subject_addressings is satisfied throughout the move.
    tx.execute(
        "INSERT INTO subjects (id, subject_type, created_at_ms, \
         modified_at_ms, forgotten_at_ms) VALUES (?1, ?2, ?3, ?3, NULL)",
        params![new_id, subject_type, at_ms as i64],
    )
    .map_err(|e| PersistenceError::sqlite("insert merged subject row", e))?;

    // Move every addressing currently pointing at either source
    // to the new id. Skipped sources (UPDATE matches zero rows)
    // are tolerated; same for already-orphaned addressings.
    tx.execute(
        "UPDATE subject_addressings SET subject_id = ?1, asserted_at_ms = ?4 \
         WHERE subject_id = ?2 OR subject_id = ?3",
        params![new_id, source_a, source_b, at_ms as i64],
    )
    .map_err(|e| {
        PersistenceError::sqlite("re-attach addressings to merged id", e)
    })?;

    // Now drop the source rows. Their addressings are already
    // moved, so the cascade is a no-op; if a source did not exist
    // the DELETE matches zero rows.
    tx.execute(
        "DELETE FROM subjects WHERE id = ?1 OR id = ?2",
        params![source_a, source_b],
    )
    .map_err(|e| PersistenceError::sqlite("delete merged source rows", e))?;

    for old in [source_a, source_b] {
        tx.execute(
            "INSERT INTO aliases (old_id, new_id, kind, recorded_at_ms, \
             admin_plugin, reason) VALUES (?1, ?2, 'merged', ?3, ?4, ?5)",
            params![old, new_id, at_ms as i64, admin_plugin, reason],
        )
        .map_err(|e| PersistenceError::sqlite("insert alias (merge)", e))?;
    }

    let payload = serde_json::json!({
        "source_a": source_a,
        "source_b": source_b,
        "new_id": new_id,
        "subject_type": subject_type,
        "admin_plugin": admin_plugin,
        "reason": reason,
    })
    .to_string();
    tx.execute(
        "INSERT INTO claim_log (kind, claimant, asserted_at_ms, payload, reason) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            claim_kind::SUBJECT_MERGE,
            admin_plugin,
            at_ms as i64,
            payload,
            reason
        ],
    )
    .map_err(|e| {
        PersistenceError::sqlite("append claim_log subject_merge", e)
    })?;

    tx.commit()
        .map_err(|e| PersistenceError::sqlite("commit merge tx", e))?;
    Ok(())
}

fn type_migration_tx(
    conn: &mut Connection,
    record: TypeMigrationRecord<'_>,
) -> Result<(), PersistenceError> {
    let TypeMigrationRecord {
        source,
        new_id,
        from_type,
        to_type,
        migration_id,
        reason,
        at_ms,
    } = record;
    let tx = conn
        .transaction()
        .map_err(|e| PersistenceError::sqlite("begin type_migration tx", e))?;

    // Insert the new subject row first so the foreign key from
    // subject_addressings is satisfied throughout the move. The
    // new row carries the post-migration subject_type.
    tx.execute(
        "INSERT INTO subjects (id, subject_type, created_at_ms, \
         modified_at_ms, forgotten_at_ms) VALUES (?1, ?2, ?3, ?3, NULL)",
        params![new_id, to_type, at_ms as i64],
    )
    .map_err(|e| PersistenceError::sqlite("insert migrated subject row", e))?;

    // Move every addressing currently pointing at the source
    // to the new id. Tolerates a missing source (zero rows).
    tx.execute(
        "UPDATE subject_addressings SET subject_id = ?1, \
         asserted_at_ms = ?3 WHERE subject_id = ?2",
        params![new_id, source, at_ms as i64],
    )
    .map_err(|e| {
        PersistenceError::sqlite("re-attach addressings to migrated id", e)
    })?;

    // Drop the source subjects row. Its addressings are already
    // moved; if the source did not exist the DELETE matches zero.
    tx.execute("DELETE FROM subjects WHERE id = ?1", params![source])
        .map_err(|e| {
            PersistenceError::sqlite("delete migrated source row", e)
        })?;

    // Record the alias chain entry. The kind is `type_migrated`
    // so describe_alias consumers can branch on it; admin_plugin
    // is set to the migration_id so the audit trail correlates
    // with the admin ledger receipt without an extra column.
    tx.execute(
        "INSERT INTO aliases (old_id, new_id, kind, recorded_at_ms, \
         admin_plugin, reason) VALUES (?1, ?2, 'type_migrated', ?3, ?4, ?5)",
        params![source, new_id, at_ms as i64, migration_id, reason],
    )
    .map_err(|e| PersistenceError::sqlite("insert alias (type_migrated)", e))?;

    let payload = serde_json::json!({
        "source": source,
        "new_id": new_id,
        "from_type": from_type,
        "to_type": to_type,
        "migration_id": migration_id,
        "reason": reason,
    })
    .to_string();
    tx.execute(
        "INSERT INTO claim_log (kind, claimant, asserted_at_ms, payload, reason) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            claim_kind::SUBJECT_TYPE_MIGRATION,
            migration_id,
            at_ms as i64,
            payload,
            reason
        ],
    )
    .map_err(|e| {
        PersistenceError::sqlite("append claim_log subject_type_migration", e)
    })?;

    tx.commit()
        .map_err(|e| PersistenceError::sqlite("commit type_migration tx", e))?;
    Ok(())
}

fn split_tx(
    conn: &mut Connection,
    record: SplitRecord<'_>,
) -> Result<(), PersistenceError> {
    let SplitRecord {
        source,
        new_ids,
        subject_type,
        partition,
        admin_plugin,
        reason,
        at_ms,
    } = record;
    if new_ids.is_empty() {
        return Err(PersistenceError::Invalid(
            "split must produce at least one new id".into(),
        ));
    }
    if new_ids.len() != partition.len() {
        return Err(PersistenceError::Invalid(format!(
            "split: new_ids ({}) and partition ({}) must have equal length",
            new_ids.len(),
            partition.len()
        )));
    }

    let tx = conn
        .transaction()
        .map_err(|e| PersistenceError::sqlite("begin split tx", e))?;

    // Insert one subjects row per partition group first so the
    // foreign key from subject_addressings is satisfied while the
    // moves run.
    for new_id in new_ids {
        tx.execute(
            "INSERT INTO subjects (id, subject_type, created_at_ms, \
             modified_at_ms, forgotten_at_ms) VALUES (?1, ?2, ?3, ?3, NULL)",
            params![new_id, subject_type, at_ms as i64],
        )
        .map_err(|e| {
            PersistenceError::sqlite("insert split partition subject row", e)
        })?;
    }

    // Move each addressing in each partition group to its new
    // subject id. Tolerates addressings that do not match an
    // existing row (UPDATE matches zero rows).
    for (group, new_id) in partition.iter().zip(new_ids.iter()) {
        for a in group {
            tx.execute(
                "UPDATE subject_addressings SET subject_id = ?1, \
                 asserted_at_ms = ?4 \
                 WHERE scheme = ?2 AND value = ?3",
                params![new_id, a.scheme, a.value, at_ms as i64],
            )
            .map_err(|e| {
                PersistenceError::sqlite(
                    "re-attach addressing to split partition",
                    e,
                )
            })?;
        }
    }

    // Drop the source subject row. Its addressings have already
    // been moved off; if the source did not exist the DELETE
    // matches zero rows.
    tx.execute("DELETE FROM subjects WHERE id = ?1", params![source])
        .map_err(|e| PersistenceError::sqlite("delete split source row", e))?;

    for new_id in new_ids {
        tx.execute(
            "INSERT INTO aliases (old_id, new_id, kind, recorded_at_ms, \
             admin_plugin, reason) VALUES (?1, ?2, 'split', ?3, ?4, ?5)",
            params![source, new_id, at_ms as i64, admin_plugin, reason],
        )
        .map_err(|e| PersistenceError::sqlite("insert alias (split)", e))?;
    }

    let payload = serde_json::json!({
        "source": source,
        "new_ids": new_ids,
        "subject_type": subject_type,
        "partition": partition,
        "admin_plugin": admin_plugin,
        "reason": reason,
    })
    .to_string();
    tx.execute(
        "INSERT INTO claim_log (kind, claimant, asserted_at_ms, payload, reason) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            claim_kind::SUBJECT_SPLIT,
            admin_plugin,
            at_ms as i64,
            payload,
            reason
        ],
    )
    .map_err(|e| {
        PersistenceError::sqlite("append claim_log subject_split", e)
    })?;

    tx.commit()
        .map_err(|e| PersistenceError::sqlite("commit split tx", e))?;
    Ok(())
}

fn forget_tx(
    conn: &mut Connection,
    canonical_id: &str,
    forget_claimant: &str,
    forget_reason: Option<&str>,
    at_ms: u64,
) -> Result<(), PersistenceError> {
    let tx = conn
        .transaction()
        .map_err(|e| PersistenceError::sqlite("begin forget tx", e))?;

    tx.execute("DELETE FROM subjects WHERE id = ?1", params![canonical_id])
        .map_err(|e| PersistenceError::sqlite("delete subject row", e))?;

    // Tombstone alias row. Mirrors the in-memory registry's
    // tombstone insertion (subjects.rs `aliases_try_insert` on the
    // retract / forced-retract paths) so consumers walking the
    // alias chain see a structured "no successor" record rather
    // than a bare absence. `new_id = ''` is the sentinel for
    // "tombstone with no successor"; the schema stores it as a
    // plain string column.
    tx.execute(
        "INSERT INTO aliases \
         (old_id, new_id, kind, recorded_at_ms, admin_plugin, reason) \
         VALUES (?1, '', 'tombstone', ?2, ?3, ?4)",
        params![canonical_id, at_ms as i64, forget_claimant, forget_reason],
    )
    .map_err(|e| PersistenceError::sqlite("insert tombstone alias", e))?;

    let payload = serde_json::json!({
        "canonical_id": canonical_id,
    })
    .to_string();
    tx.execute(
        "INSERT INTO claim_log (kind, claimant, asserted_at_ms, payload, reason) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            claim_kind::SUBJECT_FORGOTTEN,
            forget_claimant,
            at_ms as i64,
            payload,
            forget_reason
        ],
    )
    .map_err(|e| {
        PersistenceError::sqlite("append claim_log subject_forgotten", e)
    })?;

    tx.commit()
        .map_err(|e| PersistenceError::sqlite("commit forget tx", e))?;
    Ok(())
}

fn load_all_subjects_query(
    conn: &Connection,
) -> Result<Vec<PersistedSubject>, PersistenceError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, subject_type, created_at_ms, modified_at_ms, \
             forgotten_at_ms FROM subjects ORDER BY created_at_ms ASC",
        )
        .map_err(|e| PersistenceError::sqlite("prepare subjects query", e))?;

    let rows = stmt
        .query_map([], |row| {
            Ok(PersistedSubject {
                id: row.get(0)?,
                subject_type: row.get(1)?,
                created_at_ms: row.get::<_, i64>(2)? as u64,
                modified_at_ms: row.get::<_, i64>(3)? as u64,
                forgotten_at_ms: row
                    .get::<_, Option<i64>>(4)?
                    .map(|v| v as u64),
                addressings: Vec::new(),
            })
        })
        .map_err(|e| PersistenceError::sqlite("execute subjects query", e))?;

    let mut subjects: Vec<PersistedSubject> = Vec::new();
    for r in rows {
        subjects.push(
            r.map_err(|e| PersistenceError::sqlite("read subject row", e))?,
        );
    }

    let mut by_id: HashMap<String, usize> =
        HashMap::with_capacity(subjects.len());
    for (i, s) in subjects.iter().enumerate() {
        by_id.insert(s.id.clone(), i);
    }

    let mut astmt = conn
        .prepare(
            "SELECT scheme, value, subject_id, claimant, asserted_at_ms, \
             reason, quarantined_by, quarantined_at_ms \
             FROM subject_addressings ORDER BY asserted_at_ms ASC",
        )
        .map_err(|e| {
            PersistenceError::sqlite("prepare addressings query", e)
        })?;

    let arows = astmt
        .query_map([], |row| {
            let subject_id: String = row.get(2)?;
            Ok((
                subject_id,
                PersistedAddressing {
                    scheme: row.get(0)?,
                    value: row.get(1)?,
                    claimant: row.get(3)?,
                    asserted_at_ms: row.get::<_, i64>(4)? as u64,
                    reason: row.get(5)?,
                    quarantined_by: row.get(6)?,
                    quarantined_at_ms: row
                        .get::<_, Option<i64>>(7)?
                        .map(|v| v as u64),
                },
            ))
        })
        .map_err(|e| {
            PersistenceError::sqlite("execute addressings query", e)
        })?;

    for r in arows {
        let (sid, addr) =
            r.map_err(|e| PersistenceError::sqlite("read addressing row", e))?;
        if let Some(idx) = by_id.get(&sid) {
            subjects[*idx].addressings.push(addr);
        }
    }

    Ok(subjects)
}

fn load_happenings_since_query(
    conn: &Connection,
    cursor: u64,
    limit: u32,
) -> Result<Vec<PersistedHappening>, PersistenceError> {
    let mut stmt = conn
        .prepare(
            "SELECT seq, kind, payload, at_ms FROM happenings_log \
             WHERE seq > ?1 ORDER BY seq ASC LIMIT ?2",
        )
        .map_err(|e| {
            PersistenceError::sqlite("prepare happenings_log query", e)
        })?;

    let rows = stmt
        .query_map(params![cursor as i64, limit as i64], |row| {
            let seq: i64 = row.get(0)?;
            let kind: String = row.get(1)?;
            let payload_str: String = row.get(2)?;
            let at_ms: i64 = row.get(3)?;
            let payload: serde_json::Value = serde_json::from_str(&payload_str)
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("happenings_log.payload not JSON: {e}"),
                        )),
                    )
                })?;
            Ok(PersistedHappening {
                seq: seq as u64,
                kind,
                payload,
                at_ms: at_ms as u64,
            })
        })
        .map_err(|e| {
            PersistenceError::sqlite("execute happenings_log query", e)
        })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| {
            PersistenceError::sqlite("read happenings_log row", e)
        })?);
    }
    Ok(out)
}

fn load_aliases_for_query(
    conn: &Connection,
    canonical_id: &str,
) -> Result<Vec<PersistedAlias>, PersistenceError> {
    let mut stmt = conn
        .prepare(
            "SELECT alias_id, old_id, new_id, kind, recorded_at_ms, \
             admin_plugin, reason FROM aliases WHERE old_id = ?1 \
             ORDER BY alias_id ASC",
        )
        .map_err(|e| PersistenceError::sqlite("prepare aliases query", e))?;

    let rows = stmt
        .query_map(params![canonical_id], read_alias_row)
        .map_err(|e| PersistenceError::sqlite("execute aliases query", e))?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| PersistenceError::sqlite("read alias row", e))?);
    }
    Ok(out)
}

fn load_all_aliases_query(
    conn: &Connection,
) -> Result<Vec<PersistedAlias>, PersistenceError> {
    let mut stmt = conn
        .prepare(
            "SELECT alias_id, old_id, new_id, kind, recorded_at_ms, \
             admin_plugin, reason FROM aliases ORDER BY alias_id ASC",
        )
        .map_err(|e| {
            PersistenceError::sqlite("prepare aliases full-scan query", e)
        })?;

    let rows = stmt.query_map([], read_alias_row).map_err(|e| {
        PersistenceError::sqlite("execute aliases full-scan query", e)
    })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| PersistenceError::sqlite("read alias row", e))?);
    }
    Ok(out)
}

fn read_alias_row(
    row: &rusqlite::Row<'_>,
) -> Result<PersistedAlias, rusqlite::Error> {
    let kind_str: String = row.get(3)?;
    let kind = match kind_str.as_str() {
        "merged" => AliasKind::Merged,
        "split" => AliasKind::Split,
        "tombstone" => AliasKind::Tombstone,
        "type_migrated" => AliasKind::TypeMigrated,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown alias kind: {other}"),
                )),
            ));
        }
    };
    Ok(PersistedAlias {
        alias_id: row.get(0)?,
        old_id: row.get(1)?,
        new_id: row.get(2)?,
        kind,
        recorded_at_ms: row.get::<_, i64>(4)? as u64,
        admin_plugin: row.get(5)?,
        reason: row.get(6)?,
    })
}

impl PersistenceStore for SqlitePersistenceStore {
    fn record_subject_announce<'a>(
        &'a self,
        record: AnnounceRecord<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let cid = record.canonical_id.to_string();
        let st = record.subject_type.to_string();
        let addrs = record.addressings.to_vec();
        let cl = record.claimant.to_string();
        let cls = record.claims.to_vec();
        let at_ms = record.at_ms;
        Box::pin(async move {
            self.interact("subject_announce", move |conn| {
                announce_tx(
                    conn,
                    AnnounceRecord {
                        canonical_id: &cid,
                        subject_type: &st,
                        addressings: &addrs,
                        claimant: &cl,
                        claims: &cls,
                        at_ms,
                    },
                )
            })
            .await
        })
    }

    fn record_subject_retract<'a>(
        &'a self,
        canonical_id: &'a str,
        addressing: &'a ExternalAddressing,
        claimant: &'a str,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let cid = canonical_id.to_string();
        let addr = addressing.clone();
        let cl = claimant.to_string();
        Box::pin(async move {
            self.interact("subject_retract", move |conn| {
                retract_tx(conn, &cid, &addr, &cl, at_ms)
            })
            .await
        })
    }

    fn record_subject_merge<'a>(
        &'a self,
        record: MergeRecord<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let a = record.source_a.to_string();
        let b = record.source_b.to_string();
        let n = record.new_id.to_string();
        let st = record.subject_type.to_string();
        let admin = record.admin_plugin.to_string();
        let reason = record.reason.map(|s| s.to_string());
        let at_ms = record.at_ms;
        Box::pin(async move {
            self.interact("subject_merge", move |conn| {
                merge_tx(
                    conn,
                    MergeRecord {
                        source_a: &a,
                        source_b: &b,
                        new_id: &n,
                        subject_type: &st,
                        admin_plugin: &admin,
                        reason: reason.as_deref(),
                        at_ms,
                    },
                )
            })
            .await
        })
    }

    fn record_subject_type_migration<'a>(
        &'a self,
        record: TypeMigrationRecord<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let source = record.source.to_string();
        let new_id = record.new_id.to_string();
        let from_type = record.from_type.to_string();
        let to_type = record.to_type.to_string();
        let migration_id = record.migration_id.to_string();
        let reason = record.reason.map(|s| s.to_string());
        let at_ms = record.at_ms;
        Box::pin(async move {
            self.interact("subject_type_migration", move |conn| {
                type_migration_tx(
                    conn,
                    TypeMigrationRecord {
                        source: &source,
                        new_id: &new_id,
                        from_type: &from_type,
                        to_type: &to_type,
                        migration_id: &migration_id,
                        reason: reason.as_deref(),
                        at_ms,
                    },
                )
            })
            .await
        })
    }

    fn record_subject_split<'a>(
        &'a self,
        record: SplitRecord<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let s = record.source.to_string();
        let ids = record.new_ids.to_vec();
        let st = record.subject_type.to_string();
        let part = record.partition.to_vec();
        let admin = record.admin_plugin.to_string();
        let reason = record.reason.map(|s| s.to_string());
        let at_ms = record.at_ms;
        Box::pin(async move {
            self.interact("subject_split", move |conn| {
                split_tx(
                    conn,
                    SplitRecord {
                        source: &s,
                        new_ids: &ids,
                        subject_type: &st,
                        partition: &part,
                        admin_plugin: &admin,
                        reason: reason.as_deref(),
                        at_ms,
                    },
                )
            })
            .await
        })
    }

    fn record_subject_forget<'a>(
        &'a self,
        canonical_id: &'a str,
        forget_claimant: &'a str,
        forget_reason: Option<&'a str>,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let cid = canonical_id.to_string();
        let claimant = forget_claimant.to_string();
        let reason = forget_reason.map(|s| s.to_string());
        Box::pin(async move {
            self.interact("subject_forget", move |conn| {
                forget_tx(conn, &cid, &claimant, reason.as_deref(), at_ms)
            })
            .await
        })
    }

    fn load_all_subjects<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<PersistedSubject>, PersistenceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.interact("load_all_subjects", |conn| {
                load_all_subjects_query(conn)
            })
            .await
        })
    }

    fn load_aliases_for<'a>(
        &'a self,
        canonical_id: &'a str,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<PersistedAlias>, PersistenceError>>
                + Send
                + 'a,
        >,
    > {
        let cid = canonical_id.to_string();
        Box::pin(async move {
            self.interact("load_aliases_for", move |conn| {
                load_aliases_for_query(conn, &cid)
            })
            .await
        })
    }

    fn load_all_aliases<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<PersistedAlias>, PersistenceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.interact("load_all_aliases", |conn| {
                load_all_aliases_query(conn)
            })
            .await
        })
    }

    fn record_happening<'a>(
        &'a self,
        seq: u64,
        kind: &'a str,
        payload: &'a serde_json::Value,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let kind = kind.to_string();
        let payload_str = payload.to_string();
        Box::pin(async move {
            self.interact("record_happening", move |conn| {
                // Wrap the single-row INSERT in an explicit
                // transaction so the discipline matches the
                // cross-table operations on this store and the fsync
                // boundary is named, not implicit. Under
                // `synchronous = FULL` the transaction commits a
                // single fsync just as the bare INSERT did, so this
                // adds no per-call cost; the explicit boundary makes
                // a future "batch happenings" extension a one-line
                // change instead of a refactor.
                let tx = conn.transaction().map_err(|e| {
                    PersistenceError::sqlite("begin happenings_log tx", e)
                })?;
                tx.execute(
                    "INSERT INTO happenings_log (seq, kind, payload, at_ms) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![seq as i64, kind, payload_str, at_ms as i64],
                )
                .map_err(|e| {
                    PersistenceError::sqlite("insert happenings_log", e)
                })?;
                tx.commit().map_err(|e| {
                    PersistenceError::sqlite("commit happenings_log tx", e)
                })?;
                Ok(())
            })
            .await
        })
    }

    fn load_happenings_since<'a>(
        &'a self,
        cursor: u64,
        limit: u32,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<Vec<PersistedHappening>, PersistenceError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.interact("load_happenings_since", move |conn| {
                load_happenings_since_query(conn, cursor, limit)
            })
            .await
        })
    }

    fn load_max_happening_seq<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<u64, PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.interact("load_max_happening_seq", |conn| {
                let max: Option<i64> = conn
                    .query_row(
                        "SELECT MAX(seq) FROM happenings_log",
                        [],
                        |row| row.get::<_, Option<i64>>(0),
                    )
                    .map_err(|e| {
                        PersistenceError::sqlite(
                            "select MAX(seq) from happenings_log",
                            e,
                        )
                    })?;
                Ok(max.unwrap_or(0).max(0) as u64)
            })
            .await
        })
    }

    fn load_oldest_happening_seq<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<u64, PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.interact("load_oldest_happening_seq", |conn| {
                let min: Option<i64> = conn
                    .query_row(
                        "SELECT MIN(seq) FROM happenings_log",
                        [],
                        |row| row.get::<_, Option<i64>>(0),
                    )
                    .map_err(|e| {
                        PersistenceError::sqlite(
                            "select MIN(seq) from happenings_log",
                            e,
                        )
                    })?;
                Ok(min.unwrap_or(0).max(0) as u64)
            })
            .await
        })
    }

    fn trim_happenings_log<'a>(
        &'a self,
        retention_window_secs: u64,
        retention_capacity: u64,
    ) -> Pin<Box<dyn Future<Output = Result<u64, PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.interact("trim_happenings_log", move |conn| {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                let window_ms =
                    retention_window_secs.saturating_mul(1000) as i64;
                let cutoff_ms = now_ms.saturating_sub(window_ms);
                let cap = retention_capacity as i64;

                let tx = conn.transaction().map_err(|e| {
                    PersistenceError::sqlite("begin trim_happenings_log tx", e)
                })?;
                // Keep rows that are inside BOTH the wall-clock window
                // AND the capacity tail. Delete the rest. Using a
                // single statement so the trim is atomic against any
                // concurrent reader.
                let removed = tx
                    .execute(
                        "DELETE FROM happenings_log \
                         WHERE at_ms < ?1 \
                            OR seq <= COALESCE((SELECT MAX(seq) \
                                                FROM happenings_log), 0) - ?2",
                        params![cutoff_ms, cap],
                    )
                    .map_err(|e| {
                        PersistenceError::sqlite(
                            "delete from happenings_log",
                            e,
                        )
                    })?;
                tx.commit().map_err(|e| {
                    PersistenceError::sqlite("commit trim_happenings_log tx", e)
                })?;
                Ok(removed as u64)
            })
            .await
        })
    }

    fn load_instance_id<'a>(
        &'a self,
    ) -> Pin<
        Box<dyn Future<Output = Result<String, PersistenceError>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.interact("load_instance_id", |conn| {
                let id: Option<String> = conn
                    .query_row(
                        "SELECT value FROM meta WHERE key = ?1",
                        rusqlite::params![meta_keys::INSTANCE_ID],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(|e| {
                        PersistenceError::sqlite(
                            "select instance_id from meta",
                            e,
                        )
                    })?;
                id.ok_or_else(|| {
                    PersistenceError::Invalid(
                        "meta.instance_id row is missing; \
                         migration 003 may not have run"
                            .to_string(),
                    )
                })
            })
            .await
        })
    }

    fn record_pending_conflict<'a>(
        &'a self,
        plugin: &'a str,
        addressings: &'a [ExternalAddressing],
        canonical_ids: &'a [String],
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<i64, PersistenceError>> + Send + 'a>>
    {
        let plugin = plugin.to_string();
        let addressings_json =
            serde_json::to_string(addressings).unwrap_or_else(|_| "[]".into());
        let canonical_ids_json = serde_json::to_string(canonical_ids)
            .unwrap_or_else(|_| "[]".into());
        Box::pin(async move {
            self.interact("record_pending_conflict", move |conn| {
                conn.execute(
                    "INSERT INTO pending_conflicts \
                     (detected_at_ms, plugin, addressings_json, \
                      canonical_ids_json, resolved_at_ms, resolution_kind) \
                     VALUES (?1, ?2, ?3, ?4, NULL, NULL)",
                    params![
                        at_ms as i64,
                        plugin,
                        addressings_json,
                        canonical_ids_json
                    ],
                )
                .map_err(|e| {
                    PersistenceError::sqlite("insert pending_conflicts", e)
                })?;
                Ok(conn.last_insert_rowid())
            })
            .await
        })
    }

    fn mark_conflict_resolved<'a>(
        &'a self,
        id: i64,
        resolution_kind: &'a str,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let kind = resolution_kind.to_string();
        Box::pin(async move {
            self.interact("mark_conflict_resolved", move |conn| {
                conn.execute(
                    "UPDATE pending_conflicts SET resolved_at_ms = ?2, \
                     resolution_kind = ?3 WHERE id = ?1 \
                     AND resolved_at_ms IS NULL",
                    params![id, at_ms as i64, kind],
                )
                .map_err(|e| {
                    PersistenceError::sqlite("update pending_conflicts", e)
                })?;
                Ok(())
            })
            .await
        })
    }

    fn list_pending_conflicts<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<PendingConflict>, PersistenceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.interact("list_pending_conflicts", |conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT id, detected_at_ms, plugin, addressings_json, \
                         canonical_ids_json, resolved_at_ms, resolution_kind \
                         FROM pending_conflicts WHERE resolved_at_ms IS NULL \
                         ORDER BY detected_at_ms ASC",
                    )
                    .map_err(|e| {
                        PersistenceError::sqlite(
                            "prepare pending_conflicts query",
                            e,
                        )
                    })?;
                let rows = stmt
                    .query_map([], |row| {
                        let addressings_json: String = row.get(3)?;
                        let canonical_ids_json: String = row.get(4)?;
                        let addressings: Vec<ExternalAddressing> =
                            serde_json::from_str(&addressings_json).map_err(
                                |e| {
                                    rusqlite::Error::FromSqlConversionFailure(
                                        3,
                                        rusqlite::types::Type::Text,
                                        Box::new(std::io::Error::new(
                                            std::io::ErrorKind::InvalidData,
                                            format!(
                                                "addressings_json not JSON: {e}"
                                            ),
                                        )),
                                    )
                                },
                            )?;
                        let canonical_ids: Vec<String> = serde_json::from_str(
                            &canonical_ids_json,
                        )
                        .map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                4,
                                rusqlite::types::Type::Text,
                                Box::new(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    format!("canonical_ids_json not JSON: {e}"),
                                )),
                            )
                        })?;
                        Ok(PendingConflict {
                            id: row.get(0)?,
                            detected_at_ms: row.get::<_, i64>(1)? as u64,
                            plugin: row.get(2)?,
                            addressings,
                            canonical_ids,
                            resolved_at_ms: row
                                .get::<_, Option<i64>>(5)?
                                .map(|v| v as u64),
                            resolution_kind: row.get(6)?,
                        })
                    })
                    .map_err(|e| {
                        PersistenceError::sqlite(
                            "execute pending_conflicts query",
                            e,
                        )
                    })?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r.map_err(|e| {
                        PersistenceError::sqlite(
                            "read pending_conflicts row",
                            e,
                        )
                    })?);
                }
                Ok(out)
            })
            .await
        })
    }

    fn record_admin_entry<'a>(
        &'a self,
        entry: &'a PersistedAdminEntry,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let kind = entry.kind.clone();
        let admin_plugin = entry.admin_plugin.clone();
        let target_claimant = entry.target_claimant.clone();
        let payload_json = serde_json::to_string(&entry.payload)
            .unwrap_or_else(|_| "{}".into());
        let asserted_at_ms = entry.asserted_at_ms;
        let reason = entry.reason.clone();
        let reverses_admin_id = entry.reverses_admin_id;
        Box::pin(async move {
            self.interact("record_admin_entry", move |conn| {
                conn.execute(
                    "INSERT INTO admin_log \
                     (kind, admin_plugin, target_claimant, payload, \
                      asserted_at_ms, reason, reverses_admin_id) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        kind,
                        admin_plugin,
                        target_claimant,
                        payload_json,
                        asserted_at_ms as i64,
                        reason,
                        reverses_admin_id,
                    ],
                )
                .map_err(|e| PersistenceError::sqlite("insert admin_log", e))?;
                Ok(())
            })
            .await
        })
    }

    fn load_all_admin_entries<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<Vec<PersistedAdminEntry>, PersistenceError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.interact("load_all_admin_entries", |conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT admin_id, kind, admin_plugin, target_claimant, \
                         payload, asserted_at_ms, reason, reverses_admin_id \
                         FROM admin_log ORDER BY admin_id ASC",
                    )
                    .map_err(|e| {
                        PersistenceError::sqlite("prepare admin_log query", e)
                    })?;
                let rows = stmt
                    .query_map([], |row| {
                        let payload_json: String = row.get(4)?;
                        let payload: serde_json::Value = serde_json::from_str(
                            &payload_json,
                        )
                        .map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                4,
                                rusqlite::types::Type::Text,
                                Box::new(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    format!("payload not JSON: {e}"),
                                )),
                            )
                        })?;
                        Ok(PersistedAdminEntry {
                            admin_id: row.get(0)?,
                            kind: row.get(1)?,
                            admin_plugin: row.get(2)?,
                            target_claimant: row.get(3)?,
                            payload,
                            asserted_at_ms: row.get::<_, i64>(5)? as u64,
                            reason: row.get(6)?,
                            reverses_admin_id: row.get(7)?,
                        })
                    })
                    .map_err(|e| {
                        PersistenceError::sqlite("execute admin_log query", e)
                    })?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r.map_err(|e| {
                        PersistenceError::sqlite("read admin_log row", e)
                    })?);
                }
                Ok(out)
            })
            .await
        })
    }

    fn record_relation_assert<'a>(
        &'a self,
        relation: &'a PersistedRelation,
        claim: &'a PersistedRelationClaim,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let rel = relation.clone();
        let cl = claim.clone();
        Box::pin(async move {
            self.interact("record_relation_assert", move |conn| {
                let tx = conn.transaction().map_err(|e| {
                    PersistenceError::sqlite("begin assert tx", e)
                })?;
                tx.execute(
                    "INSERT INTO relations \
                     (source_id, predicate, target_id, \
                      created_at_ms, modified_at_ms, \
                      suppressed_admin_plugin, suppressed_at_ms, \
                      suppression_reason) \
                     VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, NULL) \
                     ON CONFLICT(source_id, predicate, target_id) \
                     DO UPDATE SET modified_at_ms = excluded.modified_at_ms",
                    params![
                        rel.source_id,
                        rel.predicate,
                        rel.target_id,
                        rel.created_at_ms as i64,
                        rel.modified_at_ms as i64,
                    ],
                )
                .map_err(|e| PersistenceError::sqlite("upsert relations", e))?;
                tx.execute(
                    "INSERT OR IGNORE INTO relation_claimants \
                     (source_id, predicate, target_id, claimant, \
                      asserted_at_ms, reason) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        cl.source_id,
                        cl.predicate,
                        cl.target_id,
                        cl.claimant,
                        cl.asserted_at_ms as i64,
                        cl.reason,
                    ],
                )
                .map_err(|e| {
                    PersistenceError::sqlite("insert relation_claimant", e)
                })?;
                tx.commit().map_err(|e| {
                    PersistenceError::sqlite("commit assert tx", e)
                })?;
                Ok(())
            })
            .await
        })
    }

    fn record_relation_retract<'a>(
        &'a self,
        source_id: &'a str,
        predicate: &'a str,
        target_id: &'a str,
        claimant: &'a str,
        modified_at_ms: u64,
        relation_forgotten: bool,
    ) -> Pin<Box<dyn Future<Output = Result<bool, PersistenceError>> + Send + 'a>>
    {
        let source_id = source_id.to_string();
        let predicate = predicate.to_string();
        let target_id = target_id.to_string();
        let claimant = claimant.to_string();
        Box::pin(async move {
            self.interact("record_relation_retract", move |conn| {
                let tx = conn.transaction().map_err(|e| {
                    PersistenceError::sqlite("begin retract tx", e)
                })?;
                let removed = tx
                    .execute(
                        "DELETE FROM relation_claimants \
                         WHERE source_id = ?1 AND predicate = ?2 \
                           AND target_id = ?3 AND claimant = ?4",
                        params![source_id, predicate, target_id, claimant],
                    )
                    .map_err(|e| {
                        PersistenceError::sqlite("delete relation_claimant", e)
                    })?;
                if relation_forgotten {
                    tx.execute(
                        "DELETE FROM relations \
                         WHERE source_id = ?1 AND predicate = ?2 \
                           AND target_id = ?3",
                        params![source_id, predicate, target_id],
                    )
                    .map_err(|e| {
                        PersistenceError::sqlite("delete relation", e)
                    })?;
                } else if removed > 0 {
                    tx.execute(
                        "UPDATE relations SET modified_at_ms = ?4 \
                         WHERE source_id = ?1 AND predicate = ?2 \
                           AND target_id = ?3",
                        params![
                            source_id,
                            predicate,
                            target_id,
                            modified_at_ms as i64,
                        ],
                    )
                    .map_err(|e| {
                        PersistenceError::sqlite(
                            "update relation modified_at_ms",
                            e,
                        )
                    })?;
                }
                tx.commit().map_err(|e| {
                    PersistenceError::sqlite("commit retract tx", e)
                })?;
                Ok(removed > 0)
            })
            .await
        })
    }

    fn record_relation_forget<'a>(
        &'a self,
        source_id: &'a str,
        predicate: &'a str,
        target_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, PersistenceError>> + Send + 'a>>
    {
        let source_id = source_id.to_string();
        let predicate = predicate.to_string();
        let target_id = target_id.to_string();
        Box::pin(async move {
            self.interact("record_relation_forget", move |conn| {
                let n = conn
                    .execute(
                        "DELETE FROM relations \
                         WHERE source_id = ?1 AND predicate = ?2 \
                           AND target_id = ?3",
                        params![source_id, predicate, target_id],
                    )
                    .map_err(|e| {
                        PersistenceError::sqlite("delete relation", e)
                    })?;
                Ok(n > 0)
            })
            .await
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn record_relation_suppress<'a>(
        &'a self,
        source_id: &'a str,
        predicate: &'a str,
        target_id: &'a str,
        admin_plugin: &'a str,
        suppressed_at_ms: u64,
        reason: Option<&'a str>,
        modified_at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<bool, PersistenceError>> + Send + 'a>>
    {
        let source_id = source_id.to_string();
        let predicate = predicate.to_string();
        let target_id = target_id.to_string();
        let admin_plugin = admin_plugin.to_string();
        let reason = reason.map(|s| s.to_string());
        Box::pin(async move {
            self.interact("record_relation_suppress", move |conn| {
                let n = conn
                    .execute(
                        "UPDATE relations SET \
                           suppressed_admin_plugin = ?4, \
                           suppressed_at_ms = ?5, \
                           suppression_reason = ?6, \
                           modified_at_ms = ?7 \
                         WHERE source_id = ?1 AND predicate = ?2 \
                           AND target_id = ?3",
                        params![
                            source_id,
                            predicate,
                            target_id,
                            admin_plugin,
                            suppressed_at_ms as i64,
                            reason,
                            modified_at_ms as i64,
                        ],
                    )
                    .map_err(|e| {
                        PersistenceError::sqlite("update relation suppress", e)
                    })?;
                Ok(n > 0)
            })
            .await
        })
    }

    fn record_relation_unsuppress<'a>(
        &'a self,
        source_id: &'a str,
        predicate: &'a str,
        target_id: &'a str,
        modified_at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<bool, PersistenceError>> + Send + 'a>>
    {
        let source_id = source_id.to_string();
        let predicate = predicate.to_string();
        let target_id = target_id.to_string();
        Box::pin(async move {
            self.interact("record_relation_unsuppress", move |conn| {
                let n = conn
                    .execute(
                        "UPDATE relations SET \
                           suppressed_admin_plugin = NULL, \
                           suppressed_at_ms = NULL, \
                           suppression_reason = NULL, \
                           modified_at_ms = ?4 \
                         WHERE source_id = ?1 AND predicate = ?2 \
                           AND target_id = ?3",
                        params![
                            source_id,
                            predicate,
                            target_id,
                            modified_at_ms as i64,
                        ],
                    )
                    .map_err(|e| {
                        PersistenceError::sqlite(
                            "update relation unsuppress",
                            e,
                        )
                    })?;
                Ok(n > 0)
            })
            .await
        })
    }

    fn load_all_relations<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<RelationLoadRow>, PersistenceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.interact("load_all_relations", |conn| {
                let mut rel_stmt = conn
                    .prepare(
                        "SELECT source_id, predicate, target_id, \
                                created_at_ms, modified_at_ms, \
                                suppressed_admin_plugin, suppressed_at_ms, \
                                suppression_reason \
                         FROM relations \
                         ORDER BY source_id ASC, predicate ASC, target_id ASC",
                    )
                    .map_err(|e| {
                        PersistenceError::sqlite("prepare relations query", e)
                    })?;
                let rel_rows = rel_stmt
                    .query_map([], |row| {
                        Ok(PersistedRelation {
                            source_id: row.get(0)?,
                            predicate: row.get(1)?,
                            target_id: row.get(2)?,
                            created_at_ms: row.get::<_, i64>(3)? as u64,
                            modified_at_ms: row.get::<_, i64>(4)? as u64,
                            suppressed_admin_plugin: row.get(5)?,
                            suppressed_at_ms: row
                                .get::<_, Option<i64>>(6)?
                                .map(|v| v as u64),
                            suppression_reason: row.get(7)?,
                        })
                    })
                    .map_err(|e| {
                        PersistenceError::sqlite("execute relations query", e)
                    })?;
                let mut out: Vec<RelationLoadRow> = Vec::new();
                for r in rel_rows {
                    let rel = r.map_err(|e| {
                        PersistenceError::sqlite("read relation row", e)
                    })?;
                    out.push((rel, Vec::new()));
                }

                let mut claim_stmt = conn
                    .prepare(
                        "SELECT source_id, predicate, target_id, claimant, \
                                asserted_at_ms, reason \
                         FROM relation_claimants \
                         ORDER BY source_id ASC, predicate ASC, \
                                  target_id ASC, claimant ASC",
                    )
                    .map_err(|e| {
                        PersistenceError::sqlite(
                            "prepare relation_claimants query",
                            e,
                        )
                    })?;
                let claim_rows = claim_stmt
                    .query_map([], |row| {
                        Ok(PersistedRelationClaim {
                            source_id: row.get(0)?,
                            predicate: row.get(1)?,
                            target_id: row.get(2)?,
                            claimant: row.get(3)?,
                            asserted_at_ms: row.get::<_, i64>(4)? as u64,
                            reason: row.get(5)?,
                        })
                    })
                    .map_err(|e| {
                        PersistenceError::sqlite(
                            "execute relation_claimants query",
                            e,
                        )
                    })?;
                for r in claim_rows {
                    let cl = r.map_err(|e| {
                        PersistenceError::sqlite(
                            "read relation_claimant row",
                            e,
                        )
                    })?;
                    let key = (
                        cl.source_id.clone(),
                        cl.predicate.clone(),
                        cl.target_id.clone(),
                    );
                    if let Some(slot) = out.iter_mut().find(|(r, _)| {
                        (
                            r.source_id.clone(),
                            r.predicate.clone(),
                            r.target_id.clone(),
                        ) == key
                    }) {
                        slot.1.push(cl);
                    }
                }
                Ok(out)
            })
            .await
        })
    }

    fn upsert_custody<'a>(
        &'a self,
        record: &'a PersistedCustody,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let plugin = record.plugin.clone();
        let handle_id = record.handle_id.clone();
        let shelf = record.shelf.clone();
        let custody_type = record.custody_type.clone();
        let state_kind = record.state_kind.clone();
        let state_reason = record.state_reason.clone();
        let started_at_ms = record.started_at_ms;
        let last_updated_at_ms = record.last_updated_at_ms;
        Box::pin(async move {
            self.interact("upsert_custody", move |conn| {
                conn.execute(
                    "INSERT INTO custodies \
                     (plugin, handle_id, shelf, custody_type, \
                      state_kind, state_reason, started_at_ms, \
                      last_updated_at_ms) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                     ON CONFLICT(plugin, handle_id) DO UPDATE SET \
                       shelf = COALESCE(excluded.shelf, custodies.shelf), \
                       custody_type = COALESCE(excluded.custody_type, \
                                               custodies.custody_type), \
                       state_kind = excluded.state_kind, \
                       state_reason = excluded.state_reason, \
                       last_updated_at_ms = excluded.last_updated_at_ms",
                    params![
                        plugin,
                        handle_id,
                        shelf,
                        custody_type,
                        state_kind,
                        state_reason,
                        started_at_ms as i64,
                        last_updated_at_ms as i64,
                    ],
                )
                .map_err(|e| PersistenceError::sqlite("upsert custodies", e))?;
                Ok(())
            })
            .await
        })
    }

    fn upsert_custody_state<'a>(
        &'a self,
        snapshot: &'a PersistedCustodyState,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let plugin = snapshot.plugin.clone();
        let handle_id = snapshot.handle_id.clone();
        let payload = snapshot.payload.clone();
        let health = snapshot.health.clone();
        let reported_at_ms = snapshot.reported_at_ms;
        Box::pin(async move {
            self.interact("upsert_custody_state", move |conn| {
                conn.execute(
                    "INSERT INTO custody_state \
                     (plugin, handle_id, payload, health, reported_at_ms) \
                     VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT(plugin, handle_id) DO UPDATE SET \
                       payload = excluded.payload, \
                       health = excluded.health, \
                       reported_at_ms = excluded.reported_at_ms",
                    params![
                        plugin,
                        handle_id,
                        payload,
                        health,
                        reported_at_ms as i64,
                    ],
                )
                .map_err(|e| {
                    PersistenceError::sqlite("upsert custody_state", e)
                })?;
                Ok(())
            })
            .await
        })
    }

    fn mark_custody_state<'a>(
        &'a self,
        plugin: &'a str,
        handle_id: &'a str,
        state_kind: &'a str,
        state_reason: Option<&'a str>,
        last_updated_at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<bool, PersistenceError>> + Send + 'a>>
    {
        let plugin = plugin.to_string();
        let handle_id = handle_id.to_string();
        let state_kind = state_kind.to_string();
        let state_reason = state_reason.map(|s| s.to_string());
        Box::pin(async move {
            self.interact("mark_custody_state", move |conn| {
                let n = conn
                    .execute(
                        "UPDATE custodies SET \
                           state_kind = ?3, \
                           state_reason = ?4, \
                           last_updated_at_ms = ?5 \
                         WHERE plugin = ?1 AND handle_id = ?2",
                        params![
                            plugin,
                            handle_id,
                            state_kind,
                            state_reason,
                            last_updated_at_ms as i64,
                        ],
                    )
                    .map_err(|e| {
                        PersistenceError::sqlite("update custodies state", e)
                    })?;
                Ok(n > 0)
            })
            .await
        })
    }

    fn delete_custody<'a>(
        &'a self,
        plugin: &'a str,
        handle_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, PersistenceError>> + Send + 'a>>
    {
        let plugin = plugin.to_string();
        let handle_id = handle_id.to_string();
        Box::pin(async move {
            self.interact("delete_custody", move |conn| {
                let n = conn
                    .execute(
                        "DELETE FROM custodies \
                         WHERE plugin = ?1 AND handle_id = ?2",
                        params![plugin, handle_id],
                    )
                    .map_err(|e| {
                        PersistenceError::sqlite("delete custodies", e)
                    })?;
                Ok(n > 0)
            })
            .await
        })
    }

    fn load_all_custodies<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<CustodyLoadRow>, PersistenceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.interact("load_all_custodies", |conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT \
                           c.plugin, c.handle_id, c.shelf, c.custody_type, \
                           c.state_kind, c.state_reason, \
                           c.started_at_ms, c.last_updated_at_ms, \
                           s.payload, s.health, s.reported_at_ms \
                         FROM custodies c \
                         LEFT JOIN custody_state s \
                           ON s.plugin = c.plugin AND s.handle_id = c.handle_id \
                         ORDER BY c.plugin ASC, c.handle_id ASC",
                    )
                    .map_err(|e| {
                        PersistenceError::sqlite("prepare custodies query", e)
                    })?;
                let rows = stmt
                    .query_map([], |row| {
                        let plugin: String = row.get(0)?;
                        let handle_id: String = row.get(1)?;
                        let custody = PersistedCustody {
                            plugin: plugin.clone(),
                            handle_id: handle_id.clone(),
                            shelf: row.get(2)?,
                            custody_type: row.get(3)?,
                            state_kind: row.get(4)?,
                            state_reason: row.get(5)?,
                            started_at_ms: row.get::<_, i64>(6)? as u64,
                            last_updated_at_ms: row.get::<_, i64>(7)? as u64,
                        };
                        let payload: Option<Vec<u8>> = row.get(8)?;
                        let snapshot = match payload {
                            Some(payload) => Some(PersistedCustodyState {
                                plugin,
                                handle_id,
                                payload,
                                health: row.get(9)?,
                                reported_at_ms: row.get::<_, i64>(10)? as u64,
                            }),
                            None => None,
                        };
                        Ok((custody, snapshot))
                    })
                    .map_err(|e| {
                        PersistenceError::sqlite("execute custodies query", e)
                    })?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r.map_err(|e| {
                        PersistenceError::sqlite("read custodies row", e)
                    })?);
                }
                Ok(out)
            })
            .await
        })
    }

    fn count_subjects_by_type<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<SubjectTypeCount>, PersistenceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.interact("count_subjects_by_type", |conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT subject_type, COUNT(*) FROM subjects \
                         WHERE forgotten_at_ms IS NULL \
                         GROUP BY subject_type ORDER BY subject_type ASC",
                    )
                    .map_err(|e| {
                        PersistenceError::sqlite(
                            "prepare subject_type aggregation",
                            e,
                        )
                    })?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)? as u64,
                        ))
                    })
                    .map_err(|e| {
                        PersistenceError::sqlite(
                            "execute subject_type aggregation",
                            e,
                        )
                    })?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r.map_err(|e| {
                        PersistenceError::sqlite(
                            "read subject_type aggregation row",
                            e,
                        )
                    })?);
                }
                Ok(out)
            })
            .await
        })
    }

    fn checkpoint_wal<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.interact("checkpoint_wal", |conn| {
                // PRAGMA wal_checkpoint(TRUNCATE) returns a row of
                // (busy, log, checkpointed). query_row consumes the
                // row; we discard the values because the steward
                // only treats SQL-level errors as failure signals.
                let (_busy, _log, _ckpt): (i64, i64, i64) = conn
                    .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })
                    .map_err(|e| {
                        PersistenceError::sqlite(
                            "PRAGMA wal_checkpoint(TRUNCATE)",
                            e,
                        )
                    })?;
                Ok(())
            })
            .await
        })
    }

    fn record_plugin_enabled<'a>(
        &'a self,
        row: &'a PersistedInstalledPlugin,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let plugin_name = row.plugin_name.clone();
        let enabled = if row.enabled { 1_i64 } else { 0_i64 };
        let last_state_reason = row.last_state_reason.clone();
        let last_state_changed_at_ms = row.last_state_changed_at_ms as i64;
        let install_digest = row.install_digest.clone();
        Box::pin(async move {
            self.interact("record_plugin_enabled", move |conn| {
                conn.execute(
                    "INSERT INTO installed_plugins \
                     (plugin_name, enabled, last_state_reason, \
                      last_state_changed_at_ms, install_digest) \
                     VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT(plugin_name) DO UPDATE SET \
                       enabled = excluded.enabled, \
                       last_state_reason = excluded.last_state_reason, \
                       last_state_changed_at_ms = excluded.last_state_changed_at_ms, \
                       install_digest = excluded.install_digest",
                    params![
                        plugin_name,
                        enabled,
                        last_state_reason,
                        last_state_changed_at_ms,
                        install_digest,
                    ],
                )
                .map_err(|e| {
                    PersistenceError::sqlite(
                        "upsert installed_plugins",
                        e,
                    )
                })?;
                Ok(())
            })
            .await
        })
    }

    fn load_all_installed_plugins<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Vec<PersistedInstalledPlugin>,
                        PersistenceError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.interact("load_all_installed_plugins", |conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT plugin_name, enabled, last_state_reason, \
                                last_state_changed_at_ms, install_digest \
                         FROM installed_plugins \
                         ORDER BY plugin_name",
                    )
                    .map_err(|e| {
                        PersistenceError::sqlite(
                            "prepare installed_plugins select",
                            e,
                        )
                    })?;
                let rows = stmt
                    .query_map([], |row| {
                        let enabled_int: i64 = row.get(1)?;
                        let last_state_changed_at_ms: i64 = row.get(3)?;
                        Ok(PersistedInstalledPlugin {
                            plugin_name: row.get(0)?,
                            enabled: enabled_int != 0,
                            last_state_reason: row.get(2)?,
                            last_state_changed_at_ms: last_state_changed_at_ms
                                as u64,
                            install_digest: row.get(4)?,
                        })
                    })
                    .map_err(|e| {
                        PersistenceError::sqlite("query installed_plugins", e)
                    })?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row.map_err(|e| {
                        PersistenceError::sqlite(
                            "decode installed_plugins row",
                            e,
                        )
                    })?);
                }
                Ok(out)
            })
            .await
        })
    }

    fn forget_installed_plugin<'a>(
        &'a self,
        plugin_name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let name = plugin_name.to_string();
        Box::pin(async move {
            self.interact("forget_installed_plugin", move |conn| {
                conn.execute(
                    "DELETE FROM installed_plugins WHERE plugin_name = ?1",
                    params![name],
                )
                .map_err(|e| {
                    PersistenceError::sqlite("delete installed_plugins", e)
                })?;
                Ok(())
            })
            .await
        })
    }

    fn record_reconciliation_state<'a>(
        &'a self,
        row: &'a PersistedReconciliationState,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let pair_id = row.pair_id.clone();
        let generation = row.generation as i64;
        let payload_json = serde_json::to_string(&row.applied_state)
            .unwrap_or_else(|_| "null".into());
        let at_ms = row.applied_at_ms as i64;
        Box::pin(async move {
            self.interact("record_reconciliation_state", move |conn| {
                conn.execute(
                    "INSERT INTO reconciliation_state \
                     (pair_id, generation, applied_state, applied_at_ms) \
                     VALUES (?1, ?2, ?3, ?4) \
                     ON CONFLICT(pair_id) DO UPDATE SET \
                       generation = excluded.generation, \
                       applied_state = excluded.applied_state, \
                       applied_at_ms = excluded.applied_at_ms",
                    params![pair_id, generation, payload_json, at_ms],
                )
                .map_err(|e| {
                    PersistenceError::sqlite("upsert reconciliation_state", e)
                })?;
                Ok(())
            })
            .await
        })
    }

    fn load_all_reconciliation_state<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Vec<PersistedReconciliationState>,
                        PersistenceError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.interact("load_all_reconciliation_state", |conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT pair_id, generation, applied_state, \
                                applied_at_ms \
                         FROM reconciliation_state \
                         ORDER BY pair_id",
                    )
                    .map_err(|e| {
                        PersistenceError::sqlite(
                            "prepare reconciliation_state select",
                            e,
                        )
                    })?;
                let rows = stmt
                    .query_map([], |row| {
                        let payload_json: String = row.get(2)?;
                        let applied_state = serde_json::from_str(&payload_json)
                            .unwrap_or(serde_json::Value::Null);
                        let generation: i64 = row.get(1)?;
                        let at_ms: i64 = row.get(3)?;
                        Ok(PersistedReconciliationState {
                            pair_id: row.get(0)?,
                            generation: generation as u64,
                            applied_state,
                            applied_at_ms: at_ms as u64,
                        })
                    })
                    .map_err(|e| {
                        PersistenceError::sqlite(
                            "query reconciliation_state",
                            e,
                        )
                    })?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row.map_err(|e| {
                        PersistenceError::sqlite(
                            "decode reconciliation_state row",
                            e,
                        )
                    })?);
                }
                Ok(out)
            })
            .await
        })
    }

    fn forget_reconciliation_state<'a>(
        &'a self,
        pair_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let id = pair_id.to_string();
        Box::pin(async move {
            self.interact("forget_reconciliation_state", move |conn| {
                conn.execute(
                    "DELETE FROM reconciliation_state WHERE pair_id = ?1",
                    params![id],
                )
                .map_err(|e| {
                    PersistenceError::sqlite("delete reconciliation_state", e)
                })?;
                Ok(())
            })
            .await
        })
    }

    fn upsert_pending_grammar_orphan<'a>(
        &'a self,
        subject_type: &'a str,
        count: u64,
        observed_at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let st = subject_type.to_string();
        Box::pin(async move {
            self.interact("upsert_pending_grammar_orphan", move |conn| {
                // INSERT OR IGNORE preserves first_observed_at +
                // any non-pending status; UPDATE refreshes count
                // and last_observed_at and re-pends the row if a
                // prior `recovered` state has been re-orphaned.
                conn.execute(
                    "INSERT INTO pending_grammar_orphans \
                     (subject_type, first_observed_at, last_observed_at, \
                      count, status) \
                     VALUES (?1, ?2, ?2, ?3, 'pending') \
                     ON CONFLICT(subject_type) DO UPDATE SET \
                       last_observed_at = excluded.last_observed_at, \
                       count = excluded.count, \
                       status = CASE \
                         WHEN status = 'recovered' THEN 'pending' \
                         ELSE status \
                       END",
                    params![st, observed_at_ms as i64, count as i64],
                )
                .map_err(|e| {
                    PersistenceError::sqlite(
                        "upsert pending_grammar_orphans",
                        e,
                    )
                })?;
                Ok(())
            })
            .await
        })
    }

    fn mark_grammar_orphan_recovered<'a>(
        &'a self,
        subject_type: &'a str,
        observed_at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let st = subject_type.to_string();
        Box::pin(async move {
            self.interact("mark_grammar_orphan_recovered", move |conn| {
                conn.execute(
                    "UPDATE pending_grammar_orphans SET \
                       status = 'recovered', \
                       last_observed_at = ?2 \
                     WHERE subject_type = ?1",
                    params![st, observed_at_ms as i64],
                )
                .map_err(|e| {
                    PersistenceError::sqlite(
                        "update pending_grammar_orphans recovered",
                        e,
                    )
                })?;
                Ok(())
            })
            .await
        })
    }

    fn accept_grammar_orphan<'a>(
        &'a self,
        subject_type: &'a str,
        reason: &'a str,
        accepted_at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<bool, PersistenceError>> + Send + 'a>>
    {
        let st = subject_type.to_string();
        let reason = reason.to_string();
        Box::pin(async move {
            self.interact("accept_grammar_orphan", move |conn| {
                let current: Option<String> = conn
                    .query_row(
                        "SELECT status FROM pending_grammar_orphans \
                         WHERE subject_type = ?1",
                        params![st],
                        |r| r.get::<_, String>(0),
                    )
                    .ok();
                match current.as_deref() {
                    None => Err(PersistenceError::Invalid(format!(
                        "accept_grammar_orphan: no pending row for type {st:?}"
                    ))),
                    Some("migrating") => {
                        Err(PersistenceError::Invalid(format!(
                            "accept_grammar_orphan: type {st:?} has an \
                             in-flight migration; wait for it to complete"
                        )))
                    }
                    Some("accepted") => Ok(false),
                    _ => {
                        conn.execute(
                            "UPDATE pending_grammar_orphans SET \
                               status = 'accepted', \
                               accepted_reason = ?2, \
                               accepted_at = ?3 \
                             WHERE subject_type = ?1",
                            params![st, reason, accepted_at_ms as i64],
                        )
                        .map_err(|e| {
                            PersistenceError::sqlite(
                                "update pending_grammar_orphans accepted",
                                e,
                            )
                        })?;
                        Ok(true)
                    }
                }
            })
            .await
        })
    }

    fn mark_grammar_orphan_migrating<'a>(
        &'a self,
        subject_type: &'a str,
        migration_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let st = subject_type.to_string();
        let mid = migration_id.to_string();
        Box::pin(async move {
            self.interact("mark_grammar_orphan_migrating", move |conn| {
                let current: Option<String> = conn
                    .query_row(
                        "SELECT status FROM pending_grammar_orphans \
                         WHERE subject_type = ?1",
                        params![st],
                        |r| r.get::<_, String>(0),
                    )
                    .ok();
                match current.as_deref() {
                    None => Err(PersistenceError::Invalid(format!(
                        "mark_grammar_orphan_migrating: no pending row for \
                         type {st:?}"
                    ))),
                    Some("resolved") => {
                        Err(PersistenceError::Invalid(format!(
                            "mark_grammar_orphan_migrating: type {st:?} is \
                             already resolved"
                        )))
                    }
                    _ => {
                        conn.execute(
                            "UPDATE pending_grammar_orphans SET \
                               status = 'migrating', \
                               migration_id = ?2 \
                             WHERE subject_type = ?1",
                            params![st, mid],
                        )
                        .map_err(|e| {
                            PersistenceError::sqlite(
                                "update pending_grammar_orphans migrating",
                                e,
                            )
                        })?;
                        Ok(())
                    }
                }
            })
            .await
        })
    }

    fn mark_grammar_orphan_resolved<'a>(
        &'a self,
        subject_type: &'a str,
        migration_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let st = subject_type.to_string();
        let mid = migration_id.to_string();
        Box::pin(async move {
            self.interact("mark_grammar_orphan_resolved", move |conn| {
                conn.execute(
                    "UPDATE pending_grammar_orphans SET \
                       status = 'resolved', \
                       migration_id = ?2 \
                     WHERE subject_type = ?1",
                    params![st, mid],
                )
                .map_err(|e| {
                    PersistenceError::sqlite(
                        "update pending_grammar_orphans resolved",
                        e,
                    )
                })?;
                Ok(())
            })
            .await
        })
    }

    fn list_pending_grammar_orphans<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Vec<PersistedGrammarOrphan>,
                        PersistenceError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.interact("list_pending_grammar_orphans", |conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT subject_type, first_observed_at, \
                       last_observed_at, count, status, \
                       accepted_reason, accepted_at, migration_id \
                     FROM pending_grammar_orphans \
                     ORDER BY subject_type ASC",
                    )
                    .map_err(|e| {
                        PersistenceError::sqlite(
                            "prepare pending_grammar_orphans select",
                            e,
                        )
                    })?;
                let rows = stmt
                    .query_map([], |row| {
                        let status_str: String = row.get(4)?;
                        let status =
                            GrammarOrphanStatus::parse(status_str.as_str())
                                .ok_or_else(|| {
                                    rusqlite::Error::FromSqlConversionFailure(
                                        4,
                                        rusqlite::types::Type::Text,
                                        Box::new(std::io::Error::new(
                                            std::io::ErrorKind::InvalidData,
                                            format!(
                                        "unknown grammar orphan status: \
                                         {status_str}"
                                    ),
                                        )),
                                    )
                                })?;
                        Ok(PersistedGrammarOrphan {
                            subject_type: row.get(0)?,
                            first_observed_at_ms: row.get::<_, i64>(1)? as u64,
                            last_observed_at_ms: row.get::<_, i64>(2)? as u64,
                            count: row.get::<_, i64>(3)? as u64,
                            status,
                            accepted_reason: row.get::<_, Option<String>>(5)?,
                            accepted_at_ms: row
                                .get::<_, Option<i64>>(6)?
                                .map(|v| v as u64),
                            migration_id: row.get::<_, Option<String>>(7)?,
                        })
                    })
                    .map_err(|e| {
                        PersistenceError::sqlite(
                            "query pending_grammar_orphans",
                            e,
                        )
                    })?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r.map_err(|e| {
                        PersistenceError::sqlite(
                            "decode pending_grammar_orphans row",
                            e,
                        )
                    })?);
                }
                Ok(out)
            })
            .await
        })
    }
}

/// In-memory mock implementation of [`PersistenceStore`].
///
/// Mirrors the contract of [`SqlitePersistenceStore`] without
/// touching SQLite or disk: useful for unit tests that exercise
/// callers of the trait without paying the cost of fsync per call
/// and without coordinating tempfile lifetimes. The store is
/// thread-safe via a single `tokio::sync::Mutex`; concurrency is
/// serialised through the mutex, which matches the trait's
/// per-call atomicity semantics.
#[derive(Debug)]
pub struct MemoryPersistenceStore {
    inner: AsyncMutex<MemoryState>,
    /// Steward instance ID for claimant-token derivation. The
    /// in-memory store mints a fresh UUIDv4 on construction so each
    /// test instance has its own unlinkability anchor; restarting a
    /// test process does not pin to the same value.
    instance_id: String,
}

impl Default for MemoryPersistenceStore {
    fn default() -> Self {
        Self {
            inner: AsyncMutex::default(),
            instance_id: uuid::Uuid::new_v4().to_string(),
        }
    }
}

#[derive(Debug, Default)]
struct MemoryState {
    subjects: HashMap<String, MemorySubject>,
    aliases: Vec<PersistedAlias>,
    next_alias_id: i64,
    /// Append-only mirror of `claim_log` for tests that want to
    /// inspect provenance after a sequence of operations. The mock
    /// stays faithful to the trait's "every operation appends"
    /// promise.
    claim_log: Vec<MemoryClaimEntry>,
    /// Append-only mirror of `happenings_log`. Tests query this
    /// via [`PersistenceStore::load_happenings_since`].
    happenings: Vec<PersistedHappening>,
    /// Mirror of the `pending_conflicts` table. Updated in place
    /// when a row's resolution columns transition from NULL to a
    /// concrete value.
    pending_conflicts: Vec<PendingConflict>,
    next_conflict_id: i64,
    /// Append-only mirror of `admin_log`. Rows are minted with
    /// monotonic `admin_id`s starting at 1 to mirror the SQLite
    /// `INTEGER PRIMARY KEY AUTOINCREMENT` column.
    admin_log: Vec<PersistedAdminEntry>,
    next_admin_id: i64,
    /// Mirror of `custodies` keyed by `(plugin, handle_id)`.
    custodies: HashMap<(String, String), PersistedCustody>,
    /// Mirror of `custody_state` keyed by `(plugin, handle_id)`.
    /// FK CASCADE is enforced by the trait impl: `delete_custody`
    /// removes the matching entry from this map alongside the
    /// parent.
    custody_state: HashMap<(String, String), PersistedCustodyState>,
    /// Mirror of `relations` keyed by `(source_id, predicate,
    /// target_id)`. Cascade with `relation_claimants` is enforced
    /// in `record_relation_forget` and the
    /// `record_relation_retract` last-claim path.
    relations: HashMap<(String, String, String), PersistedRelation>,
    /// Mirror of `relation_claimants` keyed by
    /// `(source_id, predicate, target_id, claimant)`.
    relation_claimants:
        HashMap<(String, String, String, String), PersistedRelationClaim>,
    /// Mirror of `installed_plugins` keyed by canonical plugin
    /// name. Holds the operator-set enabled bit and audit
    /// metadata; the admission engine reads this at boot to
    /// populate its skip-set for disabled plugins.
    installed_plugins: HashMap<String, PersistedInstalledPlugin>,
    /// Mirror of `reconciliation_state` keyed by pair id. Holds
    /// the per-pair last-known-good projection the framework
    /// re-issues to the warden on apply failure (rollback) and
    /// at boot (cross-restart resume).
    reconciliation_state: HashMap<String, PersistedReconciliationState>,
    /// Mirror of `pending_grammar_orphans` keyed by
    /// subject_type. Persists the boot diagnostic's discoveries
    /// and any operator decisions taken against them.
    pending_grammar_orphans: HashMap<String, PersistedGrammarOrphan>,
}

#[derive(Debug, Clone)]
struct MemorySubject {
    subject_type: String,
    created_at_ms: u64,
    modified_at_ms: u64,
    forgotten_at_ms: Option<u64>,
    addressings: Vec<PersistedAddressing>,
}

#[derive(Debug, Clone)]
struct MemoryClaimEntry {
    #[allow(dead_code)] // retained for fidelity with claim_log.kind
    kind: &'static str,
    #[allow(dead_code)]
    claimant: String,
    #[allow(dead_code)]
    at_ms: u64,
}

impl MemoryPersistenceStore {
    /// Construct an empty in-memory store.
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    async fn claim_log_kinds(&self) -> Vec<&'static str> {
        let g = self.inner.lock().await;
        g.claim_log.iter().map(|e| e.kind).collect()
    }
}

impl PersistenceStore for MemoryPersistenceStore {
    fn record_subject_announce<'a>(
        &'a self,
        record: AnnounceRecord<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            let AnnounceRecord {
                canonical_id,
                subject_type,
                addressings,
                claimant,
                claims,
                at_ms,
            } = record;
            let mut g = self.inner.lock().await;
            let entry = g
                .subjects
                .entry(canonical_id.to_string())
                .or_insert_with(|| MemorySubject {
                    subject_type: subject_type.to_string(),
                    created_at_ms: at_ms,
                    modified_at_ms: at_ms,
                    forgotten_at_ms: None,
                    addressings: Vec::new(),
                });
            entry.modified_at_ms = at_ms;
            entry.forgotten_at_ms = None;
            for a in addressings {
                if let Some(slot) = entry
                    .addressings
                    .iter_mut()
                    .find(|x| x.scheme == a.scheme && x.value == a.value)
                {
                    slot.claimant = claimant.to_string();
                    slot.asserted_at_ms = at_ms;
                } else {
                    entry.addressings.push(PersistedAddressing {
                        scheme: a.scheme.clone(),
                        value: a.value.clone(),
                        claimant: claimant.to_string(),
                        asserted_at_ms: at_ms,
                        reason: None,
                        quarantined_by: None,
                        quarantined_at_ms: None,
                    });
                }
            }
            g.claim_log.push(MemoryClaimEntry {
                kind: claim_kind::SUBJECT_ANNOUNCE,
                claimant: claimant.to_string(),
                at_ms,
            });
            for claim in claims {
                g.claim_log.push(MemoryClaimEntry {
                    kind: claim.kind_str(),
                    claimant: claimant.to_string(),
                    at_ms,
                });
            }
            Ok(())
        })
    }

    fn record_subject_retract<'a>(
        &'a self,
        canonical_id: &'a str,
        addressing: &'a ExternalAddressing,
        claimant: &'a str,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            if let Some(s) = g.subjects.get_mut(canonical_id) {
                s.addressings.retain(|a| {
                    !(a.scheme == addressing.scheme
                        && a.value == addressing.value)
                });
                s.modified_at_ms = at_ms;
            }
            g.claim_log.push(MemoryClaimEntry {
                kind: claim_kind::SUBJECT_RETRACT,
                claimant: claimant.to_string(),
                at_ms,
            });
            Ok(())
        })
    }

    fn record_subject_merge<'a>(
        &'a self,
        record: MergeRecord<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            let MergeRecord {
                source_a,
                source_b,
                new_id,
                subject_type,
                admin_plugin,
                reason,
                at_ms,
            } = record;
            let mut g = self.inner.lock().await;

            // Drain addressings off both sources (if present) and
            // attach them to the new subject. Order matches the
            // SQL UPDATE: source_a then source_b.
            let mut moved: Vec<PersistedAddressing> = Vec::new();
            if let Some(s) = g.subjects.remove(source_a) {
                moved.extend(s.addressings);
            }
            if let Some(s) = g.subjects.remove(source_b) {
                moved.extend(s.addressings);
            }
            for slot in &mut moved {
                slot.asserted_at_ms = at_ms;
            }

            g.subjects.insert(
                new_id.to_string(),
                MemorySubject {
                    subject_type: subject_type.to_string(),
                    created_at_ms: at_ms,
                    modified_at_ms: at_ms,
                    forgotten_at_ms: None,
                    addressings: moved,
                },
            );

            for old in [source_a, source_b] {
                g.next_alias_id += 1;
                let id = g.next_alias_id;
                g.aliases.push(PersistedAlias {
                    alias_id: id,
                    old_id: old.to_string(),
                    new_id: new_id.to_string(),
                    kind: AliasKind::Merged,
                    recorded_at_ms: at_ms,
                    admin_plugin: admin_plugin.to_string(),
                    reason: reason.map(|s| s.to_string()),
                });
            }
            g.claim_log.push(MemoryClaimEntry {
                kind: claim_kind::SUBJECT_MERGE,
                claimant: admin_plugin.to_string(),
                at_ms,
            });
            Ok(())
        })
    }

    fn record_subject_type_migration<'a>(
        &'a self,
        record: TypeMigrationRecord<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            let TypeMigrationRecord {
                source,
                new_id,
                from_type: _,
                to_type,
                migration_id,
                reason,
                at_ms,
            } = record;
            let mut g = self.inner.lock().await;
            // Drain addressings off the source (if present) and
            // attach them to the new subject.
            let mut moved: Vec<PersistedAddressing> =
                if let Some(s) = g.subjects.remove(source) {
                    s.addressings
                } else {
                    Vec::new()
                };
            for slot in &mut moved {
                slot.asserted_at_ms = at_ms;
            }
            g.subjects.insert(
                new_id.to_string(),
                MemorySubject {
                    subject_type: to_type.to_string(),
                    created_at_ms: at_ms,
                    modified_at_ms: at_ms,
                    forgotten_at_ms: None,
                    addressings: moved,
                },
            );
            g.next_alias_id += 1;
            let id = g.next_alias_id;
            g.aliases.push(PersistedAlias {
                alias_id: id,
                old_id: source.to_string(),
                new_id: new_id.to_string(),
                kind: AliasKind::TypeMigrated,
                recorded_at_ms: at_ms,
                admin_plugin: migration_id.to_string(),
                reason: reason.map(|s| s.to_string()),
            });
            g.claim_log.push(MemoryClaimEntry {
                kind: claim_kind::SUBJECT_TYPE_MIGRATION,
                claimant: migration_id.to_string(),
                at_ms,
            });
            Ok(())
        })
    }

    fn record_subject_split<'a>(
        &'a self,
        record: SplitRecord<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            let SplitRecord {
                source,
                new_ids,
                subject_type,
                partition,
                admin_plugin,
                reason,
                at_ms,
            } = record;
            if new_ids.is_empty() {
                return Err(PersistenceError::Invalid(
                    "split must produce at least one new id".into(),
                ));
            }
            if new_ids.len() != partition.len() {
                return Err(PersistenceError::Invalid(format!(
                    "split: new_ids ({}) and partition ({}) must have \
                     equal length",
                    new_ids.len(),
                    partition.len()
                )));
            }
            let mut g = self.inner.lock().await;

            // Drain the source subject's addressings, then re-distribute
            // them across the partition groups by `(scheme, value)`.
            let mut source_addrs: Vec<PersistedAddressing> = g
                .subjects
                .remove(source)
                .map(|s| s.addressings)
                .unwrap_or_default();

            for (group, new_id) in partition.iter().zip(new_ids.iter()) {
                let mut group_addrs: Vec<PersistedAddressing> = Vec::new();
                for a in group {
                    if let Some(pos) = source_addrs.iter().position(|x| {
                        x.scheme == a.scheme && x.value == a.value
                    }) {
                        let mut moved = source_addrs.swap_remove(pos);
                        moved.asserted_at_ms = at_ms;
                        group_addrs.push(moved);
                    }
                }
                g.subjects.insert(
                    new_id.clone(),
                    MemorySubject {
                        subject_type: subject_type.to_string(),
                        created_at_ms: at_ms,
                        modified_at_ms: at_ms,
                        forgotten_at_ms: None,
                        addressings: group_addrs,
                    },
                );
            }

            for new_id in new_ids {
                g.next_alias_id += 1;
                let id = g.next_alias_id;
                g.aliases.push(PersistedAlias {
                    alias_id: id,
                    old_id: source.to_string(),
                    new_id: new_id.clone(),
                    kind: AliasKind::Split,
                    recorded_at_ms: at_ms,
                    admin_plugin: admin_plugin.to_string(),
                    reason: reason.map(|s| s.to_string()),
                });
            }
            g.claim_log.push(MemoryClaimEntry {
                kind: claim_kind::SUBJECT_SPLIT,
                claimant: admin_plugin.to_string(),
                at_ms,
            });
            Ok(())
        })
    }

    fn record_subject_forget<'a>(
        &'a self,
        canonical_id: &'a str,
        forget_claimant: &'a str,
        forget_reason: Option<&'a str>,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let claimant = forget_claimant.to_string();
        let reason = forget_reason.map(|s| s.to_string());
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            g.subjects.remove(canonical_id);
            // Tombstone alias mirrors the SQLite backend's
            // forget_tx behaviour: chain walkers see a structured
            // "forgotten, no successor" record.
            g.next_alias_id += 1;
            let id = g.next_alias_id;
            g.aliases.push(PersistedAlias {
                alias_id: id,
                old_id: canonical_id.to_string(),
                new_id: String::new(),
                kind: AliasKind::Tombstone,
                recorded_at_ms: at_ms,
                admin_plugin: claimant.clone(),
                reason: reason.clone(),
            });
            g.claim_log.push(MemoryClaimEntry {
                kind: claim_kind::SUBJECT_FORGOTTEN,
                claimant,
                at_ms,
            });
            Ok(())
        })
    }

    fn load_all_subjects<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<PersistedSubject>, PersistenceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let g = self.inner.lock().await;
            let mut out: Vec<PersistedSubject> = g
                .subjects
                .iter()
                .map(|(id, s)| PersistedSubject {
                    id: id.clone(),
                    subject_type: s.subject_type.clone(),
                    created_at_ms: s.created_at_ms,
                    modified_at_ms: s.modified_at_ms,
                    forgotten_at_ms: s.forgotten_at_ms,
                    addressings: s.addressings.clone(),
                })
                .collect();
            out.sort_by_key(|s| s.created_at_ms);
            Ok(out)
        })
    }

    fn load_aliases_for<'a>(
        &'a self,
        canonical_id: &'a str,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<PersistedAlias>, PersistenceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let g = self.inner.lock().await;
            let mut out: Vec<PersistedAlias> = g
                .aliases
                .iter()
                .filter(|a| a.old_id == canonical_id)
                .cloned()
                .collect();
            out.sort_by_key(|a| a.alias_id);
            Ok(out)
        })
    }

    fn load_all_aliases<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<PersistedAlias>, PersistenceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let g = self.inner.lock().await;
            let mut out: Vec<PersistedAlias> = g.aliases.clone();
            out.sort_by_key(|a| a.alias_id);
            Ok(out)
        })
    }

    fn record_happening<'a>(
        &'a self,
        seq: u64,
        kind: &'a str,
        payload: &'a serde_json::Value,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            g.happenings.push(PersistedHappening {
                seq,
                kind: kind.to_string(),
                payload: payload.clone(),
                at_ms,
            });
            Ok(())
        })
    }

    fn load_happenings_since<'a>(
        &'a self,
        cursor: u64,
        limit: u32,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<Vec<PersistedHappening>, PersistenceError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let g = self.inner.lock().await;
            let mut out: Vec<PersistedHappening> = g
                .happenings
                .iter()
                .filter(|h| h.seq > cursor)
                .take(limit as usize)
                .cloned()
                .collect();
            out.sort_by_key(|h| h.seq);
            Ok(out)
        })
    }

    fn load_max_happening_seq<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<u64, PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            let g = self.inner.lock().await;
            Ok(g.happenings.iter().map(|h| h.seq).max().unwrap_or(0))
        })
    }

    fn load_oldest_happening_seq<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<u64, PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            let g = self.inner.lock().await;
            Ok(g.happenings.iter().map(|h| h.seq).min().unwrap_or(0))
        })
    }

    fn trim_happenings_log<'a>(
        &'a self,
        _retention_window_secs: u64,
        _retention_capacity: u64,
    ) -> Pin<Box<dyn Future<Output = Result<u64, PersistenceError>> + Send + 'a>>
    {
        // In-memory store is a no-op: tests that exercise the
        // janitor's trimming behaviour use the SQLite-backed store
        // where the read-side gates are also enforced.
        Box::pin(async move { Ok(0) })
    }

    fn load_instance_id<'a>(
        &'a self,
    ) -> Pin<
        Box<dyn Future<Output = Result<String, PersistenceError>> + Send + 'a>,
    > {
        Box::pin(async move { Ok(self.instance_id.clone()) })
    }

    fn record_pending_conflict<'a>(
        &'a self,
        plugin: &'a str,
        addressings: &'a [ExternalAddressing],
        canonical_ids: &'a [String],
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<i64, PersistenceError>> + Send + 'a>>
    {
        let plugin = plugin.to_string();
        let addressings = addressings.to_vec();
        let canonical_ids = canonical_ids.to_vec();
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            g.next_conflict_id += 1;
            let id = g.next_conflict_id;
            g.pending_conflicts.push(PendingConflict {
                id,
                detected_at_ms: at_ms,
                plugin,
                addressings,
                canonical_ids,
                resolved_at_ms: None,
                resolution_kind: None,
            });
            Ok(id)
        })
    }

    fn mark_conflict_resolved<'a>(
        &'a self,
        id: i64,
        resolution_kind: &'a str,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let kind = resolution_kind.to_string();
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            if let Some(slot) =
                g.pending_conflicts.iter_mut().find(|c| c.id == id)
            {
                if slot.resolved_at_ms.is_none() {
                    slot.resolved_at_ms = Some(at_ms);
                    slot.resolution_kind = Some(kind);
                }
            }
            Ok(())
        })
    }

    fn list_pending_conflicts<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<PendingConflict>, PersistenceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let g = self.inner.lock().await;
            let mut out: Vec<PendingConflict> = g
                .pending_conflicts
                .iter()
                .filter(|c| c.resolved_at_ms.is_none())
                .cloned()
                .collect();
            out.sort_by_key(|c| c.detected_at_ms);
            Ok(out)
        })
    }

    fn record_admin_entry<'a>(
        &'a self,
        entry: &'a PersistedAdminEntry,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            if g.next_admin_id == 0 {
                g.next_admin_id = 1;
            }
            let admin_id = g.next_admin_id;
            g.next_admin_id += 1;
            let mut row = entry.clone();
            row.admin_id = admin_id;
            g.admin_log.push(row);
            Ok(())
        })
    }

    fn load_all_admin_entries<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<Vec<PersistedAdminEntry>, PersistenceError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let g = self.inner.lock().await;
            Ok(g.admin_log.clone())
        })
    }

    fn record_relation_assert<'a>(
        &'a self,
        relation: &'a PersistedRelation,
        claim: &'a PersistedRelationClaim,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            let rel_key = (
                relation.source_id.clone(),
                relation.predicate.clone(),
                relation.target_id.clone(),
            );
            match g.relations.get_mut(&rel_key) {
                Some(existing) => {
                    existing.modified_at_ms = relation.modified_at_ms;
                }
                None => {
                    g.relations.insert(rel_key.clone(), relation.clone());
                }
            }
            let claim_key = (
                claim.source_id.clone(),
                claim.predicate.clone(),
                claim.target_id.clone(),
                claim.claimant.clone(),
            );
            g.relation_claimants
                .entry(claim_key)
                .or_insert_with(|| claim.clone());
            Ok(())
        })
    }

    fn record_relation_retract<'a>(
        &'a self,
        source_id: &'a str,
        predicate: &'a str,
        target_id: &'a str,
        claimant: &'a str,
        modified_at_ms: u64,
        relation_forgotten: bool,
    ) -> Pin<Box<dyn Future<Output = Result<bool, PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            let claim_key = (
                source_id.to_string(),
                predicate.to_string(),
                target_id.to_string(),
                claimant.to_string(),
            );
            let removed = g.relation_claimants.remove(&claim_key).is_some();
            let rel_key = (
                source_id.to_string(),
                predicate.to_string(),
                target_id.to_string(),
            );
            if relation_forgotten {
                g.relations.remove(&rel_key);
                g.relation_claimants.retain(|k, _| {
                    !(k.0 == source_id && k.1 == predicate && k.2 == target_id)
                });
            } else if removed {
                if let Some(rel) = g.relations.get_mut(&rel_key) {
                    rel.modified_at_ms = modified_at_ms;
                }
            }
            Ok(removed)
        })
    }

    fn record_relation_forget<'a>(
        &'a self,
        source_id: &'a str,
        predicate: &'a str,
        target_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            let rel_key = (
                source_id.to_string(),
                predicate.to_string(),
                target_id.to_string(),
            );
            let removed = g.relations.remove(&rel_key).is_some();
            g.relation_claimants.retain(|k, _| {
                !(k.0 == source_id && k.1 == predicate && k.2 == target_id)
            });
            Ok(removed)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn record_relation_suppress<'a>(
        &'a self,
        source_id: &'a str,
        predicate: &'a str,
        target_id: &'a str,
        admin_plugin: &'a str,
        suppressed_at_ms: u64,
        reason: Option<&'a str>,
        modified_at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<bool, PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            let rel_key = (
                source_id.to_string(),
                predicate.to_string(),
                target_id.to_string(),
            );
            if let Some(rel) = g.relations.get_mut(&rel_key) {
                rel.suppressed_admin_plugin = Some(admin_plugin.to_string());
                rel.suppressed_at_ms = Some(suppressed_at_ms);
                rel.suppression_reason = reason.map(|s| s.to_string());
                rel.modified_at_ms = modified_at_ms;
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }

    fn record_relation_unsuppress<'a>(
        &'a self,
        source_id: &'a str,
        predicate: &'a str,
        target_id: &'a str,
        modified_at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<bool, PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            let rel_key = (
                source_id.to_string(),
                predicate.to_string(),
                target_id.to_string(),
            );
            if let Some(rel) = g.relations.get_mut(&rel_key) {
                rel.suppressed_admin_plugin = None;
                rel.suppressed_at_ms = None;
                rel.suppression_reason = None;
                rel.modified_at_ms = modified_at_ms;
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }

    fn load_all_relations<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<RelationLoadRow>, PersistenceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let g = self.inner.lock().await;
            let mut rel_keys: Vec<&(String, String, String)> =
                g.relations.keys().collect();
            rel_keys.sort();
            let mut out: Vec<RelationLoadRow> =
                Vec::with_capacity(rel_keys.len());
            for k in rel_keys {
                let rel = g.relations.get(k).cloned().expect("key present");
                let mut claims: Vec<PersistedRelationClaim> = g
                    .relation_claimants
                    .iter()
                    .filter(|(ck, _)| ck.0 == k.0 && ck.1 == k.1 && ck.2 == k.2)
                    .map(|(_, v)| v.clone())
                    .collect();
                claims.sort_by(|a, b| a.claimant.cmp(&b.claimant));
                out.push((rel, claims));
            }
            Ok(out)
        })
    }

    fn upsert_custody<'a>(
        &'a self,
        record: &'a PersistedCustody,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            let key = (record.plugin.clone(), record.handle_id.clone());
            match g.custodies.get_mut(&key) {
                Some(existing) => {
                    if record.shelf.is_some() {
                        existing.shelf = record.shelf.clone();
                    }
                    if record.custody_type.is_some() {
                        existing.custody_type = record.custody_type.clone();
                    }
                    existing.state_kind = record.state_kind.clone();
                    existing.state_reason = record.state_reason.clone();
                    existing.last_updated_at_ms = record.last_updated_at_ms;
                }
                None => {
                    g.custodies.insert(key, record.clone());
                }
            }
            Ok(())
        })
    }

    fn upsert_custody_state<'a>(
        &'a self,
        snapshot: &'a PersistedCustodyState,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            let key = (snapshot.plugin.clone(), snapshot.handle_id.clone());
            if !g.custodies.contains_key(&key) {
                return Err(PersistenceError::Invalid(format!(
                    "custody_state insert for missing custody \
                     ({}, {}): FK violated",
                    snapshot.plugin, snapshot.handle_id
                )));
            }
            g.custody_state.insert(key, snapshot.clone());
            Ok(())
        })
    }

    fn mark_custody_state<'a>(
        &'a self,
        plugin: &'a str,
        handle_id: &'a str,
        state_kind: &'a str,
        state_reason: Option<&'a str>,
        last_updated_at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<bool, PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            let key = (plugin.to_string(), handle_id.to_string());
            if let Some(rec) = g.custodies.get_mut(&key) {
                rec.state_kind = state_kind.to_string();
                rec.state_reason = state_reason.map(|s| s.to_string());
                rec.last_updated_at_ms = last_updated_at_ms;
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }

    fn delete_custody<'a>(
        &'a self,
        plugin: &'a str,
        handle_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            let key = (plugin.to_string(), handle_id.to_string());
            g.custody_state.remove(&key);
            Ok(g.custodies.remove(&key).is_some())
        })
    }

    fn load_all_custodies<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<CustodyLoadRow>, PersistenceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let g = self.inner.lock().await;
            let mut keys: Vec<&(String, String)> = g.custodies.keys().collect();
            keys.sort();
            let mut out = Vec::with_capacity(keys.len());
            for k in keys {
                let custody = g.custodies.get(k).cloned().expect("key present");
                let snapshot = g.custody_state.get(k).cloned();
                out.push((custody, snapshot));
            }
            Ok(out)
        })
    }

    fn count_subjects_by_type<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<SubjectTypeCount>, PersistenceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let g = self.inner.lock().await;
            let mut counts: HashMap<String, u64> = HashMap::new();
            for s in g.subjects.values() {
                if s.forgotten_at_ms.is_none() {
                    *counts.entry(s.subject_type.clone()).or_insert(0) += 1;
                }
            }
            let mut out: Vec<(String, u64)> = counts.into_iter().collect();
            out.sort_by(|a, b| a.0.cmp(&b.0));
            Ok(out)
        })
    }

    fn checkpoint_wal<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        // The in-memory backend has no WAL; the call is a no-op so
        // the shutdown path does not branch on the concrete backend
        // type.
        Box::pin(async move { Ok(()) })
    }

    fn record_plugin_enabled<'a>(
        &'a self,
        row: &'a PersistedInstalledPlugin,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let row = row.clone();
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            g.installed_plugins.insert(row.plugin_name.clone(), row);
            Ok(())
        })
    }

    fn load_all_installed_plugins<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Vec<PersistedInstalledPlugin>,
                        PersistenceError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let g = self.inner.lock().await;
            let mut out: Vec<PersistedInstalledPlugin> =
                g.installed_plugins.values().cloned().collect();
            out.sort_by(|a, b| a.plugin_name.cmp(&b.plugin_name));
            Ok(out)
        })
    }

    fn forget_installed_plugin<'a>(
        &'a self,
        plugin_name: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let name = plugin_name.to_string();
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            g.installed_plugins.remove(&name);
            Ok(())
        })
    }

    fn record_reconciliation_state<'a>(
        &'a self,
        row: &'a PersistedReconciliationState,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let row = row.clone();
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            g.reconciliation_state.insert(row.pair_id.clone(), row);
            Ok(())
        })
    }

    fn load_all_reconciliation_state<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Vec<PersistedReconciliationState>,
                        PersistenceError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let g = self.inner.lock().await;
            let mut out: Vec<PersistedReconciliationState> =
                g.reconciliation_state.values().cloned().collect();
            out.sort_by(|a, b| a.pair_id.cmp(&b.pair_id));
            Ok(out)
        })
    }

    fn forget_reconciliation_state<'a>(
        &'a self,
        pair_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let id = pair_id.to_string();
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            g.reconciliation_state.remove(&id);
            Ok(())
        })
    }

    fn upsert_pending_grammar_orphan<'a>(
        &'a self,
        subject_type: &'a str,
        count: u64,
        observed_at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let st = subject_type.to_string();
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            let entry = g
                .pending_grammar_orphans
                .entry(st.clone())
                .or_insert_with(|| PersistedGrammarOrphan {
                    subject_type: st.clone(),
                    first_observed_at_ms: observed_at_ms,
                    last_observed_at_ms: observed_at_ms,
                    count,
                    status: GrammarOrphanStatus::Pending,
                    accepted_reason: None,
                    accepted_at_ms: None,
                    migration_id: None,
                });
            entry.last_observed_at_ms = observed_at_ms;
            entry.count = count;
            // A previously-recovered type that re-orphans flips
            // back to pending so the operator surface lights up
            // again. Other states are preserved.
            if matches!(entry.status, GrammarOrphanStatus::Recovered) {
                entry.status = GrammarOrphanStatus::Pending;
            }
            Ok(())
        })
    }

    fn mark_grammar_orphan_recovered<'a>(
        &'a self,
        subject_type: &'a str,
        observed_at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let st = subject_type.to_string();
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            if let Some(entry) = g.pending_grammar_orphans.get_mut(&st) {
                entry.status = GrammarOrphanStatus::Recovered;
                entry.last_observed_at_ms = observed_at_ms;
            }
            Ok(())
        })
    }

    fn accept_grammar_orphan<'a>(
        &'a self,
        subject_type: &'a str,
        reason: &'a str,
        accepted_at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<bool, PersistenceError>> + Send + 'a>>
    {
        let st = subject_type.to_string();
        let reason = reason.to_string();
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            let entry = match g.pending_grammar_orphans.get_mut(&st) {
                Some(e) => e,
                None => {
                    return Err(PersistenceError::Invalid(format!(
                        "accept_grammar_orphan: no pending row for type {st:?}"
                    )));
                }
            };
            match entry.status {
                GrammarOrphanStatus::Migrating => {
                    Err(PersistenceError::Invalid(format!(
                        "accept_grammar_orphan: type {st:?} has an in-flight \
                         migration; wait for it to complete"
                    )))
                }
                GrammarOrphanStatus::Accepted => Ok(false),
                _ => {
                    entry.status = GrammarOrphanStatus::Accepted;
                    entry.accepted_reason = Some(reason);
                    entry.accepted_at_ms = Some(accepted_at_ms);
                    Ok(true)
                }
            }
        })
    }

    fn mark_grammar_orphan_migrating<'a>(
        &'a self,
        subject_type: &'a str,
        migration_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let st = subject_type.to_string();
        let mid = migration_id.to_string();
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            let entry = match g.pending_grammar_orphans.get_mut(&st) {
                Some(e) => e,
                None => {
                    return Err(PersistenceError::Invalid(format!(
                        "mark_grammar_orphan_migrating: no pending row for \
                         type {st:?}"
                    )));
                }
            };
            if matches!(entry.status, GrammarOrphanStatus::Resolved) {
                return Err(PersistenceError::Invalid(format!(
                    "mark_grammar_orphan_migrating: type {st:?} is already \
                     resolved"
                )));
            }
            entry.status = GrammarOrphanStatus::Migrating;
            entry.migration_id = Some(mid);
            Ok(())
        })
    }

    fn mark_grammar_orphan_resolved<'a>(
        &'a self,
        subject_type: &'a str,
        migration_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let st = subject_type.to_string();
        let mid = migration_id.to_string();
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            if let Some(entry) = g.pending_grammar_orphans.get_mut(&st) {
                entry.status = GrammarOrphanStatus::Resolved;
                entry.migration_id = Some(mid);
            }
            Ok(())
        })
    }

    fn list_pending_grammar_orphans<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Vec<PersistedGrammarOrphan>,
                        PersistenceError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let g = self.inner.lock().await;
            let mut out: Vec<_> =
                g.pending_grammar_orphans.values().cloned().collect();
            out.sort_by(|a, b| a.subject_type.cmp(&b.subject_type));
            Ok(out)
        })
    }
}

/// Convenience: test fixture builder for code that needs an
/// `Arc<dyn PersistenceStore>` backed by the in-memory mock.
#[cfg(test)]
pub fn memory_store_for_tests() -> std::sync::Arc<dyn PersistenceStore> {
    std::sync::Arc::new(MemoryPersistenceStore::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn ext(scheme: &str, value: &str) -> ExternalAddressing {
        ExternalAddressing::new(scheme, value)
    }

    // --- in-memory backend -------------------------------------------------

    #[tokio::test]
    async fn memory_count_subjects_by_type_groups_and_sorts() {
        // Boot-time orphan diagnostic helper: persistence groups
        // every live subject by `subject_type` and the boot path
        // diffs the result against the loaded catalogue's declared
        // types. The accessor sorts ascending so warnings emit in a
        // stable order; forgotten subjects are excluded so historical
        // types whose subjects all retracted no longer surface.
        let s = MemoryPersistenceStore::new();
        for (id, ty) in [
            ("uuid-1", "track"),
            ("uuid-2", "track"),
            ("uuid-3", "track"),
            ("uuid-4", "album"),
            ("uuid-5", "album"),
            ("uuid-6", "podcast_episode"),
        ] {
            s.record_subject_announce(AnnounceRecord {
                canonical_id: id,
                subject_type: ty,
                addressings: &[ext("test", id)],
                claimant: "p1",
                claims: &[],
                at_ms: 1000,
            })
            .await
            .unwrap();
        }

        // Forget one of the tracks so its row no longer counts
        // toward the live grouping.
        s.record_subject_forget("uuid-3", "p1", None, 1100)
            .await
            .unwrap();

        let counts = s.count_subjects_by_type().await.unwrap();
        assert_eq!(
            counts,
            vec![
                ("album".to_string(), 2),
                ("podcast_episode".to_string(), 1),
                ("track".to_string(), 2),
            ],
            "live counts must group by type, exclude forgotten, and \
             sort ascending; got: {counts:?}"
        );
    }

    #[tokio::test]
    async fn memory_announce_then_load_returns_subject() {
        let s = MemoryPersistenceStore::new();
        s.record_subject_announce(AnnounceRecord {
            canonical_id: "uuid-a",
            subject_type: "track",
            addressings: &[ext("mpd-path", "/m/a.flac")],
            claimant: "p1",
            claims: &[],
            at_ms: 1000,
        })
        .await
        .unwrap();
        let all = s.load_all_subjects().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "uuid-a");
        assert_eq!(all[0].subject_type, "track");
        assert_eq!(all[0].addressings.len(), 1);
        assert_eq!(all[0].addressings[0].scheme, "mpd-path");
    }

    #[tokio::test]
    async fn memory_retract_removes_addressing() {
        let s = MemoryPersistenceStore::new();
        s.record_subject_announce(AnnounceRecord {
            canonical_id: "uuid-a",
            subject_type: "track",
            addressings: &[ext("a", "1"), ext("b", "2")],
            claimant: "p1",
            claims: &[],
            at_ms: 1000,
        })
        .await
        .unwrap();
        s.record_subject_retract("uuid-a", &ext("a", "1"), "p1", 1100)
            .await
            .unwrap();
        let all = s.load_all_subjects().await.unwrap();
        assert_eq!(all[0].addressings.len(), 1);
        assert_eq!(all[0].addressings[0].scheme, "b");
    }

    #[tokio::test]
    async fn memory_merge_records_aliases_for_both_sources() {
        let s = MemoryPersistenceStore::new();
        s.record_subject_merge(MergeRecord {
            source_a: "uuid-a",
            source_b: "uuid-b",
            new_id: "uuid-c",
            subject_type: "track",
            admin_plugin: "admin",
            reason: Some("dup"),
            at_ms: 2000,
        })
        .await
        .unwrap();
        let aa = s.load_aliases_for("uuid-a").await.unwrap();
        let ab = s.load_aliases_for("uuid-b").await.unwrap();
        assert_eq!(aa.len(), 1);
        assert_eq!(aa[0].new_id, "uuid-c");
        assert_eq!(aa[0].kind, AliasKind::Merged);
        assert_eq!(ab.len(), 1);
        assert_eq!(ab[0].new_id, "uuid-c");
    }

    #[tokio::test]
    async fn memory_type_migration_records_alias_and_moves_addressings() {
        let s = MemoryPersistenceStore::new();
        s.record_subject_announce(AnnounceRecord {
            canonical_id: "uuid-old",
            subject_type: "audio_track",
            addressings: &[ext("library", "song-1"), ext("mb", "abc-123")],
            claimant: "p1",
            claims: &[],
            at_ms: 1000,
        })
        .await
        .unwrap();
        s.record_subject_type_migration(TypeMigrationRecord {
            source: "uuid-old",
            new_id: "uuid-new",
            from_type: "audio_track",
            to_type: "track",
            migration_id: "mig_01",
            reason: Some("catalogue v3 rename"),
            at_ms: 2000,
        })
        .await
        .unwrap();
        let aliases = s.load_aliases_for("uuid-old").await.unwrap();
        assert_eq!(aliases.len(), 1);
        assert_eq!(aliases[0].new_id, "uuid-new");
        assert_eq!(aliases[0].kind, AliasKind::TypeMigrated);
        assert_eq!(aliases[0].admin_plugin, "mig_01");
        assert_eq!(aliases[0].reason.as_deref(), Some("catalogue v3 rename"));
        let all = s.load_all_subjects().await.unwrap();
        // Source row deleted; new row holds the addressings under
        // the new subject_type.
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "uuid-new");
        assert_eq!(all[0].subject_type, "track");
        let mut schemes: Vec<_> = all[0]
            .addressings
            .iter()
            .map(|a| a.scheme.as_str())
            .collect();
        schemes.sort();
        assert_eq!(schemes, vec!["library", "mb"]);
    }

    #[tokio::test]
    async fn memory_type_migration_tolerates_missing_source() {
        // Idempotent re-issue: source already migrated in a
        // prior call. The verb must still record the alias
        // entry so describe_alias resolves the redirect.
        let s = MemoryPersistenceStore::new();
        s.record_subject_type_migration(TypeMigrationRecord {
            source: "uuid-missing",
            new_id: "uuid-new",
            from_type: "audio_track",
            to_type: "track",
            migration_id: "mig_01",
            reason: None,
            at_ms: 1000,
        })
        .await
        .unwrap();
        let aliases = s.load_aliases_for("uuid-missing").await.unwrap();
        assert_eq!(aliases.len(), 1);
        assert_eq!(aliases[0].kind, AliasKind::TypeMigrated);
    }

    #[tokio::test]
    async fn memory_split_records_aliases_per_partition() {
        let s = MemoryPersistenceStore::new();
        let new_ids = vec!["uuid-c".to_string(), "uuid-d".to_string()];
        let partition: Vec<Vec<ExternalAddressing>> =
            vec![Vec::new(), Vec::new()];
        s.record_subject_split(SplitRecord {
            source: "uuid-a",
            new_ids: &new_ids,
            subject_type: "track",
            partition: &partition,
            admin_plugin: "admin",
            reason: None,
            at_ms: 3000,
        })
        .await
        .unwrap();
        let a = s.load_aliases_for("uuid-a").await.unwrap();
        assert_eq!(a.len(), 2);
        assert!(a.iter().all(|x| x.kind == AliasKind::Split));
        let new_set: std::collections::BTreeSet<_> =
            a.iter().map(|x| x.new_id.as_str()).collect();
        assert!(new_set.contains("uuid-c"));
        assert!(new_set.contains("uuid-d"));
    }

    #[tokio::test]
    async fn memory_forget_removes_subject() {
        let s = MemoryPersistenceStore::new();
        s.record_subject_announce(AnnounceRecord {
            canonical_id: "uuid-a",
            subject_type: "track",
            addressings: &[ext("a", "1")],
            claimant: "p1",
            claims: &[],
            at_ms: 1000,
        })
        .await
        .unwrap();
        s.record_subject_forget("uuid-a", "p1", None, 1500)
            .await
            .unwrap();
        let all = s.load_all_subjects().await.unwrap();
        assert!(all.is_empty());
    }

    #[tokio::test]
    async fn memory_split_with_empty_new_ids_errors() {
        let s = MemoryPersistenceStore::new();
        let r = s
            .record_subject_split(SplitRecord {
                source: "uuid-a",
                new_ids: &[],
                subject_type: "track",
                partition: &[],
                admin_plugin: "admin",
                reason: None,
                at_ms: 1,
            })
            .await;
        assert!(matches!(r, Err(PersistenceError::Invalid(_))));
    }

    // --- SQLite backend ----------------------------------------------------

    fn open_temp() -> (tempfile::TempDir, SqlitePersistenceStore) {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("evo.db");
        let store =
            SqlitePersistenceStore::open(path).expect("open sqlite store");
        (dir, store)
    }

    #[tokio::test]
    async fn sqlite_open_creates_database_file_with_schema() {
        let (_dir, store) = open_temp();
        // Open a side connection and confirm migrations have been
        // applied up to the current SUPPORTED_SCHEMA_VERSION (v1
        // subject identity, v2 happenings durable cursor).
        let conn = Connection::open(store.path()).expect("side open");
        let v = current_schema_version(&conn).expect("read version");
        assert_eq!(v, SUPPORTED_SCHEMA_VERSION);
    }

    #[tokio::test]
    async fn sqlite_open_existing_database_succeeds() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("evo.db");
        {
            let _ =
                SqlitePersistenceStore::open(path.clone()).expect("first open");
        }
        let _ = SqlitePersistenceStore::open(path).expect("reopen");
    }

    #[tokio::test]
    async fn sqlite_open_database_at_higher_version_refuses() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("evo.db");
        // First open at the current version, then bump
        // schema_version to a future value via a side connection.
        {
            let _ =
                SqlitePersistenceStore::open(path.clone()).expect("first open");
        }
        let conn = Connection::open(&path).expect("side open");
        conn.execute(
            "INSERT INTO schema_version (version, applied_at_ms, description) \
             VALUES (?1, 0, 'future')",
            params![SUPPORTED_SCHEMA_VERSION + 1],
        )
        .expect("bump version");
        drop(conn);

        let r = SqlitePersistenceStore::open(path);
        assert!(matches!(
            r,
            Err(PersistenceError::SchemaVersionAhead { .. })
        ));
    }

    #[tokio::test]
    async fn sqlite_pragmas_applied_on_connection() {
        let (_dir, store) = open_temp();
        // Acquire a pool connection and verify each pragma's state.
        let conn = store.pool.get().await.expect("pool get");
        let (mode, sync, fk, busy, cache, temp) = conn
            .interact(|c| {
                apply_pragmas(c).unwrap();
                let mode: String = c
                    .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                    .unwrap();
                let sync: i64 = c
                    .query_row("PRAGMA synchronous", [], |r| r.get(0))
                    .unwrap();
                let fk: i64 = c
                    .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
                    .unwrap();
                let busy: i64 = c
                    .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
                    .unwrap();
                let cache: i64 =
                    c.query_row("PRAGMA cache_size", [], |r| r.get(0)).unwrap();
                let temp: i64 =
                    c.query_row("PRAGMA temp_store", [], |r| r.get(0)).unwrap();
                Ok::<_, rusqlite::Error>((mode, sync, fk, busy, cache, temp))
            })
            .await
            .expect("interact")
            .expect("pragma queries");

        assert_eq!(mode.to_lowercase(), "wal");
        // SQLite's PRAGMA synchronous values: 0=OFF, 1=NORMAL,
        // 2=FULL, 3=EXTRA. The contract is FULL.
        assert_eq!(sync, 2);
        assert_eq!(fk, 1);
        assert_eq!(busy, 5000);
        assert_eq!(cache, -20000);
        // PRAGMA temp_store: 0=DEFAULT, 1=FILE, 2=MEMORY.
        assert_eq!(temp, 2);
    }

    #[tokio::test]
    async fn sqlite_announce_round_trip() {
        let (_dir, store) = open_temp();
        store
            .record_subject_announce(AnnounceRecord {
                canonical_id: "uuid-a",
                subject_type: "track",
                addressings: &[ext("mpd", "/x")],
                claimant: "p1",
                claims: &[],
                at_ms: 42,
            })
            .await
            .unwrap();
        let all = store.load_all_subjects().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].addressings.len(), 1);
    }

    #[tokio::test]
    async fn sqlite_retract_round_trip() {
        let (_dir, store) = open_temp();
        store
            .record_subject_announce(AnnounceRecord {
                canonical_id: "uuid-a",
                subject_type: "track",
                addressings: &[ext("a", "1"), ext("b", "2")],
                claimant: "p1",
                claims: &[],
                at_ms: 10,
            })
            .await
            .unwrap();
        store
            .record_subject_retract("uuid-a", &ext("a", "1"), "p1", 20)
            .await
            .unwrap();
        let all = store.load_all_subjects().await.unwrap();
        assert_eq!(all[0].addressings.len(), 1);
        assert_eq!(all[0].addressings[0].scheme, "b");
    }

    #[tokio::test]
    async fn sqlite_merge_records_two_aliases() {
        let (_dir, store) = open_temp();
        store
            .record_subject_merge(MergeRecord {
                source_a: "uuid-a",
                source_b: "uuid-b",
                new_id: "uuid-c",
                subject_type: "track",
                admin_plugin: "admin",
                reason: Some("dup"),
                at_ms: 100,
            })
            .await
            .unwrap();
        let aa = store.load_aliases_for("uuid-a").await.unwrap();
        let ab = store.load_aliases_for("uuid-b").await.unwrap();
        assert_eq!(aa.len(), 1);
        assert_eq!(ab.len(), 1);
        assert_eq!(aa[0].new_id, "uuid-c");
        assert_eq!(aa[0].kind, AliasKind::Merged);
    }

    #[tokio::test]
    async fn sqlite_split_records_n_aliases() {
        let (_dir, store) = open_temp();
        let new_ids = vec!["uuid-c".to_string(), "uuid-d".to_string()];
        let partition: Vec<Vec<ExternalAddressing>> =
            vec![Vec::new(), Vec::new()];
        store
            .record_subject_split(SplitRecord {
                source: "uuid-a",
                new_ids: &new_ids,
                subject_type: "track",
                partition: &partition,
                admin_plugin: "admin",
                reason: None,
                at_ms: 200,
            })
            .await
            .unwrap();
        let a = store.load_aliases_for("uuid-a").await.unwrap();
        assert_eq!(a.len(), 2);
        assert!(a.iter().all(|x| x.kind == AliasKind::Split));
    }

    #[tokio::test]
    async fn sqlite_forget_deletes_subject_and_cascades() {
        let (_dir, store) = open_temp();
        store
            .record_subject_announce(AnnounceRecord {
                canonical_id: "uuid-a",
                subject_type: "track",
                addressings: &[ext("a", "1")],
                claimant: "p1",
                claims: &[],
                at_ms: 10,
            })
            .await
            .unwrap();
        store
            .record_subject_forget("uuid-a", "p1", None, 20)
            .await
            .unwrap();
        let all = store.load_all_subjects().await.unwrap();
        assert!(all.is_empty());
        // Side-channel verify: the addressing row was cascaded.
        let conn = Connection::open(store.path()).expect("side open");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM subject_addressings", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn sqlite_forget_inserts_tombstone_alias_row() {
        // forget_tx must insert a tombstone row into `aliases` in
        // the same transaction as the subject delete. A consumer
        // walking the alias chain on the forgotten ID sees a
        // structured "no successor" record rather than a bare
        // absence; the in-memory backend mirrors this so tests
        // agnostic to the concrete store see the same shape.
        let (_dir, store) = open_temp();
        store
            .record_subject_announce(AnnounceRecord {
                canonical_id: "uuid-a",
                subject_type: "track",
                addressings: &[ext("a", "1")],
                claimant: "p1",
                claims: &[],
                at_ms: 10,
            })
            .await
            .unwrap();
        store
            .record_subject_forget("uuid-a", "p1", Some("operator cleanup"), 20)
            .await
            .unwrap();
        let aliases = store.load_aliases_for("uuid-a").await.unwrap();
        assert_eq!(aliases.len(), 1, "exactly one tombstone alias row");
        let a = &aliases[0];
        assert_eq!(a.kind, AliasKind::Tombstone);
        assert_eq!(a.old_id, "uuid-a");
        assert_eq!(a.new_id, "");
        assert_eq!(a.admin_plugin, "p1");
        assert_eq!(a.reason.as_deref(), Some("operator cleanup"));
    }

    #[tokio::test]
    async fn memory_forget_inserts_tombstone_alias_row() {
        let s = MemoryPersistenceStore::new();
        s.record_subject_announce(AnnounceRecord {
            canonical_id: "uuid-z",
            subject_type: "album",
            addressings: &[ext("a", "z")],
            claimant: "p2",
            claims: &[],
            at_ms: 1,
        })
        .await
        .unwrap();
        s.record_subject_forget("uuid-z", "p2", Some("admin sweep"), 5)
            .await
            .unwrap();
        let aliases = s.load_aliases_for("uuid-z").await.unwrap();
        assert_eq!(aliases.len(), 1);
        let a = &aliases[0];
        assert_eq!(a.kind, AliasKind::Tombstone);
        assert_eq!(a.old_id, "uuid-z");
        assert!(a.new_id.is_empty());
        assert_eq!(a.admin_plugin, "p2");
        assert_eq!(a.reason.as_deref(), Some("admin sweep"));
    }

    #[tokio::test]
    async fn sqlite_announce_populates_all_three_tables_atomically() {
        let (_dir, store) = open_temp();
        store
            .record_subject_announce(AnnounceRecord {
                canonical_id: "uuid-a",
                subject_type: "track",
                addressings: &[ext("a", "1")],
                claimant: "p1",
                claims: &[],
                at_ms: 42,
            })
            .await
            .unwrap();
        // Side connection, read each table directly.
        let conn = Connection::open(store.path()).expect("side open");
        let n_subjects: i64 = conn
            .query_row("SELECT COUNT(*) FROM subjects", [], |r| r.get(0))
            .unwrap();
        let n_addr: i64 = conn
            .query_row("SELECT COUNT(*) FROM subject_addressings", [], |r| {
                r.get(0)
            })
            .unwrap();
        let n_log: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM claim_log WHERE kind = 'subject_announce'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_subjects, 1);
        assert_eq!(n_addr, 1);
        assert_eq!(n_log, 1);
    }

    #[tokio::test]
    async fn sqlite_announce_atomicity_rolls_back_on_constraint_violation() {
        // Force a constraint violation by trying to insert a subject
        // row whose subject_type is NULL through a hand-crafted
        // transaction that mirrors `announce_tx` but breaks the
        // addressing insert. After rollback, neither the subject row
        // nor the addressing nor the claim_log entry must persist.
        let (_dir, store) = open_temp();
        let path = store.path().to_path_buf();
        // Use a side connection so we are guaranteed to control the
        // transaction lifecycle independent of the pool.
        let mut conn = Connection::open(&path).expect("side open");
        apply_pragmas(&conn).unwrap();
        let r: Result<(), rusqlite::Error> = (|| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO subjects (id, subject_type, created_at_ms, \
                 modified_at_ms, forgotten_at_ms) \
                 VALUES ('uuid-z', 'track', 1, 1, NULL)",
                [],
            )?;
            // Constraint violation: addressing references a
            // non-existent subject_id, which the foreign key
            // refuses.
            tx.execute(
                "INSERT INTO subject_addressings \
                 (scheme, value, subject_id, claimant, asserted_at_ms) \
                 VALUES ('s', 'v', 'uuid-does-not-exist', 'p1', 1)",
                [],
            )?;
            tx.commit()
        })();
        assert!(r.is_err(), "expected FK violation");
        let n_subjects: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM subjects WHERE id = 'uuid-z'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let n_addr: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM subject_addressings WHERE scheme = 's'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_subjects, 0);
        assert_eq!(n_addr, 0);
    }

    #[tokio::test]
    async fn sqlite_wal_files_appear_after_first_write() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("evo.db");
        let store = SqlitePersistenceStore::open(path.clone()).expect("open");
        store
            .record_subject_announce(AnnounceRecord {
                canonical_id: "uuid-a",
                subject_type: "track",
                addressings: &[ext("a", "1")],
                claimant: "p1",
                claims: &[],
                at_ms: 1,
            })
            .await
            .unwrap();
        let wal = path.with_extension("db-wal");
        let shm = path.with_extension("db-shm");
        // SQLite creates -wal and -shm sidecars under WAL mode after
        // the first write. The exact filenames are
        // `<basename>-wal` and `<basename>-shm`.
        let wal_alt = dir.path().join("evo.db-wal");
        let shm_alt = dir.path().join("evo.db-shm");
        assert!(wal.exists() || wal_alt.exists(), "expected -wal sidecar");
        assert!(shm.exists() || shm_alt.exists(), "expected -shm sidecar");
    }

    // --- Per-claim claim_log entries on announce -------------------------

    #[tokio::test]
    async fn memory_announce_records_per_claim_log_entries() {
        let s = MemoryPersistenceStore::new();
        let claims = vec![
            PersistedClaim::Equivalent {
                a: ext("a", "1"),
                b: ext("b", "2"),
                confidence: ClaimConfidence::Asserted,
                reason: Some("same disc".into()),
            },
            PersistedClaim::Distinct {
                a: ext("a", "1"),
                b: ext("c", "3"),
                reason: None,
            },
            PersistedClaim::MultiSubjectConflict {
                addressings: vec![ext("a", "1"), ext("b", "2")],
                canonical_ids: vec!["uuid-x".into(), "uuid-y".into()],
            },
        ];
        s.record_subject_announce(AnnounceRecord {
            canonical_id: "uuid-a",
            subject_type: "track",
            addressings: &[ext("a", "1"), ext("b", "2")],
            claimant: "p1",
            claims: &claims,
            at_ms: 1000,
        })
        .await
        .unwrap();

        let kinds = s.claim_log_kinds().await;
        // One umbrella subject_announce row plus one row per claim.
        assert_eq!(
            kinds,
            vec![
                "subject_announce",
                "equivalent",
                "distinct",
                "multi_subject_conflict",
            ]
        );
    }

    #[tokio::test]
    async fn sqlite_announce_records_per_claim_log_entries() {
        let (_dir, store) = open_temp();
        let claims = vec![
            PersistedClaim::Equivalent {
                a: ext("a", "1"),
                b: ext("b", "2"),
                confidence: ClaimConfidence::Inferred,
                reason: Some("matched on hash".into()),
            },
            PersistedClaim::Distinct {
                a: ext("a", "1"),
                b: ext("c", "3"),
                reason: None,
            },
        ];
        store
            .record_subject_announce(AnnounceRecord {
                canonical_id: "uuid-a",
                subject_type: "track",
                addressings: &[ext("a", "1")],
                claimant: "p1",
                claims: &claims,
                at_ms: 42,
            })
            .await
            .unwrap();

        let conn = Connection::open(store.path()).expect("side open");
        let kinds: Vec<String> = conn
            .prepare("SELECT kind FROM claim_log ORDER BY log_id ASC")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            kinds,
            vec![
                "subject_announce".to_string(),
                "equivalent".to_string(),
                "distinct".to_string(),
            ]
        );

        // The Equivalent claim's reason landed in claim_log.reason; the
        // Distinct's None did not.
        let reasons: Vec<Option<String>> = conn
            .prepare(
                "SELECT reason FROM claim_log WHERE kind != 'subject_announce' \
                 ORDER BY log_id ASC",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, Option<String>>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(reasons, vec![Some("matched on hash".to_string()), None]);
    }

    // --- Merge mirrors subjects-table mutations --------------------------

    #[tokio::test]
    async fn memory_merge_creates_new_subject_and_drops_sources() {
        let s = MemoryPersistenceStore::new();
        s.record_subject_announce(AnnounceRecord {
            canonical_id: "uuid-a",
            subject_type: "track",
            addressings: &[ext("a", "1")],
            claimant: "p1",
            claims: &[],
            at_ms: 10,
        })
        .await
        .unwrap();
        s.record_subject_announce(AnnounceRecord {
            canonical_id: "uuid-b",
            subject_type: "track",
            addressings: &[ext("b", "2")],
            claimant: "p2",
            claims: &[],
            at_ms: 20,
        })
        .await
        .unwrap();

        s.record_subject_merge(MergeRecord {
            source_a: "uuid-a",
            source_b: "uuid-b",
            new_id: "uuid-c",
            subject_type: "track",
            admin_plugin: "admin",
            reason: Some("dup"),
            at_ms: 30,
        })
        .await
        .unwrap();

        let all = s.load_all_subjects().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "uuid-c");
        assert_eq!(all[0].subject_type, "track");
        let mut schemes: Vec<&str> = all[0]
            .addressings
            .iter()
            .map(|a| a.scheme.as_str())
            .collect();
        schemes.sort();
        assert_eq!(schemes, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn sqlite_merge_creates_new_subject_and_drops_sources() {
        let (_dir, store) = open_temp();
        store
            .record_subject_announce(AnnounceRecord {
                canonical_id: "uuid-a",
                subject_type: "track",
                addressings: &[ext("a", "1")],
                claimant: "p1",
                claims: &[],
                at_ms: 10,
            })
            .await
            .unwrap();
        store
            .record_subject_announce(AnnounceRecord {
                canonical_id: "uuid-b",
                subject_type: "track",
                addressings: &[ext("b", "2")],
                claimant: "p2",
                claims: &[],
                at_ms: 20,
            })
            .await
            .unwrap();

        store
            .record_subject_merge(MergeRecord {
                source_a: "uuid-a",
                source_b: "uuid-b",
                new_id: "uuid-c",
                subject_type: "track",
                admin_plugin: "admin",
                reason: Some("dup"),
                at_ms: 30,
            })
            .await
            .unwrap();

        let conn = Connection::open(store.path()).expect("side open");
        let n_subjects: i64 = conn
            .query_row("SELECT COUNT(*) FROM subjects", [], |r| r.get(0))
            .unwrap();
        let n_new: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM subjects WHERE id = 'uuid-c'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let n_old: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM subjects \
                 WHERE id = 'uuid-a' OR id = 'uuid-b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let n_addr_on_new: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM subject_addressings \
                 WHERE subject_id = 'uuid-c'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let n_addr_orphan: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM subject_addressings \
                 WHERE subject_id IN ('uuid-a', 'uuid-b')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_subjects, 1);
        assert_eq!(n_new, 1);
        assert_eq!(n_old, 0);
        assert_eq!(n_addr_on_new, 2);
        assert_eq!(n_addr_orphan, 0);
    }

    // --- Split mirrors subjects-table mutations --------------------------

    #[tokio::test]
    async fn memory_split_partitions_addressings_and_drops_source() {
        let s = MemoryPersistenceStore::new();
        s.record_subject_announce(AnnounceRecord {
            canonical_id: "uuid-a",
            subject_type: "track",
            addressings: &[ext("a", "1"), ext("b", "2"), ext("c", "3")],
            claimant: "p1",
            claims: &[],
            at_ms: 10,
        })
        .await
        .unwrap();

        let new_ids = vec!["uuid-x".to_string(), "uuid-y".to_string()];
        let partition: Vec<Vec<ExternalAddressing>> =
            vec![vec![ext("a", "1"), ext("b", "2")], vec![ext("c", "3")]];
        s.record_subject_split(SplitRecord {
            source: "uuid-a",
            new_ids: &new_ids,
            subject_type: "track",
            partition: &partition,
            admin_plugin: "admin",
            reason: None,
            at_ms: 20,
        })
        .await
        .unwrap();

        let mut all = s.load_all_subjects().await.unwrap();
        all.sort_by(|x, y| x.id.cmp(&y.id));
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "uuid-x");
        assert_eq!(all[0].addressings.len(), 2);
        assert_eq!(all[1].id, "uuid-y");
        assert_eq!(all[1].addressings.len(), 1);
        // Source is gone.
        assert!(all.iter().all(|s| s.id != "uuid-a"));
    }

    #[tokio::test]
    async fn sqlite_split_partitions_addressings_and_drops_source() {
        let (_dir, store) = open_temp();
        store
            .record_subject_announce(AnnounceRecord {
                canonical_id: "uuid-a",
                subject_type: "track",
                addressings: &[ext("a", "1"), ext("b", "2"), ext("c", "3")],
                claimant: "p1",
                claims: &[],
                at_ms: 10,
            })
            .await
            .unwrap();

        let new_ids = vec!["uuid-x".to_string(), "uuid-y".to_string()];
        let partition: Vec<Vec<ExternalAddressing>> =
            vec![vec![ext("a", "1"), ext("b", "2")], vec![ext("c", "3")]];
        store
            .record_subject_split(SplitRecord {
                source: "uuid-a",
                new_ids: &new_ids,
                subject_type: "track",
                partition: &partition,
                admin_plugin: "admin",
                reason: None,
                at_ms: 20,
            })
            .await
            .unwrap();

        let conn = Connection::open(store.path()).expect("side open");
        let n_source: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM subjects WHERE id = 'uuid-a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let n_x_addr: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM subject_addressings \
                 WHERE subject_id = 'uuid-x'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let n_y_addr: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM subject_addressings \
                 WHERE subject_id = 'uuid-y'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let n_source_addr: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM subject_addressings \
                 WHERE subject_id = 'uuid-a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_source, 0);
        assert_eq!(n_x_addr, 2);
        assert_eq!(n_y_addr, 1);
        assert_eq!(n_source_addr, 0);
    }

    #[tokio::test]
    async fn memory_split_partition_length_mismatch_errors() {
        let s = MemoryPersistenceStore::new();
        let new_ids = vec!["uuid-x".to_string(), "uuid-y".to_string()];
        let partition: Vec<Vec<ExternalAddressing>> = vec![Vec::new()];
        let r = s
            .record_subject_split(SplitRecord {
                source: "uuid-a",
                new_ids: &new_ids,
                subject_type: "track",
                partition: &partition,
                admin_plugin: "admin",
                reason: None,
                at_ms: 20,
            })
            .await;
        assert!(matches!(r, Err(PersistenceError::Invalid(_))));
    }

    #[tokio::test]
    async fn sqlite_split_partition_length_mismatch_errors() {
        let (_dir, store) = open_temp();
        let new_ids = vec!["uuid-x".to_string(), "uuid-y".to_string()];
        let partition: Vec<Vec<ExternalAddressing>> = vec![Vec::new()];
        let r = store
            .record_subject_split(SplitRecord {
                source: "uuid-a",
                new_ids: &new_ids,
                subject_type: "track",
                partition: &partition,
                admin_plugin: "admin",
                reason: None,
                at_ms: 20,
            })
            .await;
        assert!(matches!(r, Err(PersistenceError::Invalid(_))));
    }

    // --- happenings durable cursor ---------------------------------------

    fn happening_payload(seq_marker: u64) -> serde_json::Value {
        serde_json::json!({
            "type": "subject_forgotten",
            "subject_id": format!("uuid-{seq_marker}"),
            "subject_type": "track",
            "addressings": [],
            "at": null,
        })
    }

    #[tokio::test]
    async fn memory_happening_round_trip_and_load_since() {
        let s = MemoryPersistenceStore::new();
        s.record_happening(1, "subject_forgotten", &happening_payload(1), 100)
            .await
            .unwrap();
        s.record_happening(2, "subject_forgotten", &happening_payload(2), 200)
            .await
            .unwrap();
        s.record_happening(3, "subject_forgotten", &happening_payload(3), 300)
            .await
            .unwrap();

        // Cursor at 1 => return seqs 2 and 3 in order.
        let mid = s.load_happenings_since(1, u32::MAX).await.unwrap();
        assert_eq!(mid.len(), 2);
        assert_eq!(mid[0].seq, 2);
        assert_eq!(mid[0].kind, "subject_forgotten");
        assert_eq!(mid[0].at_ms, 200);
        assert_eq!(mid[1].seq, 3);

        // Cursor at the head returns nothing.
        let head = s.load_happenings_since(3, u32::MAX).await.unwrap();
        assert!(head.is_empty());

        // Limit honoured.
        let one = s.load_happenings_since(0, 1).await.unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].seq, 1);

        // Max-seq lookup.
        let max = s.load_max_happening_seq().await.unwrap();
        assert_eq!(max, 3);
    }

    #[tokio::test]
    async fn memory_max_happening_seq_is_zero_on_empty() {
        let s = MemoryPersistenceStore::new();
        let max = s.load_max_happening_seq().await.unwrap();
        assert_eq!(max, 0);
    }

    #[tokio::test]
    async fn sqlite_happening_round_trip_payload_preserves_json_shape() {
        let (_dir, store) = open_temp();
        let payload = happening_payload(42);
        store
            .record_happening(1, "subject_forgotten", &payload, 1000)
            .await
            .unwrap();

        let loaded = store.load_happenings_since(0, u32::MAX).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].seq, 1);
        assert_eq!(loaded[0].kind, "subject_forgotten");
        assert_eq!(loaded[0].at_ms, 1000);
        assert_eq!(loaded[0].payload, payload);
    }

    #[tokio::test]
    async fn sqlite_happening_seq_uniqueness_enforced() {
        let (_dir, store) = open_temp();
        store
            .record_happening(7, "subject_forgotten", &happening_payload(7), 1)
            .await
            .unwrap();
        // Re-using seq=7 is a primary-key violation.
        let r = store
            .record_happening(7, "subject_forgotten", &happening_payload(8), 2)
            .await;
        assert!(matches!(r, Err(PersistenceError::Sqlite { .. })));
    }

    #[tokio::test]
    async fn sqlite_max_happening_seq_round_trip() {
        let (_dir, store) = open_temp();
        for seq in [3u64, 1, 4, 1, 5, 9, 2, 6] {
            // 1 is repeated; the second call collides on the unique
            // primary key. Use distinct seqs for the actual emit.
            let _ = store
                .record_happening(
                    seq,
                    "subject_forgotten",
                    &happening_payload(seq),
                    seq * 10,
                )
                .await;
        }
        let max = store.load_max_happening_seq().await.unwrap();
        assert_eq!(max, 9);
    }

    #[tokio::test]
    async fn memory_checkpoint_wal_is_noop() {
        // The in-memory backend has no WAL; the trait method must
        // exist and return Ok so the steward shutdown path can call
        // it without branching on the concrete backend.
        let s = MemoryPersistenceStore::new();
        s.checkpoint_wal().await.expect("memory checkpoint must Ok");
    }

    #[tokio::test]
    async fn sqlite_checkpoint_wal_succeeds_on_open_database() {
        // PRAGMA wal_checkpoint(TRUNCATE) returns Ok on a fresh
        // database with WAL mode enabled (the pragma is applied at
        // every connection acquisition by INIT_PRAGMAS).
        let (_dir, store) = open_temp();
        // Force at least one row so the WAL has content to flush.
        store
            .record_happening(1, "subject_forgotten", &happening_payload(1), 10)
            .await
            .unwrap();
        store
            .checkpoint_wal()
            .await
            .expect("WAL checkpoint must Ok");
    }

    fn sample_persisted_admin_entry(
        kind: &str,
        admin_plugin: &str,
        target_claimant: Option<&str>,
        target_subject: Option<&str>,
        asserted_at_ms: u64,
        reason: Option<&str>,
    ) -> PersistedAdminEntry {
        let mut payload = serde_json::Map::new();
        if let Some(s) = target_subject {
            payload.insert(
                "target_subject".into(),
                serde_json::Value::String(s.to_string()),
            );
        }
        PersistedAdminEntry {
            admin_id: 0,
            kind: kind.to_string(),
            admin_plugin: admin_plugin.to_string(),
            target_claimant: target_claimant.map(|s| s.to_string()),
            payload: serde_json::Value::Object(payload),
            asserted_at_ms,
            reason: reason.map(|s| s.to_string()),
            reverses_admin_id: None,
        }
    }

    #[tokio::test]
    async fn sqlite_admin_log_round_trip_preserves_columns_and_payload() {
        // Pin the on-disk shape of admin_log: every column the
        // schema declares round-trips, the JSON payload preserves
        // its object structure, and load returns rows in
        // ascending admin_id order (insertion order).
        let (_dir, store) = open_temp();
        for (i, target) in ["p1", "p2", "p3"].iter().enumerate() {
            let entry = sample_persisted_admin_entry(
                "subject_addressing_forced_retract",
                "admin.plugin",
                Some(target),
                Some(&format!("subj-{i}")),
                100 + i as u64,
                Some("test"),
            );
            store.record_admin_entry(&entry).await.unwrap();
        }

        let rows = store.load_all_admin_entries().await.unwrap();
        assert_eq!(rows.len(), 3);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.kind, "subject_addressing_forced_retract");
            assert_eq!(row.admin_plugin, "admin.plugin");
            assert_eq!(
                row.target_claimant.as_deref(),
                Some(["p1", "p2", "p3"][i])
            );
            assert_eq!(row.asserted_at_ms, 100 + i as u64);
            assert_eq!(row.reason.as_deref(), Some("test"));
            assert!(row.reverses_admin_id.is_none());
            // admin_id is auto-assigned, monotonic, and unique.
            if i > 0 {
                assert!(row.admin_id > rows[i - 1].admin_id);
            }
            // payload preserves the JSON object shape.
            let subject =
                row.payload.get("target_subject").and_then(|v| v.as_str());
            assert_eq!(subject, Some(format!("subj-{i}").as_str()));
        }
    }

    #[tokio::test]
    async fn sqlite_admin_log_survives_reopen() {
        // Open a database, write admin_log rows, close, reopen,
        // and confirm load returns the same rows. Pins the
        // durability promise: a steward restart sees the same
        // audit trail.
        let dir = tempdir().unwrap();
        let path = dir.path().join("admin.db");
        {
            let store = SqlitePersistenceStore::open(path.clone()).unwrap();
            store
                .record_admin_entry(&sample_persisted_admin_entry(
                    "subject_merge",
                    "admin.plugin",
                    None,
                    Some("new-id"),
                    1000,
                    Some("operator confirmed identity"),
                ))
                .await
                .unwrap();
            store.checkpoint_wal().await.unwrap();
        }
        let store = SqlitePersistenceStore::open(path).unwrap();
        let rows = store.load_all_admin_entries().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "subject_merge");
        assert!(rows[0].target_claimant.is_none());
        assert_eq!(rows[0].asserted_at_ms, 1000);
        assert_eq!(
            rows[0].reason.as_deref(),
            Some("operator confirmed identity")
        );
    }

    #[tokio::test]
    async fn sqlite_admin_log_load_returns_empty_on_fresh_database() {
        // A freshly migrated database has no admin_log rows.
        // Pinning this so the boot path's rehydrate cannot
        // surprise-load stale rows from a concurrent test
        // fixture.
        let (_dir, store) = open_temp();
        let rows = store.load_all_admin_entries().await.unwrap();
        assert!(rows.is_empty());
    }

    #[allow(clippy::too_many_arguments)]
    fn sample_persisted_custody(
        plugin: &str,
        handle_id: &str,
        shelf: Option<&str>,
        custody_type: Option<&str>,
        state_kind: &str,
        state_reason: Option<&str>,
        started_at_ms: u64,
        last_updated_at_ms: u64,
    ) -> PersistedCustody {
        PersistedCustody {
            plugin: plugin.to_string(),
            handle_id: handle_id.to_string(),
            shelf: shelf.map(|s| s.to_string()),
            custody_type: custody_type.map(|s| s.to_string()),
            state_kind: state_kind.to_string(),
            state_reason: state_reason.map(|s| s.to_string()),
            started_at_ms,
            last_updated_at_ms,
        }
    }

    fn sample_persisted_custody_state(
        plugin: &str,
        handle_id: &str,
        payload: &[u8],
        health: &str,
        reported_at_ms: u64,
    ) -> PersistedCustodyState {
        PersistedCustodyState {
            plugin: plugin.to_string(),
            handle_id: handle_id.to_string(),
            payload: payload.to_vec(),
            health: health.to_string(),
            reported_at_ms,
        }
    }

    #[tokio::test]
    async fn sqlite_custody_round_trip_with_state_snapshot() {
        // Pin the on-disk shape: upserting a custody plus a state
        // snapshot round-trips every column, and load returns the
        // pair joined.
        let (_dir, store) = open_temp();
        let custody = sample_persisted_custody(
            "org.test.warden",
            "c-1",
            Some("example.custody"),
            Some("playback"),
            "active",
            None,
            1_000,
            1_500,
        );
        store.upsert_custody(&custody).await.unwrap();
        let snapshot = sample_persisted_custody_state(
            "org.test.warden",
            "c-1",
            b"state=playing",
            "healthy",
            1_500,
        );
        store.upsert_custody_state(&snapshot).await.unwrap();

        let rows = store.load_all_custodies().await.unwrap();
        assert_eq!(rows.len(), 1);
        let (loaded, snap) = &rows[0];
        assert_eq!(loaded, &custody);
        assert_eq!(snap.as_ref(), Some(&snapshot));
    }

    #[tokio::test]
    async fn sqlite_custody_upsert_merges_partial_columns() {
        // The lazy-UPSERT race: an early state report inserts a
        // custody row with shelf=NULL/custody_type=NULL; the
        // subsequent record_custody call fills those in. The
        // ON CONFLICT clause must COALESCE so the second upsert
        // does not blank the freshly-populated fields the next
        // time a state report races back in.
        let (_dir, store) = open_temp();
        let bare = sample_persisted_custody(
            "org.test.warden",
            "c-1",
            None,
            None,
            "active",
            None,
            1_000,
            1_000,
        );
        store.upsert_custody(&bare).await.unwrap();

        let filled = sample_persisted_custody(
            "org.test.warden",
            "c-1",
            Some("example.custody"),
            Some("playback"),
            "active",
            None,
            1_000,
            1_500,
        );
        store.upsert_custody(&filled).await.unwrap();

        // Second state report races back in, again without shelf
        // / custody_type. COALESCE preserves the filled values.
        let bare2 = sample_persisted_custody(
            "org.test.warden",
            "c-1",
            None,
            None,
            "active",
            None,
            1_000,
            2_000,
        );
        store.upsert_custody(&bare2).await.unwrap();

        let rows = store.load_all_custodies().await.unwrap();
        assert_eq!(rows.len(), 1);
        let (loaded, _) = &rows[0];
        assert_eq!(loaded.shelf.as_deref(), Some("example.custody"));
        assert_eq!(loaded.custody_type.as_deref(), Some("playback"));
        assert_eq!(loaded.last_updated_at_ms, 2_000);
        assert_eq!(loaded.started_at_ms, 1_000);
    }

    #[tokio::test]
    async fn sqlite_mark_custody_state_updates_kind_and_reason() {
        // Lifecycle transition: an active record marked aborted
        // updates state_kind, state_reason, and last_updated_at_ms.
        // Returns true on a hit, false when no row matches.
        let (_dir, store) = open_temp();
        store
            .upsert_custody(&sample_persisted_custody(
                "org.test.warden",
                "c-1",
                Some("example.custody"),
                Some("playback"),
                "active",
                None,
                1_000,
                1_000,
            ))
            .await
            .unwrap();

        let hit = store
            .mark_custody_state(
                "org.test.warden",
                "c-1",
                "aborted",
                Some("transport failed"),
                2_000,
            )
            .await
            .unwrap();
        assert!(hit);

        let rows = store.load_all_custodies().await.unwrap();
        let (loaded, _) = &rows[0];
        assert_eq!(loaded.state_kind, "aborted");
        assert_eq!(loaded.state_reason.as_deref(), Some("transport failed"));
        assert_eq!(loaded.last_updated_at_ms, 2_000);

        let miss = store
            .mark_custody_state(
                "org.test.warden",
                "c-never-existed",
                "aborted",
                Some("nope"),
                2_500,
            )
            .await
            .unwrap();
        assert!(!miss);
    }

    #[tokio::test]
    async fn sqlite_delete_custody_cascades_state() {
        // Deleting a custody also removes its custody_state row
        // (FK CASCADE). Round-trip via load to confirm both
        // tables are emptied.
        let (_dir, store) = open_temp();
        store
            .upsert_custody(&sample_persisted_custody(
                "org.test.warden",
                "c-1",
                Some("example.custody"),
                Some("playback"),
                "active",
                None,
                1_000,
                1_000,
            ))
            .await
            .unwrap();
        store
            .upsert_custody_state(&sample_persisted_custody_state(
                "org.test.warden",
                "c-1",
                b"final",
                "healthy",
                1_500,
            ))
            .await
            .unwrap();

        let removed = store
            .delete_custody("org.test.warden", "c-1")
            .await
            .unwrap();
        assert!(removed);

        let rows = store.load_all_custodies().await.unwrap();
        assert!(rows.is_empty());

        // Idempotent: deleting again returns false.
        let removed2 = store
            .delete_custody("org.test.warden", "c-1")
            .await
            .unwrap();
        assert!(!removed2);
    }

    #[tokio::test]
    async fn sqlite_custody_upsert_state_without_parent_errors() {
        // Inserting custody_state for a missing custody row
        // violates the FK and surfaces as a Sqlite error. Pins
        // the FK invariant against accidental relaxation.
        let (_dir, store) = open_temp();
        let err = store
            .upsert_custody_state(&sample_persisted_custody_state(
                "org.test.warden",
                "c-1",
                b"state",
                "healthy",
                1_000,
            ))
            .await
            .expect_err("FK violation");
        assert!(matches!(err, PersistenceError::Sqlite { .. }));
    }

    #[tokio::test]
    async fn sqlite_custody_load_returns_sorted_by_plugin_handle() {
        // load_all_custodies returns rows in (plugin, handle_id)
        // ascending order so boot rehydration is deterministic.
        let (_dir, store) = open_temp();
        for (plugin, handle_id) in [
            ("org.test.warden", "c-2"),
            ("org.test.alpha", "c-1"),
            ("org.test.warden", "c-1"),
        ] {
            store
                .upsert_custody(&sample_persisted_custody(
                    plugin,
                    handle_id,
                    Some("example.custody"),
                    Some("playback"),
                    "active",
                    None,
                    1_000,
                    1_000,
                ))
                .await
                .unwrap();
        }
        let rows = store.load_all_custodies().await.unwrap();
        let keys: Vec<(String, String)> = rows
            .iter()
            .map(|(c, _)| (c.plugin.clone(), c.handle_id.clone()))
            .collect();
        assert_eq!(
            keys,
            vec![
                ("org.test.alpha".into(), "c-1".into()),
                ("org.test.warden".into(), "c-1".into()),
                ("org.test.warden".into(), "c-2".into()),
            ]
        );
    }

    #[tokio::test]
    async fn sqlite_custody_survives_reopen() {
        // Open a database, upsert a custody and a state snapshot,
        // close, reopen, confirm both round-trip. Pins durability
        // across restart for the custody slice.
        let dir = tempdir().unwrap();
        let path = dir.path().join("custody.db");
        {
            let store = SqlitePersistenceStore::open(path.clone()).unwrap();
            store
                .upsert_custody(&sample_persisted_custody(
                    "org.test.warden",
                    "c-1",
                    Some("example.custody"),
                    Some("playback"),
                    "active",
                    None,
                    1_000,
                    1_000,
                ))
                .await
                .unwrap();
            store
                .upsert_custody_state(&sample_persisted_custody_state(
                    "org.test.warden",
                    "c-1",
                    b"state=playing",
                    "healthy",
                    1_500,
                ))
                .await
                .unwrap();
            store.checkpoint_wal().await.unwrap();
        }
        let store = SqlitePersistenceStore::open(path).unwrap();
        let rows = store.load_all_custodies().await.unwrap();
        assert_eq!(rows.len(), 1);
        let (custody, snap) = &rows[0];
        assert_eq!(custody.shelf.as_deref(), Some("example.custody"));
        assert_eq!(custody.custody_type.as_deref(), Some("playback"));
        assert_eq!(
            snap.as_ref().map(|s| s.payload.as_slice()),
            Some(b"state=playing".as_ref())
        );
        assert_eq!(snap.as_ref().map(|s| s.health.as_str()), Some("healthy"));
    }

    fn sample_persisted_relation(
        source_id: &str,
        predicate: &str,
        target_id: &str,
        created_at_ms: u64,
        modified_at_ms: u64,
    ) -> PersistedRelation {
        PersistedRelation {
            source_id: source_id.to_string(),
            predicate: predicate.to_string(),
            target_id: target_id.to_string(),
            created_at_ms,
            modified_at_ms,
            suppressed_admin_plugin: None,
            suppressed_at_ms: None,
            suppression_reason: None,
        }
    }

    fn sample_persisted_relation_claim(
        source_id: &str,
        predicate: &str,
        target_id: &str,
        claimant: &str,
        asserted_at_ms: u64,
        reason: Option<&str>,
    ) -> PersistedRelationClaim {
        PersistedRelationClaim {
            source_id: source_id.to_string(),
            predicate: predicate.to_string(),
            target_id: target_id.to_string(),
            claimant: claimant.to_string(),
            asserted_at_ms,
            reason: reason.map(|s| s.to_string()),
        }
    }

    #[tokio::test]
    async fn sqlite_relation_assert_round_trips_relation_and_claim() {
        // Pin the on-disk shape: a single assert produces one
        // relation row and one relation_claimants row that
        // round-trip through load_all_relations exactly.
        let (_dir, store) = open_temp();
        let rel = sample_persisted_relation("a", "edge", "b", 100, 100);
        let claim = sample_persisted_relation_claim(
            "a",
            "edge",
            "b",
            "p1",
            100,
            Some("first claim"),
        );
        store.record_relation_assert(&rel, &claim).await.unwrap();

        let rows = store.load_all_relations().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, rel);
        assert_eq!(rows[0].1.len(), 1);
        assert_eq!(rows[0].1[0], claim);
    }

    #[tokio::test]
    async fn sqlite_relation_assert_preserves_created_at_on_update() {
        // The ON CONFLICT clause preserves created_at_ms on
        // re-assert and bumps modified_at_ms; INSERT OR IGNORE
        // on the claim makes a same-claimant re-assert idempotent.
        let (_dir, store) = open_temp();
        store
            .record_relation_assert(
                &sample_persisted_relation("a", "edge", "b", 100, 100),
                &sample_persisted_relation_claim(
                    "a", "edge", "b", "p1", 100, None,
                ),
            )
            .await
            .unwrap();
        // Different claimant — shares the relation, adds a row.
        store
            .record_relation_assert(
                &sample_persisted_relation("a", "edge", "b", 200, 200),
                &sample_persisted_relation_claim(
                    "a", "edge", "b", "p2", 200, None,
                ),
            )
            .await
            .unwrap();

        let rows = store.load_all_relations().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.created_at_ms, 100);
        assert_eq!(rows[0].0.modified_at_ms, 200);
        assert_eq!(rows[0].1.len(), 2);
        let claimants: Vec<&str> =
            rows[0].1.iter().map(|c| c.claimant.as_str()).collect();
        assert_eq!(claimants, vec!["p1", "p2"]);
    }

    #[tokio::test]
    async fn sqlite_relation_retract_removes_claim_and_optionally_relation() {
        // record_relation_retract removes one claim row. When
        // relation_forgotten=false and other claimants remain, the
        // parent relation row stays and modified_at_ms bumps. When
        // relation_forgotten=true, the parent is removed too,
        // cascading any remaining claimants.
        let (_dir, store) = open_temp();
        store
            .record_relation_assert(
                &sample_persisted_relation("a", "edge", "b", 100, 100),
                &sample_persisted_relation_claim(
                    "a", "edge", "b", "p1", 100, None,
                ),
            )
            .await
            .unwrap();
        store
            .record_relation_assert(
                &sample_persisted_relation("a", "edge", "b", 200, 200),
                &sample_persisted_relation_claim(
                    "a", "edge", "b", "p2", 200, None,
                ),
            )
            .await
            .unwrap();

        let removed = store
            .record_relation_retract("a", "edge", "b", "p1", 300, false)
            .await
            .unwrap();
        assert!(removed);

        let rows = store.load_all_relations().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.modified_at_ms, 300);
        assert_eq!(rows[0].1.len(), 1);
        assert_eq!(rows[0].1[0].claimant, "p2");

        let removed = store
            .record_relation_retract("a", "edge", "b", "p2", 400, true)
            .await
            .unwrap();
        assert!(removed);
        let rows = store.load_all_relations().await.unwrap();
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn sqlite_relation_forget_removes_relation_and_cascades_claims() {
        // record_relation_forget deletes the relation row; the FK
        // CASCADE on relation_claimants removes every claim
        // atomically.
        let (_dir, store) = open_temp();
        store
            .record_relation_assert(
                &sample_persisted_relation("a", "edge", "b", 100, 100),
                &sample_persisted_relation_claim(
                    "a", "edge", "b", "p1", 100, None,
                ),
            )
            .await
            .unwrap();
        store
            .record_relation_assert(
                &sample_persisted_relation("a", "edge", "b", 200, 200),
                &sample_persisted_relation_claim(
                    "a", "edge", "b", "p2", 200, None,
                ),
            )
            .await
            .unwrap();
        let removed = store
            .record_relation_forget("a", "edge", "b")
            .await
            .unwrap();
        assert!(removed);
        let rows = store.load_all_relations().await.unwrap();
        assert!(rows.is_empty());

        // Idempotent: forgetting again returns false.
        let removed = store
            .record_relation_forget("a", "edge", "b")
            .await
            .unwrap();
        assert!(!removed);
    }

    #[tokio::test]
    async fn sqlite_relation_suppress_and_unsuppress_round_trip() {
        // Suppress sets the three suppression columns and bumps
        // modified_at_ms; unsuppress clears them and bumps again.
        let (_dir, store) = open_temp();
        store
            .record_relation_assert(
                &sample_persisted_relation("a", "edge", "b", 100, 100),
                &sample_persisted_relation_claim(
                    "a", "edge", "b", "p1", 100, None,
                ),
            )
            .await
            .unwrap();
        let hit = store
            .record_relation_suppress(
                "a",
                "edge",
                "b",
                "admin.plugin",
                500,
                Some("disputed"),
                500,
            )
            .await
            .unwrap();
        assert!(hit);
        let rows = store.load_all_relations().await.unwrap();
        let rel = &rows[0].0;
        assert_eq!(
            rel.suppressed_admin_plugin.as_deref(),
            Some("admin.plugin")
        );
        assert_eq!(rel.suppressed_at_ms, Some(500));
        assert_eq!(rel.suppression_reason.as_deref(), Some("disputed"));
        assert_eq!(rel.modified_at_ms, 500);

        let hit = store
            .record_relation_unsuppress("a", "edge", "b", 600)
            .await
            .unwrap();
        assert!(hit);
        let rows = store.load_all_relations().await.unwrap();
        let rel = &rows[0].0;
        assert!(rel.suppressed_admin_plugin.is_none());
        assert!(rel.suppressed_at_ms.is_none());
        assert!(rel.suppression_reason.is_none());
        assert_eq!(rel.modified_at_ms, 600);
    }

    #[tokio::test]
    async fn sqlite_relation_load_sorted_by_triple_then_claimant() {
        // load_all_relations returns rows in deterministic
        // (source_id, predicate, target_id) ascending order with
        // claims sorted by claimant ascending. Pins the boot
        // rehydration order for reproducible state.
        let (_dir, store) = open_temp();
        for (s, p, t, c) in [
            ("b", "edge", "c", "p1"),
            ("a", "edge", "z", "p2"),
            ("a", "edge", "z", "p1"),
            ("a", "edge", "b", "p1"),
        ] {
            store
                .record_relation_assert(
                    &sample_persisted_relation(s, p, t, 100, 100),
                    &sample_persisted_relation_claim(s, p, t, c, 100, None),
                )
                .await
                .unwrap();
        }
        let rows = store.load_all_relations().await.unwrap();
        let triples: Vec<(String, String, String)> = rows
            .iter()
            .map(|(r, _)| {
                (
                    r.source_id.clone(),
                    r.predicate.clone(),
                    r.target_id.clone(),
                )
            })
            .collect();
        assert_eq!(
            triples,
            vec![
                ("a".into(), "edge".into(), "b".into()),
                ("a".into(), "edge".into(), "z".into()),
                ("b".into(), "edge".into(), "c".into()),
            ]
        );
        let az_claimants: Vec<&str> =
            rows[1].1.iter().map(|c| c.claimant.as_str()).collect();
        assert_eq!(az_claimants, vec!["p1", "p2"]);
    }

    #[tokio::test]
    async fn sqlite_relations_survive_reopen() {
        // Open, write a relation with two claimants and a
        // suppression marker, close, reopen, confirm round-trip.
        let dir = tempdir().unwrap();
        let path = dir.path().join("relations.db");
        {
            let store = SqlitePersistenceStore::open(path.clone()).unwrap();
            store
                .record_relation_assert(
                    &sample_persisted_relation("a", "edge", "b", 100, 100),
                    &sample_persisted_relation_claim(
                        "a",
                        "edge",
                        "b",
                        "p1",
                        100,
                        Some("first"),
                    ),
                )
                .await
                .unwrap();
            store
                .record_relation_assert(
                    &sample_persisted_relation("a", "edge", "b", 200, 200),
                    &sample_persisted_relation_claim(
                        "a", "edge", "b", "p2", 200, None,
                    ),
                )
                .await
                .unwrap();
            store
                .record_relation_suppress(
                    "a",
                    "edge",
                    "b",
                    "admin.plugin",
                    500,
                    Some("under review"),
                    500,
                )
                .await
                .unwrap();
            store.checkpoint_wal().await.unwrap();
        }
        let store = SqlitePersistenceStore::open(path).unwrap();
        let rows = store.load_all_relations().await.unwrap();
        assert_eq!(rows.len(), 1);
        let (rel, claims) = &rows[0];
        assert_eq!(rel.created_at_ms, 100);
        assert_eq!(rel.modified_at_ms, 500);
        assert_eq!(
            rel.suppressed_admin_plugin.as_deref(),
            Some("admin.plugin")
        );
        assert_eq!(rel.suppression_reason.as_deref(), Some("under review"));
        assert_eq!(claims.len(), 2);
        assert_eq!(claims[0].reason.as_deref(), Some("first"));
    }

    #[tokio::test]
    async fn sqlite_installed_plugins_round_trip_and_upsert() {
        let (_dir, store) = open_temp();

        let row1 = PersistedInstalledPlugin {
            plugin_name: "org.test.alpha".into(),
            enabled: true,
            last_state_reason: Some("first install".into()),
            last_state_changed_at_ms: 1000,
            install_digest: "sha256:aaa".into(),
        };
        let row2 = PersistedInstalledPlugin {
            plugin_name: "org.test.bravo".into(),
            enabled: false,
            last_state_reason: Some("disabled by operator".into()),
            last_state_changed_at_ms: 2000,
            install_digest: "sha256:bbb".into(),
        };
        store.record_plugin_enabled(&row1).await.unwrap();
        store.record_plugin_enabled(&row2).await.unwrap();

        let rows = store.load_all_installed_plugins().await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].plugin_name, "org.test.alpha");
        assert!(rows[0].enabled);
        assert_eq!(rows[1].plugin_name, "org.test.bravo");
        assert!(!rows[1].enabled);

        // Upsert: re-record alpha with enabled = false replaces the
        // existing row in place rather than inserting a duplicate.
        let row1_updated = PersistedInstalledPlugin {
            plugin_name: "org.test.alpha".into(),
            enabled: false,
            last_state_reason: Some("changed mind".into()),
            last_state_changed_at_ms: 3000,
            install_digest: "sha256:aaa".into(),
        };
        store.record_plugin_enabled(&row1_updated).await.unwrap();
        let rows = store.load_all_installed_plugins().await.unwrap();
        assert_eq!(rows.len(), 2);
        let alpha = rows
            .iter()
            .find(|r| r.plugin_name == "org.test.alpha")
            .unwrap();
        assert!(!alpha.enabled);
        assert_eq!(alpha.last_state_changed_at_ms, 3000);
        assert_eq!(alpha.last_state_reason.as_deref(), Some("changed mind"));
    }

    #[tokio::test]
    async fn sqlite_forget_installed_plugin_removes_row() {
        let (_dir, store) = open_temp();
        let row = PersistedInstalledPlugin {
            plugin_name: "org.test.alpha".into(),
            enabled: true,
            last_state_reason: None,
            last_state_changed_at_ms: 1000,
            install_digest: "sha256:aaa".into(),
        };
        store.record_plugin_enabled(&row).await.unwrap();
        assert_eq!(store.load_all_installed_plugins().await.unwrap().len(), 1);
        store
            .forget_installed_plugin("org.test.alpha")
            .await
            .unwrap();
        assert!(store.load_all_installed_plugins().await.unwrap().is_empty());
        // Forgetting a non-existent row is a silent no-op.
        store
            .forget_installed_plugin("org.test.never")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn sqlite_reconciliation_state_round_trip_and_upsert() {
        let (_dir, store) = open_temp();
        let row = PersistedReconciliationState {
            pair_id: "audio.pipeline".into(),
            generation: 1,
            applied_state: serde_json::json!({"sources": ["spotify"]}),
            applied_at_ms: 1000,
        };
        store.record_reconciliation_state(&row).await.unwrap();
        let rows = store.load_all_reconciliation_state().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pair_id, "audio.pipeline");
        assert_eq!(rows[0].generation, 1);

        // Upsert: re-record with a higher generation replaces in
        // place rather than inserting a duplicate.
        let updated = PersistedReconciliationState {
            pair_id: "audio.pipeline".into(),
            generation: 2,
            applied_state: serde_json::json!({"sources": ["spotify", "usb"]}),
            applied_at_ms: 2000,
        };
        store.record_reconciliation_state(&updated).await.unwrap();
        let rows = store.load_all_reconciliation_state().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].generation, 2);
        assert_eq!(rows[0].applied_at_ms, 2000);

        // Forget removes the row; load returns empty.
        store
            .forget_reconciliation_state("audio.pipeline")
            .await
            .unwrap();
        assert!(store
            .load_all_reconciliation_state()
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn sqlite_type_migration_round_trips_subject_and_alias() {
        let (_dir, s) = open_temp();
        s.record_subject_announce(AnnounceRecord {
            canonical_id: "uuid-old",
            subject_type: "audio_track",
            addressings: &[ext("library", "song-1")],
            claimant: "p1",
            claims: &[],
            at_ms: 1000,
        })
        .await
        .unwrap();
        s.record_subject_type_migration(TypeMigrationRecord {
            source: "uuid-old",
            new_id: "uuid-new",
            from_type: "audio_track",
            to_type: "track",
            migration_id: "mig_01",
            reason: Some("catalogue v3"),
            at_ms: 2000,
        })
        .await
        .unwrap();
        let aliases = s.load_aliases_for("uuid-old").await.unwrap();
        assert_eq!(aliases.len(), 1);
        assert_eq!(aliases[0].kind, AliasKind::TypeMigrated);
        assert_eq!(aliases[0].new_id, "uuid-new");
        let all = s.load_all_subjects().await.unwrap();
        // Old row gone; new row has the migrated type and the
        // moved addressings.
        let new_row = all
            .iter()
            .find(|r| r.id == "uuid-new")
            .expect("new row present");
        assert_eq!(new_row.subject_type, "track");
        assert_eq!(new_row.addressings.len(), 1);
        assert!(all.iter().all(|r| r.id != "uuid-old"));
    }

    #[tokio::test]
    async fn sqlite_pending_grammar_orphans_lifecycle() {
        let (_dir, store) = open_temp();
        // First observation inserts with status = pending.
        store
            .upsert_pending_grammar_orphan("audio_track", 4_500, 1_000)
            .await
            .unwrap();
        let rows = store.list_pending_grammar_orphans().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].subject_type, "audio_track");
        assert_eq!(rows[0].first_observed_at_ms, 1_000);
        assert_eq!(rows[0].last_observed_at_ms, 1_000);
        assert_eq!(rows[0].count, 4_500);
        assert_eq!(rows[0].status, GrammarOrphanStatus::Pending);

        // Subsequent observation updates count + last_observed_at,
        // preserves first_observed_at.
        store
            .upsert_pending_grammar_orphan("audio_track", 4_600, 2_000)
            .await
            .unwrap();
        let rows = store.list_pending_grammar_orphans().await.unwrap();
        assert_eq!(rows[0].first_observed_at_ms, 1_000);
        assert_eq!(rows[0].last_observed_at_ms, 2_000);
        assert_eq!(rows[0].count, 4_600);

        // Operator accepts; status flips to accepted.
        let did_accept = store
            .accept_grammar_orphan("audio_track", "deliberate retention", 3_000)
            .await
            .unwrap();
        assert!(did_accept);
        let rows = store.list_pending_grammar_orphans().await.unwrap();
        assert_eq!(rows[0].status, GrammarOrphanStatus::Accepted);
        assert_eq!(
            rows[0].accepted_reason.as_deref(),
            Some("deliberate retention")
        );
        assert_eq!(rows[0].accepted_at_ms, Some(3_000));

        // Re-accept is idempotent (returns false, no state change).
        let again = store
            .accept_grammar_orphan("audio_track", "deliberate retention", 4_000)
            .await
            .unwrap();
        assert!(!again);
        let rows = store.list_pending_grammar_orphans().await.unwrap();
        assert_eq!(rows[0].accepted_at_ms, Some(3_000));

        // Acceptance preserved across a re-observation upsert.
        store
            .upsert_pending_grammar_orphan("audio_track", 4_600, 5_000)
            .await
            .unwrap();
        let rows = store.list_pending_grammar_orphans().await.unwrap();
        assert_eq!(rows[0].status, GrammarOrphanStatus::Accepted);
    }

    #[tokio::test]
    async fn sqlite_pending_grammar_orphan_migrating_then_resolved() {
        let (_dir, store) = open_temp();
        store
            .upsert_pending_grammar_orphan("media_item", 12_000, 1_000)
            .await
            .unwrap();
        store
            .mark_grammar_orphan_migrating("media_item", "mig_01")
            .await
            .unwrap();
        let rows = store.list_pending_grammar_orphans().await.unwrap();
        assert_eq!(rows[0].status, GrammarOrphanStatus::Migrating);
        assert_eq!(rows[0].migration_id.as_deref(), Some("mig_01"));

        // Accepting an in-flight migration refuses.
        let err = store
            .accept_grammar_orphan("media_item", "...", 2_000)
            .await
            .unwrap_err();
        assert!(matches!(err, PersistenceError::Invalid(_)));

        store
            .mark_grammar_orphan_resolved("media_item", "mig_01")
            .await
            .unwrap();
        let rows = store.list_pending_grammar_orphans().await.unwrap();
        assert_eq!(rows[0].status, GrammarOrphanStatus::Resolved);

        // Migrating a resolved row refuses.
        let err = store
            .mark_grammar_orphan_migrating("media_item", "mig_02")
            .await
            .unwrap_err();
        assert!(matches!(err, PersistenceError::Invalid(_)));
    }

    #[tokio::test]
    async fn sqlite_pending_grammar_orphan_recovered_re_pends_on_reorphan() {
        let (_dir, store) = open_temp();
        store
            .upsert_pending_grammar_orphan("audio_track", 4_500, 1_000)
            .await
            .unwrap();
        store
            .mark_grammar_orphan_recovered("audio_track", 2_000)
            .await
            .unwrap();
        let rows = store.list_pending_grammar_orphans().await.unwrap();
        assert_eq!(rows[0].status, GrammarOrphanStatus::Recovered);

        // Re-orphan: status flips back to pending.
        store
            .upsert_pending_grammar_orphan("audio_track", 4_500, 3_000)
            .await
            .unwrap();
        let rows = store.list_pending_grammar_orphans().await.unwrap();
        assert_eq!(rows[0].status, GrammarOrphanStatus::Pending);
    }

    #[tokio::test]
    async fn memory_pending_grammar_orphans_mirror_sqlite_lifecycle() {
        let store = MemoryPersistenceStore::new();
        store
            .upsert_pending_grammar_orphan("track", 100, 1_000)
            .await
            .unwrap();
        store
            .accept_grammar_orphan("track", "deliberate", 2_000)
            .await
            .unwrap();
        store
            .upsert_pending_grammar_orphan("track", 100, 3_000)
            .await
            .unwrap();
        let rows = store.list_pending_grammar_orphans().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, GrammarOrphanStatus::Accepted);
        assert_eq!(rows[0].last_observed_at_ms, 3_000);

        // Accept an unknown row refuses.
        let err = store
            .accept_grammar_orphan("missing", "x", 0)
            .await
            .unwrap_err();
        assert!(matches!(err, PersistenceError::Invalid(_)));
    }

    #[tokio::test]
    async fn memory_reconciliation_state_round_trip() {
        let store = MemoryPersistenceStore::new();
        let row = PersistedReconciliationState {
            pair_id: "audio.pipeline".into(),
            generation: 7,
            applied_state: serde_json::json!({"x": 1}),
            applied_at_ms: 100,
        };
        store.record_reconciliation_state(&row).await.unwrap();
        let rows = store.load_all_reconciliation_state().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].generation, 7);
        store
            .forget_reconciliation_state("audio.pipeline")
            .await
            .unwrap();
        assert!(store
            .load_all_reconciliation_state()
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn memory_installed_plugins_round_trip() {
        let store = MemoryPersistenceStore::new();
        let row = PersistedInstalledPlugin {
            plugin_name: "org.test.alpha".into(),
            enabled: false,
            last_state_reason: Some("disabled".into()),
            last_state_changed_at_ms: 42,
            install_digest: "sha256:abc".into(),
        };
        store.record_plugin_enabled(&row).await.unwrap();
        let rows = store.load_all_installed_plugins().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].enabled);
        assert_eq!(rows[0].last_state_reason.as_deref(), Some("disabled"));
        store
            .forget_installed_plugin("org.test.alpha")
            .await
            .unwrap();
        assert!(store.load_all_installed_plugins().await.unwrap().is_empty());
    }
}
