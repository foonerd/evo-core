// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Shared artwork resolve cascade.
//!
//! Single implementation of the local → identity-synthesised-
//! online cascade, consumed by both the standalone artwork
//! resolve endpoint (`GET /api/v1/audio/artwork`) and the
//! composite track-detail endpoint's artwork sub-source. Both
//! surfaces MUST return the same content_hash for the same
//! target — the guardrail is one-canonical-path.
//!
//! The service holds every stateful piece of the pipeline:
//! the negative memo, the coalescer, and the admission gate.
//! `resolve()` runs the full cascade with all three composed
//! in the right order — negative memo pre-check → coalescer
//! (which absorbs concurrent same-key callers into one plugin
//! dispatch) → admission gate INSIDE the fetcher (same-key
//! waiters share the fetcher's permit) → local tier → local
//! NotFound branch synthesises an mpd-album target for the
//! online tier when identity is present → returns
//! [`CascadeOutcome`] in every terminal case.
//!
//! Callers (endpoints) translate `CascadeOutcome` into their
//! transport-appropriate response shape:
//!
//! - artwork_resolve_endpoint → 302 / 404 / 503 with
//!   `X-Artwork-Provider` header.
//! - track_detail_endpoint → SubSource {ok, not_found, error}
//!   with `provider_id` inline.

use std::sync::Arc;

use evo_plugin_sdk::contract::AssetCache;
use serde_json::{json, Value};

use crate::artwork_admission::{Admission, AdmissionError};
use crate::artwork_negative_cache::NegativeCache;
use crate::artwork_resolve_coalescer::{
    ArtworkResolveCoalescer, CoalesceError,
};
use crate::dispatcher::Dispatcher;
use crate::principal::Principal;
use evo_projection_core::WireOpId;

/// Terminal outcome of `resolve()`. Every variant is
/// exhaustive with a clear operator-readable detail so the
/// caller (endpoint) can render an accurate response.
#[derive(Debug, Clone)]
pub enum CascadeOutcome {
    /// A provider (local or online) or a positive coalescer
    /// memo produced content.
    Resolved {
        /// Content-addressed hash the caller redirects to for
        /// bytes (immutable-cacheable at the hash endpoint).
        content_hash: String,
        /// Which provider produced the content
        /// (`local_sidecar`, `local_embedded`, `cover_art_archive`,
        /// `lastfm`, `itunes`, `volumio_meta`). Absent on
        /// pre-provenance plugin builds.
        provider_id: Option<String>,
    },
    /// Both provider tiers structured-NotFound — the caller
    /// UI's placeholder floor applies. `provider_id` carries
    /// the last-tier id from the fresh cascade OR from a
    /// negative-memo hit (so the caller can label "we tried
    /// and gave up at online/itunes" vs "at local").
    NotFound {
        /// Last-tier provider id from the fresh cascade or from
        /// the negative-memo hit. Absent on cold all-tier
        /// misses (the endpoint substitutes a boundary default).
        provider_id: Option<String>,
        /// Human-readable failure message ("no sidecar; no
        /// embedded picture", "cover_art_archive: 404").
        detail: String,
    },
    /// Caller-input error — bad_request from a plugin, bad
    /// scheme/value/size at the endpoint boundary. Not
    /// cached negatively (the client can fix and retry).
    BadRequest {
        /// Boundary or plugin-emitted refusal message.
        detail: String,
    },
    /// Admission bucket saturated; caller should retry after
    /// backoff. Signals structured backpressure to the UI.
    AdmissionDeadline {
        /// Which bucket + how long the caller waited before
        /// the admission gate refused.
        detail: String,
    },
    /// Coalescer's inflight sleeper deadline elapsed — the
    /// in-flight fetcher is taking longer than the framework
    /// tolerates. Caller should retry.
    CoalescerDeadline {
        /// Key + waited_ms — the framework's retry hint.
        detail: String,
    },
    /// Dispatch / envelope / wire-op failure — not a tier
    /// fault; the operator sees the underlying error.
    Transient {
        /// Underlying wire / envelope / dispatch error.
        detail: String,
    },
}

/// Provenance identifier for negative-cache hits that did
/// not memoise a specific tier id — the operator UI can
/// distinguish memoised negatives from live cascade misses.
pub const PROVENANCE_NEGATIVE_CACHE: &str = "negative_cache";

/// Provenance identifier surfaced on positive-hit responses
/// served straight from the persistent resolve-index without
/// re-running the plugin cascade. Operator diagnostic surfaces
/// use this to distinguish "we shortcut the browse from the
/// index" from "we ran the full local → online chain" — the
/// gap in cost between the two is orders of magnitude on
/// large libraries.
pub const PROVENANCE_RESOLVE_INDEX: &str = "resolve_index";

/// The shared cascade service. Constructed once at boot;
/// shared via `Arc` by every endpoint that needs artwork
/// resolution.
pub struct ArtworkCascade {
    dispatcher: Arc<dyn Dispatcher>,
    coalescer: Arc<ArtworkResolveCoalescer>,
    admission: Arc<Admission>,
    negative_cache: Arc<NegativeCache>,
    /// Persistent (scheme, value, size) → content_hash index.
    /// The FAST PATH: every resolve consults this before the
    /// coalescer memo and the plugin dispatch. On hit the
    /// endpoint 302-redirects to `/api/v1/audio/artwork/<hash>`
    /// without touching the plugin — turning browse artwork
    /// resolution from an O(library) per-tile tag walk into
    /// an O(1) lookup that survives restart, memo expiry, and
    /// coalescer eviction. Populated on every successful
    /// resolve (see `remember_positive`) and evicted by the
    /// operator's `?refresh=1` gesture (see `forget`).
    resolve_index:
        Option<Arc<crate::artwork_resolve_index::ArtworkResolveIndex>>,
    /// AssetCache handle used by the `?refresh=1` gesture to
    /// evict the positive-hash entry when the operator asks for
    /// a fresh resolve. `None` when the steward has no asset
    /// cache wired — in that case the cascade still works but
    /// the refresh gesture can only evict the negative memo.
    asset_cache: Option<Arc<dyn AssetCache>>,
}

impl ArtworkCascade {
    /// Construct a cascade with the caller-supplied dispatcher
    /// and default coalescer / admission / negative-cache
    /// tunables. The framework router builds one at boot and
    /// clones its `Arc<Self>` into every endpoint that needs
    /// artwork resolution.
    ///
    /// `asset_cache` is optional: `Some` wires the
    /// positive-index eviction path so `?refresh=1` drops the
    /// content-hash bytes; `None` leaves the cascade with
    /// negative-memo-only eviction.
    ///
    /// `resolve_index` is optional: `Some` wires the O(1)
    /// persistent positive index so browse resolves become
    /// index-lookup-fast; `None` leaves the cascade with
    /// coalescer-memo-only positive memoisation (30 s TTL,
    /// in-memory, lost on restart — the pre-2026-07-29
    /// behaviour).
    pub fn new(
        dispatcher: Arc<dyn Dispatcher>,
        asset_cache: Option<Arc<dyn AssetCache>>,
        resolve_index: Option<
            Arc<crate::artwork_resolve_index::ArtworkResolveIndex>,
        >,
    ) -> Arc<Self> {
        Arc::new(Self {
            dispatcher,
            coalescer: Arc::new(ArtworkResolveCoalescer::new()),
            admission: Arc::new(Admission::new()),
            negative_cache: Arc::new(NegativeCache::new()),
            resolve_index,
            asset_cache,
        })
    }

    /// Evict any negative memo AND any positive-hash entry for
    /// `(scheme, value, size)`.
    ///
    /// The operator escape hatch: `?refresh=1` on the resolve
    /// endpoint calls this before dispatching, so a target
    /// whose memo is stuck on a pre-fix / pre-retag miss can
    /// be cleared without waiting for TTL. When a positive
    /// hash is memoised for the same target, this call also
    /// evicts the AssetCache bytes at that hash — completing
    /// the "clear + re-resolve" gesture the operator UI
    /// invokes when it wants a fresh serve regardless of
    /// whether the last outcome was positive or negative.
    pub async fn forget(&self, scheme: &str, value: &str, size: &str) {
        let neg_key = (scheme.to_string(), value.to_string(), size.to_string());
        self.negative_cache.forget(&neg_key);
        // Evict the coalescer's memo too — without this, the
        // next resolve for the same target re-hydrates the
        // cached outcome (content_hash + provider_id) from the
        // memo and skips the plugin dispatch. That would race
        // with the AssetCache eviction below: the endpoint
        // would 302 to the same hash whose bytes we just
        // deleted, and the hash endpoint would 404 until the
        // memo TTL elapsed.
        self.coalescer.forget(scheme, value, size);
        // Look up and remove the persistent resolve-index
        // entry before touching the AssetCache. Order matters:
        // if the AssetCache delete succeeds and the index
        // remove fails, the index would point at bytes that no
        // longer exist — a persistent inconsistency. Removing
        // the index first means a subsequent AssetCache
        // failure only wastes bytes (which the next successful
        // resolve overwrites).
        let hash = if let Some(index) = &self.resolve_index {
            let h = index.get(scheme, value, size).await;
            if let Err(e) = index.forget(scheme, value, size).await {
                tracing::warn!(
                    scheme = %scheme,
                    value = %value,
                    size = %size,
                    error = %e,
                    "artwork cascade: resolve-index forget failed"
                );
            }
            h
        } else {
            None
        };
        if let Some(hash) = hash {
            if let Some(cache) = &self.asset_cache {
                match cache.delete(&hash).await {
                    Ok(existed) => {
                        tracing::info!(
                            scheme = %scheme,
                            value = %value,
                            size = %size,
                            content_hash = %hash,
                            existed = existed,
                            "artwork cascade: positive-index refresh evicted asset-cache entry"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            scheme = %scheme,
                            value = %value,
                            size = %size,
                            content_hash = %hash,
                            error = %e,
                            "artwork cascade: positive-index refresh could not evict asset-cache entry"
                        );
                    }
                }
            }
        }
    }

    /// Record a successful resolve so a later `?refresh=1`
    /// gesture can find the content hash to evict AND — more
    /// importantly — so the next resolve for this target
    /// short-circuits to an O(1) index lookup (fast path in
    /// `resolve`). Called from the cascade's Ok path.
    async fn remember_positive(
        &self,
        scheme: &str,
        value: &str,
        size: &str,
        content_hash: &str,
    ) {
        if let Some(index) = &self.resolve_index {
            if let Err(e) = index.put(scheme, value, size, content_hash).await {
                tracing::warn!(
                    scheme = %scheme,
                    value = %value,
                    size = %size,
                    content_hash = %content_hash,
                    error = %e,
                    "artwork cascade: resolve-index put failed; \
                     browse fast-path will miss until next successful put"
                );
            }
        }
    }

    /// Run the cascade. `scheme` + `value` + `size` are the
    /// canonical inputs (size pre-canonicalised by the caller
    /// — e.g., `tiny` → `small`). Returns [`CascadeOutcome`]
    /// with a fully classified terminal state.
    pub async fn resolve(
        &self,
        principal: &Principal,
        scheme: &str,
        value: &str,
        size: &str,
    ) -> CascadeOutcome {
        let negative_key =
            (scheme.to_string(), value.to_string(), size.to_string());

        // Long-TTL negative memo pre-check.
        if let Some(entry) = self.negative_cache.get(&negative_key) {
            let provider_id = entry
                .provider_id
                .or_else(|| Some(PROVENANCE_NEGATIVE_CACHE.to_string()));
            let detail = format!(
                "artwork not resolved (memoised {}): status={} detail={}",
                PROVENANCE_NEGATIVE_CACHE, entry.status, entry.detail
            );
            return CascadeOutcome::NotFound {
                provider_id,
                detail,
            };
        }

        // Persistent positive-index FAST PATH. Consulted BEFORE
        // the coalescer memo and the plugin dispatch so a
        // library-scale browse over resolved albums is O(1)
        // per tile — the index survives restart, memo expiry,
        // and the coalescer's 30 s TTL. The index is only a
        // claim about which hash to serve; if the AssetCache
        // bytes were evicted (LRU quota, operator delete), the
        // hash endpoint will surface the 404 verbatim and the
        // operator UI can retry with `?refresh=1`. On the
        // common case (index populated, bytes present) this
        // reduces browse cost from O(library) tag-walk per tile
        // to one file read + one memory hash lookup at the
        // endpoint layer.
        if let Some(index) = &self.resolve_index {
            if let Some(hash) = index.get(scheme, value, size).await {
                return CascadeOutcome::Resolved {
                    content_hash: hash,
                    provider_id: Some(PROVENANCE_RESOLVE_INDEX.to_string()),
                };
            }
        }

        // Coalescer + fetcher closure. Captures the dispatch
        // machinery, the admission gate, and the negative-
        // cache write side.
        let dispatcher = Arc::clone(&self.dispatcher);
        let admission = Arc::clone(&self.admission);
        let negative_cache_for_fetcher = Arc::clone(&self.negative_cache);
        let negative_key_for_fetcher = negative_key.clone();
        let scheme_owned = scheme.to_string();
        let value_owned = value.to_string();
        let size_owned = size.to_string();
        let principal_clone = principal.clone();

        let outcome = self
            .coalescer
            .resolve_or_coalesce(scheme, value, size, move || async move {
                run_cascade(
                    dispatcher,
                    admission,
                    principal_clone,
                    scheme_owned,
                    value_owned,
                    size_owned,
                    negative_cache_for_fetcher,
                    negative_key_for_fetcher,
                )
                .await
            })
            .await;

        match outcome {
            Ok((content_hash, provider_id)) => {
                // Persistent positive-index population. On the
                // next resolve for the same target the fast
                // path above short-circuits to O(1) without
                // touching the plugin. Enables the `?refresh=1`
                // gesture to reach the asset-cache bytes too.
                self.remember_positive(scheme, value, size, &content_hash)
                    .await;
                CascadeOutcome::Resolved {
                    content_hash,
                    provider_id,
                }
            }
            Err(CoalesceError::FetcherError { reason, .. }) => {
                classify_fetcher_error(reason)
            }
            Err(CoalesceError::WaitDeadlineElapsed {
                scheme,
                value,
                size,
                waited_ms,
            }) => CascadeOutcome::CoalescerDeadline {
                detail: format!(
                    "artwork resolve coalescer wait deadline elapsed for \
                     (scheme={scheme}, value={value}, size={size}) after \
                     {waited_ms} ms — upstream provider is slow; retry"
                ),
            },
        }
    }
}

// -----------------------------------------------------------
// Internal helpers
// -----------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn run_cascade(
    dispatcher: Arc<dyn Dispatcher>,
    admission: Arc<Admission>,
    principal: Principal,
    scheme_owned: String,
    value_owned: String,
    size_owned: String,
    negative_cache: Arc<NegativeCache>,
    negative_key: (String, String, String),
) -> Result<(String, Option<String>), String> {
    // Admission BEFORE dispatch. Same-key waiters (in the
    // coalescer's sleeper arm) share this permit; only the
    // fetcher arm holds admission.
    let _permit = match admission.admit(&scheme_owned).await {
        Ok(guard) => guard,
        Err(AdmissionError::DeadlineElapsed {
            scheme,
            bucket,
            waited_ms,
        }) => {
            return Err(format!(
                "artwork resolve admission deadline elapsed on \
                 {bucket} bucket for scheme={scheme} after \
                 {waited_ms} ms"
            ));
        }
        Err(AdmissionError::ClosedRuntime) => {
            return Err("artwork resolve admission runtime closed".into());
        }
    };
    use base64::Engine;

    // Scheme-branch: artist schemes route DIRECTLY to the
    // online plugin's byte-cached artist verb. Skipping the
    // local tier is honest — this plugin's local tier is
    // per-track / per-album / per-directory sidecar art; it
    // has no artist-portrait knowledge, so a Tier 1 dispatch
    // on an artist scheme would land as `bad_request` and
    // short-circuit the cascade entirely. See
    // `artwork.resolve_artist_online` in the artwork.online
    // plugin for the byte-cache contract (fetch + transcode +
    // AssetCache push + Deezer live-fetch refusal).
    if scheme_owned == "artist-name" || scheme_owned == "artist-mbid" {
        let artist_payload = json!({
            "v": 1,
            "target": {"scheme": &scheme_owned, "value": &value_owned},
            "size": &size_owned,
        });
        let artist_payload_b64 = base64::engine::general_purpose::STANDARD
            .encode(artist_payload.to_string().as_bytes());
        let artist_envelope = json!({
            "shelf": "artwork.providers",
            "request_type": "artwork.resolve_artist_online",
            "payload_b64": &artist_payload_b64,
        });
        let op_id = WireOpId::new("request")
            .map_err(|e| format!("wire-op id construction failed: {e}"))?;
        let artist_dispatch = dispatcher
            .dispatch(&op_id, artist_envelope, &principal)
            .await;
        let artist_env = match artist_dispatch {
            Ok(e) => e,
            Err(e) => {
                return Err(format!(
                    "artwork resolve dispatch failed \
                     (artwork.resolve_artist_online): {e}"
                ));
            }
        };
        let artist_response =
            peel_plugin_response(&artist_env).ok_or_else(|| {
                format!(
                    "artwork resolve: malformed dispatch envelope (no \
                     payload_b64 or non-JSON inner payload) for \
                     artwork.resolve_artist_online: {artist_env}"
                )
            })?;
        let artist_provider_id = extract_provider_id(&artist_response);
        if let Some(hash) = extract_content_hash(&artist_response) {
            return Ok((hash, artist_provider_id));
        }
        let artist_status = extract_status(&artist_response);
        let artist_detail = extract_detail(&artist_response);
        if artist_status == "not_found" {
            negative_cache.put(
                negative_key.clone(),
                artist_status.clone(),
                artist_detail.clone(),
                artist_provider_id.clone(),
            );
            return Err(format!(
                "artwork not resolved: status={artist_status} \
                 detail={artist_detail}"
            ));
        } else if artist_status == "unavailable" {
            return Err(format!(
                "artwork upstream unavailable: status={artist_status} \
                 detail={artist_detail}"
            ));
        } else {
            return Err(format!(
                "artwork not resolved: status={artist_status} \
                 detail={artist_detail}"
            ));
        }
    }

    // Tier 1: local dispatch with the caller's original target.
    let local_payload = json!({
        "v": 1,
        "target": {"scheme": &scheme_owned, "value": &value_owned},
        "size": &size_owned,
    });
    let local_payload_b64 = base64::engine::general_purpose::STANDARD
        .encode(local_payload.to_string().as_bytes());
    let local_envelope = json!({
        "shelf": "artwork.providers",
        "request_type": "artwork.resolve",
        "payload_b64": &local_payload_b64,
    });
    let op_id = WireOpId::new("request")
        .map_err(|e| format!("wire-op id construction failed: {e}"))?;
    let local_dispatch = dispatcher
        .dispatch(&op_id, local_envelope, &principal)
        .await;
    let local_env = match local_dispatch {
        Ok(e) => e,
        Err(e) => {
            return Err(format!(
                "artwork resolve dispatch failed (artwork.resolve): {e}"
            ));
        }
    };
    let local_response = peel_plugin_response(&local_env).ok_or_else(|| {
        format!(
            "artwork resolve: malformed dispatch envelope (no \
             payload_b64 or non-JSON inner payload) for \
             artwork.resolve: {local_env}"
        )
    })?;
    let local_provider_id = extract_provider_id(&local_response);
    if let Some(hash) = extract_content_hash(&local_response) {
        return Ok((hash, local_provider_id));
    }
    let local_status = extract_status(&local_response);
    let local_detail = extract_detail(&local_response);
    if local_status == "bad_request" {
        // Caller-input error at the local tier; do not cache
        // negatively — the caller can fix and retry.
        return Err(format!(
            "artwork not resolved: status={local_status} \
             detail={local_detail}"
        ));
    }

    // Local NotFound: synthesise online target from identity.
    let identity = extract_identity(&local_response);
    let Some((artist, album)) = identity else {
        // No identity available — skip online, memoise
        // negatively so a browse burst re-visiting this
        // target does not re-invoke the local tag-read.
        negative_cache.put(
            negative_key.clone(),
            local_status.clone(),
            local_detail.clone(),
            local_provider_id.clone(),
        );
        return Err(format!(
            "artwork not resolved: status={local_status} \
             detail={local_detail}"
        ));
    };

    // Tier 2: online dispatch with synthesised mpd-album.
    let online_target_value = format!("{artist}|{album}");
    let online_payload = json!({
        "v": 1,
        "target": {"scheme": "mpd-album", "value": &online_target_value},
        "size": &size_owned,
    });
    let online_payload_b64 = base64::engine::general_purpose::STANDARD
        .encode(online_payload.to_string().as_bytes());
    let online_envelope = json!({
        "shelf": "artwork.providers",
        "request_type": "artwork.resolve_online",
        "payload_b64": &online_payload_b64,
    });
    let op_id = WireOpId::new("request")
        .map_err(|e| format!("wire-op id construction failed: {e}"))?;
    let online_dispatch = dispatcher
        .dispatch(&op_id, online_envelope, &principal)
        .await;
    let online_env = match online_dispatch {
        Ok(e) => e,
        Err(e) => {
            return Err(format!(
                "artwork resolve dispatch failed \
                 (artwork.resolve_online): {e}"
            ));
        }
    };
    let online_response =
        peel_plugin_response(&online_env).ok_or_else(|| {
            format!(
                "artwork resolve: malformed dispatch envelope (no \
                 payload_b64 or non-JSON inner payload) for \
                 artwork.resolve_online: {online_env}"
            )
        })?;
    let online_provider_id = extract_provider_id(&online_response);
    if let Some(hash) = extract_content_hash(&online_response) {
        return Ok((hash, online_provider_id));
    }
    let online_status = extract_status(&online_response);
    let online_detail = extract_detail(&online_response);
    // Only a genuine, provider-confirmed "not_found" is
    // eligible for the negative memo. `unavailable`,
    // `bad_request`, or anything else the plugin might emit
    // is transient / caller-fixable and must not be cached —
    // otherwise a transient upstream failure would masquerade
    // as definitive absence until the memo expires.
    if online_status == "not_found" {
        negative_cache.put(
            negative_key.clone(),
            online_status.clone(),
            online_detail.clone(),
            online_provider_id.clone(),
        );
        Err(format!(
            "artwork not resolved: status={online_status} \
             detail={online_detail}"
        ))
    } else if online_status == "unavailable" {
        // Distinct prefix so classify_fetcher_error routes this
        // to CascadeOutcome::Transient (not NotFound).
        Err(format!(
            "artwork upstream unavailable: status={online_status} \
             detail={online_detail}"
        ))
    } else {
        Err(format!(
            "artwork not resolved: status={online_status} \
             detail={online_detail}"
        ))
    }
}

fn classify_fetcher_error(reason: String) -> CascadeOutcome {
    if reason.starts_with("artwork resolve admission deadline") {
        CascadeOutcome::AdmissionDeadline { detail: reason }
    } else if reason.starts_with("artwork upstream unavailable:") {
        // Upstream (online tier providers) was reachable-but-
        // transient. Distinct from NotFound so the caller can
        // retry and the endpoint can surface a 502 rather than
        // a 404 (a "we could not reach anyone" outcome, not "we
        // reached everyone and confirmed absence").
        CascadeOutcome::Transient { detail: reason }
    } else if reason.starts_with("artwork not resolved:") {
        CascadeOutcome::NotFound {
            provider_id: None,
            detail: reason,
        }
    } else {
        // dispatch / envelope / wire-op failures land here.
        CascadeOutcome::Transient { detail: reason }
    }
}

// -----------------------------------------------------------
// Peel + extract helpers (canonical for both endpoints)
// -----------------------------------------------------------

fn peel_plugin_response(envelope: &Value) -> Option<Value> {
    use base64::Engine;
    let payload_b64 = envelope.as_object()?.get("payload_b64")?.as_str()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload_b64)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn extract_content_hash(plugin_response: &Value) -> Option<String> {
    plugin_response
        .as_object()
        .and_then(|m| m.get("content_hash"))
        .and_then(Value::as_str)
        .map(String::from)
}

fn extract_status(plugin_response: &Value) -> String {
    plugin_response
        .as_object()
        .and_then(|m| m.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}

fn extract_detail(plugin_response: &Value) -> String {
    plugin_response
        .as_object()
        .and_then(|m| m.get("detail"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn extract_provider_id(plugin_response: &Value) -> Option<String> {
    plugin_response
        .as_object()
        .and_then(|m| m.get("provider_id"))
        .and_then(Value::as_str)
        .map(String::from)
}

fn extract_identity(plugin_response: &Value) -> Option<(String, String)> {
    let identity = plugin_response.as_object()?.get("identity")?.as_object()?;
    let artist = identity.get("artist")?.as_str()?.trim();
    let album = identity.get("album")?.as_str()?.trim();
    if artist.is_empty() || album.is_empty() {
        return None;
    }
    Some((artist.to_string(), album.to_string()))
}
