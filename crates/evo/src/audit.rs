// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Forensic-query reader for the audit-grade ledger.
//!
//! The framework records per-call lifecycle events from the
//! in-process plugin-facing primitives (streams, notifications,
//! metadata) into the signed `evo.lifecycle` ledger track on every
//! plugin-initiated operation. This module gives operators and
//! security analysts a read-side surface against the same
//! substrate the framework writes to: open the steward's
//! database read-only, filter by plugin / time range / event type,
//! deserialise into typed [`LifecycleEntry`] rows, return them in
//! per-ledger causal order.
//!
//! ## Read-only by construction
//!
//! [`query_lifecycle`] opens the underlying SQLite file with
//! `SQLITE_OPEN_READ_ONLY` and pins `query_only = 1` on the
//! connection. The reader cannot mutate the substrate even if a
//! caller manages to inject SQL through the filter — the filter
//! shape uses parameterised binds, but the read-only flag is the
//! defence in depth. Multiple readers can run concurrently with
//! a writing steward in WAL mode without coordination.
//!
//! ## Witness-grade ordering
//!
//! Rows are returned in `(created_at_ms ASC, entry_id ASC)` order
//! per the substrate's index. Combined with the
//! [`LedgerPrimitive`](crate::ledger::LedgerPrimitive)'s
//! per-ledger-id monotonic-floor invariant on `created_at_ms`,
//! this is causal order — the order in which the framework
//! recorded the events, regardless of wall-clock resolution.
//!
//! ## What this module does not own
//!
//! - Verification of signed entries. The reader returns rows
//!   verbatim; verification against a vendor-supplied
//!   [`CryptographicServices`](crate::ledger::CryptographicServices)
//!   implementation is a separate audit step that runs against
//!   the same row set.
//! - Output formatting (json / tsv / human). The CLI binary
//!   formats; the reader returns typed rows.
//! - Cross-ledger queries. This reader targets `evo.lifecycle`
//!   exclusively; other audit ledger ids
//!   (`evo.consent` / `evo.trust` / `evo.action`) get their own
//!   readers when their forensic surfaces land.

use crate::ledger::{LifecycleEntry, LifecycleEventType, LEDGER_LIFECYCLE};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use thiserror::Error;

/// Filter for [`query_lifecycle`]. Every field is optional; an
/// empty filter returns every entry in the ledger (subject to
/// `limit`).
#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    /// Restrict to entries whose `source_plugin` matches.
    /// `None` returns entries for every plugin.
    pub plugin: Option<String>,
    /// Restrict to entries with `recorded_at_ms >= since_ms`.
    pub since_ms: Option<u64>,
    /// Restrict to entries with `recorded_at_ms <= until_ms`.
    pub until_ms: Option<u64>,
    /// Restrict to entries whose `event_type` matches. Filtered
    /// post-deserialisation since `event_type` lives inside the
    /// payload JSON, not as a substrate column. The query still
    /// uses substrate-side filters for plugin and time range, so
    /// the per-deserialisation pass only sees rows already
    /// narrowed by the indexed columns.
    pub event_type: Option<LifecycleEventType>,
    /// Maximum number of entries to return. `None` returns all
    /// matches.
    pub limit: Option<usize>,
    /// When `false`, exclude entries that have been withdrawn via
    /// the substrate's two-step withdrawal flow.
    pub include_withdrawn: bool,
}

/// Errors raised by the audit reader.
#[derive(Debug, Error)]
pub enum AuditError {
    /// Could not open the database file.
    #[error("could not open database at {path}: {source}")]
    OpenDatabase {
        /// Path the reader tried to open.
        path: String,
        /// Underlying SQLite error.
        #[source]
        source: rusqlite::Error,
    },
    /// SQL prepare or execute failed.
    #[error("ledger query failed: {source}")]
    Query {
        /// Underlying SQLite error.
        #[source]
        source: rusqlite::Error,
    },
    /// A row's payload JSON did not deserialise into a
    /// [`LifecycleEntry`]. Carries the offending row's entry id
    /// so the operator can investigate the corrupt entry directly.
    #[error(
        "lifecycle payload at entry {entry_id} did not deserialise: {source}"
    )]
    Deserialise {
        /// Entry id of the malformed row.
        entry_id: String,
        /// Underlying serde error.
        #[source]
        source: serde_json::Error,
    },
}

/// Open the steward's database at `path` read-only and return
/// every `evo.lifecycle` entry matching `filter`, ordered by
/// causal sequence (per-ledger monotonic `created_at_ms`).
///
/// Read-only by construction: the connection is opened with
/// [`OpenFlags::SQLITE_OPEN_READ_ONLY`] and pinned with
/// `query_only = 1`. Concurrent with a running steward via SQLite
/// WAL.
pub fn query_lifecycle(
    path: &Path,
    filter: &AuditFilter,
) -> Result<Vec<LifecycleEntry>, AuditError> {
    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|source| AuditError::OpenDatabase {
                path: path.display().to_string(),
                source,
            })?;
    // Defence in depth: refuse any mutation even if the connection
    // is later mis-used. SQLite's read-only flag already prevents
    // schema or row writes, but `query_only` makes the refusal
    // explicit at the parser layer.
    conn.execute_batch("PRAGMA query_only = 1;")
        .map_err(|source| AuditError::Query { source })?;

    // Build SQL with bound parameters. The filter shape uses only
    // indexed columns at the substrate side; event_type is
    // post-filtered after deserialisation.
    let mut sql = String::from(
        "SELECT entry_id, payload_json, created_at_ms, subject_plugin \
         FROM ledger_entries WHERE ledger_id = ?1",
    );
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> =
        vec![Box::new(LEDGER_LIFECYCLE.to_string())];
    if let Some(plugin) = &filter.plugin {
        sql.push_str(" AND subject_plugin = ?");
        sql.push_str(&(binds.len() + 1).to_string());
        binds.push(Box::new(plugin.clone()));
    }
    if let Some(since_ms) = filter.since_ms {
        sql.push_str(" AND created_at_ms >= ?");
        sql.push_str(&(binds.len() + 1).to_string());
        binds.push(Box::new(since_ms as i64));
    }
    if let Some(until_ms) = filter.until_ms {
        sql.push_str(" AND created_at_ms <= ?");
        sql.push_str(&(binds.len() + 1).to_string());
        binds.push(Box::new(until_ms as i64));
    }
    if !filter.include_withdrawn {
        sql.push_str(" AND withdrawn_by_entry_id IS NULL");
    }
    sql.push_str(" ORDER BY created_at_ms ASC, entry_id ASC");

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|source| AuditError::Query { source })?;

    let bind_refs: Vec<&dyn rusqlite::ToSql> =
        binds.iter().map(|b| b.as_ref()).collect();
    let row_iter = stmt
        .query_map(rusqlite::params_from_iter(bind_refs), |row| {
            let entry_id: String = row.get(0)?;
            let payload_json: String = row.get(1)?;
            let _created_at_ms: i64 = row.get(2)?;
            let _subject_plugin: Option<String> = row.get(3)?;
            Ok((entry_id, payload_json))
        })
        .map_err(|source| AuditError::Query { source })?;

    let mut out = Vec::new();
    for row in row_iter {
        let (entry_id, payload_json) =
            row.map_err(|source| AuditError::Query { source })?;
        let entry: LifecycleEntry = serde_json::from_str(&payload_json)
            .map_err(|source| AuditError::Deserialise {
                entry_id: entry_id.clone(),
                source,
            })?;
        if let Some(filter_type) = filter.event_type {
            if entry.event_type != filter_type {
                continue;
            }
        }
        out.push(entry);
        if let Some(limit) = filter.limit {
            if out.len() >= limit {
                break;
            }
        }
    }

    // Drop the prepared statement before consuming the connection
    // (rusqlite owns the lifetime).
    drop(stmt);
    drop(conn);
    Ok(out)
}

/// Output format selector for the operator-facing CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Human-readable, one row per line, columns: ISO-8601
    /// timestamp, source_plugin, event_type, target, outcome.
    /// Default when `--format` is not supplied.
    Human,
    /// Tab-separated, machine-friendly for shell pipelines
    /// (`grep`, `awk`, `cut`). Same column order as Human.
    Tsv,
    /// Single JSON array of typed [`LifecycleEntry`] rows. Used by
    /// tooling that consumes structured output.
    Json,
}

impl OutputFormat {
    /// Parse an `--format` value into a typed selector. Accepts
    /// `human` / `tsv` / `json`; case-insensitive.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "human" => Some(Self::Human),
            "tsv" => Some(Self::Tsv),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

/// Render a slice of [`LifecycleEntry`] rows to `out` in the
/// requested format. Shared between the CLI and any in-process
/// caller (test harnesses, tooling).
pub fn render_entries(
    entries: &[LifecycleEntry],
    format: OutputFormat,
    out: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    match format {
        OutputFormat::Human => {
            for entry in entries {
                let target = format_target(&entry.target);
                let outcome = format_outcome(&entry.outcome);
                writeln!(
                    out,
                    "{}\t{}\t{:?}\t{}\t{}",
                    format_iso8601(entry.recorded_at_ms),
                    entry.source_plugin,
                    entry.event_type,
                    target,
                    outcome,
                )?;
            }
            Ok(())
        }
        OutputFormat::Tsv => {
            for entry in entries {
                let target = format_target(&entry.target);
                let outcome = format_outcome(&entry.outcome);
                writeln!(
                    out,
                    "{}\t{}\t{:?}\t{}\t{}",
                    entry.recorded_at_ms,
                    entry.source_plugin,
                    entry.event_type,
                    target,
                    outcome,
                )?;
            }
            Ok(())
        }
        OutputFormat::Json => {
            let json = serde_json::to_string_pretty(entries)
                .expect("LifecycleEntry serialises");
            writeln!(out, "{}", json)
        }
    }
}

fn format_target(target: &crate::ledger::LifecycleTarget) -> String {
    use crate::ledger::LifecycleTarget;
    match target {
        LifecycleTarget::Stream { stream_id } => {
            format!("stream:{}", stream_id)
        }
        LifecycleTarget::Notification { handle } => {
            format!("notification:{}", handle)
        }
        LifecycleTarget::MetadataQuery { query_digest } => {
            format!("query:{}", &query_digest[..16.min(query_digest.len())])
        }
        LifecycleTarget::MetadataProvider { provider_id } => {
            format!("provider:{}", provider_id)
        }
        LifecycleTarget::SourcePlugin { plugin_id } => {
            format!("source-plugin:{}", plugin_id)
        }
        LifecycleTarget::Publisher { publisher_id } => {
            format!("publisher:{}", publisher_id)
        }
        LifecycleTarget::AuthSession { username } => {
            format!("auth-session:{}", username)
        }
        LifecycleTarget::Operation { operation } => {
            format!("operation:{}", operation)
        }
    }
}

fn format_outcome(outcome: &crate::ledger::LifecycleOutcome) -> String {
    use crate::ledger::LifecycleOutcome;
    match outcome {
        LifecycleOutcome::Success => "success".to_string(),
        LifecycleOutcome::Failed { reason } => format!("failed: {}", reason),
    }
}

fn format_iso8601(ms: u64) -> String {
    // Compact ISO-8601 in UTC. The CLI is operator-facing so we
    // keep the format predictable without needing a tz-aware
    // formatter; analysts converting to local time use the
    // raw recorded_at_ms in the JSON output.
    use chrono::DateTime;
    DateTime::from_timestamp_millis(ms as i64)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_else(|| format!("{}ms", ms))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{
        LedgerPrimitive, LifecycleEntry, LifecycleEventType, LifecycleOutcome,
        LifecycleTarget,
    };
    use crate::persistence::SqlitePersistenceStore;
    use std::sync::Arc;

    fn fixture_entry(
        plugin: &str,
        event_type: LifecycleEventType,
        outcome: LifecycleOutcome,
    ) -> LifecycleEntry {
        LifecycleEntry {
            event_type,
            source_plugin: plugin.into(),
            target: LifecycleTarget::Stream {
                stream_id: "test.stream".into(),
            },
            recorded_at_ms: 0,
            outcome,
            payload: serde_json::json!({}),
        }
    }

    async fn seed_db(path: &std::path::Path, entries: Vec<LifecycleEntry>) {
        let store: Arc<dyn crate::persistence::PersistenceStore> =
            Arc::new(SqlitePersistenceStore::open(path.to_path_buf()).unwrap());
        let ledger = LedgerPrimitive::with_no_op_crypto(Arc::clone(&store));
        for entry in entries {
            ledger.append_lifecycle(&entry).await.unwrap();
        }
    }

    #[tokio::test]
    async fn query_lifecycle_returns_all_entries_under_default_filter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.db");
        seed_db(
            &path,
            vec![
                fixture_entry(
                    "org.test.alpha",
                    LifecycleEventType::StreamOpened,
                    LifecycleOutcome::Success,
                ),
                fixture_entry(
                    "org.test.beta",
                    LifecycleEventType::NotificationSent,
                    LifecycleOutcome::Success,
                ),
            ],
        )
        .await;

        let entries = query_lifecycle(&path, &AuditFilter::default()).unwrap();
        assert_eq!(entries.len(), 2);
        // Causal order: alpha (StreamOpened) wrote first.
        assert_eq!(entries[0].source_plugin, "org.test.alpha");
        assert_eq!(entries[1].source_plugin, "org.test.beta");
    }

    #[tokio::test]
    async fn query_lifecycle_filters_by_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.db");
        seed_db(
            &path,
            vec![
                fixture_entry(
                    "org.test.alpha",
                    LifecycleEventType::StreamOpened,
                    LifecycleOutcome::Success,
                ),
                fixture_entry(
                    "org.test.beta",
                    LifecycleEventType::NotificationSent,
                    LifecycleOutcome::Success,
                ),
            ],
        )
        .await;

        let entries = query_lifecycle(
            &path,
            &AuditFilter {
                plugin: Some("org.test.beta".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source_plugin, "org.test.beta");
    }

    #[tokio::test]
    async fn query_lifecycle_filters_by_event_type() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.db");
        seed_db(
            &path,
            vec![
                fixture_entry(
                    "org.test.alpha",
                    LifecycleEventType::StreamOpened,
                    LifecycleOutcome::Success,
                ),
                fixture_entry(
                    "org.test.alpha",
                    LifecycleEventType::StreamClosed,
                    LifecycleOutcome::Success,
                ),
                fixture_entry(
                    "org.test.alpha",
                    LifecycleEventType::NotificationSent,
                    LifecycleOutcome::Success,
                ),
            ],
        )
        .await;

        let entries = query_lifecycle(
            &path,
            &AuditFilter {
                event_type: Some(LifecycleEventType::StreamOpened),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event_type, LifecycleEventType::StreamOpened);
    }

    #[tokio::test]
    async fn query_lifecycle_limit_caps_result_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.db");
        let entries: Vec<LifecycleEntry> = (0..10)
            .map(|_| {
                fixture_entry(
                    "org.test",
                    LifecycleEventType::StreamOpened,
                    LifecycleOutcome::Success,
                )
            })
            .collect();
        seed_db(&path, entries).await;

        let result = query_lifecycle(
            &path,
            &AuditFilter {
                limit: Some(3),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(result.len(), 3);
    }

    #[tokio::test]
    async fn query_lifecycle_filters_by_time_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.db");
        seed_db(
            &path,
            vec![
                fixture_entry(
                    "org.test",
                    LifecycleEventType::StreamOpened,
                    LifecycleOutcome::Success,
                ),
                fixture_entry(
                    "org.test",
                    LifecycleEventType::StreamClosed,
                    LifecycleOutcome::Success,
                ),
            ],
        )
        .await;

        // Time-window filter that excludes everything (way in the
        // past). The reader returns an empty vec rather than an
        // error.
        let entries = query_lifecycle(
            &path,
            &AuditFilter {
                until_ms: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn query_lifecycle_open_database_failure_surfaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.db");
        let err = query_lifecycle(&path, &AuditFilter::default()).unwrap_err();
        assert!(matches!(err, AuditError::OpenDatabase { .. }));
    }

    #[test]
    fn output_format_parse_round_trip() {
        assert_eq!(OutputFormat::parse("human"), Some(OutputFormat::Human));
        assert_eq!(OutputFormat::parse("HUMAN"), Some(OutputFormat::Human));
        assert_eq!(OutputFormat::parse("tsv"), Some(OutputFormat::Tsv));
        assert_eq!(OutputFormat::parse("json"), Some(OutputFormat::Json));
        assert_eq!(OutputFormat::parse("garbage"), None);
    }

    #[test]
    fn render_entries_human_emits_one_line_per_entry() {
        let entries = vec![fixture_entry(
            "org.test",
            LifecycleEventType::StreamOpened,
            LifecycleOutcome::Success,
        )];
        let mut buf = Vec::new();
        render_entries(&entries, OutputFormat::Human, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("org.test"));
        assert!(text.contains("StreamOpened"));
        assert!(text.contains("success"));
    }

    #[test]
    fn render_entries_json_round_trips_through_serde() {
        let entries = vec![fixture_entry(
            "org.test",
            LifecycleEventType::NotificationSent,
            LifecycleOutcome::Success,
        )];
        let mut buf = Vec::new();
        render_entries(&entries, OutputFormat::Json, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        let decoded: Vec<LifecycleEntry> =
            serde_json::from_str(text.trim()).unwrap();
        assert_eq!(decoded, entries);
    }

    #[test]
    fn render_entries_tsv_uses_tab_separators() {
        let entries = vec![fixture_entry(
            "org.test",
            LifecycleEventType::StreamOpened,
            LifecycleOutcome::Success,
        )];
        let mut buf = Vec::new();
        render_entries(&entries, OutputFormat::Tsv, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains('\t'));
        assert!(text.starts_with('0')); // recorded_at_ms = 0
    }

    #[test]
    fn render_entries_handles_failed_outcome() {
        let entries = vec![fixture_entry(
            "org.test",
            LifecycleEventType::MetadataProviderTimedOut,
            LifecycleOutcome::Failed {
                reason: "deadline exceeded".into(),
            },
        )];
        let mut buf = Vec::new();
        render_entries(&entries, OutputFormat::Human, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("failed: deadline exceeded"));
    }
}
