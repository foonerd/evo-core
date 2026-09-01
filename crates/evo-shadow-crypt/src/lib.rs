// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Local-only `/etc/shadow` password verification for the evo
//! framework.
//!
//! Wraps `libcrypt`'s `crypt_r(3)` via runtime dynamic-load
//! (`libloading`) so every hash format libcrypt supports —
//! yescrypt (`$y$`), SHA-512-crypt (`$6$`), SHA-256-crypt
//! (`$5$`), MD5-crypt (`$1$`), bcrypt (`$2b$`) — verifies
//! identically. The framework can then follow OS-standard
//! password rotation (via `passwd`, `chpasswd`, and any other
//! shadow-writing tool the distribution ships) without needing
//! to force a specific hash format on `/etc/shadow`.
//!
//! ## Why runtime dynamic-load
//!
//! `#[link(name = "crypt")]` compile-time linkage requires
//! `libcrypt`-dev headers in the target sysroot for every
//! cross-compile arch. `libloading` skips that entirely — the
//! symbol resolves at first `verify()` call from the target's
//! own `libcrypt.so.1`. Every Debian-family distribution
//! carries `libcrypt.so.1` as part of the base install
//! (`libcrypt1` package); no distribution-side install step is
//! required at runtime.
//!
//! ## Why a sibling crate
//!
//! The parent `evo` crate runs at `#![forbid(unsafe_code)]`.
//! FFI to `crypt_r(3)` requires `unsafe`. Splitting the wrapper
//! into a sibling crate whose `Cargo.toml` sets
//! `unsafe_code = "deny"` (locally relaxable) preserves the
//! parent crate's forbid posture while letting the two
//! well-documented `unsafe` blocks below live in a bounded
//! auditable surface. Same pattern as `evo-os-clock` for
//! `adjtimex(2)`.
//!
//! ## Local-only invariant
//!
//! This crate reaches no network authority at any code path.
//! Every symbol used here resolves from `libcrypt.so.1` (which
//! is a pure computational library — no PAM stack, no winbind,
//! no sssd, no LDAP, no Kerberos). A domain-controller outage
//! cannot affect any call in this crate. The verifier resolves
//! the runtime user via the local shadow file regardless of
//! winbind or sssd presence in the host's name-service stack.

use std::ffi::CStr;

use thiserror::Error;

/// `libcrypt` SONAME. Framework loads this via
/// [`libloading::Library::new`] at first [`verify`] call.
/// Runtime resolves the target's own `libcrypt.so.1` — every
/// Debian-family distribution ships this as part of the
/// `libcrypt1` package (base install).
pub const LIBCRYPT_SONAME: &str = "libcrypt.so.1";

/// `struct crypt_data` size — glibc uses ~32 KiB, libxcrypt
/// ~132 KiB. We allocate 256 KiB for headroom on any current
/// libcrypt build. The buffer is stack-owned per-call and
/// zeroed on allocation; no heap allocation on the hot path
/// beyond the Vec grow for the phrase / setting NUL-terminated
/// copies.
const CRYPT_DATA_BYTES: usize = 256 * 1024;

/// Errors surfaced by [`verify`].
#[derive(Debug, Error)]
pub enum VerifyError {
    /// `libcrypt.so.1` could not be dlopen'd. Indicates a
    /// broken sysroot on the target — every Debian ships the
    /// library as part of `libcrypt1` (base install). The
    /// underlying `libloading` diagnostic is preserved.
    #[error("libloading::Library::new({LIBCRYPT_SONAME}): {0}")]
    LibcryptLoadFailed(String),
    /// `crypt_r` symbol could not be resolved in the loaded
    /// library. Indicates a `libcrypt.so.1` that does not
    /// export the thread-safe reentrant variant — unusual for
    /// current libxcrypt but possible on older glibc-only
    /// builds. Same-shape error surface as `LibcryptLoadFailed`.
    #[error("libloading resolve crypt_r in {LIBCRYPT_SONAME}: {0}")]
    CryptRSymbolMissing(String),
    /// `crypt_r` returned NULL. In libxcrypt this means the
    /// input setting was rejected (malformed prefix, unknown
    /// algorithm) or an internal allocation failed. The stored
    /// hash cannot be verified against this password.
    #[error("crypt_r returned NULL — setting rejected or internal failure")]
    CryptRReturnedNull,
    /// `crypt_r` returned a non-UTF-8 string. crypt strings are
    /// canonically ASCII; a non-UTF-8 output signals a
    /// libcrypt-side defect and is treated as a verification
    /// failure.
    #[error("crypt_r produced non-UTF-8 output")]
    NonUtf8Output,
    /// The presented phrase contains an interior NUL byte. C
    /// strings cannot carry NUL bytes; the phrase would be
    /// truncated silently, so we refuse before calling.
    #[error("phrase contains an interior NUL byte")]
    PhraseHasInteriorNul,
    /// The stored hash / salt setting contains an interior NUL
    /// byte. Same reason as [`Self::PhraseHasInteriorNul`].
    #[error("setting contains an interior NUL byte")]
    SettingHasInteriorNul,
}

/// Verify that `phrase` matches `stored_hash` using `libcrypt`'s
/// `crypt_r(3)` routine.
///
/// The `stored_hash` argument is passed to `crypt_r` as the
/// "setting" parameter — `crypt_r` parses the `$id$` prefix
/// (and any algorithm-specific parameters) from it and produces
/// the canonical hash for `phrase` in the same format. This
/// crate then constant-time-compares the two.
///
/// Returns:
///
/// - `Ok(true)` — `crypt_r` produced a hash byte-identical to
///   `stored_hash`; the password matches.
/// - `Ok(false)` — `crypt_r` produced a different hash; the
///   password does not match. Also returned when `crypt_r`'s
///   output starts with `*0` / `*1` (libxcrypt convention for
///   "unsupported setting"), which will never match any legal
///   hash.
/// - `Err(VerifyError::*)` — a hard error preventing
///   verification (library not loadable, symbol missing,
///   phrase or setting malformed).
pub fn verify(phrase: &[u8], stored_hash: &str) -> Result<bool, VerifyError> {
    if phrase.contains(&0) {
        return Err(VerifyError::PhraseHasInteriorNul);
    }
    if stored_hash.as_bytes().contains(&0) {
        return Err(VerifyError::SettingHasInteriorNul);
    }
    let derived = call_crypt_r(phrase, stored_hash.as_bytes())?;
    Ok(constant_time_eq(derived.as_bytes(), stored_hash.as_bytes()))
}

/// Invoke `crypt_r(phrase, setting, &mut crypt_data)` on the
/// runtime-loaded `libcrypt.so.1`. Returns the canonical output
/// as an owned `String` copied out of the `crypt_data` buffer
/// before the buffer drops.
#[allow(unsafe_code)]
fn call_crypt_r(phrase: &[u8], setting: &[u8]) -> Result<String, VerifyError> {
    // NUL-terminate both C strings on the stack via Vec (heap;
    // small — sub-KiB — and short-lived).
    let mut phrase_c = Vec::with_capacity(phrase.len() + 1);
    phrase_c.extend_from_slice(phrase);
    phrase_c.push(0);
    let mut setting_c = Vec::with_capacity(setting.len() + 1);
    setting_c.extend_from_slice(setting);
    setting_c.push(0);

    // struct crypt_data must be zero-initialised per libcrypt
    // ABI. Box::new(...) heap-allocates; zeroed via the [0; N]
    // literal on construction.
    #[repr(C, align(8))]
    struct CryptData([u8; CRYPT_DATA_BYTES]);
    let mut data: Box<CryptData> = Box::new(CryptData([0u8; CRYPT_DATA_BYTES]));

    // dlopen libcrypt.so.1. Safety: passing a static string
    // literal as the library name; libloading validates.
    // SAFETY: `Library::new` is unsafe because loading arbitrary
    // shared libraries can execute arbitrary code (init/fini).
    // Here the library is `libcrypt.so.1` from a Debian base
    // install — a trusted system library the OS itself loads
    // for `passwd`, `login`, `sshd`, etc. No caller-controlled
    // path.
    let lib = unsafe { libloading::Library::new(LIBCRYPT_SONAME) }
        .map_err(|e| VerifyError::LibcryptLoadFailed(e.to_string()))?;

    // Resolve crypt_r. Signature:
    //   char *crypt_r(const char *phrase, const char *setting,
    //                 struct crypt_data *data);
    type CryptRFn = unsafe extern "C" fn(
        *const libc::c_char,
        *const libc::c_char,
        *mut libc::c_void,
    ) -> *const libc::c_char;

    // SAFETY: `Library::get` is unsafe because the returned
    // symbol's ABI must match the type we cast to. `crypt_r`'s
    // signature is stable across glibc + libxcrypt versions:
    // three pointer args, one pointer return. Documented in the
    // POSIX `crypt(3)` man page and libxcrypt's `crypt.h`.
    let crypt_r: libloading::Symbol<'_, CryptRFn> =
        unsafe { lib.get(b"crypt_r\0") }
            .map_err(|e| VerifyError::CryptRSymbolMissing(e.to_string()))?;

    // SAFETY: All three pointer args are non-NULL and point at
    // properly-owned + properly-sized memory:
    //   - phrase_c and setting_c are stack-owned Vecs, NUL-
    //     terminated, byte layout matches `const char *`.
    //   - data is a Box-owned zero-initialised CRYPT_DATA_BYTES
    //     buffer; `crypt_r` reads and writes it in place.
    // The returned pointer references memory inside the same
    // `data` buffer; we copy the C string out before this
    // function returns and the buffer drops.
    let result_ptr = unsafe {
        crypt_r(
            phrase_c.as_ptr() as *const libc::c_char,
            setting_c.as_ptr() as *const libc::c_char,
            data.0.as_mut_ptr() as *mut libc::c_void,
        )
    };
    if result_ptr.is_null() {
        return Err(VerifyError::CryptRReturnedNull);
    }

    // SAFETY: `result_ptr` is non-NULL (checked above) and
    // references a NUL-terminated C string produced by
    // `crypt_r` inside the still-live `data` buffer. `CStr::
    // from_ptr` requires only that the pointer is valid for at
    // least one byte and is NUL-terminated; both guaranteed by
    // the `crypt_r` contract. The returned CStr borrows from
    // `data`; we copy to owned `String` before the borrow ends.
    let cstr = unsafe { CStr::from_ptr(result_ptr) };
    let derived = std::str::from_utf8(cstr.to_bytes())
        .map_err(|_| VerifyError::NonUtf8Output)?
        .to_string();
    Ok(derived)
}

/// Produce a canonical `crypt(3)` hash for `phrase` under the
/// supplied `setting` prefix using the same runtime-loaded
/// `libcrypt.so.1` that [`verify`] would consult. Intended for
/// test scaffolding — hand a setting like `"$y$j9T$abcdefghij"`
/// (yescrypt), `"$6$abcdefghij"` (SHA-512-crypt), or
/// `"$5$abcdefghij"` (SHA-256-crypt) and receive the
/// libcrypt-computed hash back.
///
/// A returned string beginning with `*` signals libxcrypt's
/// "unsupported setting" convention (the local `libcrypt.so.1`
/// does not implement the requested algorithm). Callers should
/// try a different setting or skip the test.
///
/// Public so downstream crates (like `evo` itself) can build
/// regression tests around real crypt hashes without
/// re-implementing the FFI wrapper.
pub fn hash_for_test(
    phrase: &[u8],
    setting: &str,
) -> Result<String, VerifyError> {
    if phrase.contains(&0) {
        return Err(VerifyError::PhraseHasInteriorNul);
    }
    if setting.as_bytes().contains(&0) {
        return Err(VerifyError::SettingHasInteriorNul);
    }
    call_crypt_r(phrase, setting.as_bytes())
}

/// Constant-time byte-slice equality. Public so callers that
/// compose this crate can use the same routine for their own
/// comparisons; kept small + auditable.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
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

    /// Sanity: dlopen'ing libcrypt.so.1 must succeed on any
    /// Debian-family test host. If this fails, the sysroot is
    /// broken; other tests in this file will fail identically
    /// so the diagnostic here is the entry point for triage.
    #[test]
    fn libcrypt_loads_on_this_host() {
        // Reach into the internals via a dummy call: phrase +
        // setting are arbitrary; we only care whether the load
        // + resolve steps succeed.
        let result = call_crypt_r(b"probe", b"$6$abcdefghij");
        assert!(
            !matches!(
                result,
                Err(VerifyError::LibcryptLoadFailed(_))
                    | Err(VerifyError::CryptRSymbolMissing(_))
            ),
            "libcrypt.so.1 unavailable on this test host: {result:?}",
        );
    }

    #[test]
    fn verify_matches_sha512_hash_from_libcrypt() {
        // Generate a $6$ (SHA-512-crypt) hash for "correct-pw"
        // via crypt_r itself, then verify against it.
        let stored = call_crypt_r(b"correct-pw", b"$6$abcdefghij")
            .expect("crypt_r must produce a hash for $6$");
        assert!(
            verify(b"correct-pw", &stored).unwrap(),
            "verify against known-good SHA-512 hash: {stored:?}"
        );
        assert!(
            !verify(b"wrong-pw", &stored).unwrap(),
            "verify against wrong password must return false"
        );
    }

    /// Yescrypt round-trip: modern Debian defaults write
    /// yescrypt via `passwd`, so the framework must verify
    /// OS-issued yescrypt hashes without forcing SHA-512.
    /// Skipped on hosts whose libcrypt does not implement
    /// yescrypt (rare on current Debian).
    #[test]
    fn verify_succeeds_against_yescrypt_hash() {
        let hashed = call_crypt_r(b"trixie-default", b"$y$j9T$abcdefghij");
        let stored = match hashed {
            Ok(s) if !s.starts_with('*') => s,
            other => {
                eprintln!(
                    "skipping verify_succeeds_against_yescrypt_hash — \
                     libcrypt.so.1 on this host does not implement \
                     yescrypt: {other:?}"
                );
                return;
            }
        };
        assert!(stored.starts_with("$y$"), "stored hash: {stored:?}");
        assert!(
            verify(b"trixie-default", &stored).unwrap(),
            "yescrypt verify against known-good password must succeed"
        );
        assert!(
            !verify(b"wrong-pw", &stored).unwrap(),
            "yescrypt verify against wrong password must return false"
        );
    }

    /// Additional coverage — every classic Unix-crypt format the
    /// framework may encounter in an older shadow row. Each
    /// generates its own hash via crypt_r and verifies against
    /// it; test proves the same code path handles all of them
    /// uniformly.
    #[test]
    fn verify_covers_all_libcrypt_formats() {
        for setting in [
            "$6$abcdefghij", // SHA-512-crypt
            "$5$abcdefghij", // SHA-256-crypt
            "$1$abcdefgh",   // MD5-crypt
        ] {
            let hashed = call_crypt_r(b"probe-pw", setting.as_bytes())
                .expect("crypt_r loads");
            if hashed.starts_with('*') {
                eprintln!(
                    "skipping format {setting:?} — libcrypt on this host \
                     does not implement it: {hashed:?}"
                );
                continue;
            }
            assert!(
                verify(b"probe-pw", &hashed).unwrap(),
                "format {setting:?}: verify against known-good password \
                 failed; hash: {hashed:?}"
            );
        }
    }

    #[test]
    fn verify_refuses_interior_nul_in_phrase() {
        let err = verify(b"has\0nul", "$6$salt$hash").unwrap_err();
        assert!(matches!(err, VerifyError::PhraseHasInteriorNul));
    }

    #[test]
    fn verify_refuses_interior_nul_in_setting() {
        let err = verify(b"phrase", "$6$has\0nul$hash").unwrap_err();
        assert!(matches!(err, VerifyError::SettingHasInteriorNul));
    }

    #[test]
    fn constant_time_eq_matches_only_full_match() {
        assert!(constant_time_eq(b"abcd", b"abcd"));
        assert!(!constant_time_eq(b"abcd", b"abce"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }
}
