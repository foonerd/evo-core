// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Remote-browser pair-once ceremony.
//!
//! A remote browser (phone, laptop, tablet) reaching the
//! reference-device operator UI on the LAN for the first time
//! obtains a durable bearer via a single-code pairing ceremony:
//!
//! 1. Browser calls `pair.begin` with a device hint (UA-derived
//!    label). Framework generates an 8-digit random code, records
//!    a pending attempt with a 90 s TTL, and publishes the code +
//!    QR payload to the prompt-ledger substrate so the kiosk
//!    display renders it full-screen.
//! 2. Operator reads the code from the kiosk and types it on the
//!    remote browser (or scans the QR with a phone).
//! 3. Browser calls `pair.complete` with the attempt id + typed
//!    code. Framework verifies (constant-time compare, single-use
//!    consume, TTL check), mints a bearer bound to a fresh
//!    `paired_device_id`, and records the paired device in the
//!    persistent subject store.
//!
//! Subsequent connections from that browser present the bearer;
//! the operator never sees the ceremony again. `pair.list` +
//! `pair.revoke` manage the paired-device set.
//!
//! This module carries the store + the primitives that the wire
//! handlers wrap. Subject-store persistence + kiosk-display
//! publishing live in the wire integration; here we only own the
//! in-memory pairing state and the pure-logic operations.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use rand::RngCore;
use serde::{Deserialize, Serialize};

/// TTL applied to a pending pair attempt from the moment
/// `pair.begin` records it. The kiosk display renders the code
/// for the same window; on expiry the attempt is dropped and any
/// `pair.complete` referencing the id returns `410 Gone`.
pub const PAIR_ATTEMPT_TTL: Duration = Duration::from_secs(90);

/// Number of decimal digits in a pairing code. 8 digits give
/// 10^8 = 100 million codes; combined with the 90 s TTL and the
/// per-id single-use consume, an on-line brute-force attacker
/// needs on average 5 × 10^7 guesses in 90 s to hit one code — an
/// attempt rate of ~555 kHz on a socket capped at a few hundred
/// concurrent requests. The pairing-code guess path is also rate
/// limited by the wire layer (see `pair.complete` handler wiring).
pub const PAIR_CODE_DIGITS: usize = 8;

/// Length, in bytes, of the random material backing a paired
/// device id. URL-safe base64-encoded (no padding) for the wire.
const PAIRED_DEVICE_ID_RANDOM_BYTES: usize = 16;

/// Length, in bytes, of the random material backing a pair
/// attempt id.
const PAIR_ATTEMPT_ID_RANDOM_BYTES: usize = 12;

/// Opaque identifier for a pending pair attempt. Returned by
/// `pair.begin`; presented by the browser to `pair.complete` with
/// the code the operator typed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PairAttemptId(pub String);

/// Opaque identifier for a paired device. Persisted; carried on
/// the bearer as an audit correlator and referenced by
/// `pair.revoke`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PairedDeviceId(pub String);

/// Numeric pairing code shown on the kiosk display. Kept as a
/// String because leading zeros are meaningful ("01234567" is a
/// valid displayed code and must survive round-trip).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairCode(pub String);

/// Pending pair attempt. Records the code the operator must type,
/// the browser's supplied device hint (for the kiosk display
/// label), the attempt's creation time, and its expiry.
#[derive(Debug, Clone)]
pub struct PairAttempt {
    /// Attempt id; carried by the browser through `pair.complete`.
    pub id: PairAttemptId,
    /// Pairing code the kiosk displays; the browser MUST NOT
    /// know this value ahead of the operator typing it.
    pub code: PairCode,
    /// Browser-supplied device hint (UA-derived; free-form). Shown
    /// on the kiosk display so the operator knows which device is
    /// asking to pair ("Chrome on Pixel 8"; "Safari on iPad").
    pub device_hint: String,
    /// Attempt creation instant (ms since Unix epoch).
    pub created_at_ms: u64,
    /// Attempt expiry instant (ms since Unix epoch).
    pub expires_at_ms: u64,
}

/// A device that completed the pairing ceremony. Persisted across
/// steward restarts via the subject store; represented in memory
/// on the current process by the `PairingStore`'s device map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedDevice {
    /// Stable id assigned at completion; never re-used, even
    /// after revocation.
    pub id: PairedDeviceId,
    /// Operator-facing label. Defaults to the device hint the
    /// browser supplied at `pair.begin`; the operator may rename
    /// via a future `pair.rename` op.
    pub label: String,
    /// First-paired instant (ms since Unix epoch).
    pub first_paired_at_ms: u64,
    /// Last-seen instant (ms since Unix epoch). Updated when the
    /// paired browser reconnects; a stale `last_seen` hint tells
    /// the operator "this device has not touched the player in a
    /// month" so they can revoke stale entries.
    pub last_seen_at_ms: u64,
}

/// Reasons `pair.complete` may refuse. The wire layer maps each
/// variant to a structured error class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairCompleteError {
    /// The attempt id was not found in the pending map. Either the
    /// browser fabricated an id, or the attempt already expired
    /// and was reaped.
    UnknownAttempt,
    /// The attempt id was found but its TTL elapsed before
    /// `pair.complete` arrived. The attempt was consumed and
    /// dropped; the browser must call `pair.begin` again.
    Expired,
    /// The typed code does not match the recorded code. The
    /// attempt is consumed on the first wrong guess to enforce
    /// single-use — the browser gets one shot per `pair.begin`.
    /// This closes the guess-then-retry-cheap on-line attack.
    WrongCode,
}

/// Ephemeral store of pending attempts + in-memory mirror of the
/// paired-device set. The persistent mirror is the subject store
/// (`subjects/system/paired_devices/*`); this store is the runtime
/// index kept in sync with it.
#[derive(Debug, Default)]
pub struct PairingStore {
    attempts: Mutex<HashMap<PairAttemptId, PairAttempt>>,
    devices: Mutex<HashMap<PairedDeviceId, PairedDevice>>,
}

impl PairingStore {
    /// Fresh store with no pending attempts and no paired devices.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of pending pair attempts currently in flight.
    pub fn pending_attempt_count(&self) -> usize {
        self.attempts
            .lock()
            .expect("PairingStore attempts mutex poisoned")
            .len()
    }

    /// Number of paired devices currently registered.
    pub fn paired_device_count(&self) -> usize {
        self.devices
            .lock()
            .expect("PairingStore devices mutex poisoned")
            .len()
    }

    /// Begin a new pair attempt. Records the attempt under a fresh
    /// id + code and returns the attempt so the wire layer can
    /// publish the code to the kiosk display.
    pub fn begin(&self, device_hint: String) -> PairAttempt {
        self.begin_at(device_hint, now_ms())
    }

    /// Testable variant of [`Self::begin`] that accepts an
    /// explicit "now" instead of reading the system clock.
    pub fn begin_at(&self, device_hint: String, now_ms: u64) -> PairAttempt {
        let attempt = PairAttempt {
            id: PairAttemptId(random_id_b64(PAIR_ATTEMPT_ID_RANDOM_BYTES)),
            code: PairCode(random_code(PAIR_CODE_DIGITS)),
            device_hint,
            created_at_ms: now_ms,
            expires_at_ms: now_ms + PAIR_ATTEMPT_TTL.as_millis() as u64,
        };
        self.attempts
            .lock()
            .expect("PairingStore attempts mutex poisoned")
            .insert(attempt.id.clone(), attempt.clone());
        attempt
    }

    /// Complete a pair attempt. Consumes the attempt atomically on
    /// any outcome (success, wrong code, expiry) so the browser
    /// gets exactly one guess per `begin`. On success, mints a
    /// fresh paired-device entry, records it, and returns it.
    pub fn complete(
        &self,
        id: &PairAttemptId,
        code: &PairCode,
    ) -> Result<PairedDevice, PairCompleteError> {
        self.complete_at(id, code, now_ms())
    }

    /// Testable variant of [`Self::complete`] that accepts an
    /// explicit "now".
    pub fn complete_at(
        &self,
        id: &PairAttemptId,
        code: &PairCode,
        now_ms: u64,
    ) -> Result<PairedDevice, PairCompleteError> {
        let attempt = {
            let mut guard = self
                .attempts
                .lock()
                .expect("PairingStore attempts mutex poisoned");
            guard.remove(id)
        };
        let attempt = attempt.ok_or(PairCompleteError::UnknownAttempt)?;
        if now_ms >= attempt.expires_at_ms {
            return Err(PairCompleteError::Expired);
        }
        if !constant_time_eq(attempt.code.0.as_bytes(), code.0.as_bytes()) {
            return Err(PairCompleteError::WrongCode);
        }
        let device = PairedDevice {
            id: PairedDeviceId(random_id_b64(PAIRED_DEVICE_ID_RANDOM_BYTES)),
            label: attempt.device_hint,
            first_paired_at_ms: now_ms,
            last_seen_at_ms: now_ms,
        };
        self.devices
            .lock()
            .expect("PairingStore devices mutex poisoned")
            .insert(device.id.clone(), device.clone());
        Ok(device)
    }

    /// Reap expired attempts. Returns the number of attempts
    /// removed. Called opportunistically at every complete + list;
    /// an explicit invocation is offered for a background reaper.
    pub fn purge_expired(&self) -> usize {
        self.purge_expired_at(now_ms())
    }

    /// Testable variant of [`Self::purge_expired`].
    pub fn purge_expired_at(&self, now_ms: u64) -> usize {
        let mut guard = self
            .attempts
            .lock()
            .expect("PairingStore attempts mutex poisoned");
        let before = guard.len();
        guard.retain(|_, a| a.expires_at_ms > now_ms);
        before - guard.len()
    }

    /// Snapshot the paired-device set. Order not stable across
    /// calls (backed by a HashMap); the wire layer sorts by
    /// `first_paired_at_ms` for a stable operator-facing list.
    pub fn list_devices(&self) -> Vec<PairedDevice> {
        self.devices
            .lock()
            .expect("PairingStore devices mutex poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// Revoke a paired device. Returns `true` when the device was
    /// present and removed; `false` when the id was unknown.
    /// Idempotent so an operator double-tapping the revoke button
    /// does not surface an error.
    pub fn revoke_device(&self, id: &PairedDeviceId) -> bool {
        self.devices
            .lock()
            .expect("PairingStore devices mutex poisoned")
            .remove(id)
            .is_some()
    }

    /// Note that a paired device just reconnected. Refreshes its
    /// `last_seen_at_ms`. No-op when the id is unknown (revoked
    /// mid-connection, ignore).
    pub fn touch_device(&self, id: &PairedDeviceId) {
        self.touch_device_at(id, now_ms())
    }

    /// Testable variant of [`Self::touch_device`].
    pub fn touch_device_at(&self, id: &PairedDeviceId, now_ms: u64) {
        if let Some(dev) = self
            .devices
            .lock()
            .expect("PairingStore devices mutex poisoned")
            .get_mut(id)
        {
            dev.last_seen_at_ms = now_ms;
        }
    }

    /// Seed a paired device (used at boot when replaying the
    /// persistent subject-store mirror into the in-memory map).
    pub fn seed_device(&self, device: PairedDevice) {
        self.devices
            .lock()
            .expect("PairingStore devices mutex poisoned")
            .insert(device.id.clone(), device);
    }

    /// Register a paired device directly, without going through a
    /// pair-once ceremony. Used by the password-authenticated
    /// pairing path (`pair_authenticate` wire op) where trust is
    /// established by the caller proving knowledge of the OS
    /// password rather than by relaying a kiosk-displayed code.
    ///
    /// Called AFTER the auth-service verify has succeeded — this
    /// method performs NO credential check on its own; the caller
    /// is responsible for gating access.
    pub fn register_paired_device(&self, device_hint: String) -> PairedDevice {
        self.register_paired_device_at(device_hint, now_ms())
    }

    /// Testable variant of [`Self::register_paired_device`] that
    /// accepts an explicit "now".
    pub fn register_paired_device_at(
        &self,
        device_hint: String,
        now_ms: u64,
    ) -> PairedDevice {
        let device = PairedDevice {
            id: PairedDeviceId(random_id_b64(PAIRED_DEVICE_ID_RANDOM_BYTES)),
            label: device_hint,
            first_paired_at_ms: now_ms,
            last_seen_at_ms: now_ms,
        };
        self.devices
            .lock()
            .expect("PairingStore devices mutex poisoned")
            .insert(device.id.clone(), device.clone());
        device
    }

    /// Seed a bootstrap pair attempt from a boot-partition
    /// preseed file. Used by headless devices — the operator
    /// drops a preseed value onto the boot partition BEFORE
    /// first boot (same convention as `wpa_supplicant.conf`),
    /// framework reads it, and the first browser to complete
    /// with the same preseed pairs without needing a
    /// kiosk-displayed code.
    ///
    /// The seeded attempt uses [`BOOTSTRAP_PAIR_ID`] as its id
    /// (well-known so the browser knows to present it), the
    /// preseed as its code, and `u64::MAX` as its expiry — no
    /// time window. Single-use consumption still applies via
    /// [`Self::complete`]: the first successful pair burns the
    /// attempt.
    pub fn seed_bootstrap(&self, preseed: String, device_hint: String) {
        let attempt = PairAttempt {
            id: PairAttemptId(BOOTSTRAP_PAIR_ID.to_string()),
            code: PairCode(preseed),
            device_hint,
            created_at_ms: 0,
            expires_at_ms: u64::MAX,
        };
        self.attempts
            .lock()
            .expect("PairingStore attempts mutex poisoned")
            .insert(attempt.id.clone(), attempt);
    }

    /// Whether a bootstrap pair attempt is currently seeded. The
    /// operator UI probes this to render the "enter your
    /// bootstrap value" screen instead of the kiosk-code screen
    /// on headless devices.
    pub fn has_bootstrap_seed(&self) -> bool {
        self.attempts
            .lock()
            .expect("PairingStore attempts mutex poisoned")
            .contains_key(&PairAttemptId(BOOTSTRAP_PAIR_ID.to_string()))
    }
}

/// Well-known pair id used for the bootstrap-preseed path. The
/// operator's browser calls
/// `pair_complete { pair_id: "bootstrap", code: <preseed value> }`
/// after the framework read the preseed from the boot-partition
/// file. Distinct from the random ids returned by `pair_begin`
/// so a running attempt cannot collide with the bootstrap slot.
pub const BOOTSTRAP_PAIR_ID: &str = "bootstrap";

/// Load a bootstrap preseed value from the given path. File
/// contents are the preseed value verbatim (leading + trailing
/// whitespace trimmed). Empty file / missing file returns
/// `Ok(None)`. IO errors other than NotFound bubble as
/// `Err(...)` so the operator sees a clear boot log line.
///
/// Called by the distribution boot code at startup after the
/// PairingStore is constructed; the returned preseed feeds
/// [`PairingStore::seed_bootstrap`]. The distribution
/// conventionally passes the path via `EVO_PAIR_PRESEED_FILE`
/// env var pointing at a location on the boot partition (e.g.
/// `/boot/evo/pair-preseed.txt` on an SBC with a mounted boot
/// partition).
pub fn load_bootstrap_preseed(
    path: &std::path::Path,
) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let trimmed = text.trim().to_string();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn random_id_b64(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    B64.encode(&buf)
}

fn random_code(digits: usize) -> String {
    // Generate `digits` decimal digits by drawing one random u32
    // per digit and taking mod 10. Using rand::thread_rng() so the
    // draw is CSPRNG-quality; mod bias is negligible at u32 →
    // 0..10.
    let mut rng = rand::thread_rng();
    let mut out = String::with_capacity(digits);
    for _ in 0..digits {
        let d = rng.next_u32() % 10;
        out.push(char::from(b'0' + d as u8));
    }
    out
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_has_configured_digit_count_and_only_digits() {
        let code = random_code(PAIR_CODE_DIGITS);
        assert_eq!(code.len(), PAIR_CODE_DIGITS);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn begin_records_attempt_with_ttl_from_now() {
        let store = PairingStore::new();
        let now = 1_000_000_u64;
        let attempt = store.begin_at("Chrome on Pixel".into(), now);
        assert_eq!(attempt.created_at_ms, now);
        assert_eq!(
            attempt.expires_at_ms,
            now + PAIR_ATTEMPT_TTL.as_millis() as u64
        );
        assert_eq!(store.pending_attempt_count(), 1);
    }

    #[test]
    fn complete_with_correct_code_within_ttl_produces_paired_device() {
        let store = PairingStore::new();
        let now = 1_000_000_u64;
        let attempt = store.begin_at("Chrome on Pixel".into(), now);
        let device = store
            .complete_at(&attempt.id, &attempt.code, now + 1)
            .expect("pairing should succeed within TTL");
        assert_eq!(device.label, "Chrome on Pixel");
        assert_eq!(device.first_paired_at_ms, now + 1);
        assert_eq!(device.last_seen_at_ms, now + 1);
        assert_eq!(store.paired_device_count(), 1);
        // Attempt is consumed atomically — a second call fails.
        assert_eq!(
            store.complete_at(&attempt.id, &attempt.code, now + 2),
            Err(PairCompleteError::UnknownAttempt)
        );
    }

    #[test]
    fn complete_with_wrong_code_consumes_the_attempt() {
        let store = PairingStore::new();
        let now = 1_000_000_u64;
        let attempt = store.begin_at("Safari on iPad".into(), now);
        assert_eq!(
            store.complete_at(
                &attempt.id,
                &PairCode("99999999".into()),
                now + 1
            ),
            Err(PairCompleteError::WrongCode)
        );
        // Second try with the real code no longer works — the
        // attempt was consumed on the first (wrong) guess. This
        // enforces single-shot: a browser gets one guess per
        // begin.
        assert_eq!(
            store.complete_at(&attempt.id, &attempt.code, now + 2),
            Err(PairCompleteError::UnknownAttempt)
        );
    }

    #[test]
    fn complete_after_ttl_refuses_expired() {
        let store = PairingStore::new();
        let now = 1_000_000_u64;
        let attempt = store.begin_at("Firefox on ThinkPad".into(), now);
        let past_ttl = attempt.expires_at_ms + 1;
        assert_eq!(
            store.complete_at(&attempt.id, &attempt.code, past_ttl),
            Err(PairCompleteError::Expired)
        );
    }

    #[test]
    fn complete_unknown_attempt_refuses_unknown() {
        let store = PairingStore::new();
        assert_eq!(
            store.complete_at(
                &PairAttemptId("fabricated".into()),
                &PairCode("12345678".into()),
                1_000_000,
            ),
            Err(PairCompleteError::UnknownAttempt)
        );
    }

    #[test]
    fn purge_reaps_expired_attempts_only() {
        let store = PairingStore::new();
        let fresh = store.begin_at("fresh".into(), 1_000_000);
        let stale = store.begin_at("stale".into(), 1_000);
        assert_eq!(store.pending_attempt_count(), 2);
        // Now sits well after `stale`'s TTL but well before
        // `fresh`'s TTL — only stale is reaped.
        let reaped = store.purge_expired_at(1_000_050);
        assert_eq!(reaped, 1);
        assert_eq!(store.pending_attempt_count(), 1);
        // fresh survives.
        let ok = store.complete_at(&fresh.id, &fresh.code, 1_000_051);
        assert!(ok.is_ok());
        // stale is gone.
        assert_eq!(
            store.complete_at(&stale.id, &stale.code, 1_000_051),
            Err(PairCompleteError::UnknownAttempt)
        );
    }

    #[test]
    fn revoke_removes_device_and_is_idempotent() {
        let store = PairingStore::new();
        let attempt = store.begin_at("phone".into(), 1_000_000);
        let dev = store
            .complete_at(&attempt.id, &attempt.code, 1_000_001)
            .unwrap();
        assert!(store.revoke_device(&dev.id));
        // Second revoke returns false; no error surfaces.
        assert!(!store.revoke_device(&dev.id));
    }

    /// `register_paired_device` is the direct-registration path
    /// the `pair_authenticate` wire op uses after the auth-service
    /// verify succeeds. No pair-once ceremony; the caller is
    /// responsible for the credential check.
    #[test]
    fn register_paired_device_creates_device_with_hint_label() {
        let store = PairingStore::new();
        let dev = store
            .register_paired_device_at("Chrome on Pixel 8".into(), 1_000_000);
        assert_eq!(dev.label, "Chrome on Pixel 8");
        assert_eq!(dev.first_paired_at_ms, 1_000_000);
        assert_eq!(dev.last_seen_at_ms, 1_000_000);
        assert!(!dev.id.0.is_empty());
        let listed = store.list_devices();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, dev.id);
    }

    /// Every call yields a fresh id — two registrations from the
    /// same UA-hint must produce two distinct paired devices (a
    /// user re-pairing on the same browser gets a NEW row, not a
    /// merged one).
    #[test]
    fn register_paired_device_two_calls_yield_two_distinct_devices() {
        let store = PairingStore::new();
        let a = store.register_paired_device_at("Firefox".into(), 1_000_000);
        let b = store.register_paired_device_at("Firefox".into(), 1_000_001);
        assert_ne!(a.id, b.id, "each registration must yield a fresh id");
        assert_eq!(store.list_devices().len(), 2);
    }

    #[test]
    fn touch_updates_last_seen() {
        let store = PairingStore::new();
        let attempt = store.begin_at("phone".into(), 1_000_000);
        let dev = store
            .complete_at(&attempt.id, &attempt.code, 1_000_001)
            .unwrap();
        store.touch_device_at(&dev.id, 2_000_000);
        let listed = store.list_devices();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].last_seen_at_ms, 2_000_000);
    }

    #[test]
    fn bootstrap_seed_pairs_without_time_window() {
        let store = PairingStore::new();
        // Simulate the distribution boot code reading
        // /boot/evo/pair-preseed.txt with the value the operator
        // wrote before first plug-in.
        store.seed_bootstrap(
            "correct-horse-battery".into(),
            "First browser".into(),
        );
        assert!(store.has_bootstrap_seed());
        // A browser calls pair_complete with the well-known
        // bootstrap id and the preseed value. Framework mints
        // a paired device — no code from any display, no time
        // window.
        let dev = store
            .complete_at(
                &PairAttemptId(BOOTSTRAP_PAIR_ID.into()),
                &PairCode("correct-horse-battery".into()),
                // Far future — proves no TTL constrains bootstrap.
                u64::MAX / 2,
            )
            .expect("bootstrap pair should succeed");
        assert_eq!(dev.label, "First browser");
        // Bootstrap slot is consumed atomically — a second attempt
        // fails. Prevents a shared-LAN neighbour who reads the SD
        // card later from also pairing.
        assert!(!store.has_bootstrap_seed());
        assert_eq!(
            store.complete_at(
                &PairAttemptId(BOOTSTRAP_PAIR_ID.into()),
                &PairCode("correct-horse-battery".into()),
                u64::MAX / 2,
            ),
            Err(PairCompleteError::UnknownAttempt)
        );
    }

    #[test]
    fn bootstrap_wrong_preseed_consumes_the_slot_like_any_pair() {
        let store = PairingStore::new();
        store.seed_bootstrap("correct".into(), "attacker".into());
        // Wrong guess consumes the bootstrap slot the same way a
        // wrong code consumes a normal pair — single-shot. A
        // shared-LAN attacker guessing the preseed burns the
        // window on the first miss.
        assert_eq!(
            store.complete_at(
                &PairAttemptId(BOOTSTRAP_PAIR_ID.into()),
                &PairCode("wrong".into()),
                1_000,
            ),
            Err(PairCompleteError::WrongCode)
        );
        assert!(!store.has_bootstrap_seed());
    }

    #[test]
    fn load_bootstrap_preseed_returns_none_when_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("missing.txt");
        assert_eq!(load_bootstrap_preseed(&path).unwrap(), None);
    }

    #[test]
    fn load_bootstrap_preseed_returns_none_when_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("preseed.txt");
        std::fs::write(&path, "   \n\t\n").unwrap();
        assert_eq!(load_bootstrap_preseed(&path).unwrap(), None);
    }

    #[test]
    fn load_bootstrap_preseed_trims_whitespace() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("preseed.txt");
        std::fs::write(&path, "\n  the-value  \n").unwrap();
        assert_eq!(
            load_bootstrap_preseed(&path).unwrap(),
            Some("the-value".to_string())
        );
    }

    #[test]
    fn seed_populates_device_map() {
        let store = PairingStore::new();
        store.seed_device(PairedDevice {
            id: PairedDeviceId("seeded".into()),
            label: "seeded".into(),
            first_paired_at_ms: 0,
            last_seen_at_ms: 0,
        });
        assert_eq!(store.paired_device_count(), 1);
        assert!(store.revoke_device(&PairedDeviceId("seeded".into())));
    }

    #[test]
    fn constant_time_eq_matches_only_full_match() {
        assert!(constant_time_eq(b"abcd", b"abcd"));
        assert!(!constant_time_eq(b"abcd", b"abce"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }
}
