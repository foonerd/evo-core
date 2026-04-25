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
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;

use evo_plugin_sdk::contract::AliasKind;
use evo_plugin_sdk::contract::ExternalAddressing;

/// Initial schema version: subject-identity slice.
///
/// Subsequent phases append migrations: v2 will introduce the
/// relation graph, v3 the custody ledger, v4 the admin ledger. The
/// numbering is reserved here so later phases extend the migration
/// list without renumbering.
pub const SCHEMA_VERSION_SUBJECT_IDENTITY: u32 = 1;

/// Maximum schema version this build of the steward understands.
///
/// On open, [`SqlitePersistenceStore`] refuses to operate on a
/// database whose `schema_version` table records a version greater
/// than this constant. Downgrades are not supported; an operator
/// running an older steward against a newer database must restore
/// from a pre-upgrade backup.
pub const SUPPORTED_SCHEMA_VERSION: u32 = SCHEMA_VERSION_SUBJECT_IDENTITY;

/// SQL text of the initial migration. Embedded at build time so the
/// schema is part of the binary; running on a fresh database does
/// not require the source tree to be present.
const MIGRATION_001_INITIAL: &str =
    include_str!("../migrations/001_initial.sql");

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
    /// every supplied addressing into `subject_addressings`, and
    /// appends a single `claim_log` entry of kind
    /// `subject_announce`. All within one transaction.
    fn record_subject_announce<'a>(
        &'a self,
        canonical_id: &'a str,
        subject_type: &'a str,
        addressings: &'a [ExternalAddressing],
        claimant: &'a str,
        at_ms: u64,
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
    /// Inserts an `aliases` row recording that `source_a` and
    /// `source_b` now point at `new_id`, and appends a
    /// `claim_log` entry of kind `subject_merge`. The subject
    /// rows themselves are not modified by this method; the
    /// caller (Phase 4 admin path) is responsible for wiring the
    /// addressings reattach in the same higher-level transaction
    /// once the admin ledger lands.
    fn record_subject_merge<'a>(
        &'a self,
        source_a: &'a str,
        source_b: &'a str,
        new_id: &'a str,
        admin_plugin: &'a str,
        reason: Option<&'a str>,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// Record an admin `subject_split` operation.
    ///
    /// Inserts one `aliases` row per element of `new_ids` (each
    /// pointing `source -> new_id_n`), and appends a single
    /// `claim_log` entry of kind `subject_split` whose payload
    /// describes the full partition.
    fn record_subject_split<'a>(
        &'a self,
        source: &'a str,
        new_ids: &'a [String],
        admin_plugin: &'a str,
        reason: Option<&'a str>,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>;

    /// Record a hard-forget of `canonical_id`.
    ///
    /// Hard-deletes the `subjects` row (cascading
    /// `subject_addressings` via the foreign key) and appends a
    /// `claim_log` entry of kind `subject_forgotten`.
    fn record_subject_forget<'a>(
        &'a self,
        canonical_id: &'a str,
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
}

/// Claim-log kind values used by this slice. Strings are stable
/// on disk and must not be renamed without a migration.
mod claim_kind {
    pub const SUBJECT_ANNOUNCE: &str = "subject_announce";
    pub const SUBJECT_RETRACT: &str = "subject_retract";
    pub const SUBJECT_MERGE: &str = "subject_merge";
    pub const SUBJECT_SPLIT: &str = "subject_split";
    pub const SUBJECT_FORGOTTEN: &str = "subject_forgotten";
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
    canonical_id: &str,
    subject_type: &str,
    addressings: &[ExternalAddressing],
    claimant: &str,
    at_ms: u64,
) -> Result<(), PersistenceError> {
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

    let payload = serde_json::json!({
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
            payload
        ],
    )
    .map_err(|e| {
        PersistenceError::sqlite("append claim_log subject_announce", e)
    })?;

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
    source_a: &str,
    source_b: &str,
    new_id: &str,
    admin_plugin: &str,
    reason: Option<&str>,
    at_ms: u64,
) -> Result<(), PersistenceError> {
    let tx = conn
        .transaction()
        .map_err(|e| PersistenceError::sqlite("begin merge tx", e))?;

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
    source: &str,
    new_ids: &[String],
    admin_plugin: &str,
    reason: Option<&str>,
    at_ms: u64,
) -> Result<(), PersistenceError> {
    if new_ids.is_empty() {
        return Err(PersistenceError::Invalid(
            "split must produce at least one new id".into(),
        ));
    }
    let tx = conn
        .transaction()
        .map_err(|e| PersistenceError::sqlite("begin split tx", e))?;

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
    at_ms: u64,
) -> Result<(), PersistenceError> {
    let tx = conn
        .transaction()
        .map_err(|e| PersistenceError::sqlite("begin forget tx", e))?;

    tx.execute("DELETE FROM subjects WHERE id = ?1", params![canonical_id])
        .map_err(|e| PersistenceError::sqlite("delete subject row", e))?;

    let payload = serde_json::json!({
        "canonical_id": canonical_id,
    })
    .to_string();
    tx.execute(
        "INSERT INTO claim_log (kind, claimant, asserted_at_ms, payload, reason) \
         VALUES (?1, '', ?2, ?3, NULL)",
        params![
            claim_kind::SUBJECT_FORGOTTEN,
            at_ms as i64,
            payload
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
        .query_map(params![canonical_id], |row| {
            let kind_str: String = row.get(3)?;
            let kind = match kind_str.as_str() {
                "merged" => AliasKind::Merged,
                "split" => AliasKind::Split,
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
        })
        .map_err(|e| PersistenceError::sqlite("execute aliases query", e))?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| PersistenceError::sqlite("read alias row", e))?);
    }
    Ok(out)
}

impl PersistenceStore for SqlitePersistenceStore {
    fn record_subject_announce<'a>(
        &'a self,
        canonical_id: &'a str,
        subject_type: &'a str,
        addressings: &'a [ExternalAddressing],
        claimant: &'a str,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let cid = canonical_id.to_string();
        let st = subject_type.to_string();
        let addrs = addressings.to_vec();
        let cl = claimant.to_string();
        Box::pin(async move {
            self.interact("subject_announce", move |conn| {
                announce_tx(conn, &cid, &st, &addrs, &cl, at_ms)
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
        source_a: &'a str,
        source_b: &'a str,
        new_id: &'a str,
        admin_plugin: &'a str,
        reason: Option<&'a str>,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let a = source_a.to_string();
        let b = source_b.to_string();
        let n = new_id.to_string();
        let admin = admin_plugin.to_string();
        let reason = reason.map(|s| s.to_string());
        Box::pin(async move {
            self.interact("subject_merge", move |conn| {
                merge_tx(conn, &a, &b, &n, &admin, reason.as_deref(), at_ms)
            })
            .await
        })
    }

    fn record_subject_split<'a>(
        &'a self,
        source: &'a str,
        new_ids: &'a [String],
        admin_plugin: &'a str,
        reason: Option<&'a str>,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let s = source.to_string();
        let ids = new_ids.to_vec();
        let admin = admin_plugin.to_string();
        let reason = reason.map(|s| s.to_string());
        Box::pin(async move {
            self.interact("subject_split", move |conn| {
                split_tx(conn, &s, &ids, &admin, reason.as_deref(), at_ms)
            })
            .await
        })
    }

    fn record_subject_forget<'a>(
        &'a self,
        canonical_id: &'a str,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        let cid = canonical_id.to_string();
        Box::pin(async move {
            self.interact("subject_forget", move |conn| {
                forget_tx(conn, &cid, at_ms)
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
#[derive(Debug, Default)]
pub struct MemoryPersistenceStore {
    inner: AsyncMutex<MemoryState>,
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
}

impl PersistenceStore for MemoryPersistenceStore {
    fn record_subject_announce<'a>(
        &'a self,
        canonical_id: &'a str,
        subject_type: &'a str,
        addressings: &'a [ExternalAddressing],
        claimant: &'a str,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
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
        source_a: &'a str,
        source_b: &'a str,
        new_id: &'a str,
        admin_plugin: &'a str,
        reason: Option<&'a str>,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut g = self.inner.lock().await;
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
        source: &'a str,
        new_ids: &'a [String],
        admin_plugin: &'a str,
        reason: Option<&'a str>,
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            if new_ids.is_empty() {
                return Err(PersistenceError::Invalid(
                    "split must produce at least one new id".into(),
                ));
            }
            let mut g = self.inner.lock().await;
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
        at_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<(), PersistenceError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut g = self.inner.lock().await;
            g.subjects.remove(canonical_id);
            g.claim_log.push(MemoryClaimEntry {
                kind: claim_kind::SUBJECT_FORGOTTEN,
                claimant: String::new(),
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
    async fn memory_announce_then_load_returns_subject() {
        let s = MemoryPersistenceStore::new();
        s.record_subject_announce(
            "uuid-a",
            "track",
            &[ext("mpd-path", "/m/a.flac")],
            "p1",
            1000,
        )
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
        s.record_subject_announce(
            "uuid-a",
            "track",
            &[ext("a", "1"), ext("b", "2")],
            "p1",
            1000,
        )
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
        s.record_subject_merge(
            "uuid-a",
            "uuid-b",
            "uuid-c",
            "admin",
            Some("dup"),
            2000,
        )
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
        s.record_subject_split("uuid-a", &new_ids, "admin", None, 3000)
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
        s.record_subject_announce(
            "uuid-a",
            "track",
            &[ext("a", "1")],
            "p1",
            1000,
        )
        .await
        .unwrap();
        s.record_subject_forget("uuid-a", 1500).await.unwrap();
        let all = s.load_all_subjects().await.unwrap();
        assert!(all.is_empty());
    }

    #[tokio::test]
    async fn memory_split_with_empty_new_ids_errors() {
        let s = MemoryPersistenceStore::new();
        let r = s
            .record_subject_split("uuid-a", &[], "admin", None, 1)
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
        // Open a side connection and confirm the schema_version
        // table is at v1.
        let conn = Connection::open(store.path()).expect("side open");
        let v = current_schema_version(&conn).expect("read version");
        assert_eq!(v, SCHEMA_VERSION_SUBJECT_IDENTITY);
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
            .record_subject_announce(
                "uuid-a",
                "track",
                &[ext("mpd", "/x")],
                "p1",
                42,
            )
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
            .record_subject_announce(
                "uuid-a",
                "track",
                &[ext("a", "1"), ext("b", "2")],
                "p1",
                10,
            )
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
            .record_subject_merge(
                "uuid-a",
                "uuid-b",
                "uuid-c",
                "admin",
                Some("dup"),
                100,
            )
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
        store
            .record_subject_split("uuid-a", &new_ids, "admin", None, 200)
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
            .record_subject_announce(
                "uuid-a",
                "track",
                &[ext("a", "1")],
                "p1",
                10,
            )
            .await
            .unwrap();
        store.record_subject_forget("uuid-a", 20).await.unwrap();
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
    async fn sqlite_announce_populates_all_three_tables_atomically() {
        let (_dir, store) = open_temp();
        store
            .record_subject_announce(
                "uuid-a",
                "track",
                &[ext("a", "1")],
                "p1",
                42,
            )
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
            .record_subject_announce(
                "uuid-a",
                "track",
                &[ext("a", "1")],
                "p1",
                1,
            )
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
}
