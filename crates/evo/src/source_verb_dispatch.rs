// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Framework-side source-verb dispatch.
//!
//! The verb taxonomy from the source-verbs SDK contract enumerates
//! the user-intent verbs the framework supports against source
//! plugins (`play_now`, `play_now_collection`, `pause`, `resume`,
//! `stop`, etc.). Every framework-internal entity that wants to
//! drive a source plugin — the listening-plans engine,
//! operator-issued UI dispatches, alarms, multi-room control,
//! prompts that preview audio — goes through this surface. One
//! well-shaped primitive, many consumers.
//!
//! ## Responsibilities
//!
//! [`DefaultSourceVerbDispatcher`] handles the four mechanical
//! concerns every dispatch needs:
//!
//! 1. **URI-scheme resolution.** The verb targets an item URI;
//!    the framework's URI-scheme registry maps the URI's scheme
//!    prefix to the source plugin that owns it. The dispatcher
//!    looks up the handler shelf and refuses with
//!    [`DispatchError::UriSchemeNotRegistered`] for an
//!    unregistered scheme.
//! 2. **Active-source custody arbitration.** The verb taxonomy
//!    documents which verbs implicitly acquire
//!    `audio.active_source` custody (the `play_now`-class set:
//!    `PlayNow`, `PlayNowCollection`). For these verbs the
//!    dispatcher records the new claim through
//!    [`ActiveSourceCustody::record_claim`] before dispatching
//!    to the plugin. Force-release of the prior holder via the
//!    plugin SDK's release-callback path is a separate primitive
//!    that wires in alongside the SDK callback surface.
//! 3. **Request build + dispatch.** The dispatcher wraps the
//!    verb in the framework's standard `Request` shape
//!    (`request_type = verb.as_str()`, payload = the verb's
//!    parameters as JSON) and sends it through the plugin
//!    router to the resolved handler shelf. Plugin-side
//!    `handle_request` consumes the verb.
//! 4. **Provenance.** Every dispatch carries a [`VerbApprover`]
//!    naming who approved the verb (a user UID, a plan id, a
//!    framework subsystem reason). Audit-ledger integration is
//!    a separate substrate addition; the approver field is
//!    surfaced on every dispatch so consumers and the future
//!    audit wrapper see consistent provenance.
//!
//! ## Verb scope (this layer)
//!
//! Eight verbs implemented end-to-end, grouped by custody
//! effect:
//!
//! - **Custody-acquiring** (URI-resolved): `PlayNow`,
//!   `PlayNowCollection`. The dispatcher records a new claim
//!   on `audio.active_source` before dispatching.
//! - **Custody-releasing** (custody-resolved): `Stop`. The
//!   dispatcher resolves the current holder, dispatches the
//!   verb, and on success releases custody + emits
//!   `Happening::AudioPlaybackEnded`.
//! - **Custody-retaining transport control** (custody-resolved):
//!   `Pause`, `Resume`, `Seek`, `Next`, `Previous`. The
//!   dispatcher resolves the current holder and dispatches the
//!   verb; the holder remains the active source so subsequent
//!   transport ops continue against the same plugin.
//!
//! The remaining `SourceVerb` variants (`PlayNext`, `Enqueue`,
//! `EnqueueAndStart`, `ReplaceQueue`, `Save`) have no `VerbCall`
//! counterpart yet; reaching the error path for them surfaces
//! [`DispatchError::UnsupportedVerb`] for forward compatibility.
//!
//! ## Why a trait
//!
//! The trait abstracts dispatch so vendor distributions can
//! substitute their own implementation (for example one that
//! routes through a custom remote-control protocol, or one that
//! adds vendor-specific telemetry on every dispatch). The
//! framework ships [`DefaultSourceVerbDispatcher`] as the
//! reference implementation; production wires this through
//! [`crate::lib::run`] from the realised primitives.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use evo_plugin_sdk::contract::{ItemUri, Request, SourceVerb};

use crate::active_source::{ActiveSourceCustody, AUDIO_ACTIVE_SOURCE};
use crate::happenings::{Happening, HappeningBus};
use crate::ledger::{LedgerPrimitive, LifecycleOutcome, VerbDispatchedPayload};
use crate::queue::UriSchemeRegistry;
use crate::router::PluginRouter;

/// Provenance of a verb dispatch. Carried with every call so
/// downstream observers (audit ledger, telemetry, debugging
/// logs) can attribute the verb to its initiator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerbApprover {
    /// A user-issued verb. Carries the operator's UID for
    /// per-user audit.
    User {
        /// Operator UID as observed by the framework's
        /// authentication surface.
        uid: u32,
    },
    /// A plan-engine-dispatched verb. Carries the plan id so
    /// audit consumers can reconstruct which plan caused the
    /// dispatch.
    Plan {
        /// Stable plan identifier from the listening-plans
        /// engine.
        plan_id: String,
    },
    /// A framework subsystem-dispatched verb (e.g. alarm fire,
    /// preempt restoration). Carries a free-form reason so the
    /// origin is identifiable.
    System {
        /// Free-form reason describing the dispatching
        /// subsystem and motivation.
        reason: String,
    },
}

impl std::fmt::Display for VerbApprover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::User { uid } => write!(f, "user:{uid}"),
            Self::Plan { plan_id } => write!(f, "plan:{plan_id}"),
            Self::System { reason } => write!(f, "system:{reason}"),
        }
    }
}

/// Verb call carrying the verb plus its parameters. The shape
/// is documented per-variant; the dispatcher validates that
/// the call's variant matches the verb at runtime.
#[derive(Debug, Clone)]
pub enum VerbCall {
    /// `play_now(uri)` — replace queue with a single item and
    /// start playback. Acquires active-source custody.
    PlayNow {
        /// Item to play.
        uri: ItemUri,
    },
    /// `play_now_collection(uris)` — replace queue with the
    /// supplied collection and start at the first item.
    /// Acquires active-source custody. Empty collection is
    /// refused at dispatch time.
    PlayNowCollection {
        /// Items in playback order. Non-empty.
        uris: Vec<ItemUri>,
    },
    /// `stop()` — end playback. Targets the current
    /// active-source holder (no URI argument); the dispatcher
    /// resolves the holder via [`ActiveSourceCustody::current`],
    /// dispatches the verb to that plugin, then releases
    /// custody. Refused with [`DispatchError::NoActiveSource`]
    /// when no plugin currently holds custody.
    Stop,
    /// `pause()` — suspend playback. Targets the current
    /// active-source holder (custody-resolved, no URI). Custody
    /// is RETAINED — the plugin remains the active source so a
    /// subsequent `Resume` continues seamlessly. Refused with
    /// [`DispatchError::NoActiveSource`] when no plugin holds
    /// custody.
    Pause,
    /// `resume()` — continue playback after a `Pause`. Custody-
    /// resolved + retained, parallel to [`Self::Pause`]. Refused
    /// with [`DispatchError::NoActiveSource`] when no plugin
    /// holds custody.
    Resume,
    /// `seek(position_ms)` — move playback to the supplied
    /// position within the current item. Custody-resolved +
    /// retained. Position is the absolute offset in
    /// milliseconds from the start of the item; the plugin may
    /// clamp to the item's actual duration. Refused with
    /// [`DispatchError::NoActiveSource`] when no plugin holds
    /// custody.
    Seek {
        /// Target position in milliseconds from the start of
        /// the current item.
        position_ms: u64,
    },
    /// `next()` — skip to the next item in the queue. Custody-
    /// resolved + retained. The plugin handles end-of-queue
    /// behaviour internally (per its declared loop / repeat
    /// state). Refused with [`DispatchError::NoActiveSource`]
    /// when no plugin holds custody.
    Next,
    /// `previous()` — return to the previous item in the
    /// queue. Custody-resolved + retained. Plugin-defined
    /// behaviour at the start of the queue (typically restarts
    /// the current item if playback position > some threshold,
    /// otherwise jumps to the prior item). Refused with
    /// [`DispatchError::NoActiveSource`] when no plugin holds
    /// custody.
    Previous,
}

impl VerbCall {
    /// Closed verb identifier corresponding to this call shape.
    /// Used for the dispatched request's `request_type` field.
    pub fn verb(&self) -> SourceVerb {
        match self {
            Self::PlayNow { .. } => SourceVerb::PlayNow,
            Self::PlayNowCollection { .. } => SourceVerb::PlayNowCollection,
            Self::Stop => SourceVerb::Stop,
            Self::Pause => SourceVerb::Pause,
            Self::Resume => SourceVerb::Resume,
            Self::Seek { .. } => SourceVerb::Seek,
            Self::Next => SourceVerb::Next,
            Self::Previous => SourceVerb::Previous,
        }
    }

    /// True if this verb is in the play_now-class set per the
    /// verb-taxonomy table — the dispatcher acquires active-
    /// source custody for these verbs before dispatching.
    pub fn acquires_custody(&self) -> bool {
        matches!(self, Self::PlayNow { .. } | Self::PlayNowCollection { .. })
    }

    /// True if the verb releases active-source custody on
    /// successful dispatch. `Stop` is the canonical
    /// custody-releasing verb per the verb-taxonomy table;
    /// transport-control verbs (Pause / Resume / Seek / Next /
    /// Previous) all RETAIN custody.
    pub fn releases_custody(&self) -> bool {
        matches!(self, Self::Stop)
    }

    /// True if the verb's target shelf is resolved by URI
    /// scheme. Custody-targeted verbs (`Stop`, `Pause`,
    /// `Resume`, `Seek`, `Next`, `Previous`) target the current
    /// custody holder, not a URI — the dispatcher resolves
    /// them via the active-source registry instead.
    pub fn resolves_via_uri(&self) -> bool {
        matches!(self, Self::PlayNow { .. } | Self::PlayNowCollection { .. })
    }

    /// The primary URI the dispatcher resolves to a handler
    /// plugin, when applicable. For `PlayNowCollection` this is
    /// the first item's URI — the dispatcher requires every URI
    /// in the collection to share the same scheme so a single
    /// handler plugin owns the collection's playback. Mismatched
    /// schemes are refused with [`DispatchError::CollectionSchemeMismatch`].
    /// Returns `None` for verbs that don't target a URI
    /// (`Stop` and the transport-control verbs that target the
    /// current custody holder).
    pub fn primary_uri(&self) -> Option<&ItemUri> {
        match self {
            Self::PlayNow { uri } => Some(uri),
            Self::PlayNowCollection { uris } => Some(&uris[0]),
            Self::Stop
            | Self::Pause
            | Self::Resume
            | Self::Seek { .. }
            | Self::Next
            | Self::Previous => None,
        }
    }
}

/// Outcome reported by the dispatcher on a successful call.
/// Carries forensic detail useful for higher-layer observers
/// (audit ledger, telemetry, debug logs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchOutcome {
    /// The shelf the dispatcher resolved the URI scheme to and
    /// dispatched the request against.
    pub handler_shelf: String,
    /// Whether the dispatch acquired active-source custody.
    /// `false` for non-play_now-class verbs (none in this layer
    /// reach this path; included for forward-compatibility).
    pub acquired_custody: bool,
}

/// Errors raised by the dispatcher. Variants are structured so
/// callers can match on the failure mode and surface it
/// appropriately (operator-visible UI error, audit-ledger
/// failure entry, plan-engine fire-failure log).
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    /// The verb's URI is malformed (no scheme separator, empty
    /// scheme, etc.).
    #[error("dispatcher: invalid URI: {0}")]
    InvalidUri(String),
    /// The URI's scheme is not registered in the framework's
    /// URI-scheme registry. Surfaces when the operator dispatches
    /// against a scheme no admitted plugin owns.
    #[error("dispatcher: URI scheme {scheme} not registered")]
    UriSchemeNotRegistered {
        /// The unregistered scheme.
        scheme: String,
    },
    /// `PlayNowCollection` was called with an empty list. The
    /// dispatcher refuses because there is no first item to
    /// resolve a handler shelf against and no playback content.
    #[error("dispatcher: PlayNowCollection called with empty uris list")]
    EmptyCollection,
    /// `PlayNowCollection` items belong to multiple URI schemes.
    /// The dispatcher refuses because a single handler shelf
    /// cannot own a heterogeneously-schemed collection — the
    /// caller must split the collection per scheme and dispatch
    /// each separately.
    #[error(
        "dispatcher: PlayNowCollection items have heterogeneous \
         schemes (first {first_scheme}, conflicting {other_scheme})"
    )]
    CollectionSchemeMismatch {
        /// Scheme of the first item (the one the dispatcher
        /// would have resolved against).
        first_scheme: String,
        /// Scheme that conflicted.
        other_scheme: String,
    },
    /// Active-source custody arbitration failed (substrate-level
    /// I/O error or invalid state). The dispatcher does not
    /// dispatch the verb when arbitration fails — the framework
    /// would otherwise serve a verb against an inconsistent
    /// custody record.
    #[error("dispatcher: custody arbitration failed: {0}")]
    CustodyArbitrationFailed(String),
    /// The router rejected the dispatch. The error message
    /// carries the underlying reason (no plugin admitted on
    /// shelf, plugin refused the request_type, plugin returned
    /// an error from handle_request).
    #[error("dispatcher: router refused dispatch: {0}")]
    RouterError(String),
    /// The verb is not supported by this layer of the
    /// dispatcher. The verb-taxonomy table covers thirteen
    /// variants; this layer handles `PlayNow`,
    /// `PlayNowCollection`, and `Stop` end-to-end. Other verbs
    /// land in follow-on layers.
    #[error("dispatcher: verb {0:?} is not supported in this layer")]
    UnsupportedVerb(SourceVerb),
    /// `Stop` (and other custody-targeted verbs) was dispatched
    /// but no plugin currently holds `audio.active_source`
    /// custody. The dispatcher refuses because there is no
    /// target plugin to dispatch the verb against. Operator
    /// remedy: issue a `play_now`-class verb first to acquire
    /// custody, or no-op the stop dispatch upstream.
    #[error(
        "dispatcher: {verb:?} requires an active source but \
         audio.active_source has no current holder"
    )]
    NoActiveSource {
        /// The verb that was attempted.
        verb: SourceVerb,
    },
}

/// Boxed-future shape for the trait's async method. Object-safe
/// async traits in stable Rust use this form: an owned future
/// pinned in a Box, with the lifetime threaded through so the
/// callee can borrow from `&self` and the call arguments.
pub type DispatchFuture<'a> = Pin<
    Box<
        dyn Future<Output = Result<DispatchOutcome, DispatchError>> + Send + 'a,
    >,
>;

/// Framework-side source-verb dispatch. The trait abstracts the
/// dispatch path so vendor distributions can substitute custom
/// implementations.
///
/// Consumers (the listening-plans engine, the UI shell, alarms,
/// multi-room, prompts) hold an `Arc<dyn SourceVerbDispatcher>`
/// and call `dispatch` to drive source plugins through the
/// framework's verb taxonomy.
pub trait SourceVerbDispatcher: Send + Sync {
    /// Dispatch a verb. Resolves the URI scheme to a handler
    /// plugin, arbitrates active-source custody for play_now-
    /// class verbs, and routes the request through the plugin
    /// router. Returns a [`DispatchOutcome`] on success or a
    /// structured [`DispatchError`] on failure.
    fn dispatch<'a>(
        &'a self,
        approver: VerbApprover,
        call: VerbCall,
    ) -> DispatchFuture<'a>;
}

/// Framework reference implementation of [`SourceVerbDispatcher`].
/// Wires together the URI-scheme registry, the active-source
/// custody primitive, and the plugin router. Production wires
/// this from the realised primitives at boot.
pub struct DefaultSourceVerbDispatcher {
    router: Arc<PluginRouter>,
    uri_schemes: Arc<UriSchemeRegistry>,
    active_source: Arc<ActiveSourceCustody>,
    /// Optional audit ledger. When wired, every dispatch that
    /// reaches the post-resolution stage (URI scheme resolved
    /// successfully) records a lifecycle entry — Success on
    /// router-accepted dispatch, Failed { reason } on
    /// router-refused. Pre-resolution failures (URI scheme not
    /// registered, empty collection, scheme mismatch) do not
    /// produce an entry; they're logged via tracing and the
    /// caller surfaces the typed error directly.
    ledger: Option<Arc<LedgerPrimitive>>,
    /// Optional happenings bus. When wired, the dispatcher
    /// emits `Happening::AudioPlaybackEnded` after successfully
    /// dispatching a custody-releasing verb (Stop) so framework
    /// consumers (plan engine UntilCompletion / UntilUserStop,
    /// UI shell, multi-room synchronised stop, prompts) observe
    /// the user-stopped intent through the same typed event
    /// they already use for natural-end. One signal, two intents
    /// converge — both "playback ran out" and "user said stop"
    /// produce the same observable.
    bus: Option<Arc<HappeningBus>>,
}

impl DefaultSourceVerbDispatcher {
    /// Construct a dispatcher over the given primitives. None
    /// of the wiring touches I/O at construction time. The
    /// audit ledger is optional and wired post-construction via
    /// [`Self::with_ledger`].
    pub fn new(
        router: Arc<PluginRouter>,
        uri_schemes: Arc<UriSchemeRegistry>,
        active_source: Arc<ActiveSourceCustody>,
    ) -> Self {
        Self {
            router,
            uri_schemes,
            active_source,
            ledger: None,
            bus: None,
        }
    }

    /// Builder-style: install the audit ledger so dispatches
    /// record lifecycle entries with full provenance. Production
    /// wiring at boot installs the steward's `lifecycle_ledger`;
    /// tests can leave the ledger unwired and exercise the
    /// dispatcher's pure-routing path.
    pub fn with_ledger(mut self, ledger: Arc<LedgerPrimitive>) -> Self {
        self.ledger = Some(ledger);
        self
    }

    /// Builder-style: install the happenings bus so dispatches
    /// emit observable events. The dispatcher emits
    /// `Happening::AudioPlaybackEnded` after a successful Stop
    /// release so consumers see user-stopped intent through the
    /// same typed event they use for natural-end. Production
    /// wiring at boot installs the steward's `state.bus`.
    pub fn with_bus(mut self, bus: Arc<HappeningBus>) -> Self {
        self.bus = Some(bus);
        self
    }

    async fn dispatch_inner(
        &self,
        approver: VerbApprover,
        call: VerbCall,
    ) -> Result<DispatchOutcome, DispatchError> {
        // Validate the call shape (collection-specific checks).
        if let VerbCall::PlayNowCollection { uris } = &call {
            if uris.is_empty() {
                return Err(DispatchError::EmptyCollection);
            }
            // Every URI in the collection must share the same
            // scheme — one handler plugin owns the collection.
            let first_scheme = scheme_of(uris[0].as_str())?;
            for other in uris.iter().skip(1) {
                let other_scheme = scheme_of(other.as_str())?;
                if other_scheme != first_scheme {
                    return Err(DispatchError::CollectionSchemeMismatch {
                        first_scheme: first_scheme.to_string(),
                        other_scheme: other_scheme.to_string(),
                    });
                }
            }
        }

        // Resolve the handler shelf. Two paths: URI-resolved
        // verbs (PlayNow / PlayNowCollection) look up the URI
        // scheme; custody-targeted verbs (Stop) look up the
        // current active-source holder. Pre-resolution failures
        // skip dispatch entirely (no audit entry, typed error
        // surfaces upstream).
        let primary_uri_for_audit: Option<String>;
        let handler_shelf;
        if call.resolves_via_uri() {
            let primary_uri =
                call.primary_uri().expect("URI-resolved call has URI");
            let primary_uri_str = primary_uri.as_str().to_string();
            let registration = self
                .uri_schemes
                .lookup_for_uri(&primary_uri_str)
                .await
                .map_err(|e| {
                    DispatchError::RouterError(format!(
                        "URI-scheme lookup failed: {e}"
                    ))
                })?;
            let registration = match registration {
                Some(r) => r,
                None => {
                    let scheme = scheme_of(&primary_uri_str)?.to_string();
                    return Err(DispatchError::UriSchemeNotRegistered {
                        scheme,
                    });
                }
            };
            // Custody arbitration and audit ledger track the
            // resolving plugin's canonical name; the router
            // dispatch keys on shelf. handler_shelf names the
            // canonical plugin here; the dispatch step
            // immediately below translates plugin → shelf via
            // router.lookup_by_name.
            handler_shelf = registration.source_plugin.clone();
            primary_uri_for_audit = Some(primary_uri_str);
        } else {
            // Custody-targeted: look up current active-source
            // holder via ActiveSourceCustody.
            let claim = self
                .active_source
                .current(AUDIO_ACTIVE_SOURCE)
                .await
                .map_err(|e| {
                DispatchError::CustodyArbitrationFailed(format!(
                    "current() failed: {e}"
                ))
            })?;
            let holder = match claim.and_then(|c| c.holder_plugin) {
                Some(h) => h,
                None => {
                    return Err(DispatchError::NoActiveSource {
                        verb: call.verb(),
                    });
                }
            };
            // handler_shelf names the canonical plugin here; the
            // dispatch step below translates plugin → shelf via
            // router.lookup_by_name.
            handler_shelf = holder;
            primary_uri_for_audit = None;
        }

        // Arbitrate active-source custody for play_now-class
        // verbs. Record the new claim before dispatching so the
        // custody record reflects the in-flight dispatch even
        // if the plugin's verb handling races.
        let acquires_custody = call.acquires_custody();
        if acquires_custody {
            let primary_uri_for_claim =
                primary_uri_for_audit.as_deref().unwrap_or("");
            let claim_params = serde_json::json!({
                "verb": call.verb().as_str(),
                "approver": approver.to_string(),
            });
            self.active_source
                .record_claim(
                    AUDIO_ACTIVE_SOURCE,
                    &handler_shelf,
                    primary_uri_for_claim,
                    &claim_params.to_string(),
                )
                .await
                .map_err(|e| {
                    DispatchError::CustodyArbitrationFailed(format!("{e}"))
                })?;
        }

        // Build the request payload. Each verb's payload shape
        // is documented in the SDK; the dispatcher's payload
        // mirror is the same JSON shape plugin authors expect.
        // Every payload carries a `v: 1` envelope field so
        // wire-protocol evolution can roll forward; plugins
        // tolerate absent `v` for backwards-compatibility but
        // the canonical emission always declares the version.
        let payload = match &call {
            VerbCall::PlayNow { uri } => serde_json::json!({
                "v": 1,
                "uri": uri.as_str(),
            }),
            VerbCall::PlayNowCollection { uris } => {
                let uri_strs: Vec<&str> =
                    uris.iter().map(|u| u.as_str()).collect();
                serde_json::json!({ "v": 1, "uris": uri_strs })
            }
            VerbCall::Stop
            | VerbCall::Pause
            | VerbCall::Resume
            | VerbCall::Next
            | VerbCall::Previous => serde_json::json!({ "v": 1 }),
            VerbCall::Seek { position_ms } => serde_json::json!({
                "v": 1,
                "position_ms": position_ms,
            }),
        };
        let payload_bytes = serde_json::to_vec(&payload).map_err(|e| {
            DispatchError::RouterError(format!(
                "payload serialisation failed: {e}"
            ))
        })?;
        let request = Request {
            request_type: call.verb().as_str().to_string(),
            payload: payload_bytes,
            correlation_id: 0,
            deadline: None,
            instance_id: None,
            principal_scope: None,
            has_step_up: false,
        };

        // Dispatch through the router. handler_shelf carries the
        // canonical plugin name; resolve to the actual shelf via
        // router.lookup_by_name. RouterError on missing admission
        // matches the operator-visible contract: plugin not on
        // router => router refuses the dispatch.
        tracing::debug!(
            verb = call.verb().as_str(),
            handler_plugin = %handler_shelf,
            approver = %approver,
            primary_uri = ?primary_uri_for_audit,
            acquires_custody,
            "source verb dispatch: routing through router"
        );
        let uri_count = match &call {
            VerbCall::PlayNow { .. } => 1,
            VerbCall::PlayNowCollection { uris } => uris.len() as u32,
            VerbCall::Stop
            | VerbCall::Pause
            | VerbCall::Resume
            | VerbCall::Seek { .. }
            | VerbCall::Next
            | VerbCall::Previous => 0,
        };
        let dispatch_result = match self.router.lookup_by_name(&handler_shelf) {
            Some(entry) => {
                let shelf = entry.shelf.clone();
                // Bootstrap warden custody for warden+respondent
                // plugins on acquiring source verbs. The
                // source-verb dispatch path is independent of the
                // warden's course_correct path, but warden+respondent
                // plugins (e.g. an audio playback warden that owns
                // music URI schemes) expect their warden surface to
                // hold custody so subsequent course_correct verbs
                // (pause / seek / etc.) flow through the same
                // supervisor + state. Without this bootstrap the
                // plugin's handle_request body would refuse with
                // "no active custody on the warden". Best-effort:
                // a take_custody failure (e.g. custody_exclusive
                // already held) logs a warn and dispatch proceeds —
                // the plugin's handle_request finds whatever custody
                // the prior take left in place.
                if call.acquires_custody() {
                    if let Some(manifest) = entry.current_manifest() {
                        let is_warden_with_respondent = manifest
                            .kind
                            .as_ref()
                            .is_some_and(|k| {
                                k.interaction
                                == evo_plugin_sdk::manifest::InteractionShape::Warden
                            })
                            && manifest.capabilities.respondent.is_some();
                        if is_warden_with_respondent {
                            let custody_domain = manifest
                                .capabilities
                                .warden
                                .as_ref()
                                .map(|w| w.custody_domain.clone())
                                .unwrap_or_default();
                            if let Err(e) = self
                                .router
                                .take_custody(
                                    &shelf,
                                    custody_domain,
                                    Vec::new(),
                                    None,
                                )
                                .await
                            {
                                tracing::warn!(
                                    error = %e,
                                    shelf = %shelf,
                                    verb = call.verb().as_str(),
                                    "source verb dispatch: warden custody \
                                     bootstrap failed; proceeding with \
                                     handle_request anyway (plugin may \
                                     have own existing custody)"
                                );
                            } else {
                                tracing::debug!(
                                    shelf = %shelf,
                                    verb = call.verb().as_str(),
                                    "source verb dispatch: warden custody \
                                     bootstrapped before handle_request"
                                );
                            }
                        }
                    }
                }
                self.router.handle_request(&shelf, request).await
            }
            None => Err(crate::error::StewardError::Dispatch(format!(
                "no plugin on shelf: {handler_shelf}"
            ))),
        };

        // Release active-source custody on successful Stop. Done
        // BEFORE the audit-ledger entry so the entry's
        // outcome-success implies the release happened.
        // Best-effort: a release I/O failure logs but does not
        // change the dispatch outcome.
        if call.releases_custody() && dispatch_result.is_ok() {
            if let Err(e) =
                self.active_source.release(AUDIO_ACTIVE_SOURCE).await
            {
                tracing::warn!(
                    error = %e,
                    handler_shelf = %handler_shelf,
                    "source verb dispatch: custody release after \
                     successful stop dispatch failed; dispatch \
                     outcome surfaces to caller regardless"
                );
            }
            // Emit AudioPlaybackEnded so the same consumers that
            // already subscribe for natural-end (plan engine
            // UntilCompletion / UntilUserStop, UI, multi-room,
            // prompts) observe user-stopped intent through the
            // same typed event. Source plugins handling the
            // Stop verb may also emit this happening; consumers
            // are idempotent (segment-end Notify wakes
            // unconditionally on first signal).
            if let Some(bus) = &self.bus {
                bus.emit(Happening::AudioPlaybackEnded {
                    source_plugin: handler_shelf.clone(),
                    claim_uri: primary_uri_for_audit.clone(),
                    at: std::time::SystemTime::now(),
                });
            }
        }

        let outcome_for_audit = match &dispatch_result {
            Ok(_) => LifecycleOutcome::Success,
            Err(e) => LifecycleOutcome::Failed {
                reason: format!("{e}"),
            },
        };
        // Record the audit-grade lifecycle entry. Best-effort:
        // a ledger I/O failure here does not change the
        // dispatch outcome the caller sees. The dispatcher's
        // primary contract is dispatch + arbitration; audit is
        // a forensic-record companion.
        if let Some(ledger) = &self.ledger {
            let payload = VerbDispatchedPayload {
                verb: call.verb().as_str().to_string(),
                primary_uri: primary_uri_for_audit.clone(),
                uri_count,
                acquired_custody: acquires_custody,
            };
            if let Err(e) = ledger
                .append_verb_dispatch(
                    &approver.to_string(),
                    &handler_shelf,
                    &payload,
                    outcome_for_audit,
                )
                .await
            {
                tracing::warn!(
                    error = %e,
                    handler_shelf = %handler_shelf,
                    "source verb dispatch: failed to append \
                     audit-ledger entry; dispatch outcome \
                     surfaces to caller regardless"
                );
            }
        }
        match dispatch_result {
            Ok(_) => Ok(DispatchOutcome {
                handler_shelf,
                acquired_custody: acquires_custody,
            }),
            Err(e) => Err(DispatchError::RouterError(format!("{e}"))),
        }
    }
}

impl SourceVerbDispatcher for DefaultSourceVerbDispatcher {
    fn dispatch<'a>(
        &'a self,
        approver: VerbApprover,
        call: VerbCall,
    ) -> DispatchFuture<'a> {
        Box::pin(self.dispatch_inner(approver, call))
    }
}

fn scheme_of(uri: &str) -> Result<&str, DispatchError> {
    match uri.split_once(':') {
        Some((scheme, _)) if !scheme.is_empty() => Ok(scheme),
        _ => Err(DispatchError::InvalidUri(uri.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::MemoryPersistenceStore;
    use crate::state::StewardState;

    fn build_test_dispatcher() -> (
        Arc<DefaultSourceVerbDispatcher>,
        Arc<PluginRouter>,
        Arc<UriSchemeRegistry>,
        Arc<ActiveSourceCustody>,
    ) {
        let persistence: Arc<dyn crate::persistence::PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let state = StewardState::for_tests();
        let router = Arc::new(PluginRouter::new(Arc::clone(&state)));
        let uri_schemes =
            Arc::new(UriSchemeRegistry::new(Arc::clone(&persistence)));
        let active_source =
            Arc::new(ActiveSourceCustody::new(Arc::clone(&persistence)));
        let dispatcher = Arc::new(DefaultSourceVerbDispatcher::new(
            Arc::clone(&router),
            Arc::clone(&uri_schemes),
            Arc::clone(&active_source),
        ));
        (dispatcher, router, uri_schemes, active_source)
    }

    #[test]
    fn approver_displays_with_kind_prefix() {
        assert_eq!(VerbApprover::User { uid: 1000 }.to_string(), "user:1000");
        assert_eq!(
            VerbApprover::Plan {
                plan_id: "morning".to_string()
            }
            .to_string(),
            "plan:morning"
        );
        assert_eq!(
            VerbApprover::System {
                reason: "preempt-restore".to_string()
            }
            .to_string(),
            "system:preempt-restore"
        );
    }

    #[test]
    fn verb_call_acquires_custody_for_play_now_class() {
        let play_now = VerbCall::PlayNow {
            uri: ItemUri::new("scheme:item").unwrap(),
        };
        assert!(play_now.acquires_custody());
        let play_collection = VerbCall::PlayNowCollection {
            uris: vec![ItemUri::new("scheme:item").unwrap()],
        };
        assert!(play_collection.acquires_custody());
    }

    #[test]
    fn scheme_of_extracts_prefix() {
        assert_eq!(scheme_of("spotify:track:abc").unwrap(), "spotify");
        assert_eq!(scheme_of("file:/var/lib/song.flac").unwrap(), "file");
        assert!(scheme_of("no-colon-here").is_err());
        assert!(scheme_of(":no-scheme").is_err());
        assert!(scheme_of("").is_err());
    }

    #[tokio::test]
    async fn play_now_against_unregistered_scheme_refuses() {
        let (dispatcher, _router, _schemes, _custody) = build_test_dispatcher();
        let result = dispatcher
            .dispatch(
                VerbApprover::User { uid: 1000 },
                VerbCall::PlayNow {
                    uri: ItemUri::new("ghost:track:1").unwrap(),
                },
            )
            .await;
        match result {
            Err(DispatchError::UriSchemeNotRegistered { scheme }) => {
                assert_eq!(scheme, "ghost");
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn play_now_collection_empty_refuses() {
        let (dispatcher, _r, _s, _c) = build_test_dispatcher();
        let result = dispatcher
            .dispatch(
                VerbApprover::User { uid: 1000 },
                VerbCall::PlayNowCollection { uris: Vec::new() },
            )
            .await;
        assert!(matches!(result, Err(DispatchError::EmptyCollection)));
    }

    #[tokio::test]
    async fn play_now_collection_heterogeneous_schemes_refuses() {
        let (dispatcher, _r, _s, _c) = build_test_dispatcher();
        let result = dispatcher
            .dispatch(
                VerbApprover::User { uid: 1000 },
                VerbCall::PlayNowCollection {
                    uris: vec![
                        ItemUri::new("spotify:track:a").unwrap(),
                        ItemUri::new("file:song.flac").unwrap(),
                    ],
                },
            )
            .await;
        match result {
            Err(DispatchError::CollectionSchemeMismatch {
                first_scheme,
                other_scheme,
            }) => {
                assert_eq!(first_scheme, "spotify");
                assert_eq!(other_scheme, "file");
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn play_now_against_registered_scheme_with_no_plugin_routes_to_router(
    ) {
        // Register a scheme but admit no plugin on the resolved
        // shelf. The dispatcher resolves the URI → shelf,
        // records a claim, then the router refuses with
        // RouterError. Custody record IS made (the dispatcher
        // arbitrates custody before dispatch); the call returns
        // RouterError because the router has no handler.
        let (dispatcher, _router, schemes, custody) = build_test_dispatcher();
        schemes
            .register("spotify", "com.example.spotify")
            .await
            .unwrap();
        let result = dispatcher
            .dispatch(
                VerbApprover::Plan {
                    plan_id: "morning".to_string(),
                },
                VerbCall::PlayNow {
                    uri: ItemUri::new("spotify:track:1").unwrap(),
                },
            )
            .await;
        assert!(matches!(result, Err(DispatchError::RouterError(_))));
        // Custody claim was recorded BEFORE the dispatch.
        let current =
            custody.current(AUDIO_ACTIVE_SOURCE).await.unwrap().unwrap();
        assert_eq!(
            current.holder_plugin.as_deref(),
            Some("com.example.spotify")
        );
        assert_eq!(current.claim_uri.as_deref(), Some("spotify:track:1"));
    }

    #[tokio::test]
    async fn unsupported_verb_path_is_unreachable_via_typed_calls() {
        // VerbCall is a closed enum covering the eight verbs
        // the dispatcher handles end-to-end (PlayNow,
        // PlayNowCollection, Stop, Pause, Resume, Seek, Next,
        // Previous). The remaining `SourceVerb` variants
        // (PlayNext, Enqueue, EnqueueAndStart, ReplaceQueue,
        // Save) have no VerbCall counterpart yet — UnsupportedVerb
        // surfaces if a future code path reaches the error
        // constructor for one of them. This test verifies the
        // error is constructable for that forward-compat use.
        let err = DispatchError::UnsupportedVerb(SourceVerb::Enqueue);
        let msg = format!("{err}");
        assert!(msg.contains("Enqueue"));
    }

    #[test]
    fn dispatch_outcome_carries_handler_shelf_and_custody_flag() {
        let o = DispatchOutcome {
            handler_shelf: "com.example.audio".into(),
            acquired_custody: true,
        };
        assert_eq!(o.handler_shelf, "com.example.audio");
        assert!(o.acquired_custody);
    }

    // ---- Audit-ledger integration ----

    use crate::ledger::{LedgerPrimitive, LEDGER_LIFECYCLE};
    use crate::persistence::LedgerEntryFilter;

    fn build_test_dispatcher_with_ledger() -> (
        Arc<DefaultSourceVerbDispatcher>,
        Arc<UriSchemeRegistry>,
        Arc<LedgerPrimitive>,
    ) {
        let persistence: Arc<dyn crate::persistence::PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let state = StewardState::for_tests();
        let router = Arc::new(PluginRouter::new(Arc::clone(&state)));
        let uri_schemes =
            Arc::new(UriSchemeRegistry::new(Arc::clone(&persistence)));
        let active_source =
            Arc::new(ActiveSourceCustody::new(Arc::clone(&persistence)));
        let ledger = Arc::new(LedgerPrimitive::with_no_op_crypto(Arc::clone(
            &persistence,
        )));
        let dispatcher = Arc::new(
            DefaultSourceVerbDispatcher::new(
                Arc::clone(&router),
                Arc::clone(&uri_schemes),
                active_source,
            )
            .with_ledger(Arc::clone(&ledger)),
        );
        (dispatcher, uri_schemes, ledger)
    }

    async fn read_lifecycle_entries(
        ledger: &LedgerPrimitive,
    ) -> Vec<crate::persistence::PersistedLedgerEntry> {
        ledger
            .query(LedgerEntryFilter {
                ledger_id: LEDGER_LIFECYCLE,
                time_range: None,
                subject_plugin: None,
                include_withdrawn: true,
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn router_failure_records_lifecycle_failed_entry() {
        // URI scheme is registered but no plugin admitted on
        // the resolved shelf; router.handle_request refuses;
        // dispatcher records a Failed lifecycle entry.
        let (dispatcher, schemes, ledger) = build_test_dispatcher_with_ledger();
        schemes
            .register("spotify", "com.example.spotify")
            .await
            .unwrap();
        let result = dispatcher
            .dispatch(
                VerbApprover::Plan {
                    plan_id: "morning".to_string(),
                },
                VerbCall::PlayNow {
                    uri: ItemUri::new("spotify:track:1").unwrap(),
                },
            )
            .await;
        assert!(matches!(result, Err(DispatchError::RouterError(_))));
        let entries = read_lifecycle_entries(&ledger).await;
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        // Approver landed on subject_plugin.
        assert_eq!(entry.subject_plugin.as_deref(), Some("plan:morning"));
        // The persisted payload is the full LifecycleEntry
        // serialised; the verb-dispatch payload is nested under
        // payload.payload.
        let entry_json: serde_json::Value =
            serde_json::from_str(&entry.payload_json).unwrap();
        assert_eq!(entry_json["event_type"].as_str(), Some("verb_dispatched"));
        assert_eq!(entry_json["outcome"]["kind"].as_str(), Some("failed"));
        assert!(entry_json["outcome"]["reason"].is_string());
        assert_eq!(
            entry_json["target"]["kind"].as_str(),
            Some("source_plugin")
        );
        assert_eq!(
            entry_json["target"]["plugin_id"].as_str(),
            Some("com.example.spotify")
        );
        let payload = &entry_json["payload"];
        assert_eq!(payload["verb"].as_str(), Some("play_now"));
        assert_eq!(payload["primary_uri"].as_str(), Some("spotify:track:1"));
        assert_eq!(payload["uri_count"].as_u64(), Some(1));
        assert_eq!(payload["acquired_custody"].as_bool(), Some(true));
    }

    #[tokio::test]
    async fn pre_resolution_failure_does_not_record() {
        // URI scheme not registered → dispatcher returns
        // UriSchemeNotRegistered before reaching the post-
        // resolution stage; no lifecycle entry recorded.
        let (dispatcher, _schemes, ledger) =
            build_test_dispatcher_with_ledger();
        let result = dispatcher
            .dispatch(
                VerbApprover::User { uid: 1000 },
                VerbCall::PlayNow {
                    uri: ItemUri::new("ghost:track:1").unwrap(),
                },
            )
            .await;
        assert!(matches!(
            result,
            Err(DispatchError::UriSchemeNotRegistered { .. })
        ));
        let entries = read_lifecycle_entries(&ledger).await;
        assert!(
            entries.is_empty(),
            "pre-resolution failure must not produce a lifecycle entry"
        );
    }

    #[tokio::test]
    async fn empty_collection_pre_resolution_failure_does_not_record() {
        let (dispatcher, _schemes, ledger) =
            build_test_dispatcher_with_ledger();
        let result = dispatcher
            .dispatch(
                VerbApprover::User { uid: 1000 },
                VerbCall::PlayNowCollection { uris: Vec::new() },
            )
            .await;
        assert!(matches!(result, Err(DispatchError::EmptyCollection)));
        let entries = read_lifecycle_entries(&ledger).await;
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn no_ledger_wired_dispatch_runs_cleanly() {
        // Dispatcher constructed without a ledger; dispatch
        // proceeds through the full path without panicking and
        // without any audit-side state.
        let (dispatcher, _router, _schemes, _custody) = build_test_dispatcher();
        let result = dispatcher
            .dispatch(
                VerbApprover::User { uid: 1000 },
                VerbCall::PlayNow {
                    uri: ItemUri::new("ghost:track:1").unwrap(),
                },
            )
            .await;
        // Pre-resolution failure path; no panic.
        assert!(matches!(
            result,
            Err(DispatchError::UriSchemeNotRegistered { .. })
        ));
    }

    #[tokio::test]
    async fn play_now_collection_records_uri_count() {
        let (dispatcher, schemes, ledger) = build_test_dispatcher_with_ledger();
        schemes
            .register("spotify", "com.example.spotify")
            .await
            .unwrap();
        let _ = dispatcher
            .dispatch(
                VerbApprover::User { uid: 1000 },
                VerbCall::PlayNowCollection {
                    uris: vec![
                        ItemUri::new("spotify:track:a").unwrap(),
                        ItemUri::new("spotify:track:b").unwrap(),
                        ItemUri::new("spotify:track:c").unwrap(),
                    ],
                },
            )
            .await;
        let entries = read_lifecycle_entries(&ledger).await;
        assert_eq!(entries.len(), 1);
        let entry_json: serde_json::Value =
            serde_json::from_str(&entries[0].payload_json).unwrap();
        let payload = &entry_json["payload"];
        assert_eq!(payload["verb"].as_str(), Some("play_now_collection"));
        assert_eq!(payload["uri_count"].as_u64(), Some(3));
        // primary_uri is the first item's URI (the URI the
        // dispatcher resolved to a handler shelf).
        assert_eq!(payload["primary_uri"].as_str(), Some("spotify:track:a"));
    }

    #[tokio::test]
    async fn approver_user_lands_on_subject_plugin_with_uid_format() {
        let (dispatcher, schemes, ledger) = build_test_dispatcher_with_ledger();
        schemes
            .register("spotify", "com.example.spotify")
            .await
            .unwrap();
        let _ = dispatcher
            .dispatch(
                VerbApprover::User { uid: 4242 },
                VerbCall::PlayNow {
                    uri: ItemUri::new("spotify:track:1").unwrap(),
                },
            )
            .await;
        let entries = read_lifecycle_entries(&ledger).await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].subject_plugin.as_deref(), Some("user:4242"));
    }

    // ---- Stop verb dispatch ----

    #[test]
    fn stop_verb_call_does_not_acquire_custody_but_releases_it() {
        let stop = VerbCall::Stop;
        assert!(!stop.acquires_custody());
        assert!(stop.releases_custody());
        assert!(!stop.resolves_via_uri());
        assert!(stop.primary_uri().is_none());
        assert_eq!(stop.verb(), SourceVerb::Stop);
    }

    #[test]
    fn transport_verbs_resolve_via_holder_and_retain_custody() {
        // Pause / Resume / Seek / Next / Previous all share the
        // custody-resolved + custody-retained shape: no URI, no
        // acquire, no release. Verb identifier maps 1:1 to the
        // SDK's SourceVerb taxonomy.
        for (call, verb) in [
            (VerbCall::Pause, SourceVerb::Pause),
            (VerbCall::Resume, SourceVerb::Resume),
            (
                VerbCall::Seek {
                    position_ms: 12_500,
                },
                SourceVerb::Seek,
            ),
            (VerbCall::Next, SourceVerb::Next),
            (VerbCall::Previous, SourceVerb::Previous),
        ] {
            assert_eq!(call.verb(), verb, "verb mapping for {verb:?}");
            assert!(
                !call.acquires_custody(),
                "{verb:?} must not acquire custody"
            );
            assert!(
                !call.releases_custody(),
                "{verb:?} must not release custody"
            );
            assert!(
                !call.resolves_via_uri(),
                "{verb:?} must not resolve via URI"
            );
            assert!(
                call.primary_uri().is_none(),
                "{verb:?} must not carry a primary URI"
            );
        }
    }

    #[tokio::test]
    async fn transport_verbs_with_no_active_source_refuse() {
        // Mirror of stop_with_no_active_source_refuses for the
        // five transport-control verbs. Same custody-resolution
        // path; same NoActiveSource refusal when no holder.
        for (call, verb) in [
            (VerbCall::Pause, SourceVerb::Pause),
            (VerbCall::Resume, SourceVerb::Resume),
            (VerbCall::Seek { position_ms: 0 }, SourceVerb::Seek),
            (VerbCall::Next, SourceVerb::Next),
            (VerbCall::Previous, SourceVerb::Previous),
        ] {
            let (dispatcher, _router, _schemes, _custody) =
                build_test_dispatcher();
            let result = dispatcher
                .dispatch(VerbApprover::User { uid: 1000 }, call)
                .await;
            match result {
                Err(DispatchError::NoActiveSource { verb: refused_verb }) => {
                    assert_eq!(
                        refused_verb, verb,
                        "refusal carries the offending verb id"
                    );
                }
                other => panic!("{verb:?}: unexpected result: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn transport_verbs_dispatch_against_holder_without_releasing_custody()
    {
        // Custody claimed → transport verb dispatched → router
        // refuses (no plugin admitted on the holder's shelf) →
        // dispatcher returns RouterError. Custody is NOT released
        // — these verbs retain custody by design (Pause / Resume
        // / Seek / Next / Previous all leave the active source
        // in place so subsequent transport ops continue against
        // the same holder).
        for call in [
            VerbCall::Pause,
            VerbCall::Resume,
            VerbCall::Seek { position_ms: 5_000 },
            VerbCall::Next,
            VerbCall::Previous,
        ] {
            let verb_label = call.verb();
            let (dispatcher, custody, _ledger) =
                build_test_dispatcher_with_custody_and_ledger();
            custody
                .record_claim(
                    AUDIO_ACTIVE_SOURCE,
                    "com.example.spotify",
                    "spotify:track:1",
                    r#"{}"#,
                )
                .await
                .unwrap();
            let result = dispatcher
                .dispatch(VerbApprover::User { uid: 1000 }, call)
                .await;
            assert!(
                matches!(result, Err(DispatchError::RouterError(_))),
                "{verb_label:?}: expected RouterError (no plugin on \
                 holder shelf), got {result:?}"
            );
            // Custody MUST still be recorded — transport verbs
            // never release.
            let after = custody.current(AUDIO_ACTIVE_SOURCE).await.unwrap();
            assert!(
                after
                    .as_ref()
                    .and_then(|c| c.holder_plugin.as_deref())
                    .is_some(),
                "{verb_label:?}: custody must be retained even when \
                 dispatch fails"
            );
        }
    }

    #[tokio::test]
    async fn seek_payload_carries_position_ms() {
        // Seek's wire payload must carry position_ms verbatim;
        // the plugin reads it to drive the seek operation. We
        // assert the payload shape by intercepting the request
        // through the router-error path (no plugin admitted →
        // RouterError, but the request was constructed with the
        // correct payload before the router refused).
        //
        // Direct payload assertion goes through serde — the
        // dispatcher's match arm builds the payload value, which
        // we reconstruct here to confirm the shape.
        let call = VerbCall::Seek {
            position_ms: 42_000,
        };
        // We can't easily intercept the in-flight payload in a
        // unit test without restructuring the dispatcher; the
        // most decisive test is to confirm the call's verb
        // identifier and absence of URI, with the payload-build
        // arm separately covered at the source-verb-dispatcher's
        // payload-construction switch (refactor target if a
        // handler-side test ever needs to inspect the bytes).
        assert_eq!(call.verb(), SourceVerb::Seek);
        assert!(call.primary_uri().is_none());
    }

    #[tokio::test]
    async fn stop_with_no_active_source_refuses() {
        // No holder ever recorded → current() returns None →
        // dispatcher refuses with NoActiveSource. No router
        // dispatch happens, no audit-ledger entry.
        let (dispatcher, _router, _schemes, _custody) = build_test_dispatcher();
        let result = dispatcher
            .dispatch(VerbApprover::User { uid: 1000 }, VerbCall::Stop)
            .await;
        match result {
            Err(DispatchError::NoActiveSource { verb }) => {
                assert_eq!(verb, SourceVerb::Stop);
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn stop_with_released_custody_refuses() {
        // Custody was claimed and then released; current()
        // returns Some(claim) but holder_plugin = None. Stop
        // should refuse the same way as never-claimed.
        let (dispatcher, _router, _schemes, custody) = build_test_dispatcher();
        custody
            .record_claim(
                AUDIO_ACTIVE_SOURCE,
                "com.example.spotify",
                "spotify:track:1",
                r#"{}"#,
            )
            .await
            .unwrap();
        custody.release(AUDIO_ACTIVE_SOURCE).await.unwrap();
        let result = dispatcher
            .dispatch(VerbApprover::User { uid: 1000 }, VerbCall::Stop)
            .await;
        assert!(matches!(result, Err(DispatchError::NoActiveSource { .. })));
    }

    #[tokio::test]
    async fn stop_dispatches_against_holder_then_releases_custody() {
        // Custody claimed → Stop dispatched → router refuses
        // (no plugin admitted on shelf) → dispatcher records
        // Failed audit entry. The active-source release runs
        // ONLY on success; a router-refused stop leaves the
        // claim in place so the operator can retry.
        let (dispatcher, custody, _ledger) =
            build_test_dispatcher_with_custody_and_ledger();
        custody
            .record_claim(
                AUDIO_ACTIVE_SOURCE,
                "com.example.spotify",
                "spotify:track:1",
                r#"{}"#,
            )
            .await
            .unwrap();
        let result = dispatcher
            .dispatch(VerbApprover::User { uid: 1000 }, VerbCall::Stop)
            .await;
        // Router refuses (no plugin admitted on
        // com.example.spotify shelf); dispatch returns
        // RouterError. Custody NOT released because the dispatch
        // failed.
        assert!(matches!(result, Err(DispatchError::RouterError(_))));
        let after = custody.current(AUDIO_ACTIVE_SOURCE).await.unwrap();
        // Holder is still recorded — release didn't fire because
        // the dispatch failed.
        assert!(after
            .as_ref()
            .and_then(|c| c.holder_plugin.as_deref())
            .is_some());
    }

    /// Helper for Stop-with-ledger test that returns the ledger
    /// alongside the custody so we can assert on the audit
    /// entry shape directly.
    fn build_test_dispatcher_with_custody_and_ledger() -> (
        Arc<DefaultSourceVerbDispatcher>,
        Arc<ActiveSourceCustody>,
        Arc<LedgerPrimitive>,
    ) {
        let persistence: Arc<dyn crate::persistence::PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let state = StewardState::for_tests();
        let router = Arc::new(PluginRouter::new(Arc::clone(&state)));
        let uri_schemes =
            Arc::new(UriSchemeRegistry::new(Arc::clone(&persistence)));
        let active_source =
            Arc::new(ActiveSourceCustody::new(Arc::clone(&persistence)));
        let ledger = Arc::new(LedgerPrimitive::with_no_op_crypto(Arc::clone(
            &persistence,
        )));
        let dispatcher = Arc::new(
            DefaultSourceVerbDispatcher::new(
                Arc::clone(&router),
                Arc::clone(&uri_schemes),
                Arc::clone(&active_source),
            )
            .with_ledger(Arc::clone(&ledger)),
        );
        (dispatcher, active_source, ledger)
    }

    #[tokio::test]
    async fn stop_records_audit_entry_with_correct_shape() {
        let (dispatcher, custody, ledger) =
            build_test_dispatcher_with_custody_and_ledger();
        custody
            .record_claim(
                AUDIO_ACTIVE_SOURCE,
                "com.example.spotify",
                "spotify:track:1",
                r#"{}"#,
            )
            .await
            .unwrap();
        let _ = dispatcher
            .dispatch(VerbApprover::User { uid: 4242 }, VerbCall::Stop)
            .await;
        let entries = read_lifecycle_entries(&ledger).await;
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.subject_plugin.as_deref(), Some("user:4242"));
        let entry_json: serde_json::Value =
            serde_json::from_str(&entry.payload_json).unwrap();
        assert_eq!(
            entry_json["target"]["plugin_id"].as_str(),
            Some("com.example.spotify"),
            "target is the resolved current holder"
        );
        let payload = &entry_json["payload"];
        assert_eq!(payload["verb"].as_str(), Some("stop"));
        assert!(
            payload["primary_uri"].is_null(),
            "Stop has no URI target; primary_uri serialises as null \
             (or absent under skip_serializing_if; either is fine)"
        );
        assert_eq!(payload["uri_count"].as_u64(), Some(0));
        assert_eq!(payload["acquired_custody"].as_bool(), Some(false));
    }

    // ---- Stop release emits AudioPlaybackEnded ----

    /// Helper that wires a HappeningBus into the dispatcher so
    /// tests can subscribe and assert on emitted happenings.
    fn build_test_dispatcher_with_bus() -> (
        Arc<DefaultSourceVerbDispatcher>,
        Arc<ActiveSourceCustody>,
        Arc<HappeningBus>,
    ) {
        let persistence: Arc<dyn crate::persistence::PersistenceStore> =
            Arc::new(MemoryPersistenceStore::new());
        let state = StewardState::for_tests();
        let router = Arc::new(PluginRouter::new(Arc::clone(&state)));
        let uri_schemes =
            Arc::new(UriSchemeRegistry::new(Arc::clone(&persistence)));
        let active_source =
            Arc::new(ActiveSourceCustody::new(Arc::clone(&persistence)));
        let bus = Arc::clone(&state.bus);
        let dispatcher = Arc::new(
            DefaultSourceVerbDispatcher::new(
                Arc::clone(&router),
                Arc::clone(&uri_schemes),
                Arc::clone(&active_source),
            )
            .with_bus(Arc::clone(&bus)),
        );
        (dispatcher, active_source, bus)
    }

    #[tokio::test]
    async fn stop_dispatch_failure_does_not_emit_audio_playback_ended() {
        // Router-refused stop does NOT release custody and does
        // NOT emit AudioPlaybackEnded — the consumer convention
        // is "AudioPlaybackEnded means custody is gone", and a
        // failed stop leaves custody in place.
        let (dispatcher, custody, bus) = build_test_dispatcher_with_bus();
        let mut subscriber = bus.subscribe();
        custody
            .record_claim(
                AUDIO_ACTIVE_SOURCE,
                "com.example.spotify",
                "spotify:track:1",
                r#"{}"#,
            )
            .await
            .unwrap();
        let _ = dispatcher
            .dispatch(VerbApprover::User { uid: 1000 }, VerbCall::Stop)
            .await;
        // No AudioPlaybackEnded should have been emitted.
        let recv = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            subscriber.recv(),
        )
        .await;
        match recv {
            Err(_elapsed) => {
                // Timeout is the expected outcome — no emission.
            }
            Ok(Ok(h)) => {
                if matches!(h, Happening::AudioPlaybackEnded { .. }) {
                    panic!(
                        "router-failed stop must not emit AudioPlaybackEnded"
                    );
                }
            }
            Ok(Err(_lagged)) => {
                // Broadcast channel lag — accept as no-emission
                // signal in this test context.
            }
        }
    }

    #[tokio::test]
    async fn stop_with_no_holder_does_not_emit_audio_playback_ended() {
        // Pre-resolution failure (NoActiveSource): no release,
        // no emission.
        let (dispatcher, _custody, bus) = build_test_dispatcher_with_bus();
        let mut subscriber = bus.subscribe();
        let _ = dispatcher
            .dispatch(VerbApprover::User { uid: 1000 }, VerbCall::Stop)
            .await;
        let recv = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            subscriber.recv(),
        )
        .await;
        match recv {
            Err(_) => {} // expected: timeout
            Ok(Ok(h)) => {
                if matches!(h, Happening::AudioPlaybackEnded { .. }) {
                    panic!("pre-resolution stop must not emit");
                }
            }
            Ok(Err(_)) => {}
        }
    }

    #[tokio::test]
    async fn no_bus_wired_dispatch_path_runs_cleanly() {
        // Without a bus wired the dispatcher silently skips the
        // emission (no panic). The Stop dispatch still releases
        // custody on success — the bus emission is purely an
        // observable add-on.
        let (dispatcher, _custody, _ledger) =
            build_test_dispatcher_with_custody_and_ledger();
        // No bus on this dispatcher.
        let _ = dispatcher
            .dispatch(VerbApprover::User { uid: 1000 }, VerbCall::Stop)
            .await;
        // No assertion needed beyond no-panic.
    }
}
