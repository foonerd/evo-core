// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Auth-tier model for the runtime mount.
//!
//! The framework's `AuthLayer` middleware admits a request
//! when a valid bearer token is presented AND its capability
//! set satisfies the route's requirement. The auth tier shapes
//! the **bearer-absent** path:
//!
//! - `Open` — admit the request anyway with a synthetic
//!   operator [`Principal`] carrying every operator scope.
//!   The operator on a trusted LAN opens the browser and
//!   works; no credential, no pairing, no typing.
//! - `Secure` — admit on LAN origin (RFC1918, loopback,
//!   link-local, IPv6 unique-local) so the operator's own
//!   browser on a trusted LAN works without a credential;
//!   refuse WAN-origin bearer-absent requests so external
//!   API consumers must present a bearer credential.
//! - `SecureIndustrial` — same admission shape as `Secure`
//!   at the framework layer; the operator-set per-credential
//!   expiry policy is enforced at credential validation.
//!
//! Default is `Open`. The operator opts into a stricter tier
//! via the UI tier-toggle screen or via the `EVO_AUTH_TIER`
//! environment variable for CLI / installer-driven control.
//!
//! [`Principal`]: crate::principal::Principal

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;

/// The operator-selectable auth tier. Controls how the
/// framework admits requests that arrive without a bearer
/// token; bearer-bearing requests continue through the
/// capability-validation path regardless of tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthTier {
    /// LAN-trust everywhere. Requests without a bearer
    /// header are admitted as the operator. Default tier.
    Open,
    /// LAN-trust for the operator's own browser only.
    /// External API consumers reaching the device from
    /// WAN origin must present a credential. Credentials
    /// minted under this tier carry no operator-set
    /// expiry policy (the per-credential expiry policy is
    /// the [`AuthTier::SecureIndustrial`] distinction).
    Secure,
    /// LAN-trust for the operator's own browser only. Each
    /// external API consumer's credential carries an
    /// operator-set scope + expiry policy enforced at
    /// validation time.
    SecureIndustrial,
}

impl AuthTier {
    /// Stable wire-string used in observation attributes and
    /// in operator-facing surfaces.
    pub fn as_str(self) -> &'static str {
        match self {
            AuthTier::Open => "open",
            AuthTier::Secure => "secure",
            AuthTier::SecureIndustrial => "secure_industrial",
        }
    }
}

impl fmt::Display for AuthTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Runtime-queryable source of the current auth tier.
///
/// The middleware consults the provider on every request so
/// the operator can flip tiers without restarting the
/// runtime — the UI's tier-toggle screen writes through this
/// surface. Implementations must be cheap to call at request
/// rate.
pub trait AuthTierProvider: Send + Sync + fmt::Debug {
    /// The tier the next admission decision will use.
    fn current(&self) -> AuthTier;
}

/// Provider that returns a fixed tier for the runtime's
/// lifetime. Suitable for env-var-configured deployments
/// where the operator picks a tier at install time and
/// restarts the runtime to apply changes.
#[derive(Debug, Clone)]
pub struct StaticAuthTier(AuthTier);

impl StaticAuthTier {
    /// Construct a provider that always returns `tier`.
    pub fn new(tier: AuthTier) -> Self {
        Self(tier)
    }

    /// The tier this provider is pinned to.
    pub fn tier(&self) -> AuthTier {
        self.0
    }
}

impl AuthTierProvider for StaticAuthTier {
    fn current(&self) -> AuthTier {
        self.0
    }
}

/// Read the operator-selected tier from the `EVO_AUTH_TIER`
/// environment variable. Recognised values: `open`,
/// `secure`, `secure_industrial` (alias `secure-industrial`).
/// Anything else — unset, empty, unrecognised — yields
/// `AuthTier::Open` (the default tier).
pub fn auth_tier_from_env() -> AuthTier {
    match std::env::var("EVO_AUTH_TIER")
        .ok()
        .map(|v| v.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("secure") => AuthTier::Secure,
        Some("secure_industrial") | Some("secure-industrial") => {
            AuthTier::SecureIndustrial
        }
        _ => AuthTier::Open,
    }
}

/// Default provider — env-var-driven static tier. Caller
/// holds the resulting `Arc<dyn AuthTierProvider>` for the
/// runtime's lifetime.
pub fn default_auth_tier_provider() -> Arc<dyn AuthTierProvider> {
    Arc::new(StaticAuthTier::new(auth_tier_from_env()))
}

/// True when `addr` is a loopback address — either IPv4
/// (`127/8`) or IPv6 (`::1`) including IPv4-mapped-IPv6
/// (`::ffff:127.x.x.x`).
fn is_loopback_addr(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return true;
            }
            if let Some(v4) = v6.to_ipv4_mapped() {
                return v4.is_loopback();
            }
            false
        }
    }
}

/// Determine the effective request origin for LAN-trust
/// classification.
///
/// In the normal case — the immediate TCP peer is the
/// operator's browser on the LAN — the effective origin is
/// simply the peer's IP address.
///
/// In the trusted-proxy case — the immediate TCP peer is
/// **loopback** (the operator's browser reaches the framework
/// indirectly via a trusted reverse-proxy on the same host) —
/// the effective origin is read from the standard
/// `X-Forwarded-For` header on the request. The leftmost
/// entry of the header (the original client per the de-facto
/// convention) is the effective origin; the remaining entries
/// describe the proxy chain and are ignored.
///
/// The trust gate is **immediate-peer-must-be-loopback**.
/// External attackers cannot reach a loopback peer (only
/// processes on the same host can), so trusting an
/// operator-supplied header on a loopback connection is
/// safe. When the immediate peer is NOT loopback, the
/// header is ignored entirely — preventing header-spoofing
/// from arbitrary external clients claiming LAN-origin.
///
/// When the header is malformed, missing, or not parseable
/// as an IP, the function falls back to the immediate peer
/// address (the safe-default).
pub fn effective_origin(
    peer: IpAddr,
    headers: &axum::http::HeaderMap,
) -> IpAddr {
    if !is_loopback_addr(peer) {
        return peer;
    }
    let Some(xff) = headers.get("x-forwarded-for") else {
        return peer;
    };
    let Ok(xff_str) = xff.to_str() else {
        return peer;
    };
    let Some(first) = xff_str.split(',').next() else {
        return peer;
    };
    match first.trim().parse::<IpAddr>() {
        Ok(addr) => addr,
        Err(_) => peer,
    }
}

/// Classify a peer IP as LAN-origin. Returns `true` for
/// RFC1918 IPv4 ranges (`10/8`, `172.16/12`, `192.168/16`),
/// loopback (`127/8`, `::1`), link-local (`169.254/16`,
/// `fe80::/10`), and IPv6 unique-local (`fc00::/7`). Returns
/// `false` for everything else (publicly-routable WAN
/// origin or unrecognised address family).
///
/// Used by the auth-tier admission path under `Secure` /
/// `SecureIndustrial` tiers to admit the operator's own
/// browser on a trusted LAN without presenting a bearer
/// credential, while external API consumers (which arrive
/// from WAN origin) continue to require one. Open tier
/// admits regardless of origin and does not consult this
/// helper.
pub fn is_lan_origin(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            // 10.0.0.0/8
            if octets[0] == 10 {
                return true;
            }
            // 172.16.0.0/12
            if octets[0] == 172 && (16..=31).contains(&octets[1]) {
                return true;
            }
            // 192.168.0.0/16
            if octets[0] == 192 && octets[1] == 168 {
                return true;
            }
            // 127.0.0.0/8 loopback
            if v4.is_loopback() {
                return true;
            }
            // 169.254.0.0/16 link-local
            if v4.is_link_local() {
                return true;
            }
            false
        }
        IpAddr::V6(v6) => {
            // ::1 loopback
            if v6.is_loopback() {
                return true;
            }
            // fe80::/10 link-local
            let seg0 = v6.segments()[0];
            if (seg0 & 0xffc0) == 0xfe80 {
                return true;
            }
            // fc00::/7 unique-local
            if (seg0 & 0xfe00) == 0xfc00 {
                return true;
            }
            // ::ffff:0:0/96 IPv4-mapped — classify by the
            // mapped IPv4
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_lan_origin(IpAddr::V4(mapped));
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    use axum::http::HeaderMap;

    fn hdr(name: &str, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            name.parse::<axum::http::HeaderName>().unwrap(),
            value.parse().unwrap(),
        );
        h
    }

    #[test]
    fn effective_origin_returns_peer_when_not_loopback() {
        // Peer is a real LAN address; X-Forwarded-For must be
        // ignored entirely (header-spoof defence).
        let peer = IpAddr::V4(Ipv4Addr::new(192, 168, 30, 24));
        let headers = hdr("x-forwarded-for", "8.8.8.8");
        assert_eq!(effective_origin(peer, &headers), peer);
    }

    #[test]
    fn effective_origin_returns_peer_when_loopback_no_header() {
        let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let headers = HeaderMap::new();
        assert_eq!(effective_origin(peer, &headers), peer);
    }

    #[test]
    fn effective_origin_returns_header_when_loopback_with_xff() {
        let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let headers = hdr("x-forwarded-for", "192.0.2.10");
        assert_eq!(
            effective_origin(peer, &headers),
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))
        );
    }

    #[test]
    fn effective_origin_takes_leftmost_entry_from_chain() {
        let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let headers =
            hdr("x-forwarded-for", "192.0.2.10, 10.0.0.5, 172.16.0.1");
        assert_eq!(
            effective_origin(peer, &headers),
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))
        );
    }

    #[test]
    fn effective_origin_tolerates_whitespace() {
        let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let headers = hdr("x-forwarded-for", "   192.0.2.10   ");
        assert_eq!(
            effective_origin(peer, &headers),
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))
        );
    }

    #[test]
    fn effective_origin_falls_back_on_malformed_header() {
        let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let headers = hdr("x-forwarded-for", "not-an-ip");
        assert_eq!(effective_origin(peer, &headers), peer);
    }

    #[test]
    fn effective_origin_honours_ipv6_loopback_peer() {
        let peer = IpAddr::V6(Ipv6Addr::LOCALHOST);
        let headers = hdr("x-forwarded-for", "192.0.2.10");
        assert_eq!(
            effective_origin(peer, &headers),
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))
        );
    }

    #[test]
    fn effective_origin_honours_ipv4_mapped_ipv6_loopback() {
        // ::ffff:127.0.0.1 — IPv4-mapped IPv6 loopback. Must
        // also gate trust on the header.
        let peer = IpAddr::V6(Ipv4Addr::LOCALHOST.to_ipv6_mapped());
        let headers = hdr("x-forwarded-for", "192.0.2.10");
        assert_eq!(
            effective_origin(peer, &headers),
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))
        );
    }

    #[test]
    fn effective_origin_ignores_header_from_non_loopback_ipv6() {
        // Public IPv6 peer with a spoofed header — header
        // MUST be ignored.
        let peer = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
        let headers = hdr("x-forwarded-for", "192.0.2.10");
        assert_eq!(effective_origin(peer, &headers), peer);
    }

    #[test]
    fn lan_origin_admits_rfc1918_ranges() {
        assert!(is_lan_origin(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(is_lan_origin(IpAddr::V4(Ipv4Addr::new(10, 255, 255, 254))));
        assert!(is_lan_origin(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(is_lan_origin(IpAddr::V4(Ipv4Addr::new(172, 31, 255, 254))));
        assert!(is_lan_origin(IpAddr::V4(Ipv4Addr::new(192, 168, 0, 1))));
        assert!(is_lan_origin(IpAddr::V4(Ipv4Addr::new(192, 168, 30, 24))));
    }

    #[test]
    fn lan_origin_refuses_outside_rfc1918() {
        assert!(!is_lan_origin(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_lan_origin(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        assert!(!is_lan_origin(IpAddr::V4(Ipv4Addr::new(172, 15, 0, 1))));
        assert!(!is_lan_origin(IpAddr::V4(Ipv4Addr::new(172, 32, 0, 1))));
        assert!(!is_lan_origin(IpAddr::V4(Ipv4Addr::new(192, 169, 0, 1))));
    }

    #[test]
    fn lan_origin_admits_loopback_and_link_local() {
        assert!(is_lan_origin(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(is_lan_origin(IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1))));
        assert!(is_lan_origin(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    #[test]
    fn lan_origin_admits_ipv6_link_local_and_unique_local() {
        // fe80::1 — link-local
        assert!(is_lan_origin(IpAddr::V6(Ipv6Addr::new(
            0xfe80, 0, 0, 0, 0, 0, 0, 1
        ))));
        // fc00::1 — unique-local
        assert!(is_lan_origin(IpAddr::V6(Ipv6Addr::new(
            0xfc00, 0, 0, 0, 0, 0, 0, 1
        ))));
        // fd12:3456::1 — unique-local
        assert!(is_lan_origin(IpAddr::V6(Ipv6Addr::new(
            0xfd12, 0x3456, 0, 0, 0, 0, 0, 1
        ))));
    }

    #[test]
    fn lan_origin_refuses_global_ipv6() {
        // 2001:db8::1 — documentation prefix, globally
        // unique by definition (not LAN-origin even though
        // it's a reserved range)
        assert!(!is_lan_origin(IpAddr::V6(Ipv6Addr::new(
            0x2001, 0x0db8, 0, 0, 0, 0, 0, 1
        ))));
    }

    #[test]
    fn lan_origin_classifies_ipv4_mapped_ipv6_by_mapped_address() {
        // A `::ffff:<LAN v4>` mapped address should classify as
        // LAN. The IPv4 half is a real RFC 1918 address the
        // classifier resolves against `192.168.0.0/16`.
        let mapped_lan = Ipv4Addr::new(192, 168, 1, 1).to_ipv6_mapped();
        assert!(is_lan_origin(IpAddr::V6(mapped_lan)));
        // ::ffff:8.8.8.8 — should classify as WAN
        let mapped_wan = Ipv4Addr::new(8, 8, 8, 8).to_ipv6_mapped();
        assert!(!is_lan_origin(IpAddr::V6(mapped_wan)));
    }

    #[test]
    fn auth_tier_as_str_renders_canonical_strings() {
        assert_eq!(AuthTier::Open.as_str(), "open");
        assert_eq!(AuthTier::Secure.as_str(), "secure");
        assert_eq!(AuthTier::SecureIndustrial.as_str(), "secure_industrial");
    }

    #[test]
    fn static_provider_returns_pinned_tier() {
        let p = StaticAuthTier::new(AuthTier::Secure);
        assert_eq!(p.current(), AuthTier::Secure);
        assert_eq!(p.tier(), AuthTier::Secure);
    }

    /// Serialise env-mutating tests. Rust's default test
    /// harness runs tests in parallel; every test in this
    /// suite that touches `EVO_AUTH_TIER` acquires this
    /// mutex first so concurrent set/remove operations
    /// cannot clobber each other's expected state. Without
    /// it, `env_unset_yields_open`'s `remove_var` and
    /// `env_secure_yields_secure`'s `set_var` race, and
    /// either observes an unexpected env value.
    fn env_test_lock() -> &'static std::sync::Mutex<()> {
        use std::sync::OnceLock;
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[test]
    fn env_unset_yields_open() {
        let _g = env_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var_os("EVO_AUTH_TIER");
        std::env::remove_var("EVO_AUTH_TIER");
        assert_eq!(auth_tier_from_env(), AuthTier::Open);
        if let Some(v) = prior {
            std::env::set_var("EVO_AUTH_TIER", v);
        }
    }

    #[test]
    fn env_secure_yields_secure() {
        let _g = env_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var_os("EVO_AUTH_TIER");
        std::env::set_var("EVO_AUTH_TIER", "secure");
        assert_eq!(auth_tier_from_env(), AuthTier::Secure);
        match prior {
            Some(v) => std::env::set_var("EVO_AUTH_TIER", v),
            None => std::env::remove_var("EVO_AUTH_TIER"),
        }
    }

    #[test]
    fn env_secure_industrial_accepts_both_separators() {
        let _g = env_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var_os("EVO_AUTH_TIER");
        std::env::set_var("EVO_AUTH_TIER", "secure_industrial");
        assert_eq!(auth_tier_from_env(), AuthTier::SecureIndustrial);
        std::env::set_var("EVO_AUTH_TIER", "secure-industrial");
        assert_eq!(auth_tier_from_env(), AuthTier::SecureIndustrial);
        match prior {
            Some(v) => std::env::set_var("EVO_AUTH_TIER", v),
            None => std::env::remove_var("EVO_AUTH_TIER"),
        }
    }

    #[test]
    fn env_unrecognised_yields_open() {
        let _g = env_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var_os("EVO_AUTH_TIER");
        std::env::set_var("EVO_AUTH_TIER", "experimental");
        assert_eq!(auth_tier_from_env(), AuthTier::Open);
        match prior {
            Some(v) => std::env::set_var("EVO_AUTH_TIER", v),
            None => std::env::remove_var("EVO_AUTH_TIER"),
        }
    }

    #[test]
    fn env_case_insensitive_and_trimmed() {
        let _g = env_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var_os("EVO_AUTH_TIER");
        std::env::set_var("EVO_AUTH_TIER", "  Secure  ");
        assert_eq!(auth_tier_from_env(), AuthTier::Secure);
        match prior {
            Some(v) => std::env::set_var("EVO_AUTH_TIER", v),
            None => std::env::remove_var("EVO_AUTH_TIER"),
        }
    }
}
