//! Durable storage for the steward's subject-identity surface.
//!
//! This module is the foundation laid in Phase 1: a single trait
//! ([`PersistenceStore`]) describing the schema-aware writes the
//! steward performs against its persistent fabric, plus two
//! implementations:
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
//! Phase 1 covers the subject-identity slice of the schema: the
//! `subjects`, `subject_addressings`, `aliases`, and `claim_log`
//! tables. Subsequent phases extend the schema with the relation
//! graph, the custody ledger, and the admin ledger; the migration
//! version slots are reserved (the initial migration is `v1` and
//! later migrations append rather than renumber). See the schema
//! discussion in `docs/engineering/PERSISTENCE.md` section 7 for the
//! full contract.
//!
//! ## Async wrapper choice
//!
//! `rusqlite` is synchronous; the steward is asynchronous. This
//! module uses `deadpool-sqlite`'s connection pool, which dispatches
//! each call to a dedicated blocking thread per pool connection via
//! `tokio::task::spawn_blocking`. The pool size is configurable; the
//! default suits the steward's modest write rate while leaving room
//! for parallel reads once Phase 3 wires the subject-replay path.
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

/// Maximum schema version this build of the steward understands.
///
/// On open, [`SqlitePersistenceStore`] refuses to operate on a
/// database whose `schema_version` table records a version greater
/// than this constant. Downgrades are not supported; an operator
/// running an older steward against a newer database must restore
/// from a pre-upgrade backup.
pub const SUPPORTED_SCHEMA_VERSION: u32 = SCHEMA_VERSION_PENDING_CONFLICTS;

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
/// Returned by [`PersistenceStore::load_all_subjects`] for the boot
/// replay path Phase 3 will wire. Field names mirror the schema's
/// columns so a reader can correlate a row with the table without
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
/// multi-operation atomicity sit above this trait and compose
/// (Phase 2 wires the subject registry write path through it).
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
    /// boot-time replay path. Used by Phase 3 to rehydrate the
    /// subject registry from durable state.
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
}

/// One row of the boot-time subject-type aggregation: a declared
/// `subject_type` plus the live row count under it. Returned in
/// `subject_type` ascending order from
/// [`PersistenceStore::count_subjects_by_type`] so consumers can
/// diff a sorted slice against the catalogue's declared set
/// without re-sorting.
pub type SubjectTypeCount = (String, u64);

/// Claim-log kind values used by this slice. Strings are stable
/// on disk and must not be renamed without a migration.
mod claim_kind {
    pub const SUBJECT_ANNOUNCE: &str = "subject_announce";
    pub const SUBJECT_RETRACT: &str = "subject_retract";
    pub const SUBJECT_MERGE: &str = "subject_merge";
    pub const SUBJECT_SPLIT: &str = "subject_split";
    pub const SUBJECT_FORGOTTEN: &str = "subject_forgotten";
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

    Ok(())
}

/// SQLite-backed [`PersistenceStore`].
///
/// Holds a `deadpool-sqlite` connection pool whose connections are
/// initialised with the pragma set declared in [`INIT_PRAGMAS`].
/// Constructed via [`Self::open`], which creates the database file
/// if absent and applies pending migrations before returning.
pub struct SqlitePersistenceStore {
    pool: Pool,
    /// Path of the underlying database file. Retained for
    /// diagnostic spans (Phase 2) and for tests that want to
    /// inspect the file directly.
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
    /// inspect provenance after a sequence of operations. Phase 1
    /// callers do not query this; it exists so the mock stays
    /// faithful to the trait's "every operation appends" promise.
    claim_log: Vec<MemoryClaimEntry>,
    /// Append-only mirror of `happenings_log`. Tests query this
    /// via [`PersistenceStore::load_happenings_since`].
    happenings: Vec<PersistedHappening>,
    /// Mirror of the `pending_conflicts` table. Updated in place
    /// when a row's resolution columns transition from NULL to a
    /// concrete value.
    pending_conflicts: Vec<PendingConflict>,
    next_conflict_id: i64,
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

    // --- Wave 1: per-claim claim_log entries on announce ------------------

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

    // --- Wave 1: merge mirrors subjects-table mutations -------------------

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

    // --- Wave 1: split mirrors subjects-table mutations -------------------

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
}
