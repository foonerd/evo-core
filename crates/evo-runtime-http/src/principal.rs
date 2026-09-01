// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Authenticated caller context attached to each request.

use evo_auth_bearer::CapabilitySet;

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
}

impl Principal {
    /// Construct a principal from a verified token id and
    /// capability set.
    pub fn new(
        token_id: impl Into<String>,
        capabilities: CapabilitySet,
    ) -> Self {
        Self {
            token_id: token_id.into(),
            capabilities,
        }
    }
}
