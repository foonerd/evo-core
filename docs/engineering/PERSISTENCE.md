# Persistence

Status: engineering-layer contract for the steward's durable state.
Audience: steward maintainers, distribution packagers, plugin authors whose assertions cross restarts, reviewers of persistence work.
Vocabulary: per `docs/CONCEPT.md`. Cross-references: `STEWARD.md`, `SUBJECTS.md`, `RELATIONS.md`, `CUSTODY.md`, `CATALOGUE.md`, `BOUNDARY.md`, `PLUGIN_PACKAGING.md`, `CONFIG.md`, `LOGGING.md`.

This document describes how the steward remembers the fabric across restarts: which stores are durable, what file format holds them, what happens when a crash interrupts a write, how schema changes over time, how the database surfaces in the Rust API, and how persistence interacts with the catalogue, the trust model, and the correction primitives. It is the design contract the implementation is held against.

Every decision in this document is deliberate. Where a decision was genuinely contested between viable options, the alternatives and the reasoning appear alongside the choice. A future reader asking "why did we do it this way" should find the answer here rather than in a commit message or a forgotten discussion.

## 1. Purpose

`STEWARD.md` section 8 names five long-lived stores: the subject registry, the relation graph, the custody ledger, the happenings bus, and the projection engine. `STEWARD.md` section 12.3 states that none of these are persisted today and that a restart yields an empty fabric. This document closes that gap for the three authoritative stores (subjects, relations, custody) and specifies which parts of the remaining two (happenings, projections) are in scope for persistence and which are deliberately transient.

A device that claims to "present coherent information about what it is doing to any consumer that looks" (per the essence statement in `CONCEPT.md`) cannot forget everything it knew the moment it reboots. The user who asserted a subject, the plugin that claimed a relation, the warden that was holding custody of playback when the power blinked: all of these must be reconstructable after restart, or the device's coherence is a lie.

Persistence is therefore not an optimisation. It is a correctness property.

## 2. Scope

### 2.1 In scope under this document

- Durable storage of the subject registry, the relation graph, and the custody ledger.
- Schema versioning with a forward-compatible migration discipline.
- Crash recovery and corruption quarantine.
- A typed Rust API surface replacing the current `Arc<Mutex<HashMap<...>>>` internals.
- Performance baselines and regression gates in CI.
- Integration with catalogue validation.
- Integration with correction primitives.
- Re-verification of persisted claims against the trust model on startup.

### 2.2 Out of scope

- **Happenings history.** The happenings bus is transient by design (`HAPPENINGS.md` section 11.4, `STEWARD.md` section 12.2). A bounded durable history is a separate concern, not part of this design.
- **Projection caching.** Projections are composed on demand from the authoritative stores. Caching composed projections is a performance concern, not a durability one. If a projection cache is ever added, it lives alongside this store, not inside it.
- **Plugin-owned state.** Plugins continue to own their own `state/` and `credentials/` directories under `/var/lib/evo/plugins/<name>/`. The steward does not write to plugin state; the contract is unchanged by persistence work (see section 5.2).
- **Audit export.** The audit trail is durable in the steward's claim log (section 7). Exporting it to an external system (syslog, SIEM) is a distribution concern, not a framework one.
- **Distributed persistence.** The steward is a single-process, single-host component. Cross-host replication, mirroring, and quorum are explicitly not in scope. Devices synchronise through their own means; the steward persists the local truth.

### 2.3 Related touchpoints

This design integrates with several other engineering surfaces. The integration points are documented in the sections noted:

| Touchpoint | Section |
|------------|---------|
| Persistence (subjects, relations, custody ledger durable across restart) | This document, primary topic |
| Predicate existence validation at assertion | Section 13.1 |
| Subject type validation against catalogue | Section 13.2 |
| Inverse predicate consistency | Section 13.3 |
| Relation cardinality enforcement | Section 13.4 |
| Dangling relation GC on subject forget | Section 13.5 |
| Framework correction primitives for administration plugins | Section 14 |

## 3. Storage Technology

### 3.1 Choice: SQLite via `rusqlite`

The steward persists state in a single SQLite database file, accessed through the `rusqlite` crate, with the Write-Ahead Log journaling mode enabled.

Rationale:

- **ACID by construction.** SQLite provides atomicity, consistency, isolation, and durability without the steward implementing any of those properties itself. The "industrial grade" standard the project holds itself to is achievable only by standing on a proven foundation; reinventing transaction semantics for an audio device is off-mission.
- **WAL enables the concurrency model the steward needs.** Multiple readers run concurrently while a writer commits. Projection composition (read-heavy) does not block subject announcement (writes). No reader/writer locks at the application layer.
- **Single-file on-disk format** simplifies backup, operator handling, and corruption diagnosis. Everything the steward's state comprises lives in one file plus its WAL sidecars.
- **Schema migrations are a solved problem** in SQLite. Standard patterns exist and have been exercised across a long list of production deployments.
- **Query expressiveness.** Secondary indexes, joins, and aggregations let the API stay thin. The Rust code describes intent; SQLite executes it. Hand-rolled indexes over a KV store would be perpetual maintenance.
- **Universal platform support.** `rusqlite`'s `bundled` feature compiles SQLite from vendored C source on every target in the MSRV matrix (`MSRV.md`, `BUILDING.md` section 3). No target-specific SQLite installation or OS package is required. The bundled source is the same SQLite version across every target, guaranteeing identical behaviour regardless of what the host OS happens to ship.
- **Diagnostic depth.** SQLite ships `PRAGMA integrity_check`, `PRAGMA wal_checkpoint`, and the `sqlite3` CLI. Corruption investigations and ad hoc state inspection by maintainers are immediate.

### 3.2 Alternatives considered and rejected

| Option | Why rejected |
|--------|--------------|
| `sled` | Unmaintained since 2022. Known crash-safety caveats. Not a foundation for industrial-grade work. |
| `fjall` | Young (under 2 years at time of writing). Ecosystem thin. May be appropriate for a later revisit but not v1. |
| `redb` | Pure-Rust, maintained, technically sound. Rejected on grounds of query expressiveness: no SQL, no joins, secondary indexes are hand-rolled. The engineering burden of building what SQLite gives for free is larger than the benefit of the pure-Rust dependency tree. Remains the defensible second choice if the C-dependency argument ever becomes load-bearing. |
| Append-only log + periodic snapshot | Would require the project to implement its own transaction semantics, compaction, integrity checks, and index rebuilds. Each of those is a subsystem with its own correctness problems. Off-mission. |
| Flat TOML / JSON files | Not atomic. No crash safety. Acceptable for configuration (where the file is tiny and human-authored) and state reports (where each file is independent). Unacceptable for a store with thousands of subjects and a referential integrity invariant. |
| PostgreSQL or another server database | Out of process boundary, out of scope for an embedded device, introduces an operational dependency evo-core is not willing to require of a distribution. |

### 3.3 Dependency footprint

Adding SQLite introduces one workspace dependency tree rooted at `rusqlite`. The crate bundles SQLite itself via the `bundled` feature, avoiding a runtime link against whatever SQLite the target OS happens to ship. This has trade-offs:

- **Bundled SQLite (chosen).** The steward pins an exact SQLite version. Every distribution's evo steward behaves identically regardless of OS-level SQLite version. No surprises from Debian backporting a security fix that changes behaviour. Build time longer by a few seconds per crate-graph-change; binary size larger by roughly 1.5 MB.
- System SQLite (`rusqlite` without `bundled`). Smaller binary; depends on OS-level `libsqlite3` which differs across targets. Rejected: industrial-grade demands reproducibility.

Recommended `rusqlite` feature set:

```toml
rusqlite = { version = "0.31", features = ["bundled", "blob", "serde_json"] }
```

(Exact minor version chosen at implementation time; MSRV compatibility verified against the full target matrix before pinning.)

Async wrapper: the `rusqlite` API is synchronous. The steward is asynchronous. The boundary between them is covered in section 10.4.

## 4. Durability Contract

### 4.1 Write semantics

The steward's durability promise to plugins and consumers: **when an announcer callback or admin operation returns `Ok`, the asserted fact is durable on disk**. A power failure the next instant does not roll the fact back.

This means every successful `announce_subject`, `assert_relation`, `retract_subject`, `retract_relation`, `record_custody`, `release_custody`, `admin_retract`, `admin_merge`, `admin_split`, and `admin_suppress` completes its SQLite transaction with an `fsync` to durable storage before returning.

SQLite surfaces this via the `synchronous` pragma. The steward sets `synchronous = FULL`.

This is the strictest non-paranoid setting. It trades some throughput (fsync per commit) for the guarantee that a transaction acknowledged to the caller will survive immediate power loss. For an audio device that has no battery-backed RAM, this trade is correct: the caller's expectation is "done means done".

`synchronous = NORMAL` was considered and rejected. NORMAL loses at most the last transaction on hard power loss. Acceptable for many applications; not acceptable for an industrial-grade fabric that uses announcement return values as synchronisation primitives.

### 4.2 Per-PRAGMA settings

The steward opens every SQLite connection with the following pragmas applied before any query runs:

| Pragma | Value | Rationale |
|--------|-------|-----------|
| `journal_mode` | `WAL` | Multi-reader / single-writer concurrency. Required for the async boundary in section 10.4. |
| `synchronous` | `FULL` | Full durability on commit. See section 4.1. |
| `foreign_keys` | `ON` | Referential integrity enforced by SQLite, not by application code. |
| `busy_timeout` | `5000` (ms) | A writer waits up to 5 seconds for a conflicting lock to clear before failing with `SQLITE_BUSY`. Tunable via config if operators report contention. |
| `mmap_size` | `268435456` (256 MB) | Memory-mapped I/O for read-heavy workloads. Large enough to cover the realistic working set on any target; small enough that 32-bit targets do not exhaust address space. |
| `cache_size` | `-20000` (20 MB) | Per-connection page cache. Negative value denotes KB; SQLite documents the sign convention. |
| `wal_autocheckpoint` | `1000` (pages) | Default. Pages flush from the WAL to the main database every 1000 pages of WAL growth. |
| `temp_store` | `MEMORY` | Temporary tables (used during `PRAGMA integrity_check` and some query plans) stay in RAM. |

These pragmas are set by the connection initializer in the connection pool; every connection handed out is already in this state. There is no code path that operates on a freshly-opened connection without these pragmas applied.

### 4.3 Transaction boundaries

Every mutation is a single SQLite transaction. There are no implicit auto-commit code paths. The Rust API makes this explicit: mutations return transaction guards that commit on success or roll back on drop (section 10.3).

A single logical fabric operation may touch multiple tables. A subject announcement that includes addressings touches `subjects`, `subject_addressings`, and `claim_log` in one transaction. The cross-table atomicity promise (a crash leaves neither a subject without its addressings nor orphan addressings referring to a non-existent subject) is the whole reason SQLite was chosen over separate KV stores. A multi-store design would require either a two-phase commit protocol the steward implements itself or an outer write-ahead log wrapping every store, both of which are subsystems with their own correctness problems and their own failure modes. SQLite gives the promise in one transaction with no added code.

## 5. Persistence Boundary

### 5.1 Steward-owned authoritative stores

The steward persists:

1. **Subject registry.** Canonical IDs, subject types, addressings, claimants, timestamps, forgotten-at markers for soft-deleted subjects awaiting GC.
2. **Relation graph.** Typed directed edges, multi-claimant sets, suppression markers.
3. **Custody ledger.** Active custodies keyed by `(plugin, handle_id)`, their shelf and custody type, and the most recent state snapshot per custody.
4. **Claim log.** Append-only audit trail covering every announcement, assertion, retraction, merge, split, suppression, and admin retraction. The forensic record.
5. **Schema version metadata.** The current schema version and the history of migrations applied.

These five surfaces occupy one database file. The subject registry, relation graph, and custody ledger are the live working set. The claim log and schema version metadata are append-mostly.

### 5.2 Plugin-owned state (unchanged)

Plugins continue to own their own durable state under `/var/lib/evo/plugins/<name>/state/` and their own credentials under `/var/lib/evo/plugins/<name>/credentials/`. The steward does not read, write, or inventory these. Their contract is part of `PLUGIN_PACKAGING.md` section 3, not this document.

A plugin may choose to use SQLite internally for its own state. That is the plugin's choice; the steward's database and the plugin's database are separate files with no shared transactional context. Plugins that want transactional guarantees across their own state and a fabric announcement must commit their own state first and then announce; if the announcement fails, the plugin's own retry logic handles reconciliation. The steward does not expose distributed transactions across plugin boundaries and never will.

### 5.3 Deliberately not persisted

- **Happenings.** Bounded durable trail. The bus keeps a configurable in-memory ring (`[happenings] retention_capacity`, default 1024) and the steward also writes every happening to the SQLite `happenings_log` table; subscribers may pass a `since` cursor to `subscribe_happenings` and the steward will replay through the ring (and through `PersistenceStore::load_happenings_since` once the ring is exhausted) before resuming the live stream. A subscriber that misses the retention window receives a structured `replay_window_exceeded` response carrying the oldest available `seq`. A periodic janitor (`[happenings] janitor_interval_secs`, default 60) calls `PersistenceStore::trim_happenings_log` to evict rows that have aged past `retention_window_secs` OR fallen out of the `retention_capacity` tail; read-side enforcement (`replay_window_exceeded`) backstops the write-side trim so a consumer with a stale cursor never observes a torn window.
- **Projections.** Recomputed on demand. A cached projection is by definition a view over authoritative stores; caching is a performance concern handled separately if ever needed.
- **Fast-path state.** Fast path (`FAST_PATH.md`) is by definition a low-latency mutation channel; durability is handled by the slow path once the fast-path mutation is committed to the fabric.
- **Connection pool state.** Obvious but explicit.
- **In-flight requests and their correlation IDs.** The steward is not a message broker.
- **Plugin discovery results.** Rediscovered on every startup; cheap enough that caching adds no value.

## 6. On-Disk Layout

### 6.1 Files under `/var/lib/evo/state/`

```
/var/lib/evo/state/
  evo.db              # Main database file. SQLite.
  evo.db-wal          # Write-Ahead Log. Present while the steward is running.
  evo.db-shm          # Shared memory file for WAL coordination. Present while running.
```

On clean shutdown the steward issues `PRAGMA wal_checkpoint(TRUNCATE)` to flush and empty the WAL; the `-wal` and `-shm` files may be absent after a clean shutdown. On crash they persist and are replayed on next startup. An operator who backs up `/var/lib/evo/state/` must copy all three files together (SQLite's backup API, used via `rusqlite`'s `Connection::backup`, is the recommended path; straight `cp` of the three files during runtime risks an inconsistent snapshot).

### 6.2 Permissions and ownership

The framework requires the following properties on the state directory and the files within it. Ownership is expressed in terms of the steward's effective UID and GID at runtime, not any specific user name:

| Path | Owner | Group | Mode |
|------|-------|-------|------|
| `/var/lib/evo/state/` | steward's effective UID | steward's effective GID | `0700` |
| `/var/lib/evo/state/evo.db` | steward's effective UID | steward's effective GID | `0600` |
| `/var/lib/evo/state/evo.db-wal` | steward's effective UID | steward's effective GID | `0600` |
| `/var/lib/evo/state/evo.db-shm` | steward's effective UID | steward's effective GID | `0600` |

The steward creates new files with these modes and verifies them on every open. It refuses to start if the state directory exists with mode more permissive than `0700` or with ownership not matching its effective UID. The error is `StewardError::InsecureStateDirectory`, carrying the offending path and the observed mode and uid. This is a structural defence, not a threat-model mitigation: the steward signals "my state is not in a sane environment; refusing to compound the problem by opening the database".

Which concrete user the steward runs as is a distribution concern, not a framework concern. The framework observes only the effective UID at runtime and the modes on disk; it has no opinion on the user name or how the user came to exist. The distribution's packaging creates the install-time service user, configures the system service unit (systemd, OpenRC, launchd, or whatever the target platform uses) to run the steward as that user, and owns the full choice: a dedicated account named `evo`, a product-named account like `volumio` or `audiokit`, systemd's `DynamicUser=`, a shared service user that predates the evo install, or - in development and certain embedded scenarios - `root`. `PLUGIN_PACKAGING.md` section 5 captures the distribution-side framing ("typical appliances run the whole evo service as a single install-time user"); the boundary split is per `BOUNDARY.md` section 6 ("Packaging" is a distribution concern).

### 6.3 Cross-references in other docs

Other engineering docs that mention where steward state lives on disk all point at the single `/var/lib/evo/state/evo.db` path declared above:

| Doc | Section | Statement |
|-----|---------|-----------|
| `PLUGIN_PACKAGING.md` | 3 | `/var/lib/evo/state/evo.db` as single SQLite database; cross-reference to `PERSISTENCE.md`. |
| `SUBJECTS.md` | 13.2 | The subject registry is persisted in `/var/lib/evo/state/evo.db`; full contract in `PERSISTENCE.md`. |
| `RELATIONS.md` | 11.2 | The relation graph will be persisted in `/var/lib/evo/state/evo.db` alongside the subject registry once the relation-graph durability slice ships; full contract in `PERSISTENCE.md`. |
| `CUSTODY.md` | 11.1 | Custody ledger destination is `/var/lib/evo/state/evo.db`; the durability slice has not yet shipped (see `PERSISTENCE.md` section 20). |
| `BOUNDARY.md` | 9 | `/var/lib/evo/state/` holds the steward's SQLite database; see `PERSISTENCE.md`. |

The revised on-disk tree:

```
/var/lib/evo/
  state/
    evo.db                             # SQLite database: subjects, relations, custody ledger, claim log, admin log.
    evo.db-wal                         # Present while the steward is running.
    evo.db-shm                         # Present while the steward is running.
```

## 7. Schema

### 7.1 Tables

The initial schema (migration version 1) declares ten tables. All tables use SQLite `STRICT` mode to enforce column types and refuse non-conforming values at insert time. Tables whose primary key is a compound of small-typed columns use `WITHOUT ROWID` for reduced storage overhead.

```sql
-- Schema version metadata. Append-only.
CREATE TABLE schema_version (
    version INTEGER PRIMARY KEY,
    applied_at_ms INTEGER NOT NULL,
    description TEXT NOT NULL
) STRICT;

-- Subjects: canonical identity + type + lifecycle timestamps.
CREATE TABLE subjects (
    id TEXT PRIMARY KEY,               -- canonical UUID, textual
    subject_type TEXT NOT NULL,        -- declared catalogue subject type
    created_at_ms INTEGER NOT NULL,
    modified_at_ms INTEGER NOT NULL,
    forgotten_at_ms INTEGER            -- NULL while live, timestamp once soft-forgotten
) STRICT;

-- Subject addressings: (scheme, value) identifies a subject externally.
CREATE TABLE subject_addressings (
    scheme TEXT NOT NULL,
    value TEXT NOT NULL,
    subject_id TEXT NOT NULL REFERENCES subjects(id) ON DELETE CASCADE,
    claimant TEXT NOT NULL,
    asserted_at_ms INTEGER NOT NULL,
    reason TEXT,
    quarantined_by TEXT,               -- reason tag if the claimant's trust was revoked, or NULL
    quarantined_at_ms INTEGER,         -- timestamp of quarantine, or NULL
    PRIMARY KEY (scheme, value)
) STRICT, WITHOUT ROWID;

-- Relations: typed directed edges between subjects.
CREATE TABLE relations (
    source_id TEXT NOT NULL REFERENCES subjects(id) ON DELETE CASCADE,
    predicate TEXT NOT NULL,
    target_id TEXT NOT NULL REFERENCES subjects(id) ON DELETE CASCADE,
    PRIMARY KEY (source_id, predicate, target_id)
) STRICT, WITHOUT ROWID;

-- Relation claimants: multi-claimant set per relation with provenance.
CREATE TABLE relation_claimants (
    source_id TEXT NOT NULL,
    predicate TEXT NOT NULL,
    target_id TEXT NOT NULL,
    claimant TEXT NOT NULL,
    asserted_at_ms INTEGER NOT NULL,
    reason TEXT,
    suppressed_by TEXT,                -- admin plugin that suppressed this claim, or NULL
    suppressed_at_ms INTEGER,          -- timestamp of suppression, or NULL
    quarantined_by TEXT,               -- reason tag if the claimant's trust was revoked, or NULL
    quarantined_at_ms INTEGER,         -- timestamp of quarantine, or NULL
    PRIMARY KEY (source_id, predicate, target_id, claimant),
    FOREIGN KEY (source_id, predicate, target_id)
        REFERENCES relations(source_id, predicate, target_id) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

-- Custody ledger: active custodies keyed by (plugin, handle_id).
CREATE TABLE custodies (
    plugin TEXT NOT NULL,
    handle_id TEXT NOT NULL,
    shelf TEXT,
    custody_type TEXT,
    started_at_ms INTEGER NOT NULL,
    last_updated_ms INTEGER NOT NULL,
    PRIMARY KEY (plugin, handle_id)
) STRICT, WITHOUT ROWID;

-- Custody state: most recent state snapshot per custody. Not a history log.
CREATE TABLE custody_state (
    plugin TEXT NOT NULL,
    handle_id TEXT NOT NULL,
    payload BLOB NOT NULL,
    health TEXT NOT NULL,              -- 'ok' | 'degraded' | 'failing'
    reported_at_ms INTEGER NOT NULL,
    PRIMARY KEY (plugin, handle_id),
    FOREIGN KEY (plugin, handle_id)
        REFERENCES custodies(plugin, handle_id) ON DELETE CASCADE
) STRICT, WITHOUT ROWID;

-- Append-only claim log. Forensic record.
CREATE TABLE claim_log (
    log_id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,                -- see kinds enumerated below
    claimant TEXT NOT NULL,
    asserted_at_ms INTEGER NOT NULL,
    payload TEXT NOT NULL,             -- JSON, variant-specific
    reason TEXT
) STRICT;

-- Admin operations log. Separate from claim_log to make audit queries cheap.
CREATE TABLE admin_log (
    admin_id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,                -- see enumeration below
    admin_plugin TEXT NOT NULL,
    target_claimant TEXT,              -- for retractions / quarantines: whose claim was affected
    payload TEXT NOT NULL,             -- JSON, variant-specific
    asserted_at_ms INTEGER NOT NULL,
    reason TEXT,
    reverses_admin_id INTEGER REFERENCES admin_log(admin_id)  -- for unsuppress, un-merge, un-split
) STRICT;

-- Trust revocation snapshot. Startup re-verification records which plugins / keys
-- were active when the fabric was last persisted.
CREATE TABLE plugin_trust_snapshot (
    plugin TEXT PRIMARY KEY,
    signing_key_fingerprint TEXT NOT NULL,
    effective_trust_class TEXT NOT NULL,
    last_seen_at_ms INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
```

Enumerated `kind` values for `claim_log`:

| Kind | Meaning |
|------|---------|
| `subject_announce` | A plugin announced a subject. |
| `subject_retract` | A plugin retracted its own subject claim. |
| `subject_forgotten` | A subject's last claimant retracted; the subject is hard-deleted. See section 13.5. |
| `relation_assert` | A plugin asserted a relation. |
| `relation_retract` | A plugin retracted its own relation claim. |
| `relation_gc_cascade` | A relation was deleted because one of its endpoints was forgotten. See section 13.5. |
| `custody_recorded` | A warden took custody. |
| `custody_released` | A warden released custody. |

Enumerated `kind` values for `admin_log`:

| Kind | Meaning |
|------|---------|
| `retract` | An admin plugin retracted another plugin's claim. |
| `merge` | An admin plugin merged two subjects into one. |
| `split` | An admin plugin split one subject into two. |
| `suppress` | An admin plugin marked a relation as suppressed. |
| `unsuppress` | An admin plugin cleared a relation's suppression. |
| `trust_quarantine` | A plugin's claims were quarantined on startup because its signing key was revoked or its trust class no longer resolves. See section 12.1. |

Admin operations land in `admin_log` instead of `claim_log` so that ordinary provenance queries ("who claimed this") don't need to filter out administrative actions. Section 14 goes into detail.

The `trust_quarantine` kind sits in `admin_log` by deliberate design: the steward acts as its own administrator when it detects on startup that a plugin's trust has lapsed. Treating the quarantine as an admin operation means the audit trail, reversibility machinery, and happening surface are the same as for any other admin action, with no new subsystem.

### 7.2 Indexes

Beyond primary-key indexes:

```sql
CREATE INDEX idx_subjects_type ON subjects(subject_type);
CREATE INDEX idx_subjects_live ON subjects(forgotten_at_ms)
    WHERE forgotten_at_ms IS NULL;

CREATE INDEX idx_addressings_subject ON subject_addressings(subject_id);
CREATE INDEX idx_addressings_claimant ON subject_addressings(claimant);

CREATE INDEX idx_relations_forward ON relations(source_id, predicate);
CREATE INDEX idx_relations_inverse ON relations(target_id, predicate);

CREATE INDEX idx_relation_claimants_claimant ON relation_claimants(claimant);

CREATE INDEX idx_custodies_plugin ON custodies(plugin);

CREATE INDEX idx_claim_log_at ON claim_log(asserted_at_ms);
CREATE INDEX idx_claim_log_claimant ON claim_log(claimant);
CREATE INDEX idx_claim_log_kind ON claim_log(kind);

CREATE INDEX idx_admin_log_at ON admin_log(asserted_at_ms);
CREATE INDEX idx_admin_log_admin ON admin_log(admin_plugin);
CREATE INDEX idx_admin_log_target ON admin_log(target_claimant)
    WHERE target_claimant IS NOT NULL;
```

The partial index on `subjects.forgotten_at_ms IS NULL` supports the common query "walk live subjects only"; most API paths filter to live subjects and the partial index is cheaper than a full-column index.

### 7.3 Foreign keys

`PRAGMA foreign_keys = ON` is set on every connection. Foreign key violations fail the transaction rather than silently succeeding. The cascade rules declared above:

- `subject_addressings.subject_id -> subjects.id ON DELETE CASCADE`: deleting a subject removes its addressings.
- `relations.source_id -> subjects.id ON DELETE CASCADE`: deleting a subject removes relations it sources.
- `relations.target_id -> subjects.id ON DELETE CASCADE`: deleting a subject removes relations it targets.
- `relation_claimants(source_id, predicate, target_id) -> relations(...) ON DELETE CASCADE`: removing a relation removes its claimants.
- `custody_state(plugin, handle_id) -> custodies(plugin, handle_id) ON DELETE CASCADE`: removing a custody removes its state snapshot.

`claim_log` and `admin_log` have no foreign keys. They are append-only historical records; nothing references them, and they reference nothing that can cascade.

### 7.4 Initial migration

The schema above ships as `crates/evo/migrations/001_initial.sql`. This file is the source of truth for the v1 schema. The Rust code reads it at build time (via `include_str!`) or at runtime (from disk on development builds); production builds include it in the binary. The migration runner (section 8.3) applies it on first startup against a fresh database.

## 8. Schema Versioning and Migrations

### 8.1 Migration discipline

Every schema change is a new migration file. Migrations are numbered sequentially: `001_initial.sql`, `002_add_subject_index.sql`, `003_extend_admin_log.sql`, and so on. Migrations are never renumbered, never deleted, and never modified after merge to `main`. The only edit a merged migration ever sees is a clarifying comment, and even that is discouraged.

A schema change that is architecturally a revert of a prior migration is expressed as a new migration that undoes the effect. Migrations are always forward-applied; the history is linear.

### 8.2 Migration file layout

Directory: `crates/evo/migrations/`.

Each file:

```sql
-- migrations/NNN_short_name.sql
--
-- One-line description of the change.
--
-- Rationale, if non-obvious, in a comment block.
--
-- Introduced in evo-core v0.X.Y.

BEGIN TRANSACTION;

-- DDL statements here.

INSERT INTO schema_version (version, applied_at_ms, description)
    VALUES (NNN, unixepoch('now', 'subsec') * 1000, 'short description');

COMMIT;
```

The migration runner executes each file as a single transaction. A migration that fails leaves the database at the prior version; the steward refuses to start and names the failing migration.

### 8.3 Migration runner

On startup, after opening the database and applying pragmas, the steward:

1. Reads `MAX(version)` from `schema_version`. On a fresh database this returns NULL; the steward treats it as 0.
2. Enumerates migration files bundled with the steward binary.
3. Applies each migration whose version is greater than the current version, in order.
4. If any migration fails, rolls back its transaction, logs the error, refuses to start with exit code non-zero.

A steward running schema version N will apply migrations N+1, N+2, ... up to its maximum bundled version. A steward running an older version than the database has applied (for example, a downgrade) refuses to start with `StewardError::SchemaVersionAhead`, naming the database version and the binary's maximum version.

### 8.4 Backward compatibility promise

- **Minor version bumps** (for example, `0.1.9` to `0.1.10`) may introduce new migrations that add tables, columns, or indexes. Reading an older database with a newer steward always succeeds (migrations run).
- **Reading a newer database with an older steward fails hard.** The steward refuses to start rather than operate on a schema it does not understand. This is the industrial-grade stance: silent best-effort operation on an unknown schema is how fabrics get corrupted.
- **Major version bumps** (for example, `0.x` to `1.0`) may introduce migrations that drop or rename data. The migration itself handles the conversion; no manual operator step is required except on explicitly flagged destructive migrations (none on the roadmap).
- **Downgrades are not supported.** An operator who needs to run an older steward against a newer database must restore from a backup taken before the upgrade. `CONFIG.md` and the release notes carry this statement explicitly so operators see it before they hit it.

### 8.5 Fixture-based migration testing

Every migration ships with a corresponding fixture test. The fixtures live under `crates/evo/tests/fixtures/persistence/` and consist of pre-built SQLite database files at specific schema versions, paired with their expected content after applying the subsequent migration.

For migration `002_add_subject_index.sql`, the fixtures are:

```
crates/evo/tests/fixtures/persistence/
  v001_after_initial.db        # Database at schema version 1 with sample data.
  v002_expected.db             # Same data after migration 002 is applied.
```

The test `tests/persistence/migrations.rs` opens the v001 fixture, runs migrations up to v002, and asserts the result equals v002_expected (via `sqlite3 .dump` equivalence, ignoring timestamps). Every migration adds a new fixture pair and the test matrix grows.

Fixtures are committed to the repository as small `.db` files. They are binary artefacts; a future refactor may generate them at test time from `.sql` seed scripts to avoid storing binaries. Either way, the test contract is the same: "migration N applied to database at version N-1 yields the expected version-N result".

## 9. What Persists: Inventory

Exhaustive enumeration. Each row specifies whether the item is durable, transient, or plugin-owned.

| Item | Durable? | Where |
|------|----------|-------|
| Subject canonical ID | Yes | `subjects.id` |
| Subject declared type | Yes | `subjects.subject_type` |
| Subject created-at timestamp | Yes | `subjects.created_at_ms` |
| Subject modified-at timestamp | Yes | `subjects.modified_at_ms` |
| Subject forgotten-at marker | Yes | `subjects.forgotten_at_ms` |
| Subject addressings with provenance | Yes | `subject_addressings` |
| Subject claim history | Yes | `claim_log` (kinds `subject_*`) |
| Relation quad `(source, predicate, target)` | Yes | `relations` |
| Relation claimants and provenance | Yes | `relation_claimants` |
| Relation suppression marker | Yes | `relation_claimants.suppressed_by` |
| Relation claim history | Yes | `claim_log` (kinds `relation_*`) |
| Custody key `(plugin, handle_id)` | Yes | `custodies` |
| Custody shelf, custody type | Yes | `custodies` |
| Custody started-at, last-updated | Yes | `custodies` |
| Custody most-recent state snapshot | Yes | `custody_state` |
| Custody state history | No | Not kept. The last snapshot overwrites the previous. A future gap may add bounded history if there is demand. |
| Admin retraction history | Yes | `admin_log` |
| Admin merge history | Yes | `admin_log` |
| Admin split history | Yes | `admin_log` |
| Admin suppression history | Yes | `admin_log` |
| Trust snapshot per plugin | Yes | `plugin_trust_snapshot` |
| Schema version + migration history | Yes | `schema_version` |
| Happenings emitted on the bus | No | Transient. Section 5.3. |
| Projection cache | No | Not maintained. Section 5.3. |
| Fast-path state | No | Section 5.3. |
| Plugin-owned state | Plugin | `/var/lib/evo/plugins/<name>/state/` |
| Plugin-owned credentials | Plugin | `/var/lib/evo/plugins/<name>/credentials/` |
| Plugin discovery results | No | Rediscovered on every startup. |
| In-flight request correlation IDs | No | Per-connection, in memory. |
| Admission lock state | No | In-memory `Arc<Mutex<AdmissionEngine>>`. |

## 10. API Shape

### 10.1 Typed IDs

The current code uses `String` for subject IDs, plugin names, claimants, and predicates. All of them are semantically distinct; the compiler should catch confusions. Persistence is the moment to switch: once data hits the database, typed columns are cheap and the Rust side stops relying on developer diligence.

New newtype wrappers in `evo-plugin-sdk::contract` (so plugins see them too):

```rust
/// A canonical subject identifier. Stable across the lifetime of the subject.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SubjectId(uuid::Uuid);

impl SubjectId {
    pub fn new_v4() -> Self { Self(uuid::Uuid::new_v4()) }
    pub fn as_uuid(&self) -> &uuid::Uuid { &self.0 }
    pub fn parse_str(s: &str) -> Result<Self, uuid::Error> { /* ... */ }
}

/// A plugin canonical name, as declared in its manifest.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct PluginName(String);

/// A claimant identity, used for provenance. In practice equal to a PluginName
/// today, but kept distinct for future cases where a claimant may be a plugin
/// subsystem rather than the plugin itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Claimant(String);

/// A relation predicate name, as declared in the catalogue's relation grammar.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Predicate(String);

/// A subject type name, as declared in the catalogue.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SubjectType(String);
```

SQLite columns store these as `TEXT`. The `rusqlite::ToSql` and `FromSql` implementations are derived or implemented by hand; the persistence layer never sees a raw `String` masquerading as an ID.

This is a public API break. Plugins currently passing `String` to announcers must switch. The break lands as part of this work; `CHANGELOG.md` and the release notes for v0.1.9 call it out explicitly.

### 10.2 Store trait

The persistence layer exposes one trait per conceptual store, each async:

```rust
pub trait SubjectStore: Send + Sync {
    async fn announce(&self, req: SubjectAnnouncement) -> Result<AnnounceOutcome, StewardError>;
    async fn retract(&self, subject: SubjectId, claimant: Claimant) -> Result<(), StewardError>;
    async fn resolve_by_addressing(&self, addr: &ExternalAddressing) -> Result<Option<SubjectId>, StewardError>;
    async fn get(&self, subject: SubjectId) -> Result<Option<SubjectRecord>, StewardError>;
    async fn list_by_type(&self, ty: &SubjectType) -> Result<Vec<SubjectRecord>, StewardError>;
    // ...
}

pub trait RelationStore: Send + Sync { /* ... */ }
pub trait CustodyStore: Send + Sync { /* ... */ }
```

The existing `SubjectRegistry`, `RelationGraph`, and `CustodyLedger` types become implementations of these traits. The in-memory implementations remain for tests (`InMemorySubjectStore`, etc.); the SQLite implementation (`SqliteSubjectStore`, etc.) is the production default.

This split lets the steward be tested without spinning up SQLite, while production uses the persistent path. The wire adapter code and admission engine see only the trait; they do not know whether they are talking to in-memory or SQLite.

### 10.3 Transactions

Multi-step operations use an explicit transaction guard:

```rust
pub struct Transaction<'a> { /* ... */ }

impl<'a> Transaction<'a> {
    pub async fn announce_subject(&mut self, req: SubjectAnnouncement) -> Result<AnnounceOutcome, StewardError>;
    pub async fn assert_relation(&mut self, rel: RelationAssertion) -> Result<(), StewardError>;
    pub async fn commit(self) -> Result<(), StewardError>;
}

impl<'a> Drop for Transaction<'a> {
    fn drop(&mut self) {
        // Rollback if not committed. Logged at warn level if the transaction
        // was non-empty, since that indicates a caller dropped work on the floor.
    }
}
```

Callers get a transaction from the store, do their work, and commit or drop. The compiler enforces commit-or-rollback semantics via move out of `commit` consuming the guard.

Ergonomic helpers for the common single-operation case that does not need a transaction guard:

```rust
// Convenience: announce-and-commit in one call.
store.announce_subject(req).await?;
```

The helper opens a transaction, performs the work, and commits. Under the hood every mutation is always inside a transaction; the API just hides it when the caller has only one mutation to make.

### 10.4 Async boundary

`rusqlite` is synchronous. The steward is asynchronous. The boundary is the connection pool (section 10.5), which internally dispatches each database call to a blocking task via `tokio::task::spawn_blocking`.

Choice of wrapper:

- `deadpool-sqlite`: chosen. Async connection pool for rusqlite, proven, tokio-native. Handles `spawn_blocking` dispatch transparently.
- `tokio-rusqlite`: alternative; single-threaded executor per connection. Rejected in favour of the explicit pool because the steward has concurrent reads.
- Hand-rolled `spawn_blocking`: possible but adds a subsystem the project would maintain. Off-mission.

Every `async fn` on the store traits is implemented as:

```rust
async fn announce(&self, req: SubjectAnnouncement) -> Result<AnnounceOutcome, StewardError> {
    let pool = self.pool.clone();
    let req = req.clone();
    pool.interact(move |conn| {
        // synchronous rusqlite code
        do_announce(conn, req)
    }).await?
}
```

The outer `.await` waits for a pool connection and for the blocking task to complete. Tokio's multi-threaded runtime gives each pool connection its own blocking thread; parallel announcements from different tasks proceed concurrently up to the pool size.

### 10.5 Connection pool

```rust
pub struct PersistentStore {
    pool: deadpool_sqlite::Pool,
}

impl PersistentStore {
    pub async fn open(path: &Path, config: PoolConfig) -> Result<Self, StewardError> {
        let manager = deadpool_sqlite::Manager::new(path, ...)
            .with_init(|conn| {
                // Apply pragmas listed in section 4.2.
                for pragma in INIT_PRAGMAS {
                    conn.execute_batch(pragma)?;
                }
                Ok(())
            });
        let pool = deadpool_sqlite::Pool::builder(manager)
            .max_size(config.max_connections)
            .build()?;
        // Apply pending migrations.
        // Verify integrity.
        // ...
        Ok(Self { pool })
    }
}
```

Pool configuration is drawn from `StewardConfig`:

```toml
[persistence]
path = "/var/lib/evo/state/evo.db"      # Override for testing.
max_connections = 8                     # Pool size. Default suits most devices.
busy_timeout_ms = 5000                  # Per-connection busy timeout.
```

Defaults chosen so a typical device runs with no configuration; operators can tune on contention.

### 10.6 Error types

`StewardError` gains new variants:

```rust
pub enum StewardError {
    // ... existing variants
    StorageIo { source: std::io::Error, operation: &'static str },
    StorageCorrupted { details: String, quarantine_path: PathBuf },
    SchemaVersionAhead { database: u32, binary: u32 },
    MigrationFailed { version: u32, source: rusqlite::Error },
    InsecureStateDirectory { path: PathBuf, observed_mode: u32, observed_uid: u32 },
    UnknownPredicate { predicate: Predicate, plugin: PluginName },
    UnknownSubjectType { declared: SubjectType, plugin: PluginName },
    CatalogueInverseMismatch { predicate: Predicate, declared_inverse: Predicate, reason: String },
    CardinalityViolation { predicate: Predicate, source: SubjectId, target: SubjectId, limit: u32 },
    DanglingRelationEndpoint { source: SubjectId, predicate: Predicate, target: SubjectId, missing: SubjectId },
    TrustClassInsufficient { operation: &'static str, required: TrustClass, observed: TrustClass },
    AdminOperationUnknown { admin_id: i64 },
    // ...
}
```

Each variant is structured so a log consumer or a plugin author can extract the specific detail from the error without parsing the Display representation. The Display impl stays human-friendly; the variant fields stay machine-consumable.

## 11. Crash Recovery and Corruption Handling

### 11.1 Normal restart

On clean shutdown the steward:

1. Drains the happenings bus (new subscribers not accepted; remaining consumers drain their buffer).
2. Unloads every admitted plugin in reverse admission order.
3. Issues `PRAGMA wal_checkpoint(TRUNCATE)` to fold the WAL into the main database file.
4. Closes the pool. All connections flushed.
5. Removes the socket file.
6. Exits 0.

On next startup the `-wal` and `-shm` files are absent or empty; the database opens in the state the last clean shutdown left it.

### 11.2 Crash mid-transaction

On crash (process kill, power loss, OOM) the WAL and SHM files remain. On next startup SQLite's standard WAL recovery replays committed transactions and discards uncommitted ones. The steward sees a consistent database whose state equals "every transaction that was fully committed before the crash, and nothing else".

This is the specific guarantee `synchronous = FULL` buys: a transaction that returned `Ok` to the caller is durable; a transaction interrupted mid-commit rolls back cleanly.

### 11.3 Integrity check on startup

After opening the database and applying pragmas, but before running migrations, the steward issues:

```sql
PRAGMA integrity_check;
```

Expected result: single row with value `ok`. Any other result is structural database corruption. The steward:

1. Logs the full integrity check output at `error` level.
2. Records the corruption in a diagnostic bundle (section 11.5).
3. Quarantines the database file (section 11.4).
4. Starts with an empty database (new file at the same path).
5. Emits a `StorageQuarantined` happening naming the quarantine path.

A degraded-mode option (continue with the corrupt database, hoping the corruption is in an unused region) is not offered. Silent operation on a corrupted persistence store is how fabrics lose consistency.

The steward also runs a structural consistency check at the application level:

- Every `relation_claimants` row references an extant row in `relations` (foreign keys enforce this, but the integrity check is belt-and-braces).
- Every `relations` row references extant subjects (same).
- Every `custody_state` row references an extant custody (same).
- `subjects` with `forgotten_at_ms IS NOT NULL` and no remaining claimants are eligible for hard-delete; this runs as part of startup GC.

Structural failures that the foreign keys should have prevented but did not are logged and treated as corruption. They should be impossible under `PRAGMA foreign_keys = ON` but are checked anyway.

### 11.4 Corruption quarantine

When corruption is detected, the steward renames the affected files aside:

```
/var/lib/evo/state/evo.db
    -> /var/lib/evo/state/evo.db.corrupted-2026-04-24T16-30-00Z
/var/lib/evo/state/evo.db-wal
    -> /var/lib/evo/state/evo.db-wal.corrupted-2026-04-24T16-30-00Z
/var/lib/evo/state/evo.db-shm
    -> /var/lib/evo/state/evo.db-shm.corrupted-2026-04-24T16-30-00Z
```

Timestamp is UTC, ISO 8601, with colons replaced by hyphens for filesystem safety. The steward then starts fresh: a new `evo.db` gets created and migrations run from scratch.

Quarantined files are never automatically deleted. Operators and maintainers inspect them; recovery tooling (section 11.6) may extract salvageable data; they age out only by explicit operator action.

### 11.5 Diagnostic bundle

Alongside the quarantined files the steward writes a diagnostic document:

```
/var/lib/evo/state/evo.db.corrupted-2026-04-24T16-30-00Z.diag.toml
```

Content:

```toml
[corruption]
detected_at_ms = 1712345678901
detection_method = "integrity_check"   # or "structural_check" or "foreign_key_violation"
steward_version = "0.1.9"
schema_version_observed = 3
sqlite_version = "3.45.1"
rusqlite_version = "0.31.0"

[integrity_check]
output = """
<full output of PRAGMA integrity_check>
"""

[row_counts]
subjects = 1247
subject_addressings = 1891
relations = 3401
relation_claimants = 3678
custodies = 4
custody_state = 4
claim_log = 15234
admin_log = 12

[recent_claim_log]
# Last 100 claim log entries by asserted_at_ms desc, full rows.
```

This is the first thing a maintainer diagnosing a field corruption report asks for. Having it written automatically on detection means the report is never "I didn't capture the state, sorry".

### 11.6 Recovery tool (planned follow-up)

A companion binary `evo-recovery` extracts salvageable data from a quarantined database:

- Reads every table, skipping rows that fail integrity checks.
- Writes a fresh database with the recovered data.
- Produces a recovery report listing what was kept and what was dropped.

This tool is valuable but large enough that it ships as a follow-up commit rather than part of the initial persistence landing. The design is captured here so the initial persistence API is shaped to support it (the tool re-uses the `PersistentStore` type with a `ReadOnlyRepair` mode).

## 12. Interaction with Trust and Plugin Lifecycle

### 12.1 Re-verification of persisted claims on startup

The steward does not assume that claims persisted yesterday are still authorised today. A signing key revoked between yesterday and today must invalidate claims signed by that key.

On startup, after migrations and integrity check, the steward iterates the `plugin_trust_snapshot` table and for each entry:

1. Reads the plugin's current manifest (if the plugin bundle is still installed).
2. Re-verifies the plugin's signing key against the current trust dir and revocation list.
3. If the key is revoked or no longer resolves to the recorded trust class, marks the plugin's claims as quarantined in a single transaction:
   - `subject_addressings` rows authored by the quarantined plugin get `quarantined_by = 'startup_trust_recheck'` and `quarantined_at_ms = now`.
   - `subjects.forgotten_at_ms` is set to now for any subject whose remaining live addressings are all now quarantined (i.e. the plugin was the only claimant, or every other claimant had previously retracted).
   - `relation_claimants` rows authored by the quarantined plugin get the same `quarantined_by` and `quarantined_at_ms` markers.
   - An `admin_log` entry with `kind = 'trust_quarantine'` is appended, carrying the plugin name, the signing key fingerprint, and the revocation reason.
   - A `PluginTrustQuarantined` happening is emitted after the transaction commits.

If the plugin is no longer installed at all (bundle deleted), its persisted claims remain in the database for forensic purposes but are marked as orphan; projections do not compose them.

This re-verification runs once per startup. It is not repeated during normal operation; a plugin that loses its trust mid-run is handled by the admission path.

Performance note: re-verification adds time to startup proportional to the number of plugins with persisted claims. For ten plugins on a typical device this is milliseconds. A benchmark establishes the baseline (section 16.1).

### 12.2 Revocation interaction

When an operator updates `plugins.revocations_path` (adding a new revoked digest), the steward re-applies revocation during its next startup. A running steward does not re-scan revocations; the operator restarts to apply.

A future addition may add hot revocation (signal the steward to re-scan without restart). For v1 the restart-to-apply behaviour is sufficient and matches the stated intent of the revocation mechanism.

### 12.3 Plugin uninstall and its effect on persistence

When `evo-plugin-tool uninstall <plugin>` removes a plugin bundle, the steward's persisted claims made by that plugin remain in the database. Rationale:

- Uninstalling a plugin is an operator action; the operator may intend to reinstall.
- Silently forgetting a plugin's claims on uninstall is data loss; the operator should be in control.
- `evo-plugin-tool uninstall --purge <plugin>` is the explicit opt-in to delete persisted claims. It translates to a transaction that retracts every claim made by the plugin and deletes the `plugin_trust_snapshot` row.

The `--purge` flag is a future addition captured here so the design does not foreclose it.

## 13. Interaction with Catalogue Validation

### 13.1 Predicate existence validation at assertion

The announcer wrapper (`context::RegistryRelationAnnouncer`) receives the loaded catalogue as a constructor argument. Before calling into the relation store, it checks the asserted predicate exists in the catalogue's relation grammar:

```rust
impl RegistryRelationAnnouncer {
    async fn assert(&self, assertion: RelationAssertion) -> Result<(), AnnouncerError> {
        if !self.catalogue.has_predicate(&assertion.predicate) {
            return Err(AnnouncerError::UnknownPredicate {
                predicate: assertion.predicate.clone(),
                plugin: self.claimant.clone(),
            });
        }
        // ... pass through to store
    }
}
```

Strictness: hard reject. No assertion with an unknown predicate reaches the store. The catalogue is the grammar; assertions that violate it are plugin bugs.

The check runs for every assertion. Relative cost is negligible (`has_predicate` is a `HashSet::contains` over the catalogue's declared predicates).

Error variant: `StewardError::UnknownPredicate { predicate, plugin }`. A plugin author reading the error knows immediately what is wrong and where to fix it.

### 13.2 Subject type validation against catalogue

Symmetric to section 13.1. The subject announcer wrapper checks the declared type exists in the catalogue's subject type declarations before calling into the subject store:

```rust
impl RegistrySubjectAnnouncer {
    async fn announce(&self, announcement: SubjectAnnouncement) -> Result<AnnounceOutcome, AnnouncerError> {
        if !self.catalogue.has_subject_type(&announcement.subject_type) {
            return Err(AnnouncerError::UnknownSubjectType {
                declared: announcement.subject_type.clone(),
                plugin: self.claimant.clone(),
            });
        }
        // ... pass through to store
    }
}
```

Strictness: hard reject. Error variant: `StewardError::UnknownSubjectType`.

### 13.3 Inverse predicate consistency

At catalogue load time, before the steward starts accepting plugins, the catalogue loader validates:

- Every predicate that declares an inverse has the inverse also declared.
- The inverse is type-symmetric: if `album_of` declares `source_type = "track", target_type = "album"`, then the declared inverse must declare `source_type = "album", target_type = "track"`.
- Declaring a predicate as its own inverse is allowed only when `source_type = target_type`.

Failures fail startup with `StewardError::CatalogueInverseMismatch { predicate, declared_inverse, reason }`. No database writes happen before the catalogue validates; a malformed catalogue never reaches persistence.

At assertion time, the steward also auto-materialises the inverse claim. When plugin A asserts `album_of(track, album)`, the relation store:

1. Validates the predicate and types (sections 13.1, 13.2, 13.4).
2. Inserts `(track, album_of, album)` into `relations` with claimant A.
3. Inserts `(album, has_album, track)` into `relations` with claimant A.
4. Appends a single `claim_log` entry covering both sides.

When A retracts its claim, both sides retract together. Consumers walking the graph in either direction see consistent provenance.

The alternative (explicit inverse assertion by plugins) was rejected: it forces symmetric work on plugins for no semantic benefit, and it admits a failure mode where the forward claim is asserted but the inverse is not.

### 13.4 Relation cardinality enforcement

At assertion time, after predicate and type validation but before insert, the store checks cardinality:

```rust
let declared = self.catalogue.predicate(&predicate).cardinality;
match declared {
    Cardinality::OneToOne | Cardinality::OneToMany => {
        // Check that source does not already have a different target.
        let existing = self.count_by_source(&source, &predicate).await?;
        if existing > 0 && !self.has_exact(&source, &predicate, &target).await? {
            return Err(StewardError::CardinalityViolation { /* ... */ });
        }
    }
    Cardinality::ManyToOne | Cardinality::OneToOne => {
        // Symmetric check on target.
    }
    Cardinality::ManyToMany => {
        // No check.
    }
}
```

Violations return `StewardError::CardinalityViolation { predicate, source, target, limit }`. The insert does not happen; the transaction rolls back; the caller sees an explicit error.

On startup, after migrations, the steward walks the relation graph and verifies no persisted relations violate the current catalogue's cardinality. Violations fail startup with `StewardError::PersistedCardinalityViolation` naming the offending rows and the operator-facing resolution path ("run `evo-state-tool prune-cardinality-violations` or edit the catalogue"). Auto-pruning is rejected on data-loss grounds.

### 13.5 Dangling relation GC on subject forget

"Forget a subject" has two flavours:

- **Soft forget** (`retract_subject` when the subject still has other claimants): the subject stays; this claimant's contribution is removed.
- **Hard forget** (last claimant retracts): the subject's `forgotten_at_ms` is set; on the same transaction, the GC pass runs.

The GC pass:

1. Walks every relation with `source_id` or `target_id` equal to the forgotten subject.
2. Deletes those relations (foreign keys cascade `relation_claimants` automatically).
3. Appends a `claim_log` entry for each deleted relation with `kind = 'relation_gc_cascade'` and `reason = 'endpoint_forgotten'`.
4. If `forgotten_at_ms` is set and no references remain, the subject row itself is hard-deleted. Its addressings cascade.
5. Appends a `claim_log` entry for the hard-deleted subject with `kind = 'subject_forgotten'`.

The GC pass is in the same transaction as the forget operation. A crash mid-forget either completes the whole forget-plus-cascade or leaves the subject live; it never leaves a half-cascaded state.

Counter-claim interaction: a subject is hard-forgotten only when every claimant has retracted. Until then, the forget is soft and the relations stand. This matches `SUBJECTS.md` section 7.5 semantics.

## 14. Interaction with Correction Primitives

### 14.1 Cross-plugin retraction

An admin plugin calls `admin_retract_claim(target_claimant, subject_id_or_relation_id, reason)`. The steward:

1. Checks the admin plugin's effective trust class is `platform`. Otherwise returns `StewardError::TrustClassInsufficient`.
2. Looks up the claim by claimant and target.
3. In a single transaction:
   - Removes the claim from `subject_addressings` or `relation_claimants`.
   - If this was the last claimant, propagates per section 13.5 (for subjects) or removes the relation (for relations).
   - Appends an `admin_log` entry with `kind = 'retract'`, `admin_plugin = <admin>`, `target_claimant = <claimant>`.

The original plugin's claim is gone from the authoritative state. The audit trail preserves the fact that the admin retracted it and when and why.

### 14.2 Subject merge and split

Merge: `admin_merge_subjects(survivor, victim, reason)`.

1. Trust check: platform only.
2. In a single transaction:
   - Every addressing of `victim` reattaches to `survivor`, preserving claimant and asserted_at_ms.
   - Every relation with `victim` as source or target rewrites to `survivor`. Duplicates collapse (the `(source, predicate, target)` primary key deduplicates automatically; the `relation_claimants` union is the union of the two claimant sets).
   - Every `claim_log` entry referencing `victim` stays unchanged (historical truth is preserved).
   - `victim` is soft-forgotten; its row gets `forgotten_at_ms = now` and is GC'd per section 13.5.
   - An `admin_log` entry with `kind = 'merge'` captures `survivor`, `victim`, reason, and a snapshot of `victim`'s addressings-at-merge-time in the payload. The snapshot enables reversal.

Split: `admin_split_subject(subject, partition, reason)`. The partition argument specifies which addressings belong to the new subject.

1. Trust check: platform only.
2. In a single transaction:
   - Create a new subject with a fresh canonical ID.
   - Reattach addressings matching the partition to the new subject.
   - Relations are harder: the partition must specify which relations attach to the new subject. The API requires this to be explicit; the steward refuses an ambiguous split.
   - An `admin_log` entry with `kind = 'split'` captures both IDs, the partition, and reason.

### 14.3 Relation suppression

`admin_suppress_relation(source, predicate, target, reason)`.

1. Trust check: platform only.
2. In a single transaction:
   - Set `relation_claimants.suppressed_by = <admin>` and `suppressed_at_ms = now` for every claimant of the relation.
   - An `admin_log` entry with `kind = 'suppress'`.
3. Projections that compose this relation skip it; queries that ask for "every relation including suppressed" surface it with its suppression marker.

Unsuppress: `admin_unsuppress_relation(source, predicate, target, reason, reverses = <admin_id>)`. Same shape; clears the suppression fields; the `admin_log` entry references the original suppression via `reverses_admin_id`.

### 14.4 Audit trail for admin operations

Every admin operation appends to `admin_log`. The `payload` JSON captures enough to understand what happened after the fact:

- For retractions: the original claim row contents.
- For merges: the victim subject's full addressing list and relation endpoints at time of merge.
- For splits: the partition that was applied.
- For suppressions: the relation quad and the claimant set that was suppressed.

A `ClaimRetractedByAdmin` / `SubjectMergedByAdmin` / `SubjectSplitByAdmin` / `RelationSuppressedByAdmin` happening fires for each operation, carrying the `admin_log.admin_id`. Subscribers watching the happenings bus see admin operations as they happen; the audit log is the durable record.

### 14.5 Reversibility

Every admin operation is logged with enough detail to reverse it. Reversal APIs:

- `admin_restore_retracted_claim(admin_id)`: re-inserts the retracted claim, references the reversed admin entry.
- `admin_unmerge_subjects(admin_id)`: re-creates the victim subject with its addressings and relations per the merge-time snapshot.
- `admin_unsplit_subjects(admin_id)`: reverses a split.
- `admin_unsuppress_relation(source, predicate, target, reverses = admin_id)`: already covered in 14.3.

Reversibility is a property of the design, not a later addition. Industrial-grade correction means corrections are never destructive.

A reversed reversal (revert the unmerge) is logged as a new admin_log entry, linking back through `reverses_admin_id`; the chain is walkable.

## 15. Testing Strategy

### 15.1 Unit tests

Every module gets the usual `#[cfg(test)] mod tests` pattern. These verify local invariants with the SQLite backend replaced by an in-memory implementation of the same trait where feasible, and with the SQLite backend itself (using `:memory:` databases) where the test specifically exercises persistence behaviour.

### 15.2 Integration tests

New test file `crates/evo/tests/persistence.rs`:

- Open a temporary database, announce subjects and assert relations through the full stack (admission engine, announcer wrappers, stores).
- Close, reopen, verify state matches.
- Exercise the trait boundary: tests use `Arc<dyn SubjectStore>`, not a concrete SQLite type.

### 15.3 Property-based tests

`crates/evo/tests/persistence_property.rs` using `proptest`:

```rust
proptest! {
    #[test]
    fn round_trip(ops in any::<Vec<FabricOp>>()) {
        let store = SqliteStore::open_temp();
        for op in &ops {
            store.apply(op).unwrap();
        }
        let snapshot_a = store.full_dump();

        let path = store.path();
        drop(store);
        let store = SqliteStore::open(path);
        let snapshot_b = store.full_dump();

        prop_assert_eq!(snapshot_a, snapshot_b);
    }
}
```

The `FabricOp` type covers announce, retract, assert, retract, record, release, admin operations. The test generates random sequences, applies them, reloads, and asserts the in-memory view matches. This catches the entire class of "I broke a codec path" bugs for free.

Further property tests:

- Retraction symmetry: every announce has a corresponding retract that returns the store to the prior state.
- Merge reversibility: merge followed by unmerge returns the store to the pre-merge state (modulo the admin_log entries, which grow).
- Cardinality enforcement holds under arbitrary assertion sequences: no reachable sequence of operations produces a state that violates catalogue cardinality.

### 15.4 Migration fixture tests

Per section 8.5, every migration ships with pre-built `.db` fixtures and a test that applies the migration and verifies the result. The test harness is deterministic; a CI run either passes or fails on migration behaviour, never on wall clock or randomness.

### 15.5 Crash-recovery simulation

`crates/evo/tests/persistence_crash.rs`:

- Start a transaction.
- Insert part of it.
- Simulate a crash by closing the connection without commit.
- Reopen.
- Verify the partial transaction is absent.

The simulated crash uses `rusqlite`'s ability to drop a connection mid-transaction; SQLite's WAL recovery then runs on reopen. This tests the actual SQLite behaviour, not a mock.

### 15.6 Benchmarks and regression detection

`crates/evo/benches/persistence.rs` using `criterion`:

- `bench_subject_announce`: median and p99 for a single announce.
- `bench_projection_compose`: median and p99 for a projection with 10-subject walk.
- `bench_startup_replay_1k`: time to startup with 1000 persisted subjects.
- `bench_startup_replay_10k`: same with 10000.

Baselines committed to `crates/evo/benches/baselines/persistence.json`. CI runs the benches on a fixed-hardware runner and fails the build if any metric regresses by more than 20% from the committed baseline. The 20% threshold is coarse enough that noise does not cause flaky CI, tight enough that a real regression does not slip through.

Baselines are updated deliberately: a PR that intentionally makes a bench slower (for a good reason) commits the new baseline as part of the PR. Reviewers see the change; the decision is explicit.

## 16. Performance Contract

### 16.1 Baseline targets

On commodity SSD (development machine, reference hardware):

| Operation | p50 target | p99 target |
|-----------|-----------|-----------|
| Subject announce (1 addressing, no relations) | 5 ms | 20 ms |
| Subject announce (10 addressings) | 15 ms | 50 ms |
| Relation assert (single) | 3 ms | 15 ms |
| Subject retract (claimant) | 5 ms | 20 ms |
| Subject retract (last claimant, GC cascade over 10 relations) | 20 ms | 80 ms |
| Custody record + state report | 4 ms | 15 ms |
| Custody release | 3 ms | 10 ms |
| Resolve subject by addressing | 0.1 ms | 1 ms |
| Project subject (no walk) | 1 ms | 5 ms |
| Project subject (depth-3 walk, 10 subjects) | 10 ms | 40 ms |
| Admin retract | 8 ms | 25 ms |
| Admin merge (two subjects, 20 relations reattached) | 40 ms | 150 ms |
| Startup replay (1000 subjects) | 100 ms | 300 ms |
| Startup replay (10000 subjects) | 1 s | 3 s |

These are initial targets based on SQLite characteristics on tested hardware. The first benchmark run after implementation sets the baseline; if the real numbers are better, the baseline tightens; if worse, the design is revisited before shipping.

### 16.2 Benchmark corpus

Benchmarks run against a corpus generator that produces representative workloads:

- **Small device**: 100 subjects, 300 relations, 3 plugins.
- **Typical device**: 1000 subjects, 5000 relations, 10 plugins.
- **Large device**: 10000 subjects, 50000 relations, 20 plugins.

The corpus generator is deterministic (seeded PRNG); the same seed produces the same corpus across runs. Committed in `crates/evo/benches/corpus.rs`.

### 16.3 Regression gate in CI

CI adds a new job `perf` that runs the criterion benchmarks and compares against `crates/evo/benches/baselines/persistence.json`. Regression by more than 20% on any metric fails the job.

The `perf` job is not part of the MSRV matrix; it runs on a fixed Ubuntu runner with a known hardware profile. Cross-platform performance testing is not attempted; the perf job's value is regression detection, not absolute performance claims.

## 17. Observability

### 17.1 Tracing spans

Every database operation gets a `tracing` span:

```rust
#[instrument(skip(self), fields(subject_id = %req.canonical_id, claimant = %req.claimant))]
async fn announce(&self, req: SubjectAnnouncement) -> Result<AnnounceOutcome, StewardError> {
    // ...
}
```

Span fields:

- `operation`: string tag, e.g. `"subject.announce"`, `"relation.assert"`, `"custody.record"`, `"admin.retract"`.
- `claimant`: the asserting plugin.
- `subject_id` / `relation` / `custody_key`: the entity being touched, for grep-ability.
- `duration_us`: filled in by the span closer.
- `outcome`: `"ok"`, `"rejected"`, `"error"`.

Log output follows the existing structured format (`LOGGING.md`).

### 17.2 Structured attributes

In addition to spans, every operation emits a `tracing::Event` at appropriate level:

- `info`: successful mutations with claimant and summary.
- `warn`: rejected mutations (unknown predicate, cardinality violation), structural consistency issues.
- `error`: corruption, I/O failures, migration failures.

### 17.3 OpenTelemetry compatibility path

The span and attribute layout is chosen to be OpenTelemetry-compatible without requiring the project to pull in the OTel SDK today. Span names use the dot-separated convention OTel expects. Attribute keys use snake_case. A future distribution that wants OTel export adds the exporter to its own binary; the evo-core tracing surface is ready.

Not a commitment to ship OTel export. A commitment to keep the door open.

### 17.4 Pragma diagnostics on startup

On startup, after opening the database and applying pragmas, the steward logs at `info` level:

```
persistence.startup: path=/var/lib/evo/state/evo.db schema_version=3 sqlite_version=3.45.1 wal_mode=ok integrity_check=ok row_counts={subjects=1247, relations=3401, custodies=4}
```

A maintainer with a log stream can see the fabric's persistent scale immediately. The row counts are cheap (SELECT COUNT(*)) and cached briefly if logged frequently.

## 18. Invariants

These hold in every build of the steward and are enforced by the persistence layer:

1. Every subject has exactly one row in `subjects` with a unique `id`.
2. Every addressing references an extant subject. (Foreign key + cascade).
3. Every relation references extant source and target subjects. (Foreign key + cascade).
4. Every relation has at least one claimant. A relation with no claimants is deleted in the same transaction that removed the last claimant.
5. Every custody state snapshot references an extant custody. (Foreign key + cascade).
6. Every `claim_log` entry is append-only. No update, no delete.
7. Every `admin_log` entry is append-only. Reversals are new entries, not modifications.
8. `schema_version` is monotonically increasing. No downgrade migration runs at startup.
9. `PRAGMA integrity_check` returns `ok` on every successful startup. A startup that would see a non-`ok` result quarantines the database and starts fresh.
10. Every mutation that returns `Ok` to the caller has fsync'd to disk. (Consequence of `synchronous = FULL`).
11. Cross-plugin retraction, merge, split, and suppression require `platform` trust class. Other classes are refused at the API boundary.
12. Persisted cardinality violations on startup refuse startup rather than silently carrying on.
13. A subject soft-forgotten with remaining claimants stays visible; only a subject with zero live claimants is hard-deleted.

## 19. Open Questions

Named explicitly so a follow-on engineering layer knows where the open questions live.

| Question | Disposition |
|----------|-------------|
| Custody state history (bounded ring buffer vs last-only) | Revisit if demand materialises. Current: last-only. |
| Hot revocation without restart | Revisit if operational pressure appears. Current: restart-to-apply. |
| Distributed or replicated persistence | Out of scope; a distribution implementing this would sit above the steward, not inside it. |
| `evo-recovery` tool | Roadmap follow-up. Design sketched in section 11.6. |
| `evo-plugin-tool uninstall --purge` | Open. Section 12.3. |
| Projection cache layer | Performance question; only addressed if real devices show projection composition as a bottleneck. |
| Long-horizon happenings history beyond the configured retention window | Distribution choice; the bus is happy to be a data source for a downstream observability bridge. See `HAPPENINGS.md` section 11.4. |
| OpenTelemetry export | Distribution choice, not framework choice. |
| Hot schema migration without restart | Not on the roadmap; migrations always happen at startup. |
| Read replica connections (for projection-heavy workloads) | Performance question; SQLite WAL supports concurrent readers, and the pool already gives concurrent connections; no replica architecture needed until a benchmark disproves this. |

These are not implementation gaps. They are choices held open because the persistence design does not constrain them and each is large enough to merit its own document when picked up.

## 20. Implementation Status

Tracked here so a reader of this document knows which sections describe shipped behaviour and which describe the destination still being walked toward.

| Surface | Status |
| ------- | ------ |
| `PersistenceStore` trait and SQLite-backed `SqlitePersistenceStore` covering the subject-identity slice of section 7's schema (`subjects`, `subject_addressings`, `aliases`, `claim_log`). Trait carried on `StewardState`; `SqlitePersistenceStore` opened from `StewardConfig::persistence.path` (default `/var/lib/evo/state/evo.db`) by the steward binary. Migrations runner active. | Shipped. |
| Durable happenings log (`happenings_log` table from migration 002) recording every emitted Happening with a monotonically increasing `seq`; consumers reconnect with a `since` cursor and the bus replays from the SQLite log when the in-memory ring no longer covers the gap. | Shipped. |
| Pending-conflict surface (`pending_conflicts` table from migration 004) cross-linking each unresolved subject conflict with its addressings and canonical IDs; admin merge marks conflicts resolved as it lands. | Shipped. |
| Boot-time orphan diagnostic. `PersistenceStore::count_subjects_by_type` powers a startup pass that diffs persisted subject types against the catalogue and emits per-orphan and summary `tracing::warn!` lines. | Shipped. |
| Subject-registry write path routed through `PersistenceStore`. Every announce / retract / forget operation lands in SQLite before the in-memory registry returns success. | In progress: write path uses `record_subject_*` paths; full boot-time rehydration of the in-memory registry is the next slice. |
| Relation graph durability (extends schema with `relations`, `relation_claimants`), write path through `PersistenceStore`, on-disk relation cardinality re-validation per section 13.4. | Roadmap. |
| Custody ledger durability (extends schema with `custodies`, `custody_state`), custody write path, last-state snapshot persistence. | Roadmap. |
| Admin ledger durability (extends schema with `admin_log`), admin operations recorded persistently, reversibility primitives walk the persisted log per section 14.5. | Roadmap. |
| Trust re-verification on startup per section 12.1; `plugin_trust_snapshot` table; `trust_quarantine` audit entries. | Roadmap. |

The shipped slice ships the trait, the SQLite implementation, the in-memory mock used by every steward-internal test, and a migrations directory (`crates/evo/migrations/{001_initial,002_happenings,003_meta,004_pending_conflicts}.sql`). The trait's method shape is schema-aware: each fabric operation maps to one transaction touching every affected table atomically, satisfying the multi-table durability promise from section 4.3. Future schema additions append new migration files rather than renumber; the `schema_version` table records what has been applied.
