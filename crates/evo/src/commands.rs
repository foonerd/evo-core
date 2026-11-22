// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Operator subcommands of the steward binary.
//!
//! [`crate::run`] answers these before anything a steward starts, so
//! every binary that hands its parsed [`cli::Args`] to `run` — the
//! shipped `evo` binary and every distribution's own — carries them.
//! None of them boots a steward.
//!
//! - `audit lifecycle [...]` reads the audit-grade ledger, read-only,
//!   alongside a live steward.
//! - `auth rotate-token-key [--state-dir PATH]` replaces the bearer-
//!   token signing key with the steward stopped. See
//!   [`crate::https_boot::rotate_token_signing_key`].
//! - `auth restore-tier [--state-dir PATH]` returns to the configured
//!   auth tier after a credentials reset, with the steward stopped.
//!   See [`crate::https_boot::restore_configured_tier`].

use std::io::Write as _;

use crate::cli;

/// Answer one subcommand.
pub(crate) fn run_command(
    args: &cli::Args,
    command: &cli::Command,
) -> anyhow::Result<()> {
    match command {
        cli::Command::Audit { audit_command } => run_audit(audit_command),
        cli::Command::Auth { auth_command } => run_auth(args, auth_command),
    }
}

/// The HTTPS state directory and the steward socket an `auth`
/// subcommand acts on: the same config, socket and state-directory
/// rules the steward boots with, so the command changes what the
/// steward reads and asks the socket the steward binds.
fn auth_paths(
    args: &cli::Args,
    state_dir: &Option<std::path::PathBuf>,
) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
    let config = match &args.config {
        Some(path) => crate::config::StewardConfig::load_from_required(path)?,
        None => crate::config::StewardConfig::load()?,
    };
    let socket_path = args
        .socket
        .clone()
        .unwrap_or_else(|| config.steward.socket_path.clone());
    let root = state_dir.clone().unwrap_or_else(|| {
        crate::https_boot::https_state_root(&config.persistence.path)
    });
    Ok((root.join("https"), socket_path))
}

fn run_auth(
    args: &cli::Args,
    command: &cli::AuthCommand,
) -> anyhow::Result<()> {
    match command {
        cli::AuthCommand::RestoreTier { state_dir } => {
            let (https_dir, socket_path) = auth_paths(args, state_dir)?;
            let restored = crate::https_boot::restore_configured_tier(
                &https_dir,
                &socket_path,
            )?;
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            if restored {
                writeln!(
                    out,
                    "Removed the Open tier the credentials reset left in {}.",
                    https_dir.display()
                )?;
            } else {
                writeln!(
                    out,
                    "No credentials reset left the Open tier in {}.",
                    https_dir.display()
                )?;
            }
            writeln!(
                out,
                "The next start runs the tier the steward's configuration \
                 names (EVO_AUTH_TIER; Open when unset)."
            )?;
            Ok(())
        }
        cli::AuthCommand::RotateTokenKey { state_dir } => {
            let (https_dir, socket_path) = auth_paths(args, state_dir)?;
            let rotation = crate::https_boot::rotate_token_signing_key(
                &https_dir,
                &socket_path,
            )?;
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            writeln!(
                out,
                "New bearer-token signing key written to {}.",
                rotation.https_dir.join("tokens.key").display()
            )?;
            writeln!(
                out,
                "Cleared: {} credential record(s); revocation list {}; \
                 bootstrap token {}.",
                rotation.records_cleared,
                if rotation.revocations_cleared {
                    "removed"
                } else {
                    "absent"
                },
                if rotation.bootstrap_token_removed {
                    "removed"
                } else {
                    "absent"
                },
            )?;
            writeln!(
                out,
                "Start the steward service. From then on every bearer issued \
                 before now is refused: browsers pair again and operator \
                 credentials are minted anew. A kiosk panel, if there is \
                 one, mints a new session within about 15 seconds; it \
                 needs no restart."
            )?;
            Ok(())
        }
    }
}

fn run_audit(cmd: &cli::AuditCommand) -> anyhow::Result<()> {
    use crate::audit::{query_lifecycle, AuditFilter, OutputFormat};
    match cmd {
        cli::AuditCommand::Lifecycle {
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
            crate::audit::render_entries(&entries, format_typed, &mut handle)?;
            Ok(())
        }
    }
}

fn parse_event_type(
    s: &str,
) -> anyhow::Result<crate::ledger::LifecycleEventType> {
    use crate::ledger::LifecycleEventType;
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
