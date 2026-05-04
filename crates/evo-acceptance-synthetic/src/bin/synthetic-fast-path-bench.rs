//! Fast-path throughput bench. Opens one persistent connection
//! to the steward's `/run/evo/fast.sock`, then issues
//! `FastPathRequest::Dispatch` frames in a tight loop for the
//! configured duration. Reports the achieved throughput
//! (dispatches/sec) and the latency distribution.
//!
//! Used by `T4.fast-path-throughput`. Synthetic-acceptance only;
//! production builds never invoke this.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let socket = PathBuf::from(
        args.next()
            .unwrap_or_else(|| "/run/evo/fast.sock".to_string()),
    );
    let shelf = args
        .next()
        .unwrap_or_else(|| "acceptance.fast-path".to_string());
    let verb = args.next().unwrap_or_else(|| "ping".to_string());
    let handle_id = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: bench <socket> <shelf> <verb> <handle_id> <handle_started_at_ms> <duration_secs>"))?;
    let handle_started_at_ms: u64 = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing handle_started_at_ms"))?
        .parse()
        .context("parsing handle_started_at_ms")?;
    let duration_secs: u64 = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing duration_secs"))?
        .parse()
        .context("parsing duration_secs")?;

    let mut stream = UnixStream::connect(&socket)
        .await
        .with_context(|| format!("connecting to {}", socket.display()))?;

    let deadline = Instant::now() + Duration::from_secs(duration_secs);
    let mut count: u64 = 0;
    let mut errors: u64 = 0;
    let mut latencies_us: Vec<u64> = Vec::with_capacity(100_000);
    let started = Instant::now();
    let mut buf = vec![0u8; 64 * 1024];

    while Instant::now() < deadline {
        count += 1;
        let req = evo::fast_path::FastPathRequest::Dispatch {
            cid: count,
            shelf: shelf.clone(),
            verb: verb.clone(),
            payload: Vec::new(),
            handle_id: handle_id.clone(),
            handle_started_at_ms,
            deadline_ms: None,
        };
        let payload = evo_plugin_sdk::codec::encode_cbor_value(&req)
            .map_err(|e| anyhow::anyhow!("encode: {e}"))?;
        let len = (payload.len() as u32).to_be_bytes();
        let req_t0 = Instant::now();
        stream.write_all(&len).await?;
        stream.write_all(&payload).await?;

        let mut len_bytes = [0u8; 4];
        stream.read_exact(&mut len_bytes).await?;
        let size = u32::from_be_bytes(len_bytes) as usize;
        if size > buf.len() {
            buf.resize(size, 0);
        }
        stream.read_exact(&mut buf[..size]).await?;
        let elapsed_us = req_t0.elapsed().as_micros() as u64;
        latencies_us.push(elapsed_us);

        let resp: evo::fast_path::FastPathResponse =
            evo_plugin_sdk::codec::decode_cbor_value(&buf[..size])
                .map_err(|e| anyhow::anyhow!("decode: {e}"))?;
        match resp {
            evo::fast_path::FastPathResponse::Dispatched { .. } => {}
            evo::fast_path::FastPathResponse::Error { .. } => {
                errors += 1;
            }
        }
    }

    let elapsed = started.elapsed().as_secs_f64();
    let rate = count as f64 / elapsed;
    latencies_us.sort_unstable();
    let p50 = if latencies_us.is_empty() {
        0
    } else {
        latencies_us[latencies_us.len() / 2]
    };
    let p99 = if latencies_us.is_empty() {
        0
    } else {
        latencies_us[(latencies_us.len() * 99) / 100]
    };
    let max = *latencies_us.last().unwrap_or(&0);
    println!(
        "count={count} errors={errors} elapsed_secs={elapsed:.3} rate_per_sec={rate:.0} p50_us={p50} p99_us={p99} max_us={max}"
    );

    if errors > 0 {
        return Err(anyhow::anyhow!("{errors} dispatches refused"));
    }
    Ok(())
}
