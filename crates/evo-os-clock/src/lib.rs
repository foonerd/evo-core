//! OS clock-state interface for the evo framework.
//!
//! The framework needs to know whether the kernel believes its
//! wall-clock is synchronised against an external time source (NTP /
//! PTP / GPS / vendor sync daemon) and, when synchronised, how
//! recently the last sync completed. This crate is the consumer of
//! that information.
//!
//! ## Scope
//!
//! - **In scope**: a single safe API, [`adjtimex_status`], that
//!   returns the kernel's NTP synchronisation state plus the
//!   timestamp of the last completed sync (when the kernel knows
//!   it).
//! - **Out of scope**: running an NTP / PTP / GPS client. The
//!   framework consumes whatever the OS daemon
//!   (`systemd-timesyncd`, `chrony`, `ptp4l`, vendor agents) has
//!   negotiated. Sync configuration is distribution policy, not
//!   framework code.
//!
//! ## Why this is a separate crate
//!
//! The rest of the evo workspace runs under `unsafe_code = "forbid"`
//! at the workspace lint level. Reading the kernel's NTP state
//! requires the `adjtimex(2)` syscall, for which no safe-wrapper
//! crate (`nix`, `rustix`, etc.) exposes a public API at the
//! versions in our dep graph. The principled answer is to scope the
//! single required `unsafe` block to its own crate, where the
//! relaxation can be reviewed in isolation: this crate's `Cargo.toml`
//! declares `unsafe_code = "deny"` (allowing per-module
//! `#[allow(unsafe_code)]`) instead of inheriting the workspace's
//! `forbid`. Every other crate in the workspace stays at `forbid`.
//!
//! The whole `unsafe` surface of this crate lives in
//! [`adjtimex`](mod@adjtimex). A reader can audit it by reading one
//! file. There is no plan to expand the `unsafe` surface beyond
//! what the kernel ABI requires.
//!
//! ## Platform support
//!
//! - **Linux**: full implementation via the `adjtimex(2)` syscall.
//!   The kernel exposes its NTP state machine through the `timex`
//!   struct; we read the `STA_UNSYNC` status bit and the implied
//!   last-sync moment.
//! - **Non-Linux** (`cfg(not(target_os = "linux"))`): stub returns a
//!   permanently-unsynchronised state. Documented as graceful
//!   degradation. The framework's `TimeTrust` consumer treats the
//!   result the same way it treats a Linux box that hasn't synced
//!   yet — no time-sensitive operations proceed until a sync
//!   becomes observable.

#![warn(missing_docs)]

pub mod adjtimex;

pub use adjtimex::{adjtimex_status, AdjtimexError, TimexStatus};
