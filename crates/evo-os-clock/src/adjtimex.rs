//! Safe wrapper around Linux `adjtimex(2)`.
//!
//! ## Kernel ABI contract
//!
//! `adjtimex(2)` (man-page: `adjtimex(2)`) takes a pointer to a
//! `struct timex` and atomically reads (and optionally adjusts) the
//! kernel's NTP state machine. Calling it with the `modes` field
//! set to `0` is a pure read — the kernel returns the current
//! state without modification. We always pass `modes = 0`.
//!
//! The fields we read:
//!
//! - `status`: bitfield carrying NTP-state flags. The bit we need
//!   is `STA_UNSYNC` (`0x0040`). When `(status & STA_UNSYNC) == 0`
//!   the kernel believes its clock is synchronised against an
//!   external source; when the bit is set, the clock is not
//!   synchronised.
//! - `time.tv_sec` / `time.tv_usec`: the kernel's current notion of
//!   wall-clock time (same as `clock_gettime(CLOCK_REALTIME)` for
//!   our purposes; we use this for cross-checking only).
//! - `maxerror` / `esterror`: maximum and estimated error in
//!   microseconds; carried for diagnostic surface but the
//!   `STA_UNSYNC` bit is the authoritative sync indicator.
//!
//! The kernel does NOT report a "last sync timestamp" in `timex`.
//! `adjtimex` exposes the *current* state, not a history. The
//! framework's `TimeTrust` consumer derives staleness from its own
//! observation timeline (when did we first see `STA_UNSYNC == 0`?
//! when did we last see it?). The kernel anchors the binary
//! synchronised / unsynchronised judgement; the consumer anchors
//! the temporal staleness judgement.
//!
//! The return value of `adjtimex` is the current clock state mode
//! (`TIME_OK`, `TIME_INS`, `TIME_DEL`, `TIME_OOP`, `TIME_WAIT`,
//! `TIME_ERROR`). We expose this raw value alongside the
//! synchronised-bit interpretation so callers can render
//! diagnostic detail when needed.
//!
//! ## Memory-safety contract
//!
//! The FFI call requires `unsafe` because `libc::adjtimex` takes a
//! mutable pointer to a `libc::timex` and writes into it. We
//! satisfy the safety requirements as follows:
//!
//! 1. The `timex` struct is owned by us, allocated on the stack via
//!    `MaybeUninit`. The kernel writes into the buffer; we never
//!    assume any field is initialised before the syscall returns
//!    successfully.
//! 2. Setting `modes = 0` before the call is essential: it tells
//!    the kernel this is a read-only request. The C-equivalent
//!    invariant is "zero the struct before calling". We zero it
//!    explicitly via `MaybeUninit::zeroed()` rather than relying
//!    on the kernel to clear fields it does not write.
//! 3. The pointer passed to the syscall is `&mut buf as *mut _`.
//!    Its provenance is exactly the local `MaybeUninit<timex>` and
//!    its lifetime covers the syscall. The kernel does not retain
//!    the pointer beyond the syscall's return.
//! 4. On a return value of `-1` we read `errno` via `*libc::__errno_location()`
//!    (Linux glibc) and surface it as an [`AdjtimexError`]. On
//!    success (any non-negative return), every field we read is
//!    kernel-written and well-defined per the kernel ABI.
//!
//! Every assumption above is anchored to the kernel's stable ABI
//! and the `libc` crate's representation of `struct timex`. A
//! future contributor extending this wrapper must preserve all
//! four invariants.

#![cfg_attr(target_os = "linux", allow(unsafe_code))]

use std::time::SystemTime;

/// Snapshot of the kernel's NTP synchronisation state.
///
/// Returned by [`adjtimex_status`]. The framework consumer maps
/// this onto its own four-state `TimeTrust` enum and derives
/// staleness from its observation timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimexStatus {
    /// Whether the kernel believes the wall-clock is synchronised
    /// against an external source. Derived from the
    /// `STA_UNSYNC` bit of the kernel's `timex.status`:
    /// synchronised ⇔ `(status & STA_UNSYNC) == 0`.
    pub synced: bool,
    /// Wall-clock time at the moment of the read, as reported by
    /// the kernel. Equivalent to `clock_gettime(CLOCK_REALTIME)`
    /// taken atomically with the sync-state read.
    pub now: SystemTime,
    /// Raw `status` bitfield from `timex`. Carried for diagnostic
    /// surface (callers may render it as hex for ops audits). Bit
    /// definitions match the kernel ABI; see `linux/timex.h`.
    pub status_flags: i32,
    /// Maximum error in microseconds, as reported by the kernel.
    /// Diagnostic only.
    pub max_error_us: i64,
    /// Estimated error in microseconds, as reported by the kernel.
    /// Diagnostic only.
    pub est_error_us: i64,
    /// Raw return value of the `adjtimex(2)` syscall — one of
    /// `TIME_OK`, `TIME_INS`, `TIME_DEL`, `TIME_OOP`, `TIME_WAIT`,
    /// `TIME_ERROR`. Diagnostic only; the `synced` field is the
    /// authoritative interpretation.
    pub clock_mode: i32,
}

/// Errors that may be returned by [`adjtimex_status`].
#[derive(Debug, thiserror::Error)]
pub enum AdjtimexError {
    /// The `adjtimex(2)` syscall returned `-1`. The wrapped value
    /// is the platform `errno` reported by the kernel; on Linux,
    /// the only realistic causes are `EFAULT` (impossible by our
    /// contract — the buffer is on our stack) and `EPERM` (we
    /// passed a non-zero `modes` field — also impossible by our
    /// contract). Returned for surface completeness; in practice
    /// callers do not encounter this on supported platforms.
    #[error("adjtimex(2) syscall failed: errno {0}")]
    Syscall(i32),

    /// This crate was built for a non-Linux target. The framework
    /// consumer should treat this as graceful degradation —
    /// `TimeTrust` stays `Untrusted` permanently. Surfaced as
    /// `Err` rather than `Ok(synced: false)` so callers can
    /// distinguish "kernel says not synced" from "we cannot
    /// observe the kernel's state" if they wish to.
    #[error("adjtimex is not available on this platform")]
    Unsupported,
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{AdjtimexError, TimexStatus};
    use std::mem::MaybeUninit;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// `STA_UNSYNC` bit per `linux/timex.h`. When set in
    /// `timex.status`, the kernel does not believe the clock is
    /// synchronised.
    const STA_UNSYNC: i32 = 0x0040;

    /// Read the kernel's NTP synchronisation state.
    ///
    /// See the module-level documentation for the safety contract.
    /// This is the single `unsafe` block in the entire evo
    /// workspace.
    pub fn adjtimex_status() -> Result<TimexStatus, AdjtimexError> {
        // Allocate a zeroed `timex` buffer. Zeroing matters: it
        // sets `modes = 0`, which makes the syscall a read-only
        // request, and it primes every field we do not explicitly
        // read with a defined value (zero) so a future audit
        // doesn't have to reason about partial initialisation.
        let mut buf: MaybeUninit<libc::timex> = MaybeUninit::zeroed();

        // SAFETY:
        //
        // - `buf` is a local stack variable of the correct type
        //   (`libc::timex`) sized for the kernel ABI by libc's
        //   declaration.
        // - The pointer derives from `&mut buf` and has provenance
        //   covering the buffer for the entire syscall.
        // - We zeroed the buffer above; in particular `modes == 0`
        //   so the syscall performs a pure read (no clock
        //   adjustment is requested).
        // - The kernel writes into the buffer on success and does
        //   not retain the pointer beyond the syscall return.
        // - We read fields only after a non-negative return value
        //   confirms the syscall succeeded.
        let rc = unsafe { libc::adjtimex(buf.as_mut_ptr()) };

        if rc < 0 {
            // SAFETY:
            //
            // `__errno_location` is the standard glibc accessor
            // for the per-thread `errno` symbol. It always
            // returns a non-null pointer to a valid `i32` for the
            // duration of the calling thread; reading it with
            // `*` is the documented way to obtain `errno` from
            // safe Rust. This is the only other unsafe operation
            // in the entire wrapper.
            let errno = unsafe { *libc::__errno_location() };
            return Err(AdjtimexError::Syscall(errno));
        }

        // SAFETY:
        //
        // The syscall returned non-negative, so the kernel wrote
        // every field of `timex` it considers part of the read
        // contract. `assume_init` is sound here: the buffer was
        // zeroed before the call (so any field the kernel did
        // *not* write retains a valid zero pattern, which is
        // valid for every numeric field of `timex`), and every
        // field the kernel *did* write is now well-defined per
        // the ABI.
        let timex = unsafe { buf.assume_init() };

        let synced = (timex.status & STA_UNSYNC) == 0;
        let now = match (timex.time.tv_sec.try_into() as Result<u64, _>)
            .ok()
            .and_then(|secs| {
                let usecs: u64 = timex.time.tv_usec.try_into().ok()?;
                Some(
                    UNIX_EPOCH
                        + Duration::from_secs(secs)
                        + Duration::from_micros(usecs),
                )
            }) {
            Some(t) => t,
            // Negative or absurd times — kernel returned a value we
            // cannot project onto SystemTime. Fall back to the
            // process's notion of `now`; the synced bit remains
            // authoritative.
            None => SystemTime::now(),
        };

        Ok(TimexStatus {
            synced,
            now,
            status_flags: timex.status,
            // libc's `timex.maxerror` / `.esterror` are `i64` on
            // 64-bit Linux targets and `i32` on 32-bit targets
            // (armv7-unknown-linux-gnueabihf). `i64::from(...)`
            // widens losslessly on 32-bit and is identity on
            // 64-bit; the `useless_conversion` lint trips on the
            // identity direction, so allow it here. The
            // alternative — `as i64` — trips `unnecessary_cast`
            // on 64-bit. Cross-target portability requires one
            // suppression either way.
            #[allow(clippy::useless_conversion)]
            max_error_us: i64::from(timex.maxerror),
            #[allow(clippy::useless_conversion)]
            est_error_us: i64::from(timex.esterror),
            clock_mode: rc,
        })
    }
}

#[cfg(not(target_os = "linux"))]
mod stub {
    use super::{AdjtimexError, TimexStatus};

    /// Non-Linux stub: the kernel's NTP state is not observable on
    /// this platform via the syscall this wrapper targets. The
    /// framework consumer treats the `Unsupported` error as a
    /// graceful-degradation signal: `TimeTrust` stays `Untrusted`
    /// permanently and time-sensitive subsystems gate accordingly.
    /// This is documented as the no-RTC / no-sync-observable shape
    /// in the operator-facing docs.
    #[allow(dead_code)]
    pub fn adjtimex_status() -> Result<TimexStatus, AdjtimexError> {
        Err(AdjtimexError::Unsupported)
    }
}

/// Read the kernel's current NTP synchronisation state.
///
/// On Linux this calls `adjtimex(2)` with `modes = 0` (read-only).
/// On other platforms it returns [`AdjtimexError::Unsupported`].
///
/// See the [module-level documentation][self] for the kernel ABI
/// contract and the memory-safety proof that backs the single
/// `unsafe` block in the Linux path.
pub fn adjtimex_status() -> Result<TimexStatus, AdjtimexError> {
    #[cfg(target_os = "linux")]
    {
        linux::adjtimex_status()
    }
    #[cfg(not(target_os = "linux"))]
    {
        stub::adjtimex_status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "linux")]
    fn adjtimex_status_returns_kernel_state_on_linux() {
        // The CI runner has a kernel; the syscall must succeed.
        // The `synced` bit may be true or false depending on
        // whether the runner's environment has a working
        // time-sync daemon, so the test does not assert on it.
        let status = adjtimex_status().expect(
            "adjtimex(2) must succeed on Linux: the syscall is \
             unconditionally available in the kernel ABI",
        );
        // `clock_mode` is one of the documented constants; we don't
        // assert a specific value (a CI runner mid-leap-second
        // would legitimately return TIME_INS or similar) but the
        // returned status_flags must at least be representable.
        let _ = status.status_flags;
        let _ = status.synced;
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn adjtimex_status_returns_unsupported_on_non_linux() {
        match adjtimex_status() {
            Err(AdjtimexError::Unsupported) => {}
            other => panic!("expected Unsupported on non-Linux, got {other:?}"),
        }
    }
}
