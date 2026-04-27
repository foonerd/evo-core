//! Persistence crash-recovery integration test.
//!
//! Spawns the `crash_writer` example as a child process so that
//! sending `SIGKILL` to it terminates the writer mid-WAL without
//! taking the test runner down with it. The writer drives
//! `emit_durable` against the real `SqlitePersistenceStore`. After the
//! kill the parent reopens the database via the production
//! persistence layer (which triggers SQLite WAL recovery) and asserts
//! the recovery contract:
//!
//! - The reopen succeeds (no `PersistenceError::Sqlite` wrapping a
//!   corruption code).
//! - `load_max_happening_seq()` is at least the largest seq the
//!   writer reported as committed before the kill.
//! - Every persisted row deserialises cleanly to a `Happening`.
//! - `seq` values form a contiguous run from 1 upward (an emit either
//!   commits or its row never appears; the bus's `next_seq` reseeds
//!   from `max(seq) + 1` on the next boot, so disk-level rows must be
//!   gap-free).
//! - Reopening a second time is also clean (recovery is idempotent).
//!
//! The kill window is sized so the writer is almost certainly
//! mid-emit when the signal lands; the assertions above must hold
//! regardless of where in the emit cycle the kill arrived.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use evo::happenings::Happening;
use evo::persistence::{PersistenceStore, SqlitePersistenceStore};

fn spawn_crash_writer(db_path: &Path) -> Child {
    Command::new(env!("CARGO"))
        .args(["run", "--example", "crash_writer", "--quiet", "--"])
        .arg(db_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn crash_writer example")
}

fn read_until_ready(child: &mut Child, timeout: Duration) -> u64 {
    let stdout = child.stdout.take().expect("piped stdout");
    let mut reader = BufReader::new(stdout);
    let deadline = Instant::now() + timeout;
    let mut last_committed: u64 = 0;
    let mut line = String::new();
    while Instant::now() < deadline {
        line.clear();
        let n = reader.read_line(&mut line).expect("read READY line");
        if n == 0 {
            break;
        }
        if let Some(s) = line.trim().strip_prefix("READY ") {
            if let Ok(seq) = s.parse::<u64>() {
                last_committed = seq;
                if seq >= 1 {
                    // Put the reader back so subsequent buffered lines
                    // do not block on close. We do not care about its
                    // contents past this point; the kill will close
                    // the pipe.
                    return last_committed;
                }
            }
        }
    }
    last_committed
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigkill_during_emit_leaves_database_recoverable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("evo.db");

    let mut child = spawn_crash_writer(&db_path);

    // Confirm the writer is committing rows before the kill. If we
    // killed before any commit, we would not have proven recovery —
    // an empty WAL trivially "recovers".
    let first_committed = read_until_ready(&mut child, Duration::from_secs(30));
    assert!(
        first_committed >= 1,
        "writer never reported a committed seq before timeout",
    );

    // Let several more emits land, increasing the chance the kill
    // catches an in-flight WAL frame mid-write. The interval is short
    // enough to keep the test quick and long enough that hundreds of
    // emits typically queue up under realistic WAL throughput.
    std::thread::sleep(Duration::from_millis(150));

    // SIGKILL on Unix; immediate, unblockable, no graceful shutdown.
    child.kill().expect("kill child");
    let _ = child.wait();

    // Reopen via the production layer. This call runs WAL recovery
    // inside the bootstrap connection before the deadpool pool is
    // built. A torn-page or unrecoverable WAL would surface as a
    // `PersistenceError::Sqlite` from `open`.
    let store_path = db_path.clone();
    let store: Arc<dyn PersistenceStore> = Arc::new(
        tokio::task::spawn_blocking(move || {
            SqlitePersistenceStore::open(store_path)
        })
        .await
        .expect("spawn_blocking join")
        .expect("reopen after kill"),
    );

    let max_seq = store
        .load_max_happening_seq()
        .await
        .expect("load_max_happening_seq");
    assert!(
        max_seq >= first_committed,
        "after recovery max_seq={max_seq} < last reported committed \
         seq {first_committed}",
    );

    // Read every row; assert they are gap-free, deserialise cleanly,
    // and the kind column matches the payload variant.
    let rows = store
        .load_happenings_since(0, u32::MAX)
        .await
        .expect("load_happenings_since");
    assert_eq!(
        rows.len() as u64,
        max_seq,
        "row count {} does not match max_seq {}",
        rows.len(),
        max_seq,
    );
    for (idx, row) in rows.iter().enumerate() {
        let expected = (idx as u64) + 1;
        assert_eq!(
            row.seq, expected,
            "seq gap at position {idx}: row.seq={}, expected {expected}",
            row.seq,
        );
        let happening: Happening = serde_json::from_value(row.payload.clone())
            .unwrap_or_else(|e| {
                panic!(
                    "row at seq {} did not deserialise to Happening: {e}",
                    row.seq,
                )
            });
        // The writer only emits CustodyTaken; a torn payload that
        // happened to deserialise as something else would be caught
        // here.
        assert!(
            matches!(happening, Happening::CustodyTaken { .. }),
            "row at seq {} deserialised to unexpected variant: {:?}",
            row.seq,
            happening,
        );
        assert_eq!(
            row.kind, "custody_taken",
            "kind column mismatch at seq {}",
            row.seq
        );
    }

    // Idempotent recovery: reopening a recovered database must also
    // succeed and observe the same row set. Drop first so the WAL is
    // checkpointed (the open path runs `journal_mode = WAL`; the WAL
    // is still present until a checkpoint runs).
    drop(store);
    let store_path2 = db_path.clone();
    let store2: Arc<dyn PersistenceStore> = Arc::new(
        tokio::task::spawn_blocking(move || {
            SqlitePersistenceStore::open(store_path2)
        })
        .await
        .expect("spawn_blocking join (second open)")
        .expect("second reopen after recovery"),
    );
    let max_seq_2 = store2
        .load_max_happening_seq()
        .await
        .expect("load_max_happening_seq (second open)");
    assert_eq!(
        max_seq_2, max_seq,
        "second reopen reported max_seq={max_seq_2}, expected {max_seq}",
    );
}
