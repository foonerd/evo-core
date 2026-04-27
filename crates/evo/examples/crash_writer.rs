//! Crash-recovery test helper.
//!
//! Opens the production SQLite persistence layer at the path given on
//! `argv[1]`, constructs a durable [`HappeningBus`], and emits
//! happenings in a tight loop until the process is killed. After every
//! successful commit prints `READY <seq>` to stdout (flushed) so the
//! parent integration test knows it is safe to send `SIGKILL` and
//! verify recovery.
//!
//! Not a consumer-facing example; lives under `examples/` so the
//! integration test in `tests/crash_recovery.rs` can spawn it via
//! `cargo run --example crash_writer -- <db_path>` without pulling
//! `escargot` into dev-deps.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use evo::happenings::{Happening, HappeningBus};
use evo::persistence::{PersistenceStore, SqlitePersistenceStore};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args();
    let _exe = args.next();
    let db_path: PathBuf = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: crash_writer <db_path>"))?
        .into();

    let store: Arc<dyn PersistenceStore> =
        Arc::new(SqlitePersistenceStore::open(db_path)?);
    let bus = HappeningBus::with_persistence(Arc::clone(&store)).await?;

    let mut counter: u64 = 0;
    loop {
        let seq = bus
            .emit_durable(Happening::CustodyTaken {
                plugin: "org.crash.test".into(),
                handle_id: format!("h-{counter}"),
                shelf: "x.y".into(),
                custody_type: "playback".into(),
                at: SystemTime::UNIX_EPOCH,
            })
            .await?;
        // Print AFTER the commit so the parent only ever observes
        // seqs that are durably on disk; flush so the stream is not
        // buffered across a kill boundary.
        println!("READY {seq}");
        std::io::stdout().flush().ok();
        counter = counter.saturating_add(1);
        // Yield without sleeping so writes stay back-to-back; the
        // parent's kill window must catch a real in-flight commit.
        tokio::task::yield_now().await;
    }
}
