// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! `admin` subcommands. Operator-side wrappers over the steward's
//! plugin-lifecycle wire ops (`enable_plugin` / `disable_plugin` /
//! `uninstall_plugin` / `purge_plugin_state`), the catalogue /
//! manifest reload verbs (`reload_catalogue` / `reload_manifest`),
//! and the reconciliation read-only / admin verbs
//! (`list_reconciliation_pairs` / `project_reconciliation_pair` /
//! `reconcile_pair_now`).
//!
//! Every command opens a Unix-domain socket to the running steward
//! (default `/run/evo/evo.sock`), negotiates the capability the op
//! requires (`plugins_admin` for plugin-lifecycle ops,
//! `reconciliation_admin` for the manual reconciliation trigger,
//! none for read-only inventory and projection), sends the
//! corresponding op as a length-prefixed JSON frame, parses the
//! structured response, and prints it to stdout. Failures exit
//! with the documented exit-code contract: 0 on success, 1 on
//! operator-input errors, 2 on permission denials, 3 on I/O
//! failures.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use base64::Engine;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Default socket path the steward listens on. Distributions
/// configuring a non-default path pass `--socket=<path>`.
pub const DEFAULT_SOCKET_PATH: &str = "/run/evo/evo.sock";

/// Hard cap on a single response frame. Mirrors the steward's
/// frame-size cap so a runaway peer cannot exhaust the tool's
/// memory while reading. Pinned to the framework's hard ceiling
/// on `prepare_for_live_reload` state blobs so admin verbs that
/// proxy plugin lifecycle (e.g. reload-plugin) inherit the same
/// envelope cap.
const MAX_FRAME_SIZE: usize =
    evo_plugin_sdk::contract::MAX_LIVE_RELOAD_BLOB_BYTES;

/// Per-call deadline. Operator-issued admin ops are bounded
/// operations; 180 s covers the worst-case bounded operation
/// (bulk grammar migration over a 50 k subject set) while still
/// surfacing a true runtime hang as a structured timeout rather
/// than the operator staring at a wedged terminal.
const CALL_DEADLINE_SECS: u64 = 180;

/// Source for the new manifest in the reload verbs.
#[derive(Debug)]
pub enum ReloadSource {
    Inline(String),
    Path(PathBuf),
}

/// Inject a step-up token field into the wire request when the
/// caller passed one on the CLI. Helper used by every privileged
/// operator subcommand so the threading is consistent.
fn with_step_up(
    mut req: serde_json::Value,
    step_up_token: Option<&str>,
) -> serde_json::Value {
    if let Some(token) = step_up_token {
        req["step_up_token"] = serde_json::json!(token);
    }
    req
}

/// Run the `enable` subcommand.
pub fn enable(
    socket: &Path,
    plugin: &str,
    reason: Option<&str>,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "enable_plugin",
            "plugin": plugin,
            "reason": reason,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    print_lifecycle_outcome(&resp)
}

/// Run the `disable` subcommand. When
/// `cascade_dependents = true` the framework first disables
/// every admitted plugin that declares the target as
/// required; without the flag the call refuses with the
/// `dependents_present` subclass when dependents are
/// present.
pub fn disable(
    socket: &Path,
    plugin: &str,
    reason: Option<&str>,
    step_up_token: Option<&str>,
    cascade_dependents: bool,
) -> Result<(), anyhow::Error> {
    let mut req = serde_json::json!({
        "op": "disable_plugin",
        "plugin": plugin,
        "reason": reason,
    });
    if cascade_dependents {
        req["cascade_dependents"] = serde_json::json!(true);
    }
    let req = with_step_up(req, step_up_token);
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        // Special-case the dependents_present subclass: surface
        // the dependent list inline so the operator sees the
        // impact without a follow-up call.
        let subclass = err
            .get("details")
            .and_then(|d| d.get("subclass"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if subclass == "dependents_present" {
            let dependents = err
                .get("details")
                .and_then(|d| d.get("dependents"))
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            eprintln!(
                "disable_plugin refused: plugin {plugin} has {n} \
                 admitted dependent(s):",
                n = dependents.len()
            );
            for d in dependents {
                if let Some(s) = d.as_str() {
                    eprintln!("  {s}");
                }
            }
            eprintln!(
                "Pass --cascade-dependents to disable them too, or run \
                 'admin dependents {plugin}' to preview without acting."
            );
            return Err(anyhow::anyhow!(
                "disable refused: dependents_present (see stderr)"
            ));
        }
        return Err(format_error("disable_plugin", err));
    }
    // Cascade success surfaces a different shape; render
    // accordingly.
    if resp
        .get("plugin_lifecycle_cascaded")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let disabled = resp
            .get("disabled")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let failed = resp
            .get("failed")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        println!(
            "plugin disable cascade: succeeded={} failed={}",
            disabled.len(),
            failed.len()
        );
        if !disabled.is_empty() {
            println!("disabled (in cascade order):");
            for d in disabled {
                if let Some(s) = d.as_str() {
                    println!("  {s}");
                }
            }
        }
        if !failed.is_empty() {
            println!("failed:");
            for f in failed {
                let plugin =
                    f.get("plugin_name").and_then(|x| x.as_str()).unwrap_or("");
                let msg =
                    f.get("message").and_then(|x| x.as_str()).unwrap_or("");
                println!("  {plugin}: {msg}");
            }
        }
        return Ok(());
    }
    print_lifecycle_outcome(&resp)
}

/// Run the `uninstall` subcommand.
pub fn uninstall(
    socket: &Path,
    plugin: &str,
    reason: Option<&str>,
    purge_state: bool,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "uninstall_plugin",
            "plugin": plugin,
            "reason": reason,
            "purge_state": purge_state,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    print_lifecycle_outcome(&resp)
}

/// Run the `describe-capabilities` subcommand. Sends
/// `op = "describe_capabilities"` and prints the steward's
/// `Capabilities` response.
pub fn describe_capabilities(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "describe_capabilities"});
    let resp = call_with_caps(socket, &[], req)?;
    print_describe_capabilities(&resp)
}

/// Run the `subscribe-happenings` streaming subcommand.
///
/// Connects, sends a `subscribe_happenings` op with the optional
/// filter dimensions and `since` cursor, reads the
/// `Subscribed { current_seq }` ack, then loops reading streamed
/// `Happening { seq, happening }` frames. One JSON line per
/// happening on stdout. Exits after `max_count` events OR
/// `duration_secs` elapsed, whichever comes first. The first line
/// of output is a structured ack: `subscribed: { current_seq: N }`.
#[allow(clippy::too_many_arguments)]
pub fn subscribe_happenings(
    socket: &Path,
    max_count: u64,
    duration_secs: u64,
    since: Option<u64>,
    variants: Option<&str>,
    plugins: Option<&str>,
    shelves: Option<&str>,
    coalesce_labels: Option<&str>,
    coalesce_window_ms: Option<u32>,
) -> Result<(), anyhow::Error> {
    let socket = socket.to_path_buf();
    let variants_vec = parse_csv_filter(variants);
    let plugins_vec = parse_csv_filter(plugins);
    let shelves_vec = parse_csv_filter(shelves);
    let coalesce_labels_vec = parse_csv_filter(coalesce_labels);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let mut stream =
            UnixStream::connect(&socket).await.with_context(|| {
                format!("connecting to steward socket {}", socket.display())
            })?;
        // Build subscribe request. Filter dimensions are optional;
        // empty arrays match the steward's no-op-filter shape.
        let mut req = serde_json::json!({
            "op": "subscribe_happenings",
            "filter": {
                "variants": variants_vec,
                "plugins": plugins_vec,
                "shelves": shelves_vec,
            },
        });
        if let Some(seq) = since {
            req["since"] = serde_json::json!(seq);
        }
        if !coalesce_labels_vec.is_empty() {
            let mut coalesce = serde_json::json!({
                "labels": coalesce_labels_vec,
            });
            if let Some(ms) = coalesce_window_ms {
                coalesce["window_ms"] = serde_json::json!(ms);
            }
            req["coalesce"] = coalesce;
        }
        write_frame(&mut stream, &req).await?;

        // Read the Subscribed ack.
        let ack = read_frame(&mut stream).await?;
        if let Some(err) = ack.get("error") {
            return Err(format_error("subscribe_happenings", err));
        }
        let current_seq =
            ack.get("current_seq").and_then(Value::as_u64).unwrap_or(0);
        println!("{{\"subscribed\":true,\"current_seq\":{current_seq}}}");

        // Stream events. Bounded by max_count + duration_secs.
        let deadline =
            std::time::Instant::now() + Duration::from_secs(duration_secs);
        let mut received: u64 = 0;
        while received < max_count {
            let now = std::time::Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline - now;
            match tokio::time::timeout(remaining, read_frame(&mut stream)).await
            {
                Ok(Ok(frame)) => {
                    if let Some(err) = frame.get("error") {
                        return Err(format_error(
                            "subscribe_happenings (stream)",
                            err,
                        ));
                    }
                    println!(
                        "{}",
                        serde_json::to_string(&frame).unwrap_or_else(|_| {
                            "{\"error\":\"failed-to-serialise-frame\"}"
                                .to_string()
                        })
                    );
                    received += 1;
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => break, // duration timeout
            }
        }
        eprintln!(
            "subscribe-happenings: exit after {received} events / {} elapsed",
            duration_secs
        );
        Ok(())
    })
}

fn parse_csv_filter(input: Option<&str>) -> Vec<String> {
    input
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Run the `warden fast-path-dispatch` subcommand. Connects to
/// the steward's Fast Path UDS socket, sends a length-prefixed
/// CBOR `FastPathRequest::Dispatch` frame, reads the response.
/// Exit code reflects the dispatch outcome: Ok on `Dispatched`,
/// Err on a structured `Error` response (verb-gate refusal,
/// budget exceeded, no active custody, etc.).
pub fn warden_fast_path_dispatch(
    socket: &Path,
    shelf: &str,
    verb: &str,
    handle_id: &str,
    handle_started_at_ms: u64,
    payload_b64: &str,
    deadline_ms: Option<u32>,
) -> Result<(), anyhow::Error> {
    use base64::engine::general_purpose::STANDARD as B64;
    let payload = if payload_b64.is_empty() {
        Vec::new()
    } else {
        B64.decode(payload_b64).context("decoding --payload-b64")?
    };
    let socket = socket.to_path_buf();
    let shelf = shelf.to_string();
    let verb = verb.to_string();
    let handle_id = handle_id.to_string();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let req = evo::fast_path::FastPathRequest::Dispatch {
            cid: 1,
            shelf,
            verb: verb.clone(),
            payload,
            handle_id: handle_id.clone(),
            handle_started_at_ms,
            deadline_ms,
        };
        let payload_bytes = evo_plugin_sdk::codec::encode_cbor_value(&req)
            .map_err(|e| anyhow::anyhow!("encoding request: {e}"))?;
        let mut stream =
            UnixStream::connect(&socket).await.with_context(|| {
                format!("connecting to fast-path socket {}", socket.display())
            })?;
        let len = (payload_bytes.len() as u32).to_be_bytes();
        stream.write_all(&len).await?;
        stream.write_all(&payload_bytes).await?;
        let mut len_bytes = [0u8; 4];
        stream.read_exact(&mut len_bytes).await?;
        let size = u32::from_be_bytes(len_bytes) as usize;
        if size > 64 * 1024 {
            return Err(anyhow::anyhow!(
                "fast-path response frame too large: {size} bytes"
            ));
        }
        let mut buf = vec![0u8; size];
        stream.read_exact(&mut buf).await?;
        let resp: evo::fast_path::FastPathResponse =
            evo_plugin_sdk::codec::decode_cbor_value(&buf)
                .map_err(|e| anyhow::anyhow!("decoding response: {e}"))?;
        match resp {
            evo::fast_path::FastPathResponse::Dispatched { cid } => {
                println!("fast-path dispatch accepted:");
                println!("  cid:     {cid}");
                println!("  verb:    {verb}");
                println!("  handle:  {handle_id}");
                Ok(())
            }
            evo::fast_path::FastPathResponse::Error {
                cid,
                class,
                subclass,
                message,
            } => Err(anyhow::anyhow!(
                "fast_path_dispatch refused: cid={cid} class={class} \
                 subclass={subclass} message={message}"
            )),
        }
    })
}

/// Run the bundled `warden course-correct` subcommand.
///
/// One-shot custody flow: `take_custody` to mint a handle, then
/// `course_correct` with the supplied verb + payload, then
/// `release_custody` to clean up. Exit code reflects the
/// course-correction's outcome — Ok on accepted, Err on the
/// framework's structured refusal (verb-gate, etc.).
/// Take + release are best-effort: failures are logged but do
/// not flip the exit code, since the test posture is "did the
/// course-correction succeed or refuse?"
pub fn warden_course_correct(
    socket: &Path,
    shelf: &str,
    custody_type: &str,
    custody_payload_b64: &str,
    verb: &str,
    payload_b64: &str,
) -> Result<(), anyhow::Error> {
    // Stage 1: take_custody. Refusal here is fatal — without a
    // handle we can't even attempt the correction.
    let take_req = serde_json::json!({
        "op": "take_custody",
        "shelf": shelf,
        "custody_type": custody_type,
        "payload_b64": custody_payload_b64,
    });
    let take_resp = call_with_caps(socket, &[], take_req)?;
    if let Some(err) = take_resp.get("error") {
        return Err(format_error("take_custody", err));
    }
    let handle = take_resp.get("handle").cloned().ok_or_else(|| {
        anyhow::anyhow!("take_custody: response missing `handle` field")
    })?;

    // Stage 2: course_correct. This is the test's primary
    // outcome.
    let cc_req = serde_json::json!({
        "op": "course_correct",
        "shelf": shelf,
        "handle": handle,
        "correction_type": verb,
        "payload_b64": payload_b64,
    });
    let cc_resp = call_with_caps(socket, &[], cc_req)?;
    let cc_outcome = if let Some(err) = cc_resp.get("error") {
        Err(format_error("course_correct", err))
    } else {
        let handle_id = cc_resp
            .get("handle_id")
            .and_then(Value::as_str)
            .unwrap_or("?");
        println!("course correction accepted:");
        println!("  shelf:     {shelf}");
        println!("  handle:    {handle_id}");
        println!("  verb:      {verb}");
        Ok(())
    };

    // Stage 3: release_custody. Best-effort.
    let rel_req = serde_json::json!({
        "op": "release_custody",
        "shelf": shelf,
        "handle": handle,
    });
    if let Ok(rel_resp) = call_with_caps(socket, &[], rel_req) {
        if let Some(err) = rel_resp.get("error") {
            eprintln!(
                "release_custody (best-effort): {}",
                err.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("(no message)")
            );
        }
    }

    cc_outcome
}

/// Run the `warden take-custody` subcommand. Sends `take_custody`
/// and prints the warden-minted handle to stdout in
/// shell-friendly `key=value` form so a script can capture and
/// pass `handle_id` / `handle_started_at_ms` to a follow-up
/// `fast-path-dispatch` or `release-custody` call.
pub fn warden_take_custody(
    socket: &Path,
    shelf: &str,
    custody_type: &str,
    custody_payload_b64: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "take_custody",
        "shelf": shelf,
        "custody_type": custody_type,
        "payload_b64": custody_payload_b64,
    });
    let resp = call_with_caps(socket, &[], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("take_custody", err));
    }
    let handle = resp.get("handle").ok_or_else(|| {
        anyhow::anyhow!("take_custody: response missing `handle` field")
    })?;
    let handle_id =
        handle.get("id").and_then(Value::as_str).ok_or_else(|| {
            anyhow::anyhow!("take_custody: handle missing `id` field")
        })?;
    // CustodyHandle.started_at serialises as a SystemTime serde
    // shape (`secs_since_epoch` + `nanos_since_epoch`). Convert
    // to wall-clock milliseconds so the operator script can pass
    // it verbatim to follow-up verbs that take ms.
    let secs = handle
        .get("started_at")
        .and_then(|s| s.get("secs_since_epoch"))
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "take_custody: handle.started_at missing secs_since_epoch"
            )
        })?;
    let nanos = handle
        .get("started_at")
        .and_then(|s| s.get("nanos_since_epoch"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let started_at_ms = secs.saturating_mul(1_000) + nanos / 1_000_000;
    println!("custody taken:");
    println!("  shelf={shelf}");
    println!("  handle_id={handle_id}");
    println!("  handle_started_at_ms={started_at_ms}");
    Ok(())
}

/// Run the `warden release-custody` subcommand. Counterpart to
/// `take-custody`; sends `release_custody` and prints a brief
/// confirmation. The bundled `course-correct` verb releases
/// automatically; this verb is for scripts driving the
/// held-handle flow (fast-path, multi-step).
pub fn warden_release_custody(
    socket: &Path,
    shelf: &str,
    handle_id: &str,
    handle_started_at_ms: u64,
) -> Result<(), anyhow::Error> {
    // Reconstruct the SystemTime serde shape the steward expects.
    let secs = handle_started_at_ms / 1_000;
    let nanos = (handle_started_at_ms % 1_000) * 1_000_000;
    let req = serde_json::json!({
        "op": "release_custody",
        "shelf": shelf,
        "handle": {
            "id": handle_id,
            "started_at": {
                "secs_since_epoch": secs,
                "nanos_since_epoch": nanos,
            },
        },
    });
    let resp = call_with_caps(socket, &[], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("release_custody", err));
    }
    println!("custody released:");
    println!("  shelf={shelf}");
    println!("  handle_id={handle_id}");
    Ok(())
}

/// Run the `reload-plugin` subcommand. Sends
/// `op = "reload_plugin"` and prints the steward's
/// `PluginReloaded` response.
pub fn reload_plugin(
    socket: &Path,
    plugin: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "reload_plugin",
            "plugin": plugin,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    print_plugin_reload(&resp)
}

/// Run the `plan fire <plan_id>` subcommand. Sends
/// `op = "fire_plan"` under the `plans_admin` capability and
/// prints the steward's `PlanFired` response. The fire is
/// asynchronous on the server side; this command returns once
/// the fire is scheduled, not when the plan completes.
pub fn fire_plan(socket: &Path, plan_id: &str) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "fire_plan",
        "plan_id": plan_id,
    });
    let resp = call_with_caps(socket, &["plans_admin"], req)?;
    print_plan_fired(&resp)
}

/// Run `admin grant-publisher-trust` — records an operator
/// trust grant against a publisher's signing-key fingerprint
/// and writes the public key into the operator trust
/// directory so admission picks it up.
pub fn grant_publisher_trust(
    socket: &Path,
    publisher_id: &str,
    display_name: &str,
    public_key_pem: &str,
    scope_per_plugin: Option<&Vec<String>>,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let mut req = serde_json::json!({
        "op": "grant_publisher_trust",
        "publisher_id": publisher_id,
        "display_name": display_name,
        "public_key_pem": public_key_pem,
    });
    if let Some(plugins) = scope_per_plugin {
        req["scope_per_plugin"] = serde_json::json!(plugins);
    }
    let req = with_step_up(req, step_up_token);
    let resp = call(socket, req)?;
    print_publisher_trust_granted(&resp)
}

/// Run `admin revoke-publisher-trust` — revokes a recorded
/// publisher trust grant.
pub fn revoke_publisher_trust(
    socket: &Path,
    publisher_id: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "revoke_publisher_trust",
            "publisher_id": publisher_id,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    print_publisher_trust_revoked(&resp)
}

/// Run `admin list-publisher-trust` — list recorded
/// publisher trust grants + revocations.
pub fn list_publisher_trust(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "list_publisher_trust"});
    let resp = call_with_caps(socket, &[], req)?;
    print_publisher_trust_list(&resp)
}

/// Run `admin register-registry` — registers a plugin
/// registry with the steward.
#[allow(clippy::too_many_arguments)]
pub fn register_plugin_registry(
    socket: &Path,
    slug: &str,
    manifest_url: &str,
    signature_url: &str,
    public_key_fingerprint: &str,
    poll_interval_secs: Option<u64>,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let mut req = serde_json::json!({
        "op": "register_plugin_registry",
        "slug": slug,
        "manifest_url": manifest_url,
        "signature_url": signature_url,
        "public_key_fingerprint": public_key_fingerprint,
    });
    if let Some(secs) = poll_interval_secs {
        req["poll_interval_secs"] = serde_json::json!(secs);
    }
    let req = with_step_up(req, step_up_token);
    let resp = call(socket, req)?;
    print_plugin_registry_registered(&resp)
}

/// Run `admin unregister-registry` — forgets a registered
/// registry.
pub fn unregister_plugin_registry(
    socket: &Path,
    slug: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "unregister_plugin_registry",
            "slug": slug,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    print_plugin_registry_unregistered(&resp)
}

/// Run `admin list-registries` — prints the registered
/// registries plus their cached-manifest counts.
pub fn list_plugin_registries(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "list_plugin_registries"});
    let resp = call_with_caps(socket, &[], req)?;
    print_plugin_registries(&resp)
}

/// Run `admin refresh-registry` — bypasses the polling
/// cadence and refreshes one registry now.
pub fn refresh_plugin_registry(
    socket: &Path,
    slug: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "refresh_plugin_registry",
            "slug": slug,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    print_plugin_registry_refreshed(&resp)
}

/// Run the `admin install-from-url` subcommand. Dispatches the
/// `install_plugin_from_url` wire op under the `plugins_admin`
/// capability. The framework fetches the URL over HTTPS,
/// drops the bundle into its stage directory, and the stage
/// watcher admits on the next polling tick. The fire-and-
/// observe pattern: this command returns once the bundle is
/// staged (not when admission completes); operators tracking
/// admission outcomes subscribe to the happenings bus via
/// `evo-plugin-tool admin subscribe-happenings`.
pub fn install_plugin_from_url(
    socket: &Path,
    url: &str,
    signature_pin: Option<&str>,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let mut req = serde_json::json!({
        "op": "install_plugin_from_url",
        "url": url,
    });
    if let Some(pin) = signature_pin {
        req["signature_pin"] = serde_json::json!(pin);
    }
    let req = with_step_up(req, step_up_token);
    let resp = call(socket, req)?;
    print_plugin_install_staged(&resp)
}

/// Run the `health` subcommand. Read-only aggregate snapshot
/// of plugin counts + health/resource detail.
pub fn health_get(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "get_plugin_health"});
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("get_plugin_health", err));
    }
    let snap = resp
        .get("snapshot")
        .ok_or_else(|| anyhow::anyhow!("response missing snapshot"))?;
    let pretty = serde_json::to_string_pretty(snap)
        .unwrap_or_else(|_| "<unrenderable>".into());
    println!("plugin health snapshot:\n{pretty}");
    Ok(())
}

/// Run the `dependents <plugin>` subcommand. Read-only
/// preview of the transitive dependent set; useful before
/// `disable --cascade-dependents`.
pub fn preview_dependents(
    socket: &Path,
    plugin: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "preview_dependency_cascade",
        "plugin_name": plugin,
    });
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("preview_dependency_cascade", err));
    }
    let dependents = resp
        .get("dependents")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if dependents.is_empty() {
        println!("plugin {plugin}: no admitted dependents");
    } else {
        println!(
            "plugin {plugin}: {} dependent(s) (BFS order, outermost first):",
            dependents.len()
        );
        for d in dependents {
            if let Some(s) = d.as_str() {
                println!("  {s}");
            }
        }
    }
    Ok(())
}

/// Run the `policy list` subcommand. Read-only.
pub fn policy_list(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "list_admission_policies"});
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_admission_policies", err));
    }
    let entries = resp
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        println!("admission policies: (none recorded)");
        return Ok(());
    }
    println!("admission policies:");
    for e in entries {
        let id = e.get("policy_id").and_then(|v| v.as_str()).unwrap_or("");
        let name = e.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let active = e.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
        let author =
            e.get("authored_by").and_then(|v| v.as_str()).unwrap_or("");
        let marker = if active { " [active]" } else { "" };
        println!("  {id}{marker} ({author}) — {name}");
    }
    Ok(())
}

/// Run the `policy get <id>` subcommand. Read-only.
pub fn policy_get(socket: &Path, policy_id: &str) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "get_admission_policy",
        "policy_id": policy_id,
    });
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("get_admission_policy", err));
    }
    let Some(policy) = resp.get("policy") else {
        println!("admission policy {policy_id:?}: not found");
        return Ok(());
    };
    let pretty = serde_json::to_string_pretty(policy)
        .unwrap_or_else(|_| "<unrenderable>".into());
    println!("admission policy:\n{pretty}");
    Ok(())
}

/// Run the `policy put` subcommand.
pub fn policy_put(
    socket: &Path,
    path: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let body = if path == "-" {
        use std::io::Read;
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .context("reading policy document from stdin")?;
        s
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("reading {path}"))?
    };
    let parsed: toml::Value =
        toml::from_str(&body).with_context(|| "parsing policy TOML body")?;
    let table = parsed
        .as_table()
        .ok_or_else(|| anyhow::anyhow!("policy TOML must be a table"))?;
    let policy_id = table
        .get("policy_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("policy TOML missing policy_id"))?;
    let name = table
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("policy TOML missing name"))?;
    let description = table
        .get("description")
        .and_then(|v| v.as_str())
        .map(String::from);
    let authored_by = table
        .get("authored_by")
        .and_then(|v| v.as_str())
        .unwrap_or("user");
    let rules = table
        .get("rules")
        .ok_or_else(|| anyhow::anyhow!("policy TOML missing [rules]"))?;
    let rules_json: serde_json::Value = serde_json::to_value(rules)
        .with_context(|| "encoding [rules] section to JSON")?;

    let mut req = serde_json::json!({
        "op": "put_admission_policy",
        "policy_id": policy_id,
        "name": name,
        "authored_by": authored_by,
        "rules": rules_json,
    });
    if let Some(d) = description {
        req["description"] = serde_json::json!(d);
    }
    let req = with_step_up(req, step_up_token);
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("put_admission_policy", err));
    }
    let id = resp.get("policy_id").and_then(|v| v.as_str()).unwrap_or("");
    println!("admission policy put: {id}");
    Ok(())
}

/// Run the `policy delete` subcommand.
pub fn policy_delete(
    socket: &Path,
    policy_id: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "delete_admission_policy",
            "policy_id": policy_id,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("delete_admission_policy", err));
    }
    println!("admission policy delete: {policy_id}");
    Ok(())
}

/// Run the `policy activate` subcommand.
pub fn policy_activate(
    socket: &Path,
    policy_id: Option<&str>,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let mut req = serde_json::json!({
        "op": "set_active_admission_policy",
    });
    if let Some(id) = policy_id {
        req["policy_id"] = serde_json::json!(id);
    }
    let req = with_step_up(req, step_up_token);
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("set_active_admission_policy", err));
    }
    let id = resp.get("policy_id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        println!("active admission policy cleared (none active)");
    } else {
        println!("active admission policy: {id}");
    }
    Ok(())
}

/// Run the `policy audit <id>` subcommand.
pub fn policy_audit(
    socket: &Path,
    policy_id: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "audit_against_policy",
        "policy_id": policy_id,
    });
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("audit_against_policy", err));
    }
    let violations = resp
        .get("violations")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if violations.is_empty() {
        println!("admission policy audit ({policy_id}): no violations");
    } else {
        println!(
            "admission policy audit ({policy_id}): {} violation(s)",
            violations.len()
        );
        for v in violations {
            let plugin =
                v.get("plugin_name").and_then(|x| x.as_str()).unwrap_or("");
            let class =
                v.get("rule_class").and_then(|x| x.as_str()).unwrap_or("");
            let detail = v.get("detail").and_then(|x| x.as_str()).unwrap_or("");
            println!("  {plugin}\t{class}\t{detail}");
        }
    }
    Ok(())
}

/// Run the `capability revoke <plugin> <capability>` subcommand.
/// Records an operator-issued revocation of one capability on
/// one plugin. Idempotent on the `(plugin, capability)` pair.
pub fn capability_revoke(
    socket: &Path,
    plugin_name: &str,
    capability: &str,
    reason: Option<&str>,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let mut req = serde_json::json!({
        "op": "revoke_plugin_capability",
        "plugin_name": plugin_name,
        "capability": capability,
    });
    if let Some(r) = reason {
        req["reason"] = serde_json::json!(r);
    }
    let req = with_step_up(req, step_up_token);
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("revoke_plugin_capability", err));
    }
    let p = resp
        .get("plugin_name")
        .and_then(|v| v.as_str())
        .unwrap_or(plugin_name);
    let c = resp
        .get("capability")
        .and_then(|v| v.as_str())
        .unwrap_or(capability);
    println!("capability revoked: {p}/{c}");
    Ok(())
}

/// Run the `capability unrevoke <plugin> <capability>`
/// subcommand. Removes a previously-recorded revocation.
/// Idempotent on absent pairs.
pub fn capability_unrevoke(
    socket: &Path,
    plugin_name: &str,
    capability: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "unrevoke_plugin_capability",
            "plugin_name": plugin_name,
            "capability": capability,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("unrevoke_plugin_capability", err));
    }
    let p = resp
        .get("plugin_name")
        .and_then(|v| v.as_str())
        .unwrap_or(plugin_name);
    let c = resp
        .get("capability")
        .and_then(|v| v.as_str())
        .unwrap_or(capability);
    println!("capability un-revoked: {p}/{c}");
    Ok(())
}

/// Run the `capability list <plugin>` subcommand. Read-only.
pub fn capability_list_for_plugin(
    socket: &Path,
    plugin_name: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "list_plugin_capability_revocations",
        "plugin_name": plugin_name,
    });
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_plugin_capability_revocations", err));
    }
    print_capability_revocations(&resp, Some(plugin_name))
}

/// Run the `capability list-all` subcommand. Read-only.
pub fn capability_list_all(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "list_all_capability_revocations"});
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_all_capability_revocations", err));
    }
    print_capability_revocations(&resp, None)
}

/// Render the `entries` array carried on the
/// `plugin_capability_revocations` response.
fn print_capability_revocations(
    resp: &serde_json::Value,
    plugin_filter: Option<&str>,
) -> Result<(), anyhow::Error> {
    let entries = resp
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        match plugin_filter {
            Some(p) => {
                println!("capability revocations for {p}: (none recorded)")
            }
            None => println!("capability revocations: (none recorded)"),
        }
        return Ok(());
    }
    match plugin_filter {
        Some(p) => {
            println!("capability revocations for {p} ({n}):", n = entries.len())
        }
        None => println!("capability revocations ({n}):", n = entries.len()),
    }
    for e in entries {
        let plugin =
            e.get("plugin_name").and_then(|v| v.as_str()).unwrap_or("");
        let cap = e.get("capability").and_then(|v| v.as_str()).unwrap_or("");
        let by = e
            .get("revoked_by_principal")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let at_ms =
            e.get("revoked_at_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        let reason = e.get("reason").and_then(|v| v.as_str()).unwrap_or("");
        let reason_suffix = if reason.is_empty() {
            String::new()
        } else {
            format!(" — {reason}")
        };
        println!("  {plugin}\t{cap}\t{by}\t{at_ms}{reason_suffix}");
    }
    Ok(())
}

/// Run the `migration export` subcommand. Reads the
/// configuration bundle from the running steward and writes it to
/// the supplied path (or stdout when `path == "-"`).
pub fn migration_export(
    socket: &Path,
    path: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "export_migration_bundle"});
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("export_migration_bundle", err));
    }
    let bundle_toml = resp
        .get("bundle_toml")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "export_migration_bundle: response missing bundle_toml"
            )
        })?;
    if path == "-" {
        print!("{bundle_toml}");
    } else {
        std::fs::write(path, bundle_toml)
            .with_context(|| format!("writing migration bundle to {path}"))?;
        println!(
            "migration bundle exported to {path} ({bytes} bytes)",
            bytes = bundle_toml.len()
        );
    }
    Ok(())
}

/// Run the `migration import` subcommand. Reads the
/// configuration bundle from the supplied path (`-` for stdin),
/// dispatches `import_migration_bundle` against the running
/// steward, and prints the per-section apply summary on success.
pub fn migration_import(
    socket: &Path,
    path: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let bundle_toml = if path == "-" {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .context("reading migration bundle from stdin")?;
        buf
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("reading migration bundle from {path}"))?
    };
    let req = with_step_up(
        serde_json::json!({
            "op": "import_migration_bundle",
            "bundle_toml": bundle_toml,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("import_migration_bundle", err));
    }
    let update_channels = resp
        .get("update_channels_applied")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let plugin_tags = resp
        .get("plugin_tags_applied")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let plugin_profiles = resp
        .get("plugin_profiles_applied")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let active_profile = resp
        .get("active_profile_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let admission_policies = resp
        .get("admission_policies_applied")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let active_policy = resp
        .get("active_policy_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let capability_revocations = resp
        .get("capability_revocations_applied")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    println!("migration bundle imported:");
    println!("  update_channels:           {update_channels}");
    println!("  plugin_tags:               {plugin_tags}");
    println!("  plugin_profiles:           {plugin_profiles}");
    println!(
        "  active_profile_id:         {}",
        active_profile.as_deref().unwrap_or("(none)")
    );
    println!("  admission_policies:        {admission_policies}");
    println!(
        "  active_admission_policy:   {}",
        active_policy.as_deref().unwrap_or("(none)")
    );
    println!("  capability_revocations:    {capability_revocations}");
    Ok(())
}

/// Run the `audio profile-override put --from-toml <path>`
/// subcommand. The TOML document carries the typed
/// `HardwareIdentity` + `HardwareProfileOverride` shapes the
/// framework consumes — pass `-` to read from stdin.
pub fn audio_profile_override_put(
    socket: &Path,
    path: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let toml_str = if path == "-" {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .context("reading hardware profile override TOML from stdin")?;
        buf
    } else {
        std::fs::read_to_string(path).with_context(|| {
            format!("reading hardware profile override TOML from {path}")
        })?
    };
    let parsed: toml::Value =
        toml::from_str(&toml_str).context("parsing override TOML")?;
    let identity = parsed.get("identity").ok_or_else(|| {
        anyhow::anyhow!(
            "override TOML missing [identity] table; see \
             `audio profile-override put --help` for the expected shape"
        )
    })?;
    let override_ = parsed.get("override").ok_or_else(|| {
        anyhow::anyhow!(
            "override TOML missing [override] table; see \
             `audio profile-override put --help` for the expected shape"
        )
    })?;
    let identity_json: serde_json::Value =
        serde_json::to_value(identity).context("serialising identity")?;
    let override_json: serde_json::Value =
        serde_json::to_value(override_).context("serialising override")?;
    let req = with_step_up(
        serde_json::json!({
            "op": "put_hardware_profile_override",
            "identity": identity_json,
            "override": override_json,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("put_hardware_profile_override", err));
    }
    let key = resp.get("key").and_then(|v| v.as_str()).unwrap_or("");
    println!("hardware profile override recorded: {key}");
    Ok(())
}

/// Run the `audio profile-override get <key>` subcommand.
pub fn audio_profile_override_get(
    socket: &Path,
    key: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "get_hardware_profile_override",
        "key": key,
    });
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("get_hardware_profile_override", err));
    }
    print_hardware_profile_overrides(&resp, Some(key))
}

/// Run the `audio profile-override list` subcommand.
pub fn audio_profile_override_list(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "list_hardware_profile_overrides"});
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_hardware_profile_overrides", err));
    }
    print_hardware_profile_overrides(&resp, None)
}

/// Run the `audio profile-override clear <key>` subcommand.
pub fn audio_profile_override_clear(
    socket: &Path,
    key: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "delete_hardware_profile_override",
            "key": key,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("delete_hardware_profile_override", err));
    }
    let key = resp.get("key").and_then(|v| v.as_str()).unwrap_or(key);
    println!("hardware profile override cleared: {key}");
    Ok(())
}

/// Run the `audio policy set <target_key>` subcommand.
/// `policy_spec` is the JSON shape of `OperatorPolicy`:
/// `{"kind":"auto"}`, `{"kind":"strict_bit_perfect"}`, or
/// `{"kind":"pinned", "source_plugin":"...", "delivery_plugin":"...",
/// "composition_plugin":"..."}`.
pub fn audio_policy_set(
    socket: &Path,
    target_key: &str,
    policy_spec: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let policy: serde_json::Value = serde_json::from_str(policy_spec).context(
        "parsing --policy-json (expected an OperatorPolicy JSON, e.g. \
             '{\"kind\":\"strict_bit_perfect\"}')",
    )?;
    let req = with_step_up(
        serde_json::json!({
            "op": "put_audio_operator_policy",
            "target_key": target_key,
            "policy": policy,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("put_audio_operator_policy", err));
    }
    let key = resp
        .get("target_key")
        .and_then(|v| v.as_str())
        .unwrap_or(target_key);
    println!("audio operator policy recorded: {key}");
    Ok(())
}

/// Run the `audio policy get <target_key>` subcommand.
pub fn audio_policy_get(
    socket: &Path,
    target_key: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "get_audio_operator_policy",
        "target_key": target_key,
    });
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("get_audio_operator_policy", err));
    }
    print_audio_operator_policies(&resp, Some(target_key))
}

/// Run the `audio policy list` subcommand.
pub fn audio_policy_list(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "list_audio_operator_policies"});
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_audio_operator_policies", err));
    }
    print_audio_operator_policies(&resp, None)
}

/// Run the `audio policy clear <target_key>` subcommand.
pub fn audio_policy_clear(
    socket: &Path,
    target_key: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "delete_audio_operator_policy",
            "target_key": target_key,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("delete_audio_operator_policy", err));
    }
    let key = resp
        .get("target_key")
        .and_then(|v| v.as_str())
        .unwrap_or(target_key);
    println!("audio operator policy cleared: {key}");
    Ok(())
}

/// Run the `audio volume-mode set <target_key> <mode>` subcommand.
/// `mode` is one of `software` / `hardware` / `none`.
pub fn audio_volume_mode_set(
    socket: &Path,
    target_key: &str,
    mode: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    if !matches!(mode, "software" | "hardware" | "none") {
        return Err(anyhow::anyhow!(
            "volume mode must be one of: software, hardware, none (got {mode:?})"
        ));
    }
    let req = with_step_up(
        serde_json::json!({
            "op": "put_audio_volume_mode",
            "target_key": target_key,
            "volume_mode": mode,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("put_audio_volume_mode", err));
    }
    let key = resp
        .get("target_key")
        .and_then(|v| v.as_str())
        .unwrap_or(target_key);
    println!("audio volume mode recorded: {key} = {mode}");
    Ok(())
}

/// Run the `audio volume-mode get <target_key>` subcommand.
pub fn audio_volume_mode_get(
    socket: &Path,
    target_key: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "get_audio_volume_mode",
        "target_key": target_key,
    });
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("get_audio_volume_mode", err));
    }
    print_audio_volume_modes(&resp, Some(target_key))
}

/// Run the `audio volume-mode list` subcommand.
pub fn audio_volume_mode_list(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "list_audio_volume_modes"});
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_audio_volume_modes", err));
    }
    print_audio_volume_modes(&resp, None)
}

/// Run the `audio volume-mode clear <target_key>` subcommand.
pub fn audio_volume_mode_clear(
    socket: &Path,
    target_key: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "delete_audio_volume_mode",
            "target_key": target_key,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("delete_audio_volume_mode", err));
    }
    let key = resp
        .get("target_key")
        .and_then(|v| v.as_str())
        .unwrap_or(target_key);
    println!("audio volume mode cleared: {key}");
    Ok(())
}

/// Run the `audio topology publish --from-toml <path>`
/// subcommand. The TOML document carries the typed
/// `ActiveAudioTopology` shape the framework consumes —
/// `target_key` + `display_name` + `[[chain]]` array +
/// `volume_mode` + `bit_perfect` + `score` table + warnings.
/// Pass `-` to read from stdin.
pub fn audio_topology_publish(
    socket: &Path,
    path: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let toml_str = if path == "-" {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .context("reading active audio topology TOML from stdin")?;
        buf
    } else {
        std::fs::read_to_string(path).with_context(|| {
            format!("reading active audio topology TOML from {path}")
        })?
    };
    let parsed: toml::Value =
        toml::from_str(&toml_str).context("parsing topology TOML")?;
    let topology_json: serde_json::Value =
        serde_json::to_value(&parsed).context("serialising topology")?;
    let req = with_step_up(
        serde_json::json!({
            "op": "publish_active_audio_topology",
            "topology": topology_json,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("publish_active_audio_topology", err));
    }
    let target_key = resp
        .get("target_key")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let bit_perfect = resp
        .get("bit_perfect")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let score_total = resp
        .get("score_total")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    println!(
        "active audio topology published: {target_key} \
         (bit_perfect={bit_perfect}, score={score_total})"
    );
    Ok(())
}

/// Run the `audio topology get <target_key>` subcommand.
pub fn audio_topology_get(
    socket: &Path,
    target_key: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "get_active_audio_topology",
        "target_key": target_key,
    });
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("get_active_audio_topology", err));
    }
    print_active_audio_topologies(&resp, Some(target_key))
}

/// Run the `audio topology list` subcommand.
pub fn audio_topology_list(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "list_active_audio_topologies"});
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_active_audio_topologies", err));
    }
    print_active_audio_topologies(&resp, None)
}

/// Run the `audio topology clear <target_key>` subcommand.
pub fn audio_topology_clear(
    socket: &Path,
    target_key: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "clear_active_audio_topology",
            "target_key": target_key,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("clear_active_audio_topology", err));
    }
    let key = resp
        .get("target_key")
        .and_then(|v| v.as_str())
        .unwrap_or(target_key);
    println!("active audio topology cleared: {key}");
    Ok(())
}

/// Run the `audio topology show [target_key]` subcommand.
/// Renders a human-readable signal-path breakdown — chain
/// stages with format + endpoint per stage, volume + bit-
/// perfect verdict + score breakdown + warnings — distinct
/// from `get` / `list` which dump the raw JSON form. Used by
/// audiophile operators inspecting their device's chain at a
/// glance. When `target_key` is `None`, renders every
/// published topology.
pub fn audio_topology_show(
    socket: &Path,
    target_key: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = match target_key {
        Some(k) => serde_json::json!({
            "op": "get_active_audio_topology",
            "target_key": k,
        }),
        None => serde_json::json!({
            "op": "list_active_audio_topologies",
        }),
    };
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        let op = match target_key {
            Some(_) => "get_active_audio_topology",
            None => "list_active_audio_topologies",
        };
        return Err(format_error(op, err));
    }
    let entries = resp
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        match target_key {
            Some(k) => {
                println!("active audio topology {k}: (not published)")
            }
            None => println!("active audio topologies: (none published)"),
        }
        return Ok(());
    }
    for (idx, e) in entries.iter().enumerate() {
        if idx > 0 {
            println!();
        }
        render_topology(e);
    }
    Ok(())
}

/// Render one topology in the operator-readable form.
fn render_topology(t: &serde_json::Value) {
    let target_key = t.get("target_key").and_then(|v| v.as_str()).unwrap_or("");
    let display_name =
        t.get("display_name").and_then(|v| v.as_str()).unwrap_or("");
    let bit_perfect = t
        .get("bit_perfect")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let score_total = t
        .get("score")
        .and_then(|s| s.get("total"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let volume_mode = t
        .get("volume_mode")
        .and_then(|v| v.as_str())
        .unwrap_or("software");
    let volume_position = t.get("volume_position").and_then(|v| v.as_f64());
    let volume_db = t.get("volume_db").and_then(|v| v.as_f64());

    println!("Active topology: {target_key}");
    println!("Display:         {display_name}");
    println!(
        "Bit-perfect:     {}",
        if bit_perfect { "yes" } else { "no" }
    );
    println!("Score:           {score_total} / 100");
    println!();

    if let Some(chain) = t.get("chain").and_then(|v| v.as_array()) {
        println!("Chain:");
        for (idx, stage) in chain.iter().enumerate() {
            if idx > 0 {
                println!("       |");
                println!("       v");
            }
            render_chain_stage(stage);
        }
        println!();
    }

    let volume_str = match (volume_position, volume_db) {
        (Some(p), Some(db)) => format!(
            "{volume_mode} @ {pct}% ({db:.1} dB)",
            pct = (p * 100.0).round() as i64
        ),
        (Some(p), None) => {
            format!("{volume_mode} @ {pct}%", pct = (p * 100.0).round() as i64)
        }
        _ => volume_mode.to_string(),
    };
    println!("Volume:          {volume_str}");

    if let Some(score) = t.get("score") {
        println!();
        println!("Score breakdown:");
        let lines: &[(&str, &str)] = &[
            ("bit_perfect", "Bit-perfect"),
            ("native_rate_match", "Native rate match"),
            ("native_format_match", "Native format match"),
            ("minimum_signal_path", "Minimum signal path"),
            ("hardware_volume_engaged", "Hardware volume engaged"),
            ("implicit_resampler_penalty", "Implicit resampler penalty"),
            (
                "software_volume_when_hardware_available_penalty",
                "Software vol w/ hardware-avail penalty",
            ),
            ("dsd_to_pcm_penalty", "DSD-to-PCM penalty"),
        ];
        for (k, label) in lines {
            let v = score.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
            if v != 0 {
                println!("  {label:<40} {v:+4}");
            }
        }
        println!(
            "  {label:<40} {total:>4}",
            label = "Total",
            total = score_total
        );
    }

    println!();
    let implicit = t
        .get("implicit_conversions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if implicit.is_empty() {
        println!("Implicit conversions: (none)");
    } else {
        println!("Implicit conversions:");
        for c in implicit {
            if let Some(s) = c.as_str() {
                println!("  - {s}");
            }
        }
    }
    let warnings = t
        .get("warnings")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if warnings.is_empty() {
        println!("Warnings:             (none)");
    } else {
        println!("Warnings:");
        for w in warnings {
            if let Some(s) = w.as_str() {
                println!("  - {s}");
            }
        }
    }
}

/// Render one chain stage. Source and Delivery are flat;
/// Composition carries input + output formats + endpoints.
fn render_chain_stage(stage: &serde_json::Value) {
    let stage_kind = stage.get("stage").and_then(|v| v.as_str()).unwrap_or("?");
    let plugin = stage.get("plugin").and_then(|v| v.as_str()).unwrap_or("?");
    let label = format!("{stage_kind:<11}");
    println!("  {label} {plugin}");
    match stage_kind {
        "source" | "delivery" => {
            let format_pretty = stage
                .get("format")
                .map(format_audio_format)
                .unwrap_or_default();
            let kind = stage
                .get("endpoint_kind")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let path = stage
                .get("endpoint_path")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            println!("              format:   {format_pretty}");
            println!("              endpoint: {kind} @ {path}");
        }
        "composition" => {
            let mode =
                stage.get("mode").and_then(|v| v.as_str()).unwrap_or("?");
            let in_fmt = stage
                .get("format_in")
                .map(format_audio_format)
                .unwrap_or_default();
            let out_fmt = stage
                .get("format_out")
                .map(format_audio_format)
                .unwrap_or_default();
            let in_kind = stage
                .get("endpoint_in_kind")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let in_path = stage
                .get("endpoint_in_path")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let out_kind = stage
                .get("endpoint_out_kind")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let out_path = stage
                .get("endpoint_out_path")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            println!("              mode:     {mode}");
            println!("              in:       {in_fmt}");
            println!("                        {in_kind} @ {in_path}");
            println!("              out:      {out_fmt}");
            println!("                        {out_kind} @ {out_path}");
        }
        other => {
            println!("              (unrecognised stage kind {other:?})");
        }
    }
}

/// Render an [`AudioFormat`] JSON value as a compact human
/// label — `PCM 24/192 (pcm_s24_le, 192000 Hz, 2 ch)` for PCM,
/// `DSD64 NativeUsb 2 ch` for DSD, `Encoded ac3 48000 Hz 6 ch`
/// for encoded passthrough.
///
/// [`AudioFormat`]: evo_plugin_sdk::audio::AudioFormat
fn format_audio_format(fmt: &serde_json::Value) -> String {
    let kind = fmt.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
    match kind {
        "pcm" => {
            let codec =
                fmt.get("codec").and_then(|v| v.as_str()).unwrap_or("?");
            let rate = fmt.get("rate_hz").and_then(|v| v.as_u64()).unwrap_or(0);
            let channels =
                fmt.get("channels").and_then(|v| v.as_u64()).unwrap_or(0);
            // Bit-depth label inferred from codec token.
            let bit_depth = match codec {
                "pcm_s16_le" => "16",
                "pcm_s24_le" => "24",
                "pcm_s32_le" => "32",
                "pcm_f32" => "32f",
                _ => "?",
            };
            let rate_short = (rate / 1000) as i64;
            format!(
                "PCM {bit_depth}/{rate_short} \
                 ({codec}, {rate} Hz, {channels} ch)"
            )
        }
        "dsd" => {
            let rate = fmt.get("rate").and_then(|v| v.as_str()).unwrap_or("?");
            let transport =
                fmt.get("transport").and_then(|v| v.as_str()).unwrap_or("?");
            let channels =
                fmt.get("channels").and_then(|v| v.as_u64()).unwrap_or(0);
            format!("{rate} ({transport}, {channels} ch)")
        }
        "encoded_passthrough" => {
            let codec =
                fmt.get("codec").and_then(|v| v.as_str()).unwrap_or("?");
            let rate = fmt.get("rate_hz").and_then(|v| v.as_u64()).unwrap_or(0);
            let channels =
                fmt.get("channels").and_then(|v| v.as_u64()).unwrap_or(0);
            format!("Encoded {codec} ({rate} Hz, {channels} ch)")
        }
        other => format!("<unknown format kind {other:?}>"),
    }
}

/// Run the `device identity show` subcommand. Reads the
/// singleton device identity (canonical id + display name +
/// optional vendor + creation time + public-key fingerprint)
/// and renders it in operator-readable form.
pub fn device_identity_show(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "get_device_identity"});
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("get_device_identity", err));
    }
    print_device_identity(&resp);
    Ok(())
}

/// Run the `device identity rename <new-name>` subcommand.
/// Updates the operator-editable display name only — the
/// canonical id is immutable.
pub fn device_identity_rename(
    socket: &Path,
    new_name: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "set_device_display_name",
            "display_name": new_name,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("set_device_display_name", err));
    }
    print_device_identity(&resp);
    Ok(())
}

/// Run the `device identity reset` subcommand. Re-seeds the
/// display name from the OS hostname (or `evo-<short>` fallback)
/// and resets `name_source` to `Auto` so the collision resolver
/// regains agency.
pub fn device_identity_reset(
    socket: &Path,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({"op": "reset_device_display_name"}),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("reset_device_display_name", err));
    }
    print_device_identity(&resp);
    Ok(())
}

/// Run the `device peers list` subcommand. Reads the live
/// multi-room peer set populated by the mDNS-SD discovery
/// runtime and renders one row per peer (canonical id +
/// display + addresses + capabilities + ages).
pub fn device_peers_list(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "list_discovered_peers"});
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_discovered_peers", err));
    }
    print_discovered_peers(&resp);
    Ok(())
}

fn print_discovered_peers(resp: &serde_json::Value) {
    let entries = resp
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        println!("multi-room peers: (none observed)");
        return;
    }
    println!("multi-room peers ({n}):", n = entries.len());
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    for e in &entries {
        let device_id =
            e.get("device_id").and_then(|v| v.as_str()).unwrap_or("?");
        let display_name = e
            .get("display_name")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let vendor = e.get("vendor_id").and_then(|v| v.as_str());
        let version = e.get("framework_version").and_then(|v| v.as_str());
        let last_seen_ms =
            e.get("last_seen_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        let first_seen_ms =
            e.get("first_seen_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        let age_seen_s = now_ms.saturating_sub(last_seen_ms) / 1000;
        let lifetime_s = now_ms.saturating_sub(first_seen_ms) / 1000;
        let addresses: Vec<String> = e
            .get("addresses")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|a| a.as_str().map(str::to_string))
            .collect();
        let caps: Vec<String> = e
            .get("capability_flags")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|c| c.as_str().map(str::to_string))
            .collect();
        let pkfp_present = e
            .get("public_key_fingerprint")
            .map(|v| !v.is_null())
            .unwrap_or(false);
        let public_key_b64 = e
            .get("public_key_b64")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        println!();
        println!("  {display_name}");
        println!("    Canonical id:  {device_id}");
        println!("    Addresses:     {}", addresses.join(", "));
        if let Some(v) = vendor {
            println!("    Vendor:        {v}");
        }
        if let Some(v) = version {
            println!("    Version:       {v}");
        }
        if !caps.is_empty() {
            println!("    Capabilities:  {}", caps.join(", "));
        }
        // Public-key visibility: the full 32-byte verifying key
        // (44-char base64) when the chain-announce freshness
        // pump has observed the peer; otherwise fall through to
        // the legacy mDNS-SD fingerprint indicator.
        match (public_key_b64.as_ref(), pkfp_present) {
            (Some(pk), _) => {
                println!("    Public key:    {pk}");
            }
            (None, true) => {
                println!("    Public key:    fingerprint present");
            }
            (None, false) => {
                println!("    Public key:    <not yet observed>");
            }
        }
        println!(
            "    Seen:          last {age_seen_s}s ago (first seen {lifetime_s}s ago)"
        );

        // Track 4K projection: presence + network are joined at
        // read time on the steward side (see
        // `handle_list_discovered_peers`) and arrive on every
        // row when the chain-scope correlator + witness
        // substrate are running on the local seat. Absent
        // means the substrate has no signal yet — discovered
        // via mDNS-SD but never chain-admitted (presence) or
        // no chain-recorded endpoint observation (network).
        let presence_state = e.get("presence_state").and_then(|v| v.as_str());
        let last_transition_at_ms =
            e.get("last_transition_at_ms").and_then(|v| v.as_u64());
        let network = e.get("network").and_then(|v| v.as_str());
        if let Some(state) = presence_state {
            if let Some(transition_ms) = last_transition_at_ms {
                let in_state_s = now_ms.saturating_sub(transition_ms) / 1000;
                println!(
                    "    Presence:      {state} (in this state for \
                     {in_state_s}s)"
                );
            } else {
                println!("    Presence:      {state}");
            }
        }
        if let Some(n) = network {
            println!("    Network:       {n}");
        }
    }
}

fn print_device_identity(resp: &serde_json::Value) {
    let identity = match resp.get("identity") {
        Some(v) => v,
        None => {
            println!("device identity: <missing identity field in response>");
            return;
        }
    };
    let device_id = identity
        .get("device_id")
        .and_then(|v| v.as_str())
        .unwrap_or("<unknown>");
    let display_name = identity
        .get("display_name")
        .and_then(|v| v.as_str())
        .unwrap_or("<unknown>");
    let vendor = identity
        .get("vendor_id")
        .and_then(|v| v.as_str())
        .unwrap_or("<none>");
    let created_at_ms = identity
        .get("created_at_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let has_public_key = identity
        .get("public_key_bytes")
        .map(|v| !v.is_null())
        .unwrap_or(false);

    println!("Device identity");
    println!("  Canonical id:  {device_id}");
    println!("  Display name:  {display_name}");
    println!("  Vendor:        {vendor}");
    println!("  Created (ms):  {created_at_ms}");
    println!(
        "  Public key:    {}",
        if has_public_key {
            "present (vendor-supplied)"
        } else {
            "<none>"
        }
    );
}

/// Run the `group create <display-name> <device-id>...` subcommand.
pub fn group_create(
    socket: &Path,
    display_name: &str,
    member_ids: &[String],
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "create_group",
            "display_name": display_name,
            "members": member_ids,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("create_group", err));
    }
    print_group(&resp);
    Ok(())
}

/// Run the `group list` subcommand.
pub fn group_list(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "list_groups"});
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_groups", err));
    }
    let entries = resp
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        println!("multi-room groups: (none recorded)");
        return Ok(());
    }
    println!("multi-room groups ({n}):", n = entries.len());
    for e in &entries {
        println!();
        print_group_record(e);
    }
    Ok(())
}

/// Run the `group show <group-id>` subcommand.
pub fn group_show(socket: &Path, group_id: &str) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "get_group",
        "group_id": group_id,
    });
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("get_group", err));
    }
    print_group(&resp);
    Ok(())
}

/// Run the `group rename <group-id> <new-name>` subcommand.
pub fn group_rename(
    socket: &Path,
    group_id: &str,
    new_name: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "rename_group",
            "group_id": group_id,
            "display_name": new_name,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("rename_group", err));
    }
    print_group(&resp);
    Ok(())
}

/// Run the `group set-leader-ms <group-id> <ms>` subcommand.
/// Sets the per-group multi-room latency budget; the
/// multi-room plugin reads it on the next render frame.
pub fn group_set_leader_ms(
    socket: &Path,
    group_id: &str,
    leader_ms: u32,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "set_group_leader_ms",
            "group_id": group_id,
            "leader_ms": leader_ms,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("set_group_leader_ms", err));
    }
    print_group(&resp);
    Ok(())
}

/// Print a DeviceRole response — emits `<device-id>: <role>`.
fn print_device_role(resp: &serde_json::Value) {
    let device_id =
        resp.get("device_id").and_then(|v| v.as_str()).unwrap_or("");
    let role = resp.get("role").and_then(|v| v.as_str()).unwrap_or("");
    println!("{device_id}: {role}");
}

/// Run the `multiroom set-role <device-id> <role>` subcommand.
pub fn multiroom_set_role(
    socket: &Path,
    device_id: &str,
    role: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "set_device_role",
            "device_id": device_id,
            "role": role,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("set_device_role", err));
    }
    print_device_role(&resp);
    Ok(())
}

/// Run the `multiroom get-role <device-id>` subcommand.
pub fn multiroom_get_role(
    socket: &Path,
    device_id: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "get_device_role",
        "device_id": device_id,
    });
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("get_device_role", err));
    }
    print_device_role(&resp);
    Ok(())
}

/// Run the `multiroom list-roles` subcommand. Emits one line
/// per explicitly-gestured device; devices in the substrate-
/// empty / `auto` default are not enumerated.
pub fn multiroom_list_roles(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({ "op": "list_device_roles" });
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_device_roles", err));
    }
    let entries = resp
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        println!("(no explicit roles set; all devices default to `auto`)");
        return Ok(());
    }
    for entry in entries {
        let device_id = entry
            .get("device_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let role = entry.get("role").and_then(|v| v.as_str()).unwrap_or("");
        println!("{device_id}: {role}");
    }
    Ok(())
}

/// Run the `multiroom clear-role <device-id>` subcommand. The
/// device returns to the substrate-empty `auto` default.
pub fn multiroom_clear_role(
    socket: &Path,
    device_id: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "clear_device_role",
            "device_id": device_id,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("clear_device_role", err));
    }
    print_device_role(&resp);
    Ok(())
}

/// Run the `plugin-reload <plugin>` subcommand routing
/// through the lifecycle coordinator's operator-gesture path.
/// Distinct from `reload-plugin` (legacy `lifecycle.hot_reload`
/// surface): emits a `PluginReloadRequested` event with
/// `ReloadSource::OperatorGesture` for the admission-engine
/// integration to consume.
pub fn plugin_reload_coord(
    socket: &Path,
    plugin: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "plugin_reload",
            "plugin_name": plugin,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("plugin_reload", err));
    }
    let name = resp
        .get("plugin_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    println!("{name}: reload requested (operator gesture)");
    Ok(())
}

/// Run the `plugin-restore <plugin>` subcommand. Clears the
/// plugin's degraded-state slot in the registry and resets its
/// per-plugin failure counters.
pub fn plugin_restore(
    socket: &Path,
    plugin: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "plugin_restore",
            "plugin_name": plugin,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("plugin_restore", err));
    }
    let name = resp
        .get("plugin_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    println!("{name}: degraded slot cleared (operator restore)");
    Ok(())
}

/// Run the `multiroom reconnect <device-id>` subcommand. Runs
/// the 5-carrier reconnect storm against the peer and emits
/// the outcome (winning carrier + elapsed, or exhaustion).
pub fn multiroom_reconnect(
    socket: &Path,
    device_id: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "reconnect_peer",
            "device_id": device_id,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("reconnect_peer", err));
    }
    let reconnected = resp
        .get("reconnected")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let elapsed_ms =
        resp.get("elapsed_ms").and_then(|v| v.as_u64()).unwrap_or(0);
    let dev = resp.get("device_id").and_then(|v| v.as_str()).unwrap_or("");
    if reconnected {
        let carrier = resp
            .get("winning_carrier")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        println!("{dev}: reconnected via {carrier} (elapsed {elapsed_ms} ms)");
    } else {
        println!(
            "{dev}: storm exhausted, peer unreachable (elapsed \
             {elapsed_ms} ms)"
        );
    }
    Ok(())
}

/// Run the `group add-member <group-id> <device-id>` subcommand.
pub fn group_add_member(
    socket: &Path,
    group_id: &str,
    device_id: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "add_group_member",
            "group_id": group_id,
            "device_id": device_id,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("add_group_member", err));
    }
    print_group(&resp);
    Ok(())
}

/// Run the `group remove-member <group-id> <device-id>` subcommand.
pub fn group_remove_member(
    socket: &Path,
    group_id: &str,
    device_id: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "remove_group_member",
            "group_id": group_id,
            "device_id": device_id,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("remove_group_member", err));
    }
    print_group(&resp);
    Ok(())
}

/// Run the `group move-member <from> <to> <device-id>`
/// subcommand. Atomically moves the device from the source
/// group to the target. When the moved device is the source's
/// leader AND post-move source count would be ≥ 2, the first
/// dispatch returns LeaderSuccessorRequired; pass
/// `--successor <id>` on retry to commit.
pub fn group_move_member(
    socket: &Path,
    from_group_id: &str,
    to_group_id: &str,
    device_id: &str,
    successor: Option<&str>,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let mut req_body = serde_json::json!({
        "op": "move_group_member",
        "from_group_id": from_group_id,
        "to_group_id": to_group_id,
        "device_id": device_id,
    });
    if let Some(id) = successor {
        if let Some(obj) = req_body.as_object_mut() {
            obj.insert(
                "successor_device_id".to_string(),
                serde_json::Value::String(id.to_string()),
            );
        }
    }
    let req = with_step_up(req_body, step_up_token);
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("move_group_member", err));
    }
    // SuccessorRequired return path.
    if resp
        .get("successor_required")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let departing = resp
            .get("departing_device_id")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let eligible: Vec<String> = resp
            .get("eligible_member_ids")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        println!("move_group_member: explicit successor required");
        println!("  departing leader: {departing}");
        println!("  eligible successor candidates:");
        for id in &eligible {
            println!("    {id}");
        }
        println!();
        println!("Retry with --successor <device-id> to commit the move.");
        return Ok(());
    }
    // Move outcome return path.
    if let Some(record) = resp.get("record") {
        let from_id = record
            .get("from_group_id")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let to_id = record
            .get("to_group_id")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let device = record
            .get("device_id")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let dissolved = record
            .get("source_dissolved")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let from_after_len = record
            .get("from_members_after")
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        let to_after_len = record
            .get("to_members_after")
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        println!("move_group_member: ok");
        println!("  device:               {device}");
        println!("  from_group:           {from_id} ({from_after_len} members after)");
        println!(
            "  to_group:             {to_id} ({to_after_len} members after)"
        );
        if dissolved {
            println!("  source_dissolved:     true (source dropped below 2-member floor)");
        }
    }
    Ok(())
}

/// Run the `group delete <group-id>` subcommand.
pub fn group_delete(
    socket: &Path,
    group_id: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "delete_group",
            "group_id": group_id,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("delete_group", err));
    }
    let removed = resp
        .get("removed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let id = resp
        .get("group_id")
        .and_then(|v| v.as_str())
        .unwrap_or(group_id);
    if removed {
        println!("group deleted: {id}");
    } else {
        println!("group not found (no change): {id}");
    }
    Ok(())
}

/// Run the `group pin-source-host <group-id> <device-id>`
/// subcommand. Pins a specific device as the source-host
/// (leader) for a multi-room group — operator override of
/// the framework's canonical-min election rule.
pub fn group_pin_source_host(
    socket: &Path,
    group_id: &str,
    device_id: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "pin_source_host",
            "group_id": group_id,
            "device_id": device_id,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("pin_source_host", err));
    }
    println!("source-host pinned:");
    print_group(&resp);
    Ok(())
}

/// Run the `group unpin-source-host <group-id>` subcommand.
/// Clears the operator-pinned source-host so the framework's
/// canonical-min election resumes on the next sweep.
pub fn group_unpin_source_host(
    socket: &Path,
    group_id: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "unpin_source_host",
            "group_id": group_id,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("unpin_source_host", err));
    }
    println!("source-host pin cleared:");
    print_group(&resp);
    Ok(())
}

/// Run the `group source-host show <group-id>` subcommand.
pub fn group_source_host_show(
    socket: &Path,
    group_id: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "get_source_host",
        "group_id": group_id,
    });
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("get_source_host", err));
    }
    print_source_hosts(&resp, Some(group_id));
    Ok(())
}

/// Run the `group source-host list` subcommand.
pub fn group_source_host_list(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "list_source_hosts"});
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_source_hosts", err));
    }
    print_source_hosts(&resp, None);
    Ok(())
}

fn print_source_hosts(resp: &serde_json::Value, key_filter: Option<&str>) {
    let entries = resp
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        match key_filter {
            Some(k) => {
                println!("source-host election {k}: (no election recorded)")
            }
            None => println!("source-host elections: (none recorded)"),
        }
        return;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    match key_filter {
        Some(_) => println!("source-host election:"),
        None => println!("source-host elections ({n}):", n = entries.len()),
    }
    for e in &entries {
        let group_id =
            e.get("group_id").and_then(|v| v.as_str()).unwrap_or("?");
        let host = e.get("source_host_device_id").and_then(|v| v.as_str());
        let candidates = e
            .get("candidate_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let elected =
            e.get("elected_at_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        let age_s = now_ms.saturating_sub(elected) / 1000;
        println!();
        println!("  Group:         {group_id}");
        match host {
            Some(h) => println!("    Source-host: {h}"),
            None => println!("    Source-host: <none reachable>"),
        }
        println!("    Candidates:  {candidates}");
        println!("    Elected:     {age_s}s ago (ms = {elected})");
    }
}

/// Run the `steward restart` subcommand. Initiates the
/// graceful steward restart sequence; the framework emits
/// a `StewardRestarting` happening, drains briefly, and
/// `execve`s the target binary. Operators on the wire who
/// issued this op see the wire connection close cleanly
/// when the new process image takes over.
pub fn steward_restart(
    socket: &Path,
    reason: &str,
    target_binary: Option<&str>,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let mut payload = serde_json::json!({
        "op": "request_steward_restart",
        "reason": reason,
    });
    if let Some(p) = target_binary {
        payload["target_binary"] = serde_json::Value::String(p.to_string());
    }
    let req = with_step_up(payload, step_up_token);
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("request_steward_restart", err));
    }
    let target = resp
        .get("target_binary")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let downtime = resp
        .get("expected_downtime_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    println!("steward restart scheduled:");
    println!("  Target binary:     {target}");
    println!("  Expected downtime: {downtime} ms");
    println!();
    println!("  The wire connection will close as the steward execs;");
    println!("  reconnect after the steward boots back up.");
    Ok(())
}

/// Run the `updates list` subcommand — read the aggregated
/// inventory across every registered source.
pub fn updates_list(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "list_update_inventory"});
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_update_inventory", err));
    }
    print_update_inventory(&resp);
    Ok(())
}

/// Run the `updates check-now [source-id]` subcommand.
pub fn updates_check_now(
    socket: &Path,
    source_id: Option<&str>,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let mut payload = serde_json::json!({"op": "check_updates_now"});
    if let Some(id) = source_id {
        payload["source_id"] = serde_json::Value::String(id.to_string());
    }
    let req = with_step_up(payload, step_up_token);
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("check_updates_now", err));
    }
    print_update_inventory(&resp);
    Ok(())
}

/// Run the `updates apply <source-id> <update-id>` subcommand.
pub fn updates_apply(
    socket: &Path,
    source_id: &str,
    update_id: &str,
    dry_run: bool,
    approved_by: Option<&str>,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let mut payload = serde_json::json!({
        "op": "apply_update",
        "source_id": source_id,
        "update_id": update_id,
        "dry_run": dry_run,
    });
    if let Some(p) = approved_by {
        payload["approved_by"] = serde_json::Value::String(p.to_string());
    }
    let req = with_step_up(payload, step_up_token);
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("apply_update", err));
    }
    let outcome = resp.get("outcome").ok_or_else(|| {
        anyhow::anyhow!("apply_update: response missing outcome")
    })?;
    let component = outcome
        .get("component")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let applied_version = outcome
        .get("applied_version")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let restart = outcome
        .get("restart_initiated")
        .and_then(|v| v.as_str())
        .unwrap_or("none");
    let dr = outcome
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    println!("update applied:");
    println!("  Source:      {source_id}");
    println!("  Update id:   {update_id}");
    println!("  Component:   {component}");
    println!("  Applied:     {applied_version}");
    println!("  Restart:     {restart}");
    println!("  Dry-run:     {}", if dr { "yes" } else { "no" });
    Ok(())
}

/// Run the `updates auto-apply show` subcommand.
pub fn updates_auto_apply_show(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "get_auto_apply_policies"});
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("get_auto_apply_policies", err));
    }
    let entries = resp
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        println!("auto-apply policies: (none recorded)");
        return Ok(());
    }
    println!("auto-apply policies ({n}):", n = entries.len());
    for e in &entries {
        let source = e.get("source_id").and_then(|v| v.as_str()).unwrap_or("?");
        let enabled =
            e.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
        let threshold = e
            .get("severity_threshold")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        println!();
        println!("  {source}");
        println!("    Enabled:     {}", if enabled { "yes" } else { "no" });
        println!("    Threshold:   {threshold}");
    }
    Ok(())
}

/// Run the `updates auto-apply set <source-id> <enabled> <threshold>` subcommand.
pub fn updates_auto_apply_set(
    socket: &Path,
    source_id: &str,
    enabled: bool,
    severity_threshold: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "set_auto_apply_policy",
            "source_id": source_id,
            "enabled": enabled,
            "severity_threshold": severity_threshold,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("set_auto_apply_policy", err));
    }
    let policy = resp.get("policy").ok_or_else(|| {
        anyhow::anyhow!("set_auto_apply_policy: response missing policy")
    })?;
    let stored_source = policy
        .get("source_id")
        .and_then(|v| v.as_str())
        .unwrap_or(source_id);
    let stored_enabled = policy
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(enabled);
    let stored_threshold = policy
        .get("severity_threshold")
        .and_then(|v| v.as_str())
        .unwrap_or(severity_threshold);
    println!("auto-apply policy stored:");
    println!("  Source:      {stored_source}");
    println!(
        "  Enabled:     {}",
        if stored_enabled { "yes" } else { "no" }
    );
    println!("  Threshold:   {stored_threshold}");
    Ok(())
}

/// Run the `describe-ui-stockings` subcommand. Read-only;
/// returns the framework's admitted UI stockings, optionally
/// filtered to one shelf id.
pub fn describe_ui_stockings(
    socket: &Path,
    shelf_filter: Option<&str>,
) -> Result<(), anyhow::Error> {
    let mut payload = serde_json::json!({"op": "describe_ui_stockings"});
    if let Some(id) = shelf_filter {
        payload["shelf_filter"] = serde_json::Value::String(id.to_string());
    }
    let resp = call_with_caps(socket, &[], payload)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("describe_ui_stockings", err));
    }
    print_ui_stockings(&resp);
    Ok(())
}

/// Run the `activate-theme` subcommand. Sends
/// `op = "activate_theme"` under the `plugins_admin`
/// capability with optional step-up and prints the
/// steward's `ActiveUiSelectionSet` response. Pass `clear =
/// true` to deactivate the active-theme slot; otherwise
/// `plugin` must be supplied. Mutually exclusive at the CLI
/// surface (the parser refuses both at once).
pub fn activate_theme(
    socket: &Path,
    plugin: Option<&str>,
    clear: bool,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    activate_ui_slot(
        socket,
        "activate_theme",
        "theme",
        plugin,
        clear,
        step_up_token,
    )
}

/// Run the `activate-ui-shell` subcommand. Mirror of
/// [`activate_theme`] for the `ui_shell` slot.
pub fn activate_ui_shell(
    socket: &Path,
    plugin: Option<&str>,
    clear: bool,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    activate_ui_slot(
        socket,
        "activate_ui_shell",
        "ui_shell",
        plugin,
        clear,
        step_up_token,
    )
}

/// Shared dispatcher for `activate-theme` / `activate-ui-shell`.
/// Builds the wire request, threads step-up, prints the
/// outcome.
fn activate_ui_slot(
    socket: &Path,
    op: &'static str,
    slot_label: &'static str,
    plugin: Option<&str>,
    clear: bool,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    if !clear && plugin.is_none() {
        return Err(anyhow::anyhow!(
            "{op}: pass --plugin <name> to activate, or --clear to deactivate"
        ));
    }
    let plugin_value = if clear {
        serde_json::Value::Null
    } else {
        // Safe: validated above.
        serde_json::Value::String(plugin.unwrap().to_string())
    };
    let req = with_step_up(
        serde_json::json!({
            "op": op,
            "plugin_name": plugin_value,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error(op, err));
    }
    let active = resp.get("active_plugin_name").and_then(|v| v.as_str());
    match active {
        Some(name) => {
            println!("active {slot_label} set: plugin={name}");
        }
        None => {
            println!("active {slot_label} cleared");
        }
    }
    Ok(())
}

/// Run the `describe-active-ui-selection` subcommand.
/// Read-only; returns the active theme + active UI shell
/// plugin names (each `null` when the slot is unset).
pub fn describe_active_ui_selection(
    socket: &Path,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "describe_active_ui_selection"});
    let resp = call_with_caps(socket, &[], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("describe_active_ui_selection", err));
    }
    let theme = resp.get("theme").and_then(|v| v.as_str());
    let ui_shell = resp.get("ui_shell").and_then(|v| v.as_str());
    println!("active UI selection:");
    match theme {
        Some(name) => println!("  theme    = {name}"),
        None => println!("  theme    = (none)"),
    }
    match ui_shell {
        Some(name) => println!("  ui_shell = {name}"),
        None => println!("  ui_shell = (none)"),
    }
    Ok(())
}

fn print_ui_stockings(resp: &serde_json::Value) {
    let entries = resp.get("entries").and_then(|v| v.as_array());
    let entries = match entries {
        Some(e) => e,
        None => {
            println!("UI stockings: <missing entries field>");
            return;
        }
    };
    if entries.is_empty() {
        println!("UI stockings: (none admitted)");
        return;
    }
    println!("UI stockings ({} admitted):", entries.len());
    let mut by_shelf: std::collections::BTreeMap<
        String,
        Vec<&serde_json::Value>,
    > = std::collections::BTreeMap::new();
    for entry in entries {
        let shelf = entry
            .get("ui_shelf")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>")
            .to_string();
        by_shelf.entry(shelf).or_default().push(entry);
    }
    for (shelf, rows) in by_shelf {
        println!();
        println!("  [{shelf}]");
        for entry in rows {
            let plugin = entry
                .get("plugin")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>");
            let widget = entry
                .get("widget")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>");
            let size = entry
                .get("size")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>");
            let mode =
                entry.get("mode").and_then(|v| v.as_str()).unwrap_or("-");
            let schema = entry
                .get("schema_version")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            println!(
                "    {plugin}\n        widget = {widget}\n        \
                 size   = {size}\n        mode   = {mode}\n        \
                 schema = {schema}"
            );
        }
    }
}

fn print_update_inventory(resp: &serde_json::Value) {
    let snapshot = match resp.get("snapshot") {
        Some(s) => s,
        None => {
            println!("update inventory: <missing snapshot field>");
            return;
        }
    };
    let sources = resp
        .get("sources")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let last_check_ms = snapshot
        .get("last_check_at_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let items = snapshot
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    println!("Update inventory");
    if last_check_ms > 0 {
        let age_s = now_ms.saturating_sub(last_check_ms) / 1000;
        println!("  Last aggregate check: {age_s}s ago");
    } else {
        println!("  Last aggregate check: <never>");
    }
    println!();
    println!("Registered sources ({n}):", n = sources.len());
    if sources.is_empty() {
        println!("  (no sources registered yet — gateway / source plugins admit and register here)");
    }
    for s in &sources {
        let id = s.get("source_id").and_then(|v| v.as_str()).unwrap_or("?");
        let display =
            s.get("display_name").and_then(|v| v.as_str()).unwrap_or(id);
        println!("  - {id} \u{2014} {display}");
    }
    println!();
    println!("Available updates ({n}):", n = items.len());
    if items.is_empty() {
        println!("  (none)");
        return;
    }
    for it in &items {
        let source =
            it.get("source_id").and_then(|v| v.as_str()).unwrap_or("?");
        let upd = it.get("update").cloned().unwrap_or_default();
        let component =
            upd.get("component").and_then(|v| v.as_str()).unwrap_or("?");
        let from = upd
            .get("current_version")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let to = upd
            .get("available_version")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let severity =
            upd.get("severity").and_then(|v| v.as_str()).unwrap_or("?");
        let restart = upd
            .get("requires_restart")
            .and_then(|v| v.as_str())
            .unwrap_or("none");
        let id = upd.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        println!();
        println!("  [{source}] {component}: {from} \u{2192} {to}");
        println!("    Update id: {id}");
        println!("    Severity:  {severity}");
        println!("    Restart:   {restart}");
    }
}

/// Run the `group dispatch <group-id>` subcommand. Returns
/// the resolved dispatch target — source-host id, whether
/// it's the local node, and (when remote) the audio-plane
/// connection state.
pub fn group_dispatch(
    socket: &Path,
    group_id: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "dispatch_to_group",
        "group_id": group_id,
    });
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("dispatch_to_group", err));
    }
    let display = resp
        .get("display_name")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let host = resp.get("source_host_device_id").and_then(|v| v.as_str());
    let is_local = resp
        .get("is_local_host")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let conn_state = resp
        .get("source_host_connection_state")
        .and_then(|v| v.as_str());
    println!("Dispatch target for group: {display}");
    println!("  Group id:      {group_id}");
    match (host, is_local, conn_state) {
        (Some(h), true, _) => {
            println!("  Source-host:   {h} (this device)");
            println!(
                "  Resolution:    issue the verb against the local steward"
            );
        }
        (Some(h), false, Some(state)) => {
            println!("  Source-host:   {h}");
            println!("  Connection:    {state}");
            println!("  Resolution:    forward via the audio-plane channel");
        }
        (Some(h), false, None) => {
            println!("  Source-host:   {h}");
            println!("  Connection:    <not yet established>");
            println!(
                "  Resolution:    wait for audio-plane handshake to complete"
            );
        }
        (None, _, _) => {
            println!("  Source-host:   <none reachable>");
            println!("  Resolution:    no live group member; dispatch refused");
        }
    }
    Ok(())
}

/// Run the `group topology show <group-id>` subcommand.
pub fn group_topology_show(
    socket: &Path,
    group_id: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "get_group_active_topology",
        "group_id": group_id,
    });
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("get_group_active_topology", err));
    }
    print_group_topologies(&resp, Some(group_id));
    Ok(())
}

/// Run the `group topology list` subcommand.
pub fn group_topology_list(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "list_group_active_topologies"});
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_group_active_topologies", err));
    }
    print_group_topologies(&resp, None);
    Ok(())
}

fn print_group_topologies(resp: &serde_json::Value, key_filter: Option<&str>) {
    let entries = resp
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        match key_filter {
            Some(k) => println!("group topology {k}: (no group)"),
            None => println!("group topologies: (none recorded)"),
        }
        return;
    }
    match key_filter {
        Some(_) => println!("group topology:"),
        None => println!("group topologies ({n}):", n = entries.len()),
    }
    for e in &entries {
        let group_id =
            e.get("group_id").and_then(|v| v.as_str()).unwrap_or("?");
        let display = e
            .get("display_name")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let host = e.get("source_host_device_id").and_then(|v| v.as_str());
        let is_local = e
            .get("is_local_host")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let fully_populated = e
            .get("fully_populated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let receivers = e
            .get("receiver_legs")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        println!();
        println!("  {display}");
        println!("    Group id:        {group_id}");
        match host {
            Some(h) => {
                let role = if is_local { " (this device)" } else { "" };
                println!("    Source-host:     {h}{role}");
            }
            None => println!("    Source-host:     <none reachable>"),
        }
        println!(
            "    Status:          {}",
            if fully_populated {
                "fully populated"
            } else {
                "partial / warming"
            }
        );
        if !receivers.is_empty() {
            println!("    Receiver legs:");
            for r in &receivers {
                let did =
                    r.get("device_id").and_then(|v| v.as_str()).unwrap_or("?");
                let cstate = r.get("connection_state").and_then(|v| v.as_str());
                let frames =
                    r.get("frames_sent").and_then(|v| v.as_u64()).unwrap_or(0);
                let offset =
                    r.get("last_sync_offset_ms").and_then(|v| v.as_i64());
                let cstate_disp = cstate.unwrap_or("<no connection>");
                let offset_disp = match offset {
                    Some(o) => format!("{o:+} ms"),
                    None => "<no sample>".into(),
                };
                println!(
                    "      - {did} | conn={cstate_disp} | offset={offset_disp} | frames={frames}"
                );
            }
        }
    }
}

/// Run the `group gateways list` subcommand.
pub fn group_gateways_list(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "list_gateway_plugins"});
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_gateway_plugins", err));
    }
    let entries = resp
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        println!("gateway plugins: (none registered)");
        return Ok(());
    }
    println!("gateway plugins ({n}):", n = entries.len());
    for e in &entries {
        let plugin =
            e.get("plugin_name").and_then(|v| v.as_str()).unwrap_or("?");
        let protocol =
            e.get("protocol").and_then(|v| v.as_str()).unwrap_or("?");
        let direction =
            e.get("direction").and_then(|v| v.as_str()).unwrap_or("?");
        let licensed =
            e.get("licensed").and_then(|v| v.as_bool()).unwrap_or(false);
        println!();
        println!("  {plugin}");
        println!("    Protocol:    {protocol}");
        println!("    Direction:   {direction}");
        println!("    Licensed:    {}", if licensed { "yes" } else { "no" });
    }
    Ok(())
}

/// Run the `group network connections` subcommand. Lists
/// every active audio-plane peer connection (the TCP
/// control + data channel between this node and its
/// multi-room peers).
pub fn group_network_connections(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "list_audio_plane_connections"});
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_audio_plane_connections", err));
    }
    let entries = resp
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        println!("audio-plane peer connections: (none active)");
        return Ok(());
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    println!("audio-plane peer connections ({n}):", n = entries.len());
    for e in &entries {
        let id = e
            .get("remote_device_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let addr = e
            .get("remote_address")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let direction =
            e.get("direction").and_then(|v| v.as_str()).unwrap_or("?");
        let state = e.get("state").and_then(|v| v.as_str()).unwrap_or("?");
        let version = e
            .get("framework_version")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let last_hb = e
            .get("last_channel_activity_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let last_sync = e
            .get("last_sync_at_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let offset = e.get("last_sync_offset_ms").and_then(|v| v.as_i64());
        let frames = e
            .get("frames_received")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let claimed: Vec<String> = e
            .get("claimed_source_host_groups")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect();
        println!();
        println!("  {id}");
        println!("    Address:     {addr}");
        println!("    Direction:   {direction}");
        println!("    State:       {state}");
        if !version.is_empty() {
            println!("    Version:     {version}");
        }
        if last_hb > 0 {
            let age = now_ms.saturating_sub(last_hb) / 1000;
            println!("    Heartbeat:   {age}s ago");
        } else {
            println!("    Heartbeat:   <none>");
        }
        match (offset, last_sync) {
            (Some(o), s) if s > 0 => {
                let age = now_ms.saturating_sub(s) / 1000;
                println!("    Sync offset: {o:+} ms ({age}s ago)");
            }
            _ => println!("    Sync offset: <no sample>"),
        }
        if !claimed.is_empty() {
            println!("    Hosts:       {}", claimed.join(", "));
        }
        println!("    Frames in:   {frames}");
    }
    Ok(())
}

/// Run the `group network dial <addr>` subcommand. Manually
/// establish an outbound audio-plane TCP connection to the
/// supplied peer when auto-discovery does not surface a
/// dialable address.
pub fn group_network_dial(
    socket: &Path,
    addr: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "audio_plane_dial",
        "addr": addr,
    });
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("audio_plane_dial", err));
    }
    let remote = resp
        .get("remote_device_id")
        .and_then(|v| v.as_str())
        .unwrap_or("<unknown>");
    println!("audio-plane dial succeeded:");
    println!("  Address:   {addr}");
    println!("  Remote id: {remote}");
    Ok(())
}

/// Run the `group frame-trace` subcommand. Dispatches the
/// `audio.multiroom.frame_trace.snapshot` wire-op against the
/// admitted `audio.multiroom` shelf and renders the rolling
/// per-frame audible-time trace window.
///
/// Canonical operator surface for the per-frame audible-time
/// trace data (eight stamps per record, keyed by sequence +
/// receiver device id). The `audio.multiroom.frame_trace`
/// published subject carries the same data for in-product
/// consumers; this CLI verb pulls the latest snapshot for
/// ad-hoc inspection.
///
/// `--json` emits the wire-op envelope verbatim for piping into
/// downstream analysis tooling; the default form renders a
/// compact per-record table.
pub fn group_frame_trace(
    socket: &Path,
    json: bool,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "request",
        "shelf": "audio.multiroom",
        "request_type": "audio.multiroom.frame_trace.snapshot",
        "payload_b64": "",
    });
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("audio.multiroom.frame_trace.snapshot", err));
    }
    let payload_b64 = resp
        .get("payload_b64")
        .and_then(Value::as_str)
        .unwrap_or("");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload_b64.as_bytes())
        .context("decoding frame_trace.snapshot payload")?;
    let envelope: Value = serde_json::from_slice(&bytes)
        .context("parsing frame_trace.snapshot payload as JSON")?;

    if json {
        let pretty = serde_json::to_string_pretty(&envelope)
            .unwrap_or_else(|_| envelope.to_string());
        println!("{pretty}");
        return Ok(());
    }

    let group_id = envelope
        .get("group_id")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let window_size = envelope
        .get("window_size")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let last_update_at_ms = envelope
        .get("last_update_at_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let records = envelope
        .get("records")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    println!(
        "frame-trace snapshot: group {group_id}, {n} record(s), \
         window={window_size}, last_update_at_ms={last_update_at_ms}",
        n = records.len()
    );
    if records.is_empty() {
        return Ok(());
    }
    println!();
    println!(
        "{:>6}  {:<36}  {:>8}  {:>10}  {:>10}  {:>10}",
        "seq", "recv", "pts_ms", "src_us", "dwell_us", "writei_us"
    );
    for r in &records {
        let seq = r.get("sequence").and_then(Value::as_u64).unwrap_or(0);
        let recv = r
            .get("receiver_device_id")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let pts_ms = r
            .get("presentation_time_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let s3a = r
            .get("source_capture_readi_return_ns")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let s5a = r
            .get("source_wire_send_ns")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let s5b = r
            .get("receiver_wire_recv_ns")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let s6 = r
            .get("receiver_scheduler_dequeue_ns")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let s7 = r
            .get("receiver_writei_return_ns")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let src_us = s5a.saturating_sub(s3a) / 1000;
        let dwell_us = s6.saturating_sub(s5b) / 1000;
        let writei_us = s7.saturating_sub(s6) / 1000;
        println!(
            "{seq:>6}  {recv:<36}  {pts_ms:>8}  {src_us:>9}μ  {dwell_us:>9}μ  {writei_us:>9}μ"
        );
    }
    Ok(())
}

/// Run the `group clock show <group-id>` subcommand.
pub fn group_clock_show(
    socket: &Path,
    group_id: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "get_clock_sync",
        "group_id": group_id,
    });
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("get_clock_sync", err));
    }
    print_clock_syncs(&resp, Some(group_id));
    Ok(())
}

/// Run the `group clock list` subcommand.
pub fn group_clock_list(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "list_clock_syncs"});
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_clock_syncs", err));
    }
    print_clock_syncs(&resp, None);
    Ok(())
}

fn print_clock_syncs(resp: &serde_json::Value, key_filter: Option<&str>) {
    let entries = resp
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        match key_filter {
            Some(k) => println!("clock-sync state {k}: (no state recorded)"),
            None => println!("clock-sync states: (none recorded)"),
        }
        return;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    match key_filter {
        Some(_) => println!("clock-sync state:"),
        None => println!("clock-sync states ({n}):", n = entries.len()),
    }
    for e in &entries {
        let group_id =
            e.get("group_id").and_then(|v| v.as_str()).unwrap_or("?");
        let display = e
            .get("display_name")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let host = e.get("source_host_device_id").and_then(|v| v.as_str());
        let is_local = e
            .get("is_local_host")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let offset = e.get("offset_ms").and_then(|v| v.as_i64()).unwrap_or(0);
        let unc = e
            .get("uncertainty_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let last_sync = e
            .get("last_sync_at_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let quality = e.get("quality").and_then(|v| v.as_str()).unwrap_or("?");
        println!();
        println!("  {display}");
        println!("    Group id:    {group_id}");
        match host {
            Some(h) => {
                let role = if is_local { " (this device)" } else { "" };
                println!("    Source-host: {h}{role}");
            }
            None => println!("    Source-host: <none reachable>"),
        }
        println!("    Quality:     {quality}");
        if !is_local {
            println!("    Offset:      {offset:+} ms (\u{00b1} {unc} ms)");
            if last_sync > 0 {
                let age_s = now_ms.saturating_sub(last_sync) / 1000;
                println!("    Last sync:   {age_s}s ago");
            } else {
                println!("    Last sync:   <no sample>");
            }
        }
    }
}

fn print_group(resp: &serde_json::Value) {
    let Some(record) = resp.get("record") else {
        println!("group: <missing record field in response>");
        return;
    };
    print_group_record(record);
}

fn print_group_record(record: &serde_json::Value) {
    let id = record
        .get("group_id")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let display = record
        .get("display_name")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let created = record
        .get("created_at_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let modified = record
        .get("modified_at_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let members: Vec<String> = record
        .get("members")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| m.as_str().map(str::to_string))
        .collect();
    let pinned_source_host = record
        .get("pinned_source_host")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let effective_leader = record
        .get("effective_leader")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let leader_ms = record.get("leader_ms").and_then(|v| v.as_u64());
    println!("  {display}");
    println!("    Group id:      {id}");
    println!("    Members ({}):  {}", members.len(), members.join(", "));
    println!("    Created (ms):  {created}");
    println!("    Modified (ms): {modified}");
    match pinned_source_host.as_deref() {
        Some(p) => println!("    Source host:   {p} (operator-pinned)"),
        None => println!("    Source host:   (election will resolve)"),
    }
    match effective_leader.as_deref() {
        Some(l) => println!("    Leader:        {l}"),
        None => println!("    Leader:        (none — empty membership)"),
    }
    if let Some(ms) = leader_ms {
        println!("    Leader ms:     {ms}");
    }
}

fn print_active_audio_topologies(
    resp: &serde_json::Value,
    key_filter: Option<&str>,
) -> Result<(), anyhow::Error> {
    let entries = resp
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        match key_filter {
            Some(k) => println!("active audio topology {k}: (not published)"),
            None => println!("active audio topologies: (none published)"),
        }
        return Ok(());
    }
    match key_filter {
        Some(k) => println!("active audio topology {k}:"),
        None => println!("active audio topologies ({n}):", n = entries.len()),
    }
    for e in entries {
        let pretty = serde_json::to_string_pretty(&e)
            .unwrap_or_else(|_| "<unrenderable>".into());
        println!("{pretty}");
    }
    Ok(())
}

fn print_audio_operator_policies(
    resp: &serde_json::Value,
    key_filter: Option<&str>,
) -> Result<(), anyhow::Error> {
    let entries = resp
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        match key_filter {
            Some(k) => println!(
                "audio operator policy {k}: (not recorded; framework default \
                 = auto)"
            ),
            None => println!("audio operator policies: (none recorded)"),
        }
        return Ok(());
    }
    match key_filter {
        Some(k) => println!("audio operator policy {k}:"),
        None => println!("audio operator policies ({n}):", n = entries.len()),
    }
    for e in entries {
        let pretty = serde_json::to_string_pretty(&e)
            .unwrap_or_else(|_| "<unrenderable>".into());
        println!("{pretty}");
    }
    Ok(())
}

fn print_audio_volume_modes(
    resp: &serde_json::Value,
    key_filter: Option<&str>,
) -> Result<(), anyhow::Error> {
    let entries = resp
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        match key_filter {
            Some(k) => println!(
                "audio volume mode {k}: (not recorded; framework default = \
                 software)"
            ),
            None => println!("audio volume modes: (none recorded)"),
        }
        return Ok(());
    }
    match key_filter {
        Some(k) => println!("audio volume mode {k}:"),
        None => println!("audio volume modes ({n}):", n = entries.len()),
    }
    for e in entries {
        let target_key =
            e.get("target_key").and_then(|v| v.as_str()).unwrap_or("");
        let volume_mode =
            e.get("volume_mode").and_then(|v| v.as_str()).unwrap_or("");
        let by = e
            .get("set_by_principal")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let at = e.get("set_at_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        println!("  {target_key}\t{volume_mode}\t{by}\t{at}");
    }
    Ok(())
}

fn print_hardware_profile_overrides(
    resp: &serde_json::Value,
    key_filter: Option<&str>,
) -> Result<(), anyhow::Error> {
    let entries = resp
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        match key_filter {
            Some(k) => {
                println!("hardware profile override {k}: (not recorded)")
            }
            None => println!("hardware profile overrides: (none recorded)"),
        }
        return Ok(());
    }
    match key_filter {
        Some(k) => println!("hardware profile override {k}:"),
        None => {
            println!("hardware profile overrides ({n}):", n = entries.len())
        }
    }
    for e in entries {
        let pretty = serde_json::to_string_pretty(&e)
            .unwrap_or_else(|_| "<unrenderable>".into());
        println!("{pretty}");
    }
    Ok(())
}

/// Parse a filter JSON string and validate the basic shape
/// (must be a JSON object with a "kind" field).
fn parse_filter_json(
    filter_json: &str,
) -> Result<serde_json::Value, anyhow::Error> {
    let value: serde_json::Value = serde_json::from_str(filter_json)
        .context("parsing --filter-json (expected a JSON filter expression)")?;
    if !value.is_object() || value.get("kind").is_none() {
        return Err(anyhow::anyhow!(
            "--filter-json must be a JSON object with a 'kind' field, e.g. \
             '{{\"kind\":\"all\"}}'"
        ));
    }
    Ok(value)
}

/// Run the `bulk list-where` subcommand. Read-only preview of
/// every plugin matching the supplied filter.
pub fn bulk_list_where(
    socket: &Path,
    filter_json: &str,
) -> Result<(), anyhow::Error> {
    let filter = parse_filter_json(filter_json)?;
    let req = serde_json::json!({
        "op": "list_plugins_where",
        "filter": filter,
    });
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_plugins_where", err));
    }
    let names = resp
        .get("names")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if names.is_empty() {
        println!("plugins matching filter: (none)");
    } else {
        println!("plugins matching filter ({} match):", names.len());
        for n in names {
            if let Some(s) = n.as_str() {
                println!("  {s}");
            }
        }
    }
    Ok(())
}

/// Run the `bulk disable-where` subcommand.
pub fn bulk_disable_where(
    socket: &Path,
    filter_json: &str,
    reason: Option<&str>,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    bulk_lifecycle_where(
        socket,
        "disable_plugins_where",
        filter_json,
        reason,
        step_up_token,
    )
}

/// Run the `bulk enable-where` subcommand.
pub fn bulk_enable_where(
    socket: &Path,
    filter_json: &str,
    reason: Option<&str>,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    bulk_lifecycle_where(
        socket,
        "enable_plugins_where",
        filter_json,
        reason,
        step_up_token,
    )
}

fn bulk_lifecycle_where(
    socket: &Path,
    op: &str,
    filter_json: &str,
    reason: Option<&str>,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let filter = parse_filter_json(filter_json)?;
    let mut req = serde_json::json!({
        "op": op,
        "filter": filter,
    });
    if let Some(r) = reason {
        req["reason"] = serde_json::json!(r);
    }
    let req = with_step_up(req, step_up_token);
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error(op, err));
    }
    let succeeded = resp
        .get("succeeded")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let failed = resp
        .get("failed")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    println!(
        "{op}: succeeded={} failed={}",
        succeeded.len(),
        failed.len()
    );
    if !succeeded.is_empty() {
        println!("succeeded:");
        for n in succeeded {
            if let Some(s) = n.as_str() {
                println!("  {s}");
            }
        }
    }
    if !failed.is_empty() {
        println!("failed:");
        for f in failed {
            let plugin =
                f.get("plugin_name").and_then(|v| v.as_str()).unwrap_or("");
            let msg = f.get("message").and_then(|v| v.as_str()).unwrap_or("");
            println!("  {plugin}: {msg}");
        }
    }
    Ok(())
}

/// Run the `tag add` subcommand.
pub fn tag_add(
    socket: &Path,
    plugin_name: &str,
    tag: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "set_plugin_tag",
            "plugin_name": plugin_name,
            "tag": tag,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("set_plugin_tag", err));
    }
    println!("plugin tag set: {plugin_name} -> {tag}");
    Ok(())
}

/// Run the `tag remove` subcommand.
pub fn tag_remove(
    socket: &Path,
    plugin_name: &str,
    tag: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "delete_plugin_tag",
            "plugin_name": plugin_name,
            "tag": tag,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("delete_plugin_tag", err));
    }
    println!("plugin tag deleted: {plugin_name} -> {tag}");
    Ok(())
}

/// Run the `tag list` subcommand. Read-only.
pub fn tag_list(socket: &Path, plugin_name: &str) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "list_plugin_tags",
        "plugin_name": plugin_name,
    });
    let resp = call_with_caps(socket, &[], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_plugin_tags", err));
    }
    let tags = resp
        .get("tags")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if tags.is_empty() {
        println!("plugin {plugin_name}: (no tags)");
    } else {
        println!("plugin {plugin_name} tags:");
        for t in tags {
            let tag = t.get("tag").and_then(|v| v.as_str()).unwrap_or("");
            let set_at =
                t.get("set_at_ms").and_then(|v| v.as_u64()).unwrap_or(0);
            println!("  {tag} (set_at_ms={set_at})");
        }
    }
    Ok(())
}

/// Run the `profile list` subcommand. Read-only; returns
/// metadata only (no entries).
pub fn profile_list(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "list_plugin_profiles"});
    let resp = call_with_caps(socket, &[], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_plugin_profiles", err));
    }
    let entries = resp
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        println!("plugin profiles: (no profiles recorded)");
        return Ok(());
    }
    println!("plugin profiles:");
    for e in entries {
        let id = e.get("profile_id").and_then(|v| v.as_str()).unwrap_or("");
        let name = e.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let active = e.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
        let author =
            e.get("authored_by").and_then(|v| v.as_str()).unwrap_or("");
        let marker = if active { " [active]" } else { "" };
        println!("  {id}{marker} ({author}) — {name}");
    }
    Ok(())
}

/// Run the `profile get <id>` subcommand. Read-only.
pub fn profile_get(
    socket: &Path,
    profile_id: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "get_plugin_profile",
        "profile_id": profile_id,
    });
    let resp = call_with_caps(socket, &[], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("get_plugin_profile", err));
    }
    let Some(profile) = resp.get("profile") else {
        println!("plugin profile {profile_id:?}: not found");
        return Ok(());
    };
    let id = profile
        .get("profile_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let name = profile.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let active = profile
        .get("active")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let author = profile
        .get("authored_by")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let description = profile
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    println!("plugin profile {id}:");
    println!("  name: {name}");
    println!("  authored_by: {author}");
    println!("  active: {active}");
    if !description.is_empty() {
        println!("  description: {description}");
    }
    let entries = profile
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        println!("  entries: (none)");
    } else {
        println!("  entries:");
        for entry in entries {
            let plugin = entry
                .get("plugin_name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let state =
                entry.get("state").and_then(|v| v.as_str()).unwrap_or("");
            println!("    {state}\t{plugin}");
        }
    }
    Ok(())
}

/// Run the `profile put` subcommand. Reads a TOML profile
/// document from `path` (or stdin when `path` is `-`) and
/// dispatches `op = "put_plugin_profile"`. Capability-gated by
/// `plugins_admin` and step-up-aware.
pub fn profile_put(
    socket: &Path,
    path: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let body = if path == "-" {
        use std::io::Read;
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .context("reading profile TOML from stdin")?;
        s
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("reading {path}"))?
    };
    let parsed: toml::Value =
        toml::from_str(&body).with_context(|| "parsing profile TOML body")?;

    let table = parsed
        .as_table()
        .ok_or_else(|| anyhow::anyhow!("profile TOML must be a table"))?;
    let profile_id = table
        .get("profile_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("profile TOML missing profile_id"))?;
    let name = table
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("profile TOML missing name"))?;
    let description = table
        .get("description")
        .and_then(|v| v.as_str())
        .map(String::from);
    let authored_by = table
        .get("authored_by")
        .and_then(|v| v.as_str())
        .unwrap_or("user");

    let entries = match table.get("entries") {
        Some(toml::Value::Array(arr)) => arr
            .iter()
            .map(|v| {
                let plugin = v
                    .get("plugin_name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "profile TOML entry missing plugin_name"
                        )
                    })?;
                let state =
                    v.get("state").and_then(|v| v.as_str()).ok_or_else(
                        || anyhow::anyhow!("profile TOML entry missing state"),
                    )?;
                Ok::<_, anyhow::Error>(serde_json::json!({
                    "plugin_name": plugin,
                    "state": state,
                }))
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => Vec::new(),
    };

    let mut req = serde_json::json!({
        "op": "put_plugin_profile",
        "profile_id": profile_id,
        "name": name,
        "authored_by": authored_by,
        "entries": entries,
    });
    if let Some(d) = description {
        req["description"] = serde_json::json!(d);
    }
    let req = with_step_up(req, step_up_token);
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("put_plugin_profile", err));
    }
    let id = resp
        .get("profile_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    println!("plugin profile put: {id}");
    Ok(())
}

/// Run the `profile delete` subcommand.
pub fn profile_delete(
    socket: &Path,
    profile_id: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "delete_plugin_profile",
            "profile_id": profile_id,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("delete_plugin_profile", err));
    }
    let removed = resp
        .get("removed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    println!(
        "plugin profile delete: profile_id={profile_id} removed={removed}"
    );
    Ok(())
}

/// Run the `profile activate` subcommand. With `dry_run`, prints
/// the activation plan without dispatching transitions.
pub fn profile_activate(
    socket: &Path,
    profile_id: Option<&str>,
    dry_run: bool,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let mut req = serde_json::json!({
        "op": "set_active_plugin_profile",
        "dry_run": dry_run,
    });
    if let Some(id) = profile_id {
        req["profile_id"] = serde_json::json!(id);
    }
    let req = with_step_up(req, step_up_token);
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("set_active_plugin_profile", err));
    }
    let outcome = resp.get("outcome");
    match outcome {
        None => println!("active plugin profile cleared (none active)"),
        Some(o) => {
            let id = o.get("profile_id").and_then(|v| v.as_str()).unwrap_or("");
            let dry =
                o.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false);
            let header = if dry {
                format!(
                    "activation plan for {id} (DRY RUN — no transitions \
                     dispatched):"
                )
            } else {
                format!("activated profile {id}; transitions dispatched:")
            };
            println!("{header}");
            for key in ["enabled", "disabled", "skipped"] {
                let arr = o
                    .get(key)
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if arr.is_empty() {
                    continue;
                }
                println!("  {key}:");
                for v in arr {
                    if let Some(s) = v.as_str() {
                        println!("    {s}");
                    }
                }
            }
        }
    }
    Ok(())
}

/// Run the `update-channel set` subcommand. Sends
/// `op = "set_update_channel"` under the `plugins_admin`
/// capability and prints the steward's
/// `UpdateChannelSet` response. Capability-gated and step-up-
/// aware (the framework refuses with structured subclasses
/// `target_unsupported` / `channel_unsupported` /
/// `step_up_required` / `step_up_invalid` /
/// `update_channel_store_not_configured` for the corresponding
/// failure modes).
pub fn update_channel_set(
    socket: &Path,
    target: &str,
    channel: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "set_update_channel",
            "target": target,
            "channel": channel,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("set_update_channel", err));
    }
    let target = resp.get("target").and_then(|v| v.as_str()).unwrap_or("");
    let channel = resp.get("channel").and_then(|v| v.as_str()).unwrap_or("");
    println!("update channel set:");
    println!("  target={target}");
    println!("  channel={channel}");
    Ok(())
}

/// Run the `update-channel list` subcommand. Sends
/// `op = "get_update_channels"` and prints one line per
/// recorded preference. Empty result lists nothing (no
/// preferences set; framework default `production` applies).
pub fn update_channel_list(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "get_update_channels"});
    let resp = call_with_caps(socket, &[], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("get_update_channels", err));
    }
    let entries = resp
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        println!(
            "update channels: no preferences recorded (framework default: \
             production for both targets)"
        );
        return Ok(());
    }
    println!("update channels:");
    for entry in entries {
        let target = entry.get("target").and_then(|v| v.as_str()).unwrap_or("");
        let channel =
            entry.get("channel").and_then(|v| v.as_str()).unwrap_or("");
        let set_at_ms =
            entry.get("set_at_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        let set_by = entry
            .get("set_by_principal")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        println!(
            "  target={target} channel={channel} set_at_ms={set_at_ms} \
             set_by={set_by}"
        );
    }
    Ok(())
}

/// Run the `step-up verify <username>` subcommand. Reads a
/// password from /dev/tty without echo (falls back to stdin
/// when /dev/tty is unavailable), base64-encodes it, calls
/// `step_up_auth_verify`, and prints the issued token plus
/// the expiry timestamp on success.
///
/// The password is held in a `String` only for the duration
/// of the call; the wire request is constructed and dispatched
/// in one expression so the secret never lingers in a long-lived
/// binding. Operators MUST treat the printed token as sensitive
/// — anyone holding it can act as the verified principal until
/// expiry.
pub fn step_up_verify(
    socket: &Path,
    username: &str,
    ttl_seconds: Option<u64>,
) -> Result<(), anyhow::Error> {
    let prompt = format!("Password for {username}: ");
    let secret = rpassword::prompt_password(&prompt)
        .context("reading password from /dev/tty")?;
    use base64::Engine as _;
    let secret_b64 =
        base64::engine::general_purpose::STANDARD.encode(secret.as_bytes());
    // Drop the cleartext password as soon as the encoding is done.
    drop(secret);

    let mut req = serde_json::json!({
        "op": "step_up_auth_verify",
        "username": username,
        "secret_b64": secret_b64,
    });
    if let Some(ttl) = ttl_seconds {
        req["ttl_seconds"] = serde_json::json!(ttl);
    }
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("step_up_auth_verify", err));
    }
    let token = resp
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing token in response"))?;
    let expires_at_ms = resp
        .get("expires_at_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let principal = resp
        .get("principal_username")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    println!("step-up issued:");
    println!("  principal={principal}");
    println!("  expires_at_ms={expires_at_ms}");
    println!("  token={token}");
    Ok(())
}

/// Run the `step-up revoke <token>` subcommand. Calls
/// `step_up_auth_revoke` and prints the boolean result.
/// Idempotent — revoking an unknown / already-expired token
/// returns `revoked = false` without erroring.
pub fn step_up_revoke(socket: &Path, token: &str) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "step_up_auth_revoke",
        "token": token,
    });
    let resp = call(socket, req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("step_up_auth_revoke", err));
    }
    let revoked = resp
        .get("revoked")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    println!("step-up revoke: revoked={revoked}");
    Ok(())
}

/// Run the `auth mint-bearer-token` subcommand. Negotiates the
/// `plugins_admin` capability on the connection, dispatches
/// `mint_bearer_token`, and prints the encoded token + audit
/// metadata. SSH-required developer / recovery path of the trust
/// substrate; the consumer-grade ceremony (first-boot wizard + QR
/// install + WS-upgrade session mint) lands in a follow-on release
/// alongside the per-domain CA primitive.
pub fn mint_bearer_token(
    socket: &Path,
    reason: &str,
    ttl_seconds: Option<u64>,
    scopes: Vec<String>,
) -> Result<(), anyhow::Error> {
    let mut req = serde_json::json!({
        "op": "mint_bearer_token",
        "reason": reason,
    });
    if let Some(ttl) = ttl_seconds {
        req["ttl_seconds"] = serde_json::json!(ttl);
    }
    if !scopes.is_empty() {
        req["capabilities"] = serde_json::json!(scopes);
    }
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("mint_bearer_token", err));
    }
    let token = resp
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing token in response"))?;
    let token_id = resp.get("token_id").and_then(|v| v.as_str()).unwrap_or("");
    let expires_at_ms = resp
        .get("expires_at_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let source = resp.get("source").and_then(|v| v.as_str()).unwrap_or("");

    println!("bearer token minted:");
    println!("  token_id      = {token_id}");
    println!("  source        = {source}");
    println!("  expires_at_ms = {expires_at_ms}");
    println!("  token         = {token}");
    eprintln!();
    eprintln!("  Reach the WS substrate by either:");
    eprintln!("    Authorization: Bearer {token}");
    eprintln!("    Sec-WebSocket-Protocol: evo.bearer.{token}");
    eprintln!();
    eprintln!(
        "  Treat the encoded token as a sensitive credential — anyone in"
    );
    eprintln!("  possession can act as the operator until the listed expiry.");
    Ok(())
}

/// Run the `auth create-bearer-token` subcommand. Per-credential
/// mint: takes operator-friendly name + custom scopes + expiry
/// policy. The credential record is persisted to the operator
/// inventory so it appears in `list-bearer-tokens` output. The
/// printed token bytes are returned ONCE; the framework never
/// persists them.
pub fn create_bearer_token(
    socket: &Path,
    name: &str,
    reason: &str,
    scopes: &[String],
    expires_in_seconds: Option<u64>,
) -> Result<(), anyhow::Error> {
    let mut parsed_scopes = Vec::new();
    for s in scopes {
        let (kind, scope) = s.split_once(':').ok_or_else(|| {
            anyhow::anyhow!(
                "--scope '{s}' is not in <kind>:<scope> form (e.g. read:audio)"
            )
        })?;
        if !matches!(kind, "read" | "write" | "step_up") {
            return Err(anyhow::anyhow!(
                "--scope kind '{kind}' must be one of read / write / step_up"
            ));
        }
        parsed_scopes.push(serde_json::json!({
            "kind": kind,
            "scope": scope,
        }));
    }
    let mut req = serde_json::json!({
        "op": "create_bearer_token",
        "name": name,
        "reason": reason,
        "scopes": parsed_scopes,
    });
    // Treat 0 + None as "never"; both map to omitting the
    // field so the steward sees ExpiryPolicy::Never.
    if let Some(secs) = expires_in_seconds {
        if secs > 0 {
            req["expires_in_seconds"] = serde_json::json!(secs);
        }
    }
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("create_bearer_token", err));
    }
    let token = resp
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing token in response"))?;
    let record = resp
        .get("record")
        .ok_or_else(|| anyhow::anyhow!("missing record in response"))?;
    let token_id = record
        .get("token_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let name_str = record.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let policy = record
        .get("expiry_policy")
        .map(|v| {
            v.get("kind")
                .and_then(|k| k.as_str())
                .unwrap_or("?")
                .to_string()
        })
        .unwrap_or_default();
    println!("bearer credential created:");
    println!("  token_id      = {token_id}");
    println!("  name          = {name_str}");
    println!("  expiry_policy = {policy}");
    println!("  token         = {token}");
    eprintln!();
    eprintln!("  Treat the encoded token as a sensitive credential.");
    eprintln!("  It is returned ONCE — never stored by the framework.");
    eprintln!(
        "  Use `evo-plugin-tool admin auth list-bearer-tokens` to enumerate metadata."
    );
    Ok(())
}

/// Run the `auth list-bearer-tokens` subcommand. Reads the
/// operator-managed credential inventory (metadata only).
pub fn list_bearer_tokens(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({ "op": "list_bearer_tokens" });
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_bearer_tokens", err));
    }
    let records = resp
        .get("records")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("missing records array in response"))?;
    if records.is_empty() {
        println!("(no credentials in inventory)");
        return Ok(());
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    // Expiry-warning horizon. Credentials whose expiry is
    // within this window are flagged so the operator has
    // visibility on which consumers need a fresh credential
    // soon. Seven days mirrors typical IT rotation cadences
    // without crowding the warning column with credentials
    // that just expired.
    const EXPIRING_SOON_WINDOW_MS: u64 = 7 * 24 * 60 * 60 * 1000;
    println!(
        "{:<24} {:<20} {:<18} {:<24}  scopes",
        "token_id", "name", "status", "expiry"
    );
    let mut expiring_soon_count = 0usize;
    for r in records {
        let token_id = r.get("token_id").and_then(|v| v.as_str()).unwrap_or("");
        let name = r.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let revoked = r
            .get("revoked_at_ms")
            .map(|v| !v.is_null())
            .unwrap_or(false);
        let expires_at_ms = r.get("expires_at_ms").and_then(|v| v.as_u64());
        let status = if revoked {
            "revoked".to_string()
        } else {
            match expires_at_ms {
                None => "active".to_string(),
                Some(t) if t <= now_ms => "expired".to_string(),
                Some(t) if t <= now_ms + EXPIRING_SOON_WINDOW_MS => {
                    expiring_soon_count += 1;
                    let secs_left = (t - now_ms) / 1000;
                    let days_left = secs_left / 86_400;
                    if days_left > 0 {
                        format!("expiring ({days_left}d)")
                    } else {
                        let hrs_left = secs_left / 3_600;
                        format!("expiring ({hrs_left}h)")
                    }
                }
                Some(_) => "active".to_string(),
            }
        };
        let expiry = match expires_at_ms {
            Some(t) => {
                let delta = t.saturating_sub(now_ms);
                let days = delta / 86_400_000;
                if days > 365 * 50 {
                    "effectively never".to_string()
                } else if days > 365 {
                    let years = days / 365;
                    format!("~{years}y")
                } else if days > 0 {
                    format!("{days}d")
                } else {
                    let hrs = delta / 3_600_000;
                    format!("{hrs}h")
                }
            }
            None => "never".to_string(),
        };
        let scopes_str = r
            .get("scopes")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|s| {
                        let kind = s
                            .get("kind")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let scope = s
                            .get("scope")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        format!("{kind}:{scope}")
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        let token_display = if token_id.len() > 22 {
            format!("{}…", &token_id[..21])
        } else {
            token_id.to_string()
        };
        println!(
            "{token_display:<24} {name:<20} {status:<18} {expiry:<24}  {scopes_str}"
        );
    }
    if expiring_soon_count > 0 {
        eprintln!();
        eprintln!(
            "  WARN: {expiring_soon_count} credential(s) expire within the next 7 days."
        );
        eprintln!(
            "  Run `evo-plugin-tool admin auth create-bearer-token` to mint a"
        );
        eprintln!(
            "  replacement and `... revoke-bearer-token` to retire the old one."
        );
    }
    Ok(())
}

/// Run the `auth revoke-bearer-token` subcommand. Revokes a
/// previously-minted bearer credential by token id; the
/// revocation persists to `revoked.json` so it survives
/// steward restarts.
pub fn revoke_bearer_token(
    socket: &Path,
    token_id: &str,
    reason: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "revoke_bearer_token",
        "token_id": token_id,
        "reason": reason,
    });
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("revoke_bearer_token", err));
    }
    let record = resp
        .get("record")
        .ok_or_else(|| anyhow::anyhow!("missing record in response"))?;
    let id = record
        .get("token_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let revoked_at_ms = record
        .get("revoked_at_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    println!("bearer credential revoked:");
    println!("  token_id        = {id}");
    println!("  revoked_at_ms   = {revoked_at_ms}");
    Ok(())
}

/// Run the `auth reset-credentials-to-open` subcommand. Purges
/// every credential record + revocation entry and prepares the
/// steward to admit Open tier on next boot. Requires
/// step_up:system_admin.
pub fn reset_credentials_to_open(
    socket: &Path,
    reason: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "reset_credentials_to_open",
        "reason": reason,
    });
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("reset_credentials_to_open", err));
    }
    let records_purged = resp
        .get("records_purged")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let revocations_cleared = resp
        .get("revocations_cleared")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    println!("credential substrate reset to Open tier:");
    println!("  records_purged       = {records_purged}");
    println!("  revocations_cleared  = {revocations_cleared}");
    eprintln!();
    eprintln!("  Restart the steward (`systemctl restart evo`) to apply the");
    eprintln!("  Open-tier admission policy on next boot.");
    Ok(())
}

/// Run the `prompt list` subcommand. Sends
/// `op = "list_user_interactions"` and prints one line per open
/// prompt. Requires the `user_interaction_responder` capability.
pub fn prompt_list(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "list_user_interactions"});
    let resp = call_with_caps(socket, &["user_interaction_responder"], req)?;
    print_prompt_list(&resp)
}

/// Run the `prompt answer-text` subcommand. Sends a Text-shaped
/// `answer_user_interaction` and prints the result.
pub fn prompt_answer_text(
    socket: &Path,
    plugin: &str,
    prompt_id: &str,
    value: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "answer_user_interaction",
        "plugin": plugin,
        "prompt_id": prompt_id,
        "response": {"kind": "text", "value": value},
    });
    let resp = call_with_caps(socket, &["user_interaction_responder"], req)?;
    print_prompt_answer(&resp)
}

/// Run the `prompt cancel` subcommand. Sends
/// `cancel_user_interaction` and prints the result.
pub fn prompt_cancel(
    socket: &Path,
    plugin: &str,
    prompt_id: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "cancel_user_interaction",
        "plugin": plugin,
        "prompt_id": prompt_id,
    });
    let resp = call_with_caps(socket, &["user_interaction_responder"], req)?;
    print_prompt_cancel(&resp)
}

/// Run the `purge-state` subcommand.
pub fn purge_state(
    socket: &Path,
    plugin: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "purge_plugin_state",
            "plugin": plugin,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    print_lifecycle_outcome(&resp)
}

/// Run the `reload catalogue` subcommand.
pub fn reload_catalogue(
    socket: &Path,
    source: ReloadSource,
    dry_run: bool,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "reload_catalogue",
            "source": reload_source_json(&source)?,
            "dry_run": dry_run,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    print_catalogue_reload(&resp)
}

/// Run the `reload manifest` subcommand.
pub fn reload_manifest(
    socket: &Path,
    plugin: &str,
    source: ReloadSource,
    dry_run: bool,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "reload_manifest",
            "plugin": plugin,
            "source": reload_source_json(&source)?,
            "dry_run": dry_run,
        }),
        step_up_token,
    );
    let resp = call(socket, req)?;
    print_manifest_reload(&resp)
}

/// Run the `reconcile list` subcommand. Read-only: no
/// capability negotiation; default-allowed by the steward.
pub fn reconcile_list(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "list_reconciliation_pairs"});
    let resp = call_with_caps(socket, &[], req)?;
    print_reconciliation_pairs(&resp)
}

/// Run the `reconcile project` subcommand. Read-only.
pub fn reconcile_project(
    socket: &Path,
    pair: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "project_reconciliation_pair",
        "pair": pair,
    });
    let resp = call_with_caps(socket, &[], req)?;
    print_reconciliation_pair_projection(&resp)
}

/// Run the `reconcile now` subcommand. Negotiates the
/// `reconciliation_admin` capability (distinct from
/// `plugins_admin`).
pub fn reconcile_now(socket: &Path, pair: &str) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "reconcile_pair_now",
        "pair": pair,
    });
    let resp = call_with_caps(socket, &["reconciliation_admin"], req)?;
    print_reconcile_now(&resp)
}

/// Rack name reserved for the flight-mode hardware control
/// surface. Distributions that ship flight-mode-controllable
/// hardware declare a rack of this name in their catalogue;
/// the framework imposes no class-name taxonomy beyond the
/// rack name itself.
const FLIGHT_MODE_RACK: &str = "flight_mode";

/// Run the `flight list` subcommand. Walks the `flight_mode`
/// rack via `op = "project_rack"` and queries each shelf via
/// `op = "request"` with `request_type = "flight_mode.query"`.
/// Read-only; no admin capability required.
pub fn flight_list(socket: &Path) -> Result<(), anyhow::Error> {
    let project_req = serde_json::json!({
        "op": "project_rack",
        "rack": FLIGHT_MODE_RACK,
    });
    let project_resp = call_with_caps(socket, &[], project_req)?;
    if let Some(err) = project_resp.get("error") {
        return Err(format_error("project_rack", err));
    }
    // Tolerate the `not_found` shape that `project_rack`
    // returns for distributions that do not declare a
    // flight_mode rack: surface a friendly message rather
    // than a generic error.
    let shelves = project_resp
        .get("shelves")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if shelves.is_empty() {
        println!(
            "no flight_mode rack on this distribution (or no shelves \
             declared)"
        );
        return Ok(());
    }
    println!("flight mode classes:");
    for shelf in &shelves {
        let shelf_name =
            shelf.get("name").and_then(Value::as_str).unwrap_or("?");
        let qualified = format!("{FLIGHT_MODE_RACK}.{shelf_name}");
        let query_req = serde_json::json!({
            "op": "request",
            "shelf": qualified,
            "request_type": "flight_mode.query",
            "payload_b64": "",
        });
        let resp = call_with_caps(socket, &[], query_req)?;
        let state = match resp.get("error") {
            Some(_) => "<query refused>".into(),
            None => render_flight_state_from_response(&resp),
        };
        println!("  {qualified:38} {state}");
    }
    Ok(())
}

/// Run the `flight set <class> <on|off>` subcommand.
pub fn flight_set(
    socket: &Path,
    class: &str,
    on: bool,
) -> Result<(), anyhow::Error> {
    let qualified = format!("{FLIGHT_MODE_RACK}.{class}");
    let payload = serde_json::json!({"on": on});
    let payload_b64 = base64::engine::general_purpose::STANDARD
        .encode(serde_json::to_vec(&payload)?);
    let req = serde_json::json!({
        "op": "request",
        "shelf": qualified,
        "request_type": "flight_mode.set",
        "payload_b64": payload_b64,
    });
    let resp = call_with_caps(socket, &[], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("flight_mode.set", err));
    }
    println!(
        "flight mode {qualified}: {}",
        if on {
            "ACTIVE (radio off)"
        } else {
            "CLEARED (radio on)"
        }
    );
    Ok(())
}

/// Run the `flight all <on|off>` subcommand. Walks the rack and
/// applies the requested state to every shelf in catalogue
/// order. Per-shelf failures are surfaced to the operator but
/// do not abort the walk; the operator can re-run for
/// restartable bulk control.
pub fn flight_all(socket: &Path, on: bool) -> Result<(), anyhow::Error> {
    let project_req = serde_json::json!({
        "op": "project_rack",
        "rack": FLIGHT_MODE_RACK,
    });
    let project_resp = call_with_caps(socket, &[], project_req)?;
    if let Some(err) = project_resp.get("error") {
        return Err(format_error("project_rack", err));
    }
    let shelves = project_resp
        .get("shelves")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if shelves.is_empty() {
        println!("no flight_mode rack on this distribution");
        return Ok(());
    }
    let target = if on {
        "ACTIVE (radio off)"
    } else {
        "CLEARED (radio on)"
    };
    println!("setting flight mode {target} on every class:");
    let mut had_failure = false;
    for shelf in &shelves {
        let shelf_name =
            shelf.get("name").and_then(Value::as_str).unwrap_or("?");
        match flight_set(socket, shelf_name, on) {
            Ok(()) => {}
            Err(e) => {
                println!("  ! {shelf_name}: {e}");
                had_failure = true;
            }
        }
    }
    if had_failure {
        Err(anyhow::anyhow!(
            "one or more shelves refused; re-run to retry per-class"
        ))
    } else {
        Ok(())
    }
}

/// Pretty-print the on/off state from a `flight_mode.query`
/// response payload. The wire response carries an opaque
/// `payload_b64` field; the device plugin documents its
/// shape, but the canonical form per the ADR is `{on, last_changed_at_ms}`.
fn render_flight_state_from_response(resp: &Value) -> String {
    let payload_b64 = resp
        .get("payload_b64")
        .and_then(Value::as_str)
        .unwrap_or("");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload_b64.as_bytes())
        .unwrap_or_default();
    let parsed: Option<Value> = serde_json::from_slice(&bytes).ok();
    let on = parsed
        .as_ref()
        .and_then(|v| v.get("on"))
        .and_then(Value::as_bool);
    match on {
        Some(true) => "ACTIVE (radio off)".to_string(),
        Some(false) => "CLEARED (radio on)".to_string(),
        None => "<unknown shape>".to_string(),
    }
}

/// Run the `admin grammar list` subcommand. Negotiates
/// `grammar_admin`, calls `list_grammar_orphans`, and renders
/// the rows as a table.
pub fn grammar_list(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "list_grammar_orphans"});
    let resp = call_with_caps(socket, &["grammar_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_grammar_orphans", err));
    }
    let entries = resp
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        println!("no pending grammar orphans");
        return Ok(());
    }
    println!("grammar orphans:");
    println!(
        "  {:30} {:11} {:>10} {:>20} {:30}",
        "subject_type", "status", "count", "last_observed", "reason"
    );
    for e in &entries {
        let subject_type =
            e.get("subject_type").and_then(Value::as_str).unwrap_or("?");
        let status = e.get("status").and_then(Value::as_str).unwrap_or("?");
        let count = e.get("count").and_then(Value::as_u64).unwrap_or(0);
        let last_observed = e
            .get("last_observed_at_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let reason = e
            .get("accepted_reason")
            .and_then(Value::as_str)
            .unwrap_or("");
        println!(
            "  {subject_type:30} {status:11} {count:>10} \
             {last_observed:>20} {reason:30}"
        );
    }
    Ok(())
}

/// Run the `admin grammar plan` subcommand. Issues a dry-run
/// `migrate_grammar_orphans` and renders the plan including
/// migrated count, target-type breakdown, sample IDs, and
/// duration estimate.
pub fn grammar_plan(
    socket: &Path,
    from_type: &str,
    to_type: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "migrate_grammar_orphans",
        "from_type": from_type,
        "strategy": { "kind": "rename", "to_type": to_type },
        "dry_run": true,
    });
    let resp = call_with_caps(socket, &["grammar_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("migrate_grammar_orphans (dry_run)", err));
    }
    let migrated = resp
        .get("migrated_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let duration_ms =
        resp.get("duration_ms").and_then(Value::as_u64).unwrap_or(0);
    println!("plan: migrate {from_type:?} -> {to_type:?}");
    println!("  would migrate: {migrated} subject(s)");
    println!("  evaluated in:  {duration_ms} ms (dry-run)");
    if let Some(breakdown) =
        resp.get("target_type_breakdown").and_then(Value::as_array)
    {
        if !breakdown.is_empty() {
            println!("  target-type breakdown:");
            for entry in breakdown {
                let to =
                    entry.get("to_type").and_then(Value::as_str).unwrap_or("?");
                let n = entry.get("count").and_then(Value::as_u64).unwrap_or(0);
                println!("    {to:30} {n:>10}");
            }
        }
    }
    if let Some(first) = resp.get("sample_first").and_then(Value::as_array) {
        if !first.is_empty() {
            println!("  sample (first):");
            for id in first {
                if let Some(id) = id.as_str() {
                    println!("    {id}");
                }
            }
        }
    }
    if let Some(last) = resp.get("sample_last").and_then(Value::as_array) {
        if !last.is_empty() {
            println!("  sample (last):");
            for id in last {
                if let Some(id) = id.as_str() {
                    println!("    {id}");
                }
            }
        }
    }
    Ok(())
}

/// Run the `admin grammar migrate` subcommand. Issues the real
/// `migrate_grammar_orphans` call and prints the outcome.
pub fn grammar_migrate(
    socket: &Path,
    from_type: &str,
    to_type: &str,
    reason: Option<&str>,
    batch_size: Option<u32>,
    max_subjects: Option<u32>,
) -> Result<(), anyhow::Error> {
    let mut req = serde_json::json!({
        "op": "migrate_grammar_orphans",
        "from_type": from_type,
        "strategy": { "kind": "rename", "to_type": to_type },
        "dry_run": false,
    });
    if let Some(r) = reason {
        req.as_object_mut()
            .unwrap()
            .insert("reason".to_string(), Value::String(r.to_string()));
    }
    if let Some(b) = batch_size {
        req.as_object_mut()
            .unwrap()
            .insert("batch_size".to_string(), Value::from(b));
    }
    if let Some(m) = max_subjects {
        req.as_object_mut()
            .unwrap()
            .insert("max_subjects".to_string(), Value::from(m));
    }
    let resp = call_with_caps(socket, &["grammar_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("migrate_grammar_orphans", err));
    }
    let migration_id = resp
        .get("migration_id")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let migrated = resp
        .get("migrated_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let duration_ms =
        resp.get("duration_ms").and_then(Value::as_u64).unwrap_or(0);
    println!(
        "migrated {from_type:?} -> {to_type:?}: {migrated} subject(s) in \
         {duration_ms} ms (migration_id: {migration_id})"
    );
    Ok(())
}

/// Run the `admin grammar accept` subcommand. Records the
/// deliberate-acceptance decision via
/// `accept_grammar_orphans`.
pub fn grammar_accept(
    socket: &Path,
    from_type: &str,
    reason: &str,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "accept_grammar_orphans",
        "from_type": from_type,
        "reason": reason,
    });
    let resp = call_with_caps(socket, &["grammar_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("accept_grammar_orphans", err));
    }
    let accepted = resp
        .get("accepted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if accepted {
        println!("accepted orphans of type {from_type:?}: {reason}");
    } else {
        println!("orphans of type {from_type:?} were already accepted");
    }
    Ok(())
}

/// Run the `diagnose` subcommand. Aggregates `list_plugins` +
/// the plugin's manifest from disk into a human-readable
/// diagnostic view. Recent-events aggregation via
/// `subscribe_happenings` lands in a follow-up.
pub fn diagnose(socket: &Path, plugin: &str) -> Result<(), anyhow::Error> {
    let list = call(socket, serde_json::json!({"op": "list_plugins"}))?;
    let entry = find_plugin_entry(&list, plugin);

    println!("plugin: {plugin}");
    match entry {
        Some(e) => {
            println!("  admission:");
            if let Some(shelf) = e.get("shelf").and_then(Value::as_str) {
                println!("    shelf:            {shelf}");
            }
            if let Some(kind) =
                e.get("interaction_kind").and_then(Value::as_str)
            {
                println!("    interaction kind: {kind}");
            }
            println!("    state:            admitted");
        }
        None => {
            println!("  admission:");
            println!("    state:            not admitted");
        }
    }

    Ok(())
}

fn reload_source_json(s: &ReloadSource) -> Result<Value, anyhow::Error> {
    Ok(match s {
        ReloadSource::Inline(toml) => {
            serde_json::json!({"kind": "inline", "toml": toml})
        }
        ReloadSource::Path(p) => {
            serde_json::json!({"kind": "path", "path": p})
        }
    })
}

fn find_plugin_entry<'a>(list: &'a Value, plugin: &str) -> Option<&'a Value> {
    list.get("plugins")?
        .as_array()?
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some(plugin))
}

/// Print a `UserInteractions` reply.
fn print_prompt_list(resp: &Value) -> Result<(), anyhow::Error> {
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_user_interactions", err));
    }
    let prompts =
        resp.get("prompts")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "list_user_interactions: response missing `prompts` array"
                )
            })?;
    if prompts.is_empty() {
        println!("(no open prompts)");
        return Ok(());
    }
    println!("open prompts:");
    for entry in prompts {
        let plugin = entry.get("plugin").and_then(Value::as_str).unwrap_or("?");
        let prompt = entry.get("prompt").cloned().unwrap_or(Value::Null);
        let prompt_id = prompt
            .get("prompt_id")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let kind = prompt
            .get("prompt_type")
            .and_then(|t| t.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        println!("  plugin={plugin} prompt_id={prompt_id} kind={kind}");
    }
    Ok(())
}

/// Print an `UserInteractionAnswered` reply.
fn print_prompt_answer(resp: &Value) -> Result<(), anyhow::Error> {
    if let Some(err) = resp.get("error") {
        return Err(format_error("answer_user_interaction", err));
    }
    let plugin = resp
        .get("plugin")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let prompt_id = resp
        .get("prompt_id")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let answered = resp
        .get("answered")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    println!("prompt answered:");
    println!("  plugin:    {plugin}");
    println!("  prompt_id: {prompt_id}");
    println!("  answered:  {answered}");
    Ok(())
}

/// Print an `UserInteractionCancelled` reply.
fn print_prompt_cancel(resp: &Value) -> Result<(), anyhow::Error> {
    if let Some(err) = resp.get("error") {
        return Err(format_error("cancel_user_interaction", err));
    }
    let plugin = resp
        .get("plugin")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let prompt_id = resp
        .get("prompt_id")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let cancelled = resp
        .get("cancelled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    println!("prompt cancelled:");
    println!("  plugin:    {plugin}");
    println!("  prompt_id: {prompt_id}");
    println!("  cancelled: {cancelled}");
    Ok(())
}

/// Print a `PluginReloaded` reply.
fn print_plugin_reload(resp: &Value) -> Result<(), anyhow::Error> {
    if let Some(err) = resp.get("error") {
        return Err(format_error("reload_plugin", err));
    }
    let plugin = resp
        .get("plugin")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let reloaded = resp
        .get("plugin_reload")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    println!("plugin reloaded:");
    println!("  plugin:   {plugin}");
    println!("  reloaded: {reloaded}");
    Ok(())
}

fn print_publisher_trust_granted(resp: &Value) -> Result<(), anyhow::Error> {
    if let Some(err) = resp.get("error") {
        return Err(format_error("grant_publisher_trust", err));
    }
    let publisher_id = resp
        .get("publisher_id")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let key_file = resp.get("key_file").and_then(Value::as_str).unwrap_or("?");
    println!("publisher trust granted:");
    println!("  publisher_id: {publisher_id}");
    println!("  key_file:     {key_file}");
    Ok(())
}

fn print_publisher_trust_revoked(resp: &Value) -> Result<(), anyhow::Error> {
    if let Some(err) = resp.get("error") {
        return Err(format_error("revoke_publisher_trust", err));
    }
    let publisher_id = resp
        .get("publisher_id")
        .and_then(Value::as_str)
        .unwrap_or("?");
    println!("publisher trust revoked: {publisher_id}");
    Ok(())
}

fn print_publisher_trust_list(resp: &Value) -> Result<(), anyhow::Error> {
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_publisher_trust", err));
    }
    let entries = resp
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        println!("(no publisher trust records)");
        return Ok(());
    }
    for entry in &entries {
        let publisher_id = entry
            .get("publisher_id")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let display_name = entry
            .get("display_name")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let trust_level = entry
            .get("trust_level")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let granted_via = entry
            .get("granted_via")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let scope_kind = entry
            .get("scope_kind")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let granted_at_ms = entry
            .get("granted_at_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        println!("- {publisher_id}");
        println!("    display_name:    {display_name}");
        println!("    trust_level:     {trust_level}");
        println!("    granted_via:     {granted_via}");
        println!("    granted_at_ms:   {granted_at_ms}");
        println!("    scope_kind:      {scope_kind}");
        if let Some(scope) =
            entry.get("scope_per_plugin").and_then(Value::as_array)
        {
            if !scope.is_empty() {
                println!("    scope_plugins:");
                for plugin in scope {
                    if let Some(name) = plugin.as_str() {
                        println!("      - {name}");
                    }
                }
            }
        }
        if let Some(key_file) = entry.get("key_file").and_then(Value::as_str) {
            println!("    key_file:        {key_file}");
        }
    }
    Ok(())
}

fn print_plugin_registry_registered(resp: &Value) -> Result<(), anyhow::Error> {
    if let Some(err) = resp.get("error") {
        return Err(format_error("register_plugin_registry", err));
    }
    let slug = resp.get("slug").and_then(Value::as_str).unwrap_or("?");
    println!("plugin registry registered: {slug}");
    Ok(())
}

fn print_plugin_registry_unregistered(
    resp: &Value,
) -> Result<(), anyhow::Error> {
    if let Some(err) = resp.get("error") {
        return Err(format_error("unregister_plugin_registry", err));
    }
    let slug = resp.get("slug").and_then(Value::as_str).unwrap_or("?");
    println!("plugin registry unregistered: {slug}");
    Ok(())
}

fn print_plugin_registries(resp: &Value) -> Result<(), anyhow::Error> {
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_plugin_registries", err));
    }
    let entries = resp
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        println!("(no plugin registries registered)");
        return Ok(());
    }
    for entry in &entries {
        let slug = entry.get("slug").and_then(Value::as_str).unwrap_or("?");
        let manifest_url = entry
            .get("manifest_url")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let interval = entry
            .get("poll_interval_secs")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let last = entry
            .get("last_refreshed_at_ms")
            .and_then(Value::as_u64)
            .map(|v| v.to_string())
            .unwrap_or_else(|| "(never)".to_string());
        let count = entry
            .get("plugin_count")
            .and_then(Value::as_u64)
            .map(|v| v.to_string())
            .unwrap_or_else(|| "(no manifest)".to_string());
        println!("- {slug}");
        println!("    manifest_url:        {manifest_url}");
        println!("    poll_interval_secs:  {interval}");
        println!("    last_refreshed_at:   {last}");
        println!("    plugin_count:        {count}");
    }
    Ok(())
}

fn print_plugin_registry_refreshed(resp: &Value) -> Result<(), anyhow::Error> {
    if let Some(err) = resp.get("error") {
        return Err(format_error("refresh_plugin_registry", err));
    }
    let slug = resp.get("slug").and_then(Value::as_str).unwrap_or("?");
    let refreshed_at = resp
        .get("refreshed_at_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let plugin_count = resp
        .get("plugin_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    println!("plugin registry refreshed:");
    println!("  slug:            {slug}");
    println!("  refreshed_at_ms: {refreshed_at}");
    println!("  plugin_count:    {plugin_count}");
    Ok(())
}

fn print_plugin_install_staged(resp: &Value) -> Result<(), anyhow::Error> {
    if let Some(err) = resp.get("error") {
        return Err(format_error("install_plugin_from_url", err));
    }
    let url = resp.get("url").and_then(Value::as_str).unwrap_or("?");
    let staged_path = resp
        .get("staged_path")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let bytes = resp
        .get("bytes_fetched")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    println!("plugin install staged:");
    println!("  url:           {url}");
    println!("  staged_path:   {staged_path}");
    println!("  bytes_fetched: {bytes}");
    println!(
        "  note:          admission is asynchronous; track \
         outcome via the happenings bus (`evo-plugin-tool admin \
         subscribe-happenings`)"
    );
    Ok(())
}

fn print_plan_fired(resp: &Value) -> Result<(), anyhow::Error> {
    if let Some(err) = resp.get("error") {
        return Err(format_error("fire_plan", err));
    }
    let plan_id = resp
        .get("plan_id")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let fired = resp
        .get("plan_fired")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    println!("plan fired:");
    println!("  plan_id: {plan_id}");
    println!("  fired:   {fired}");
    println!(
        "  note:    fire is asynchronous; track execution via \
         the happenings bus (`evo-plugin-tool admin subscribe-happenings`)"
    );
    Ok(())
}

/// Print a `Capabilities` reply.
fn print_describe_capabilities(resp: &Value) -> Result<(), anyhow::Error> {
    if let Some(err) = resp.get("error") {
        return Err(format_error("describe_capabilities", err));
    }
    let wire_version = resp
        .get("wire_version")
        .and_then(Value::as_u64)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "?".to_string());
    let catalogue_source = resp
        .get("catalogue_source")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let clock_trust = resp
        .get("clock_trust")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let has_battery_rtc = resp
        .get("has_battery_rtc")
        .and_then(Value::as_bool)
        .map(|b| b.to_string())
        .unwrap_or_else(|| "?".to_string());
    let ops_count = resp
        .get("ops")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    let features_count = resp
        .get("features")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    println!("steward capabilities:");
    println!("  wire_version:     {wire_version}");
    println!("  catalogue_source: {catalogue_source}");
    println!("  clock_trust:      {clock_trust}");
    if let Some(step) = resp.get("last_clock_step") {
        let delta = step.get("delta_seconds").and_then(Value::as_i64);
        let at_ms = step.get("at_ms").and_then(Value::as_u64);
        if let (Some(d), Some(t)) = (delta, at_ms) {
            let sign = if d >= 0 { "+" } else { "" };
            println!(
                "  last_clock_step:  {sign}{d}s at unix_ms={t} \
                 (most recent NTP step > 2 s; persists until \
                 the next step or steward restart)"
            );
        }
    }
    println!("  has_battery_rtc:  {has_battery_rtc}");
    println!("  ops:              {ops_count} entries");
    println!("  features:         {features_count} entries");
    Ok(())
}

/// Print a `PluginLifecycle` reply.
fn print_lifecycle_outcome(resp: &Value) -> Result<(), anyhow::Error> {
    if let Some(err) = resp.get("error") {
        return Err(format_error("plugin lifecycle op", err));
    }
    let plugin = resp
        .get("plugin")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let admitted = resp
        .get("was_currently_admitted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let applied = resp
        .get("change_applied")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    println!("plugin: {plugin}");
    println!("  was currently admitted: {admitted}");
    println!("  change applied:         {applied}");
    Ok(())
}

fn print_catalogue_reload(resp: &Value) -> Result<(), anyhow::Error> {
    if let Some(err) = resp.get("error") {
        return Err(format_error("reload_catalogue", err));
    }
    let from_v = resp
        .get("from_schema_version")
        .cloned()
        .unwrap_or(Value::Null);
    let to_v = resp
        .get("to_schema_version")
        .cloned()
        .unwrap_or(Value::Null);
    let racks = resp.get("rack_count").cloned().unwrap_or(Value::Null);
    let dur = resp.get("duration_ms").cloned().unwrap_or(Value::Null);
    let dry = resp
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    println!("catalogue reload:");
    println!("  schema:        {from_v} -> {to_v}");
    println!("  rack count:    {racks}");
    println!("  duration ms:   {dur}");
    println!("  dry run:       {dry}");
    Ok(())
}

fn print_manifest_reload(resp: &Value) -> Result<(), anyhow::Error> {
    if let Some(err) = resp.get("error") {
        return Err(format_error("reload_manifest", err));
    }
    let plugin = resp
        .get("plugin")
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");
    let from_v = resp
        .get("from_manifest_version")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let to_v = resp
        .get("to_manifest_version")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let dur = resp.get("duration_ms").cloned().unwrap_or(Value::Null);
    let dry = resp
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    println!("manifest reload:");
    println!("  plugin:        {plugin}");
    println!("  version:       {from_v} -> {to_v}");
    println!("  duration ms:   {dur}");
    println!("  dry run:       {dry}");
    Ok(())
}

fn print_reconciliation_pairs(resp: &Value) -> Result<(), anyhow::Error> {
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_reconciliation_pairs", err));
    }
    let pairs =
        resp.get("pairs").and_then(Value::as_array).ok_or_else(|| {
            anyhow::anyhow!(
                "list_reconciliation_pairs: response missing `pairs` array"
            )
        })?;
    if pairs.is_empty() {
        println!("(no active reconciliation pairs)");
        return Ok(());
    }
    println!("reconciliation pairs:");
    for p in pairs {
        let id = p.get("pair_id").and_then(Value::as_str).unwrap_or("?");
        let composer = p
            .get("composer_shelf")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let warden =
            p.get("warden_shelf").and_then(Value::as_str).unwrap_or("?");
        let gen_ = p.get("generation").and_then(Value::as_u64).unwrap_or(0);
        let last = p
            .get("last_applied_at_ms")
            .and_then(Value::as_u64)
            .map(|v| v.to_string())
            .unwrap_or_else(|| "(never)".to_string());
        println!("  {id}");
        println!("    composer shelf:    {composer}");
        println!("    warden shelf:      {warden}");
        println!("    generation:        {gen_}");
        println!("    last applied (ms): {last}");
    }
    Ok(())
}

fn print_reconciliation_pair_projection(
    resp: &Value,
) -> Result<(), anyhow::Error> {
    if let Some(err) = resp.get("error") {
        return Err(format_error("project_reconciliation_pair", err));
    }
    let pair = resp.get("pair").and_then(Value::as_str).unwrap_or("?");
    let gen_ = resp.get("generation").and_then(Value::as_u64).unwrap_or(0);
    let applied = resp.get("applied_state").cloned().unwrap_or(Value::Null);
    let applied_pretty = serde_json::to_string_pretty(&applied)
        .unwrap_or_else(|_| applied.to_string());
    println!("reconciliation pair projection:");
    println!("  pair:           {pair}");
    println!("  generation:     {gen_}");
    println!("  applied state:");
    for line in applied_pretty.lines() {
        println!("    {line}");
    }
    Ok(())
}

fn print_reconcile_now(resp: &Value) -> Result<(), anyhow::Error> {
    if let Some(err) = resp.get("error") {
        return Err(format_error("reconcile_pair_now", err));
    }
    let pair = resp.get("pair").and_then(Value::as_str).unwrap_or("?");
    println!("reconcile now:");
    println!("  pair:    {pair}");
    println!("  status:  triggered (outcome rides the happenings stream)");
    Ok(())
}

fn format_error(op: &str, err: &Value) -> anyhow::Error {
    let class = err
        .get("class")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let msg = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("(no message)");
    let subclass = err
        .get("details")
        .and_then(|d| d.get("subclass"))
        .and_then(Value::as_str)
        .unwrap_or("");
    anyhow::anyhow!(
        "{op} refused: class={class} subclass={subclass} message={msg}"
    )
}

/// Open a single short-lived connection, negotiate
/// `plugins_admin`, send `req`, read one response, return it.
/// Convenience wrapper for the plugin-lifecycle ops; routes
/// through [`call_with_caps`] with a fixed `["plugins_admin"]`
/// requirement.
fn call(socket: &Path, req: Value) -> Result<Value, anyhow::Error> {
    call_with_caps(socket, &["plugins_admin"], req)
}

/// Open a single short-lived connection, negotiate every
/// capability in `required`, send `req`, read one response,
/// return it. When `required` is empty the negotiate step is
/// skipped entirely; the request is dispatched on a fresh
/// (unauthorised) connection. Wraps the whole exchange in a
/// per-call deadline so a wedged steward surfaces as a clean
/// timeout rather than a hung terminal.
fn call_with_caps(
    socket: &Path,
    required: &[&str],
    req: Value,
) -> Result<Value, anyhow::Error> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("building tokio runtime for admin call")?;
    runtime.block_on(async move {
        let deadline = Duration::from_secs(CALL_DEADLINE_SECS);
        tokio::time::timeout(deadline, call_async(socket, required, req))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "admin call timed out after {CALL_DEADLINE_SECS}s"
                )
            })?
    })
}

async fn call_async(
    socket: &Path,
    required: &[&str],
    req: Value,
) -> Result<Value, anyhow::Error> {
    let mut stream = UnixStream::connect(socket).await.with_context(|| {
        format!("connecting to steward socket {}", socket.display())
    })?;

    if !required.is_empty() {
        let neg = serde_json::json!({
            "op": "negotiate",
            "capabilities": required,
        });
        write_frame(&mut stream, &neg).await?;
        let neg_resp = read_frame(&mut stream).await?;
        let granted: Vec<&str> = neg_resp
            .get("granted")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        for cap in required {
            if !granted.iter().any(|g| g == cap) {
                return Err(anyhow::anyhow!(
                    "steward refused to grant {cap} on this connection \
                     (check /etc/evo/client_acl.toml)"
                ));
            }
        }
    }

    // Send the operator op.
    write_frame(&mut stream, &req).await?;
    read_frame(&mut stream).await
}

async fn write_frame(
    stream: &mut UnixStream,
    body: &Value,
) -> Result<(), anyhow::Error> {
    let bytes =
        serde_json::to_vec(body).context("serialising request frame")?;
    if bytes.len() > MAX_FRAME_SIZE {
        return Err(anyhow::anyhow!(
            "request frame too large: {} bytes (max {MAX_FRAME_SIZE})",
            bytes.len()
        ));
    }
    let len = (bytes.len() as u32).to_be_bytes();
    stream
        .write_all(&len)
        .await
        .context("writing frame length")?;
    stream
        .write_all(&bytes)
        .await
        .context("writing frame body")?;
    stream.flush().await.context("flushing frame")?;
    Ok(())
}

async fn read_frame(stream: &mut UnixStream) -> Result<Value, anyhow::Error> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .context("reading frame length")?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 {
        return Err(anyhow::anyhow!("zero-length response frame"));
    }
    if len > MAX_FRAME_SIZE {
        return Err(anyhow::anyhow!(
            "response frame too large: {len} bytes (max {MAX_FRAME_SIZE})"
        ));
    }
    let mut body = vec![0u8; len];
    stream
        .read_exact(&mut body)
        .await
        .context("reading frame body")?;
    serde_json::from_slice(&body).context("parsing response JSON")
}

/// Run the `domain list` subcommand. Projects the trust ledger
/// composed with live discovery state + per-peer display-name
/// cache.
pub fn domain_list(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "list_domain_members"});
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("list_domain_members", err));
    }
    let entries = resp
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        println!("domain roster: (empty — local device not yet seeded)");
        return Ok(());
    }
    println!("domain roster ({n}):", n = entries.len());
    for e in &entries {
        let device_id =
            e.get("device_id").and_then(Value::as_str).unwrap_or("?");
        let display_name =
            e.get("display_name").and_then(Value::as_str).unwrap_or("?");
        let is_local =
            e.get("is_local").and_then(Value::as_bool).unwrap_or(false);
        let is_advertising = e
            .get("is_currently_advertising")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let is_session_connected = e
            .get("is_session_connected")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let is_revoked = e
            .get("is_revoked")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let admitted_at_ms =
            e.get("admitted_at_ms").and_then(Value::as_u64).unwrap_or(0);
        let admitted_by = e
            .get("admitted_by_device_id")
            .and_then(Value::as_str)
            .unwrap_or("<seed>");

        // Status tags. `online` covers the common case where
        // both discovery freshness and audio-plane heartbeat
        // are bright. `connected (mdns-quiet)` covers the
        // session-level case where heartbeats are flowing
        // but the discovery signal has aged — election still
        // treats this as electable; the operator surface
        // distinguishes it for honesty. `offline` is the full
        // dark state.
        let mut tags: Vec<&str> = Vec::new();
        if is_local {
            tags.push("local");
        }
        if is_revoked {
            tags.push("revoked");
        } else if is_advertising && is_session_connected {
            tags.push("online");
        } else if is_session_connected {
            tags.push("connected (mdns-quiet)");
        } else if is_advertising {
            tags.push("advertising (no session)");
        } else {
            tags.push("offline");
        }

        println!();
        println!("  device_id:     {device_id}");
        println!("  display_name:  {display_name}");
        println!("  status:        {}", tags.join(", "));
        println!("  admitted_at:   {admitted_at_ms} (ms since epoch)");
        println!("  admitted_by:   {admitted_by}");
    }
    Ok(())
}

/// Run the `domain admit <device-id> [display-name]`
/// subcommand. Admits the named peer to the local trust ledger.
/// When `display_name` is `None`, the framework auto-resolves
/// the peer's last-observed mDNS-SD advert display_name from
/// `discovered_peers`; admission refuses when the peer has not
/// yet been observed.
pub fn domain_admit(
    socket: &Path,
    device_id: &str,
    display_name: Option<&str>,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let mut req_body = serde_json::json!({
        "op": "admit_peer_to_domain",
        "device_id": device_id,
    });
    if let Some(name) = display_name {
        if let Some(obj) = req_body.as_object_mut() {
            obj.insert(
                "display_name".to_string(),
                serde_json::Value::String(name.to_string()),
            );
        }
    }
    let req = with_step_up(req_body, step_up_token);
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("admit_peer_to_domain", err));
    }
    let record = resp
        .get("record")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    print_domain_member_record(&record);
    Ok(())
}

/// Run the `domain revoke <device-id>` subcommand. Soft-revokes
/// the named peer's domain admission.
pub fn domain_revoke(
    socket: &Path,
    device_id: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "revoke_peer_from_domain",
            "device_id": device_id,
        }),
        step_up_token,
    );
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("revoke_peer_from_domain", err));
    }
    let record = resp
        .get("record")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    print_domain_member_record(&record);
    Ok(())
}

/// Run the `domain discard <device-id>` subcommand. Operator-
/// explicit irreversible discard; appends a signed entry to
/// the domain witness chain.
pub fn domain_discard(
    socket: &Path,
    device_id: &str,
    reason: Option<&str>,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let mut req = serde_json::json!({
        "op": "discard_peer_from_domain",
        "device_id": device_id,
    });
    if let Some(r) = reason {
        req["reason"] = serde_json::Value::String(r.to_string());
    }
    let req = with_step_up(req, step_up_token);
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("discard_peer_from_domain", err));
    }
    let witness_id = resp
        .get("witness_id")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let device_id =
        resp.get("device_id").and_then(Value::as_str).unwrap_or("?");
    println!("peer discarded:");
    println!("  device_id:     {device_id}");
    println!("  witness_id:    {witness_id}");
    Ok(())
}

/// Run the `domain chain-head` subcommand. Prints the current
/// domain witness chain head hash + length.
pub fn domain_chain_head(socket: &Path) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({"op": "get_chain_head"});
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("get_chain_head", err));
    }
    let head = resp
        .get("head_hash_b64")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let length = resp
        .get("chain_length")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    println!("domain witness chain:");
    println!("  head_hash:     {head}");
    println!("  chain_length:  {length}");
    Ok(())
}

/// Run the `domain history` subcommand. Replays recent chain
/// entries chronologically.
pub fn domain_history(
    socket: &Path,
    limit: Option<usize>,
) -> Result<(), anyhow::Error> {
    let mut req = serde_json::json!({"op": "domain_history"});
    if let Some(n) = limit {
        req["limit"] = serde_json::Value::Number(n.into());
    }
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("domain_history", err));
    }
    let entries = resp
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let chain_length = resp
        .get("chain_length")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    println!(
        "domain witness history ({n} of {chain_length} entries):",
        n = entries.len()
    );
    for entry in &entries {
        let id = entry.get("id").and_then(Value::as_str).unwrap_or("?");
        let ts = entry.get("ts_ns").and_then(Value::as_u64).unwrap_or(0);
        let originator = entry
            .get("originator_device_id")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let op_kind = entry
            .get("op")
            .and_then(|o| o.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or("?");
        println!();
        println!("  id:            {id}");
        println!("  ts_ns:         {ts}");
        println!("  originator:    {originator}");
        println!("  op_kind:       {op_kind}");
    }
    Ok(())
}

/// Run the `domain reconnect <device-id>` subcommand. Fires an
/// operator-gestured reconnect storm against the named peer.
pub fn domain_reconnect(
    socket: &Path,
    device_id: &str,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({
            "op": "trigger_reconnect",
            "device_id": device_id,
        }),
        step_up_token,
    );
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("trigger_reconnect", err));
    }
    let outcome = resp.get("outcome").and_then(Value::as_str).unwrap_or("?");
    let elapsed_ms =
        resp.get("elapsed_ms").and_then(Value::as_u64).unwrap_or(0);
    println!("reconnect outcome:");
    println!("  device_id:     {device_id}");
    println!("  outcome:       {outcome}");
    println!("  elapsed_ms:    {elapsed_ms}");
    if let Some(endpoint) = resp.get("endpoint").and_then(Value::as_str) {
        println!("  endpoint:      {endpoint}");
    }
    if let Some(attempts) = resp.get("attempts").and_then(Value::as_u64) {
        println!("  attempts:      {attempts}");
    }
    Ok(())
}

/// Run the `domain bootstrap [--display-name X]` subcommand.
/// Signs the genesis self-admit witness so the local device
/// becomes the first member of a fresh domain. Refuses when
/// the chain already carries any entry.
pub fn domain_bootstrap(
    socket: &Path,
    display_name: Option<&str>,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let mut req_body = serde_json::json!({"op": "bootstrap_domain"});
    if let Some(name) = display_name {
        if let Some(obj) = req_body.as_object_mut() {
            obj.insert(
                "display_name".to_string(),
                serde_json::Value::String(name.to_string()),
            );
        }
    }
    let req = with_step_up(req_body, step_up_token);
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("bootstrap_domain", err));
    }
    let founder = resp
        .get("founder_device_id")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let witness_id = resp
        .get("witness_id")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let head = resp
        .get("head_hash_b64")
        .and_then(Value::as_str)
        .unwrap_or("?");
    println!("domain bootstrapped:");
    println!("  founder:       {founder}");
    println!("  witness_id:    {witness_id}");
    println!("  head_hash:     {head}");
    Ok(())
}

/// Run the `domain join [--endpoint host:port]` subcommand.
/// With an endpoint, dials the named peer via the audio plane
/// and broadcasts a chain-tail request so the dialed peer
/// ships their full chain. Without, emits the join happening
/// and waits for announce-driven discovery.
pub fn domain_join(
    socket: &Path,
    endpoint: Option<&str>,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let mut req_body = serde_json::json!({"op": "join_domain"});
    if let Some(addr) = endpoint {
        if let Some(obj) = req_body.as_object_mut() {
            obj.insert(
                "endpoint".to_string(),
                serde_json::Value::String(addr.to_string()),
            );
        }
    }
    let req = with_step_up(req_body, step_up_token);
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("join_domain", err));
    }
    let dialed = resp
        .get("endpoint")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .unwrap_or_else(|| "<announce-driven>".to_string());
    println!("domain join requested:");
    println!("  endpoint:      {dialed}");
    Ok(())
}

/// Run the `domain leave` subcommand. Resets the chain in-
/// process; the per-device signing key persists so a
/// subsequent join / bootstrap reuses the same identity.
pub fn domain_leave(
    socket: &Path,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req =
        with_step_up(serde_json::json!({"op": "leave_domain"}), step_up_token);
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("leave_domain", err));
    }
    let note = resp.get("note").and_then(Value::as_str).unwrap_or("");
    println!("domain left:");
    if !note.is_empty() {
        println!("  note:          {note}");
    }
    Ok(())
}

/// Run the `domain factory-reset` subcommand. Resets the
/// chain in-process AND removes the per-device signing key
/// from disk; the in-memory signing key remains until the
/// next steward restart.
pub fn domain_factory_reset(
    socket: &Path,
    step_up_token: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = with_step_up(
        serde_json::json!({"op": "factory_reset_domain"}),
        step_up_token,
    );
    let resp = call_with_caps(socket, &["plugins_admin"], req)?;
    if let Some(err) = resp.get("error") {
        return Err(format_error("factory_reset_domain", err));
    }
    let note = resp.get("note").and_then(Value::as_str).unwrap_or("");
    println!("domain factory-reset:");
    if !note.is_empty() {
        println!("  note:          {note}");
    }
    Ok(())
}

fn print_domain_member_record(record: &serde_json::Value) {
    let device_id = record
        .get("device_id")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let display_name = record
        .get("display_name")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let admitted_at_ms = record
        .get("admitted_at_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let admitted_by = record
        .get("admitted_by_device_id")
        .and_then(Value::as_str)
        .unwrap_or("<seed>");
    let revoked_at_ms = record.get("revoked_at_ms").and_then(Value::as_u64);
    println!("domain member:");
    println!("  device_id:     {device_id}");
    println!("  display_name:  {display_name}");
    println!("  admitted_at:   {admitted_at_ms} (ms since epoch)");
    println!("  admitted_by:   {admitted_by}");
    match revoked_at_ms {
        Some(ts) => println!("  revoked_at:    {ts} (ms since epoch)"),
        None => println!("  revoked_at:    <active>"),
    }
}
