//! Device tier — coarse compute / memory / OS-shape classifier.
//!
//! The framework targets six orders of magnitude of compute
//! between MCU (32 kB RAM, no MMU, firmware-image lifecycle) and
//! server (128+ GB RAM, multi-socket, regular Linux). Several
//! framework decisions are tier-dependent: lifecycle modes that
//! are tier-incompatible (`reload-cleanable` requires a process
//! supervisor that MCU has no notion of), substrate carriers
//! that are unavailable on smaller tiers (audio-plane TCP on
//! MCU), resource ceilings, and admission policy.
//!
//! This module names the four tiers the framework recognises and
//! provides a single `current_tier()` accessor the rest of the
//! framework consults. On every Linux device the framework
//! defaults to `LinuxFullParticipant`; the `EVO_TIER` environment
//! variable overrides the detection so synthetic tier-refusal
//! tests can exercise the MCU admission gate without an actual
//! MCU on the rig.
//!
//! Resource posture: the accessor reads the env var once on first
//! call and memoises the result; subsequent calls are a `Relaxed`
//! load on an atomic. No per-call allocation or syscall.

use std::env;
use std::sync::atomic::{AtomicU8, Ordering};

/// The device tier the framework runs on. Coarse-grained enough
/// to drive admission decisions without imposing per-SKU
/// classification work on the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tier {
    /// x86/Epyc server tier — high core count, large RAM,
    /// regular Linux. No tier-specific admission restrictions.
    Server,
    /// Linux SBC / small-form-factor PC / regular Linux laptop —
    /// single or low-multi-core, GB-scale RAM, full systemd +
    /// tokio runtime.
    LinuxFullParticipant,
    /// Pi Zero / very small ARM SBC — embedded Linux with
    /// constrained RAM (256-512 MB) and a single ARMv6/v7 core.
    /// Process restart cost is operator-perceptible
    /// (10-20 seconds); reload-cleanable plugins admit with an
    /// operator-visible warning.
    EmbeddedLinux,
    /// ESP32-class microcontroller — no MMU, no OS-process
    /// concept, firmware-image lifecycle. The framework's
    /// reload-cleanable mode is not implementable on this tier;
    /// admission refuses it.
    Mcu,
}

impl Tier {
    /// Parse a tier name from a string. Recognised forms
    /// (case-insensitive): `server`, `linux`,
    /// `linux-full-participant`, `embedded`, `embedded-linux`,
    /// `mcu`. Returns `None` for any other input so callers can
    /// fail loudly on misconfiguration instead of silently
    /// defaulting.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "server" => Some(Tier::Server),
            "linux" | "linux-full-participant" => {
                Some(Tier::LinuxFullParticipant)
            }
            "embedded" | "embedded-linux" => Some(Tier::EmbeddedLinux),
            "mcu" => Some(Tier::Mcu),
            _ => None,
        }
    }

    /// Stable string identifier for the tier — used in
    /// structured-error payloads, happenings, and operator-
    /// visible diagnostic output. Kebab-case to match the
    /// manifest's `[lifecycle].mode` conventions.
    pub fn as_kebab(self) -> &'static str {
        match self {
            Tier::Server => "server",
            Tier::LinuxFullParticipant => "linux-full-participant",
            Tier::EmbeddedLinux => "embedded-linux",
            Tier::Mcu => "mcu",
        }
    }
}

/// Memoised tier slot. `0xFF` = unread. Other values map to the
/// `Tier` enum via `tier_from_u8` / `tier_to_u8`.
static CACHED_TIER: AtomicU8 = AtomicU8::new(0xFF);

const TIER_UNREAD: u8 = 0xFF;

fn tier_to_u8(t: Tier) -> u8 {
    match t {
        Tier::Server => 0,
        Tier::LinuxFullParticipant => 1,
        Tier::EmbeddedLinux => 2,
        Tier::Mcu => 3,
    }
}

fn tier_from_u8(b: u8) -> Option<Tier> {
    match b {
        0 => Some(Tier::Server),
        1 => Some(Tier::LinuxFullParticipant),
        2 => Some(Tier::EmbeddedLinux),
        3 => Some(Tier::Mcu),
        _ => None,
    }
}

/// Return the device's runtime tier. Reads the `EVO_TIER`
/// environment variable on first call; subsequent calls return
/// the memoised value.
///
/// The override exists so acceptance-scenario runs can exercise
/// the MCU-admission refusal path without an actual MCU. Production
/// deployments set the var (or omit it for the
/// `LinuxFullParticipant` default) at boot via the systemd
/// unit's `Environment=` block.
pub fn current_tier() -> Tier {
    let cached = CACHED_TIER.load(Ordering::Relaxed);
    if cached != TIER_UNREAD {
        if let Some(t) = tier_from_u8(cached) {
            return t;
        }
    }
    let tier = env::var("EVO_TIER")
        .ok()
        .and_then(|v| Tier::parse(&v))
        .unwrap_or(Tier::LinuxFullParticipant);
    CACHED_TIER.store(tier_to_u8(tier), Ordering::Relaxed);
    tier
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_recognises_known_names() {
        assert_eq!(Tier::parse("server"), Some(Tier::Server));
        assert_eq!(
            Tier::parse("linux-full-participant"),
            Some(Tier::LinuxFullParticipant)
        );
        assert_eq!(Tier::parse("linux"), Some(Tier::LinuxFullParticipant));
        assert_eq!(Tier::parse("embedded-linux"), Some(Tier::EmbeddedLinux));
        assert_eq!(Tier::parse("embedded"), Some(Tier::EmbeddedLinux));
        assert_eq!(Tier::parse("mcu"), Some(Tier::Mcu));
    }

    #[test]
    fn parse_is_case_insensitive() {
        assert_eq!(Tier::parse("SERVER"), Some(Tier::Server));
        assert_eq!(Tier::parse("McU"), Some(Tier::Mcu));
    }

    #[test]
    fn parse_trims_whitespace() {
        assert_eq!(Tier::parse("  mcu  \n"), Some(Tier::Mcu));
    }

    #[test]
    fn parse_rejects_unknown() {
        assert_eq!(Tier::parse("workstation"), None);
        assert_eq!(Tier::parse(""), None);
    }

    #[test]
    fn as_kebab_is_round_trippable() {
        for t in [
            Tier::Server,
            Tier::LinuxFullParticipant,
            Tier::EmbeddedLinux,
            Tier::Mcu,
        ] {
            assert_eq!(Tier::parse(t.as_kebab()), Some(t));
        }
    }

    #[test]
    fn u8_round_trip_covers_all_variants() {
        for t in [
            Tier::Server,
            Tier::LinuxFullParticipant,
            Tier::EmbeddedLinux,
            Tier::Mcu,
        ] {
            assert_eq!(tier_from_u8(tier_to_u8(t)), Some(t));
        }
    }
}
