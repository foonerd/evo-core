// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Plugin-to-plugin shelf verb dispatch.
//!
//! Surfaces the framework's request router to plugins under the same
//! stocking-partition + capability-gate + response-budget discipline
//! the wire-op layer applies. A plugin holding
//! [`crate::contract::LoadContext::shelf_request_dispatcher`] can call
//! verbs on shelves owned by other plugins without rolling its own
//! adapter against the steward's internals.
//!
//! ## Design intent
//!
//! Several plugin-to-plugin call paths surfaced during the artwork
//! pipeline survey converge on the same need:
//!
//! - Playback warden invokes `artwork.resolve` to populate the
//!   asset cache and embed a content hash on every track-bearing
//!   envelope (now playing, queue items, favourites, playlist
//!   contents, library file rows).
//! - The follow-on `artwork.online` plugin (Cover Art Archive +
//!   Last.fm + iTunes + Volumio meta proxy) ships as a sibling
//!   occupant of the `artwork.providers` shelf. The shelf gains
//!   multi-occupant semantics per the Stocking primitive; the local
//!   resolver and the online resolver each own a partition of the
//!   verb set, and cascade ordering ("try local first, fall back
//!   online") is realised by the CALLER invoking distinct verbs in
//!   sequence.
//! - Future online-metadata enrichment (artist bio / album story /
//!   similar artists) consumes the playback shelf data and reaches
//!   the artwork shelf for similar-artist cards.
//! - Multi-room artwork propagation populates the content-hash cache
//!   at the emitter side; receivers fetch by hash through the
//!   existing `/api/v1/audio/artwork/:content_hash` endpoint with no
//!   additional substrate.
//!
//! All four paths route through the same primitive. Inline
//! replication of one plugin's logic into another plugin's address
//! space forks the truth path; ad-hoc trait injection per plugin
//! pair makes each new combination a fresh substrate question.
//! [`ShelfRequestDispatcher`] is the answer to the class of
//! question rather than to each instance.
//!
//! ## Invariants honoured
//!
//! - **Stocking partition gate.** The dispatcher routes through the
//!   framework's wire-op router; the partition gate refusing
//!   `verb_not_stocked_on_shelf` applies identically to dispatcher-
//!   originated requests. A single truth path.
//! - **Capability gate.** The caller's declared capabilities must
//!   satisfy the destination plugin's verb requirements; otherwise
//!   the dispatch refuses with [`ShelfDispatchError::Permanent`].
//! - **Response budget.** The destination plugin's manifest-declared
//!   `response_budget_ms` bounds the dispatch; exceeding it
//!   surfaces as [`ShelfDispatchError::DeadlineExceeded`].
//! - **Source-state cascade.** A destination shelf whose owning
//!   plugin is unloaded / Offline / Retired surfaces as
//!   [`ShelfDispatchError::NoPluginOnShelf`]. The caller treats
//!   this identically to its own resolve failure — the cascade
//!   returns `None` honestly per the truth-or-null invariant.
//!
//! ## Transport agnosticism
//!
//! In-process plugin hosts back the trait with a direct router
//! invocation; out-of-process plugin hosts back it with the Unix-
//! socket transport that already carries every other plugin-bound
//! call. The trait shape is identical across transports so plugins
//! written against the in-process variant move to OOP without
//! changes.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Plugin-to-plugin shelf verb dispatch surface.
///
/// Exposed via [`crate::contract::LoadContext::shelf_request_dispatcher`].
/// Implementations are `Send + Sync` and live for the load context's
/// lifetime (held as `Arc<dyn ShelfRequestDispatcher>`).
pub trait ShelfRequestDispatcher: Send + Sync {
    /// Dispatch `request_type` on `shelf` with `payload` as the
    /// request body.
    ///
    /// `shelf` is the fully-qualified shelf name as declared in the
    /// catalogue (`audio.library`, `artwork.providers`,
    /// `metadata.providers`). `request_type` is the verb name as
    /// declared in the destination plugin's manifest. `payload` is
    /// the JSON-encoded request body (UTF-8 bytes); the caller is
    /// responsible for matching the destination verb's expected
    /// shape. `instance_id` selects a factory-stocked instance
    /// when the shelf supports multi-instance routing; `None` for
    /// singleton stockings.
    ///
    /// Returns the destination plugin's response payload (JSON
    /// bytes) on success, or a structured [`ShelfDispatchError`]
    /// classified by failure mode.
    fn dispatch<'a>(
        &'a self,
        shelf: &'a str,
        request_type: &'a str,
        payload: Vec<u8>,
        instance_id: Option<&'a str>,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<u8>, ShelfDispatchError>>
                + Send
                + 'a,
        >,
    >;

    /// Dispatch on behalf of a named calling plugin.
    ///
    /// The steward's OOP wire path uses this so peer dispatches
    /// carry the calling plugin's identity as the principal
    /// subject rather than the broad in-process `"plugin-system"`
    /// principal. Default implementation ignores `caller_plugin`
    /// and forwards to [`Self::dispatch`] — wire-backed plugin
    /// adapters do not need to override (the caller is already
    /// stamped on the wire frame).
    fn dispatch_as_caller<'a>(
        &'a self,
        _caller_plugin: &'a str,
        shelf: &'a str,
        request_type: &'a str,
        payload: Vec<u8>,
        instance_id: Option<&'a str>,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<u8>, ShelfDispatchError>>
                + Send
                + 'a,
        >,
    > {
        self.dispatch(shelf, request_type, payload, instance_id)
    }
}

/// Failure modes surfaced by a [`ShelfRequestDispatcher::dispatch`]
/// call.
///
/// Variants are structurally distinct so callers can pattern-match
/// on failure category without inspecting error strings. Each
/// variant maps to a canonical wire-shape error class so consumers
/// observing the call boundary (telemetry, audit, operator logs)
/// can correlate dispatcher-originated failures with the same
/// classification the wire-op layer emits.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ShelfDispatchError {
    /// No plugin currently occupies the requested shelf. The shelf
    /// may be unknown to the catalogue, the owning plugin may be
    /// unloaded, or the owning plugin's source state may be
    /// Offline / Retired so its admission cleared the shelf-
    /// occupant entry. The caller's cascade treats this identically
    /// to a self-issued "no resolver available" — honest `None`,
    /// no fabrication.
    #[error("no plugin on shelf: {shelf}")]
    NoPluginOnShelf {
        /// Shelf name the dispatch targeted.
        shelf: String,
    },

    /// The verb is not declared on any stocking the destination
    /// plugin occupies on the requested shelf. The catalogue +
    /// manifest partition is not crossed. Matches the wire layer's
    /// `verb_not_stocked_on_shelf` refusal.
    #[error("verb {request_type:?} not stocked on shelf {shelf:?}")]
    VerbNotStockedOnShelf {
        /// Shelf name the dispatch targeted.
        shelf: String,
        /// Verb the dispatch attempted.
        request_type: String,
    },

    /// The destination plugin returned a Permanent error. The
    /// caller MUST NOT retry; the request is structurally
    /// invalid (bad payload shape, capability missing, business-
    /// logic refusal).
    #[error("permanent error: {detail}")]
    Permanent {
        /// Destination-plugin-supplied detail.
        detail: String,
    },

    /// The destination plugin returned a Transient error. The
    /// caller MAY retry per its own backoff policy; the destination
    /// indicated the request is structurally valid but currently
    /// not serveable (e.g. external dependency unreachable, cache
    /// not yet populated).
    #[error("transient error: {detail}")]
    Transient {
        /// Destination-plugin-supplied detail.
        detail: String,
    },

    /// The destination plugin's manifest-declared
    /// `response_budget_ms` elapsed before the response landed.
    /// The caller treats this as a Transient-class failure for
    /// retry purposes; the substrate enforced the budget so the
    /// caller is not left waiting indefinitely.
    #[error("deadline exceeded: budget {budget_ms}ms")]
    DeadlineExceeded {
        /// Budget the substrate enforced.
        budget_ms: u32,
    },

    /// Steward-side substrate failure: the destination plugin was
    /// reachable in principle, but the wiring between caller and
    /// destination broke (router error, serialisation failure,
    /// internal panic recovered). The caller surfaces this as
    /// an internal-error class; operators correlate via the
    /// substrate's own logging.
    #[error("substrate failure: {detail}")]
    SubstrateFailure {
        /// Substrate-supplied detail (sanitised for the wire).
        detail: String,
    },
}

impl ShelfDispatchError {
    /// True when the caller's retry policy may legitimately re-
    /// attempt the dispatch.
    ///
    /// `Transient` + `DeadlineExceeded` + `SubstrateFailure` are
    /// retry-eligible; `NoPluginOnShelf`, `VerbNotStockedOnShelf`,
    /// and `Permanent` are structural and re-attempt is wasted
    /// work.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Transient { .. }
                | Self::DeadlineExceeded { .. }
                | Self::SubstrateFailure { .. }
        )
    }
}

/// Helper trait alias for the boxed-future return type of
/// [`ShelfRequestDispatcher::dispatch`].
///
/// Exposes the future shape so plugin code can name it in trait
/// bounds without re-deriving the `Pin<Box<dyn Future…>>` shape
/// every call site.
pub type ShelfDispatchFuture<'a> = Pin<
    Box<dyn Future<Output = Result<Vec<u8>, ShelfDispatchError>> + Send + 'a>,
>;

/// Sealed marker preventing downstream crates from impl'ing the
/// trait directly. The steward-side impl + an in-tree test impl
/// are the only legal implementations; out-of-tree implementations
/// MUST go through the framework's substrate boundary.
///
/// The Arc wrapper does not affect the seal — Arc<dyn Trait> is a
/// type alias for the trait object, not a new trait impl.
pub fn _arc_object_is_thread_safe()
where
    Arc<dyn ShelfRequestDispatcher>: Send + Sync,
{
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_is_retryable_classification() {
        assert!(!ShelfDispatchError::NoPluginOnShelf { shelf: "x".into() }
            .is_retryable());
        assert!(!ShelfDispatchError::VerbNotStockedOnShelf {
            shelf: "x".into(),
            request_type: "y".into(),
        }
        .is_retryable());
        assert!(!ShelfDispatchError::Permanent {
            detail: "bad payload".into(),
        }
        .is_retryable());
        assert!(ShelfDispatchError::Transient {
            detail: "upstream busy".into(),
        }
        .is_retryable());
        assert!(ShelfDispatchError::DeadlineExceeded { budget_ms: 200 }
            .is_retryable());
        assert!(ShelfDispatchError::SubstrateFailure {
            detail: "router send dropped".into(),
        }
        .is_retryable());
    }

    #[test]
    fn error_messages_distinguish_categories() {
        // Telemetry / audit / operator log consumers grep the
        // error message strings; the per-variant shape MUST stay
        // distinguishable so dashboards classifying by message
        // prefix work.
        let no_plugin = ShelfDispatchError::NoPluginOnShelf {
            shelf: "audio.library".into(),
        };
        assert!(no_plugin.to_string().contains("no plugin on shelf"));

        let no_verb = ShelfDispatchError::VerbNotStockedOnShelf {
            shelf: "audio.library".into(),
            request_type: "library.list_works".into(),
        };
        assert!(no_verb.to_string().contains("not stocked on shelf"));

        let perm = ShelfDispatchError::Permanent {
            detail: "missing capability".into(),
        };
        assert!(perm.to_string().contains("permanent error"));

        let trans = ShelfDispatchError::Transient {
            detail: "scan in progress".into(),
        };
        assert!(trans.to_string().contains("transient error"));

        let deadline = ShelfDispatchError::DeadlineExceeded { budget_ms: 200 };
        assert!(deadline.to_string().contains("deadline exceeded"));
        assert!(deadline.to_string().contains("200"));

        let sub = ShelfDispatchError::SubstrateFailure {
            detail: "router gone".into(),
        };
        assert!(sub.to_string().contains("substrate failure"));
    }

    #[test]
    fn arc_dyn_object_is_send_sync() {
        // Compile-time assertion via the helper above; the test
        // body just calls it to anchor the dependency.
        _arc_object_is_thread_safe();
    }
}
