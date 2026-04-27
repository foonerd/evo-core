//! Consumer-facing example: connect, describe capabilities, list
//! subjects, subscribe to happenings, and reconcile via `seq`.
//!
//! This is the canonical pattern for a consumer that wants to keep
//! a local picture of the steward's subjects in sync with the
//! steward's authoritative state. The shape is:
//!
//! 1. Connect to the steward's Unix socket.
//! 2. Issue `op = "describe_capabilities"` to learn which features
//!    the running steward exposes (in particular,
//!    `subscribe_happenings_cursor` is the gate for using a cursor
//!    on the subscribe op).
//! 3. Page through `op = "list_subjects"` to obtain the snapshot of
//!    subjects, recording the `current_seq` returned with each
//!    page. Every page returned in a single pass shares the same
//!    `current_seq` because it is sampled once per call against
//!    the bus.
//! 4. Issue `op = "subscribe_happenings"` with `since` set to the
//!    `current_seq` from step 3. The steward replays any happenings
//!    emitted between the snapshot and the subscribe, then streams
//!    live events. The consumer applies replay frames to its local
//!    snapshot and then folds in live events as they arrive.
//!
//! Run against a steward whose socket lives at `/run/evo.sock`:
//!
//! ```text
//! cargo run --example subscribe_and_reconcile -- /run/evo.sock
//! ```
//!
//! Defaults to `/run/evo.sock` when no argument is supplied.
//!
//! The example uses only the wire JSON shape documented in
//! `docs/engineering/CLIENT_API.md` and does not depend on the
//! steward's internal types beyond `tokio` and `serde_json`. It is
//! intentionally small (~150 LOC) so a consumer author can read it
//! end-to-end as a worked example.

use std::path::PathBuf;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/evo.sock"));

    println!("# Connecting to steward at {}", socket_path.display());
    let mut conn = UnixStream::connect(&socket_path).await?;

    // ---- Step 1: describe_capabilities. -----------------------------
    println!("# Discovering server capabilities");
    let caps = request_response(
        &mut conn,
        &serde_json::json!({"op":"describe_capabilities"}),
    )
    .await?;
    let features: Vec<String> = caps
        .get("features")
        .and_then(|f| f.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    println!("# Steward features: {features:?}");
    if !features.iter().any(|f| f == "subscribe_happenings_cursor") {
        eprintln!(
            "# WARNING: steward does not advertise \
             subscribe_happenings_cursor; falling back to live-only stream"
        );
    }

    // ---- Step 2: page through list_subjects. -----------------------
    println!("# Snapshotting subjects");
    let mut cursor: Option<String> = None;
    let mut snapshot_seq: u64 = 0;
    let mut total_subjects = 0usize;
    loop {
        let mut req = serde_json::json!({
            "op": "list_subjects",
            "page_size": 100,
        });
        if let Some(c) = cursor.as_ref() {
            req["cursor"] = serde_json::Value::String(c.clone());
        }
        let resp = request_response(&mut conn, &req).await?;
        let page = resp
            .get("subjects")
            .and_then(|s| s.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        total_subjects += page;
        snapshot_seq = resp
            .get("current_seq")
            .and_then(|s| s.as_u64())
            .unwrap_or(snapshot_seq);
        cursor = resp
            .get("next_cursor")
            .and_then(|c| c.as_str())
            .map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    println!(
        "# Snapshot complete: {total_subjects} subjects pinned to \
         current_seq={snapshot_seq}"
    );

    // ---- Step 3: subscribe with since = snapshot_seq. --------------
    // The steward replays any happenings emitted between the
    // snapshot and this subscribe (seq > snapshot_seq), then
    // streams live events. Frames at seq <= snapshot_seq are
    // already covered by the snapshot.
    println!("# Subscribing with since={snapshot_seq}");
    let mut sub_conn = UnixStream::connect(&socket_path).await?;
    let sub_req = serde_json::json!({
        "op": "subscribe_happenings",
        "since": snapshot_seq,
    });
    write_frame(&mut sub_conn, &serde_json::to_vec(&sub_req)?).await?;

    // First frame is the Subscribed ack.
    let ack_bytes = read_frame(&mut sub_conn).await?;
    let ack: serde_json::Value = serde_json::from_slice(&ack_bytes)?;
    println!("# Subscribed: {ack}");

    // Loop over live happenings forever; consumer applies each
    // frame to its local snapshot. Press Ctrl-C to exit.
    println!("# Live stream (Ctrl-C to exit):");
    loop {
        let bytes = match read_frame(&mut sub_conn).await {
            Ok(b) => b,
            Err(e) => {
                eprintln!("# Subscription closed: {e}");
                break;
            }
        };
        let frame: serde_json::Value = serde_json::from_slice(&bytes)?;
        if let Some(seq) = frame.get("seq").and_then(|s| s.as_u64()) {
            let kind = frame
                .get("happening")
                .and_then(|h| h.get("type"))
                .and_then(|t| t.as_str())
                .unwrap_or("?");
            println!("seq={seq:>6} kind={kind}");
        } else {
            println!("# Out-of-band frame: {frame}");
        }
    }

    Ok(())
}

/// Length-prefixed frame writer (4-byte big-endian length + payload).
async fn write_frame(
    conn: &mut UnixStream,
    body: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let len = (body.len() as u32).to_be_bytes();
    conn.write_all(&len).await?;
    conn.write_all(body).await?;
    conn.flush().await?;
    Ok(())
}

/// Length-prefixed frame reader (4-byte big-endian length + payload).
async fn read_frame(
    conn: &mut UnixStream,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut len = [0u8; 4];
    conn.read_exact(&mut len).await?;
    let n = u32::from_be_bytes(len) as usize;
    if n == 0 || n > 16 * 1024 * 1024 {
        return Err(format!("absurd frame length: {n}").into());
    }
    let mut body = vec![0u8; n];
    conn.read_exact(&mut body).await?;
    Ok(body)
}

/// Send one request and read one response on the same connection.
/// Used for non-streaming ops (describe_capabilities, list_subjects).
async fn request_response(
    conn: &mut UnixStream,
    request: &serde_json::Value,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    write_frame(conn, &serde_json::to_vec(request)?).await?;
    let bytes = read_frame(conn).await?;
    Ok(serde_json::from_slice(&bytes)?)
}
