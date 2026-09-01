// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Evo steward binary entrypoint.
//!
//! Default invocation (no subcommand) parses CLI flags via clap and
//! invokes the library boot sequence with the default admission
//! strategy ([`evo::discover_plugins`]), which walks the configured
//! `plugins.search_roots` and admits out-of-process singletons.
//!
//! `evo audit lifecycle [...]` dispatches to the operator-facing
//! forensic-query reader against the steward's audit-grade ledger.
//! The query path is read-only and runs concurrently with a live
//! steward via SQLite WAL; no boot occurs.
//!
//! Distributions composing their own steward binary call
//! [`evo::run`] directly with a custom [`evo::AdmissionSetup`]; see
//! the crate-level docs.
//!
//! Tests do not touch this file; anything testable lives in the
//! library.

#![forbid(unsafe_code)]
#![allow(missing_docs)]

use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = evo::cli::Args::parse();
    match args.command.as_ref() {
        Some(evo::cli::Command::Audit { audit_command }) => {
            run_audit(audit_command)
        }
        None => evo::run(evo::RunOptions::from_args(args)).await,
    }
}

fn run_audit(cmd: &evo::cli::AuditCommand) -> anyhow::Result<()> {
    use evo::audit::{query_lifecycle, AuditFilter, OutputFormat};
    use evo::ledger::LifecycleEventType;
    match cmd {
        evo::cli::AuditCommand::Lifecycle {
            db,
            plugin,
            since,
            until,
            event_type,
            limit,
            include_withdrawn,
            format,
        } => {
            let event_type_typed = match event_type.as_deref() {
                None => None,
                Some(s) => Some(parse_event_type(s)?),
            };
            let format_typed =
                OutputFormat::parse(format).ok_or_else(|| {
                    anyhow::anyhow!(
                        "unknown --format value {:?}; expected human|tsv|json",
                        format
                    )
                })?;
            let filter = AuditFilter {
                plugin: plugin.clone(),
                since_ms: *since,
                until_ms: *until,
                event_type: event_type_typed,
                limit: *limit,
                include_withdrawn: *include_withdrawn,
            };
            let entries = query_lifecycle(db, &filter)?;
            let stdout = std::io::stdout();
            let mut handle = stdout.lock();
            evo::audit::render_entries(&entries, format_typed, &mut handle)?;
            let _ = LifecycleEventType::StreamOpened; // type alias kept in scope
            Ok(())
        }
    }
}

fn parse_event_type(
    s: &str,
) -> anyhow::Result<evo::ledger::LifecycleEventType> {
    use evo::ledger::LifecycleEventType;
    match s {
        "stream_opened" => Ok(LifecycleEventType::StreamOpened),
        "stream_closed" => Ok(LifecycleEventType::StreamClosed),
        "notification_sent" => Ok(LifecycleEventType::NotificationSent),
        "notification_cancelled" => {
            Ok(LifecycleEventType::NotificationCancelled)
        }
        "metadata_query_dispatched" => {
            Ok(LifecycleEventType::MetadataQueryDispatched)
        }
        "metadata_provider_timed_out" => {
            Ok(LifecycleEventType::MetadataProviderTimedOut)
        }
        "metadata_provider_failed" => {
            Ok(LifecycleEventType::MetadataProviderFailed)
        }
        other => Err(anyhow::anyhow!(
            "unknown --event-type value {:?}; expected one of: \
             stream_opened, stream_closed, notification_sent, \
             notification_cancelled, metadata_query_dispatched, \
             metadata_provider_timed_out, metadata_provider_failed",
            other
        )),
    }
}
