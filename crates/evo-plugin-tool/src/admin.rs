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
/// memory while reading.
const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// Per-call deadline. Operator-issued admin ops are bounded
/// operations (a plugin drain at most takes the steward's
/// shutdown deadline); 30s is generous and surfaces a runtime
/// hang as a structured timeout rather than the operator
/// staring at a wedged terminal.
const CALL_DEADLINE_SECS: u64 = 30;

/// Source for the new manifest in the reload verbs.
pub enum ReloadSource {
    Inline(String),
    Path(PathBuf),
}

/// Run the `enable` subcommand.
pub fn enable(
    socket: &Path,
    plugin: &str,
    reason: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "enable_plugin",
        "plugin": plugin,
        "reason": reason,
    });
    let resp = call(socket, req)?;
    print_lifecycle_outcome(&resp)
}

/// Run the `disable` subcommand.
pub fn disable(
    socket: &Path,
    plugin: &str,
    reason: Option<&str>,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "disable_plugin",
        "plugin": plugin,
        "reason": reason,
    });
    let resp = call(socket, req)?;
    print_lifecycle_outcome(&resp)
}

/// Run the `uninstall` subcommand.
pub fn uninstall(
    socket: &Path,
    plugin: &str,
    reason: Option<&str>,
    purge_state: bool,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "uninstall_plugin",
        "plugin": plugin,
        "reason": reason,
        "purge_state": purge_state,
    });
    let resp = call(socket, req)?;
    print_lifecycle_outcome(&resp)
}

/// Run the `purge-state` subcommand.
pub fn purge_state(socket: &Path, plugin: &str) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "purge_plugin_state",
        "plugin": plugin,
    });
    let resp = call(socket, req)?;
    print_lifecycle_outcome(&resp)
}

/// Run the `reload catalogue` subcommand.
pub fn reload_catalogue(
    socket: &Path,
    source: ReloadSource,
    dry_run: bool,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "reload_catalogue",
        "source": reload_source_json(&source)?,
        "dry_run": dry_run,
    });
    let resp = call(socket, req)?;
    print_catalogue_reload(&resp)
}

/// Run the `reload manifest` subcommand.
pub fn reload_manifest(
    socket: &Path,
    plugin: &str,
    source: ReloadSource,
    dry_run: bool,
) -> Result<(), anyhow::Error> {
    let req = serde_json::json!({
        "op": "reload_manifest",
        "plugin": plugin,
        "source": reload_source_json(&source)?,
        "dry_run": dry_run,
    });
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
