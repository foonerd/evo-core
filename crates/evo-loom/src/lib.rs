//! Empty library crate hosting the loom integration tests.
//!
//! This crate exists only as a stand-alone container for the loom
//! model-checking tests of the evo router's lookup-clone-drop
//! discipline. It is intentionally not a member of the top-level
//! workspace: `RUSTFLAGS="--cfg loom"` is a global rustc flag, and
//! recompiling tokio (which the rest of the workspace depends on)
//! under `--cfg loom` fails because tokio's source gates its
//! real-I/O surface (`tokio::fs`, `tokio::process`) under
//! `cfg(not(loom))`. Hosting the loom tests in a separate crate that
//! has no tokio dependency means the loom invocation only recompiles
//! this crate plus loom itself.
//!
//! See `tests/loom_router.rs` for the tests, the discipline they
//! pin, and the run command.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
