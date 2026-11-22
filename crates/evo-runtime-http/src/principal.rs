// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Authenticated caller context attached to each request.

use evo_auth_bearer::CapabilitySet;

/// Token id of the principal an anonymous route dispatches under.
///
/// A capability-none route skips the bearer check, so its handler
/// never sees a token. Distinct from
/// [`crate::middleware::LAN_TRUST_TOKEN_ID`], which names a caller
/// admitted on the no-bearer capability set.
pub const ANONYMOUS_TOKEN_ID: &str = "anonymous";

/// Where a caller reached the device from, as the HTTPS substrate
/// classified it when the request was admitted.
///
/// The classification reads the effective origin: the immediate
/// peer, or a trusted-proxy `X-Forwarded-For` when that peer is
/// loopback (see [`crate::auth_tier::effective_origin`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallerOrigin {
    /// A local-network address: RFC1918, loopback, link-local or
    /// IPv6 unique-local (see [`crate::auth_tier::is_lan_origin`]).
    Lan,
    /// Any other address.
    Public,
}

impl CallerOrigin {
    /// [`Self::Lan`] when `lan` holds, [`Self::Public`] otherwise.
    pub fn from_lan(lan: bool) -> Self {
        if lan {
            Self::Lan
        } else {
            Self::Public
        }
    }
}

/// The authenticated caller behind a request.
///
/// Constructed by the bearer-token middleware after the
/// token is validated; held in the request extensions so
/// route handlers and the [`crate::Dispatcher`] can attribute
/// the work.
#[derive(Debug, Clone)]
pub struct Principal {
    /// The bearer token id (its `id` field). Surfaces in
    /// audit ledger entries so an operator can correlate a
    /// request with the token that authorised it.
    pub token_id: String,

    /// The capability set the token carries. The
    /// per-endpoint middleware has already verified that
    /// this set satisfies the endpoint's capability
    /// requirement, but downstream code (e.g. fine-grained
    /// per-resource checks inside the steward) may consult
    /// the set to make additional decisions.
    pub capabilities: CapabilitySet,

    /// Where the caller came from. Stamped by the HTTPS route gate
    /// and the WebSocket upgrade. `None` when nothing classified
    /// it: an in-process dispatch, another projection, or a test.
    /// Code that admits by origin treats `None` as not local.
    pub origin: Option<CallerOrigin>,
}

impl Principal {
    /// Construct a principal from a verified token id and
    /// capability set. The origin is unclassified until
    /// [`Self::with_origin`] stamps it.
    pub fn new(
        token_id: impl Into<String>,
        capabilities: CapabilitySet,
    ) -> Self {
        Self {
            token_id: token_id.into(),
            capabilities,
            origin: None,
        }
    }

    /// Stamp the caller's origin.
    #[must_use]
    pub fn with_origin(mut self, origin: CallerOrigin) -> Self {
        self.origin = Some(origin);
        self
    }

    /// Whether the caller presented a bearer.
    ///
    /// False for the LAN-trust principal (admitted on the no-bearer
    /// capability set), the anonymous principal of a capability-none
    /// route, and a principal with no token id at all. Anything a
    /// bearer is needed for refuses a caller for whom this is false.
    pub fn presented_bearer(&self) -> bool {
        !self.token_id.is_empty()
            && self.token_id != crate::middleware::LAN_TRUST_TOKEN_ID
            && self.token_id != ANONYMOUS_TOKEN_ID
    }

    /// Whether the caller's origin was classified as local.
    pub fn is_lan_origin(&self) -> bool {
        self.origin == Some(CallerOrigin::Lan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_principal_starts_unclassified() {
        let p = Principal::new("t", CapabilitySet::default());
        assert_eq!(p.origin, None);
        assert!(!p.is_lan_origin());
        assert!(p.with_origin(CallerOrigin::Lan).is_lan_origin());
    }

    #[test]
    fn only_a_minted_token_id_counts_as_a_bearer() {
        let caps = CapabilitySet::default;
        assert!(Principal::new("tok-1", caps()).presented_bearer());
        assert!(
            !Principal::new(crate::middleware::LAN_TRUST_TOKEN_ID, caps())
                .presented_bearer()
        );
        assert!(!Principal::new(ANONYMOUS_TOKEN_ID, caps()).presented_bearer());
        assert!(!Principal::new("", caps()).presented_bearer());
    }
}
