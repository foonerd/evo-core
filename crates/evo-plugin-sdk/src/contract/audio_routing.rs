// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Audio data plane routing — plugin-side endpoint API.
//!
//! Plugins receive an [`AudioRouting`] handle on their
//! [`crate::contract::LoadContext`] when their manifest
//! declares an audio capability (`source` with an audio
//! `output_kind`, `delivery`, or `composition`). The handle
//! exposes the OS-native primitive the framework has
//! configured for the chain stage — an ALSA pcm name, a named
//! pipe, a shared-memory region, or a JACK port — without the
//! plugin having to know which substrate the framework
//! selected.
//!
//! ## What this module owns
//!
//! Type definitions only — [`EndpointKind`], [`WriteEndpoint`],
//! [`ReadEndpoint`], [`CompositionEndpoints`],
//! [`RouteChangeCallback`], [`AudioRoutingError`] — plus the
//! [`AudioRouting`] trait every framework runtime must
//! implement. The framework's
//! [`crate::contract::LoadContext::audio_routing`] field
//! carries an `Option<Arc<dyn AudioRouting>>`; plugins whose
//! manifest declares an audio capability receive `Some`,
//! everyone else receives `None`.
//!
//! ## What this module does not own
//!
//! The framework runtime that selects substrates, configures
//! ALSA loopbacks, manages shm regions, and fires
//! [`RouteChangeCallback`]s on topology rewires. That lives in
//! the framework crate alongside the reconciliation engine.
//! Audio bytes do NOT traverse this trait — the trait returns
//! an endpoint identifier (path / port / shm region); the
//! plugin then opens the OS primitive directly and reads /
//! writes audio bytes through it. The framework's role is
//! topology configuration, not byte forwarding.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::audio::AudioFormat;

/// OS-native primitive the framework selected for one endpoint
/// of the audio chain.
///
/// The framework picks the substrate per topology based on
/// plugin declarations and platform capabilities. A topology
/// may use [`AlsaPcm`](Self::AlsaPcm) between source and
/// composition and [`SharedMemory`](Self::SharedMemory)
/// between composition and delivery; the substrate choice is
/// per-stage, not global.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKind {
    /// ALSA PCM device — an `hw:` / `plughw:` / `pcm:` name
    /// the plugin opens via libasound. Default substrate for
    /// Linux audio chains; zero-copy via the kernel's
    /// snd-aloop module when both endpoints are ALSA.
    AlsaPcm,
    /// Named FIFO at the [`WriteEndpoint::path`]. Simpler
    /// substrate; works without ALSA (e.g. testing). Higher
    /// syscall overhead than ALSA loopback; not preferred for
    /// production audio.
    NamedPipe,
    /// Shared-memory region with a declared layout the plugin
    /// must honour. Used for very-low-latency cases (multi-
    /// room sync) where the lock-free shm ring beats kernel
    /// pipe / ALSA loopback.
    SharedMemory,
    /// JACK port name. Professional-audio substrate;
    /// bit-perfect; sample-accurate sync. Vendor-opt-in (the
    /// distribution must include a JACK runtime).
    JackPort,
}

/// Endpoint a source / composition-output / delivery stage
/// writes audio bytes into.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteEndpoint {
    /// Substrate kind.
    pub kind: EndpointKind,
    /// Substrate-specific path. For [`EndpointKind::AlsaPcm`]
    /// the ALSA pcm name; for [`EndpointKind::NamedPipe`] the
    /// filesystem path; for [`EndpointKind::SharedMemory`] the
    /// shm region path; for [`EndpointKind::JackPort`] the
    /// JACK port name (encoded as a path for shape uniformity).
    pub path: PathBuf,
    /// Negotiated format the writer must produce.
    pub format: AudioFormat,
    /// Suggested write-side buffer capacity in audio frames.
    /// The framework derives this from the chain's latency
    /// budget; the plugin treats it as a hint, not a
    /// requirement.
    pub buffer_frames: u32,
}

/// Endpoint a composition-input / delivery stage reads audio
/// bytes from. Mirrors [`WriteEndpoint`] structurally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadEndpoint {
    /// Substrate kind.
    pub kind: EndpointKind,
    /// Substrate-specific path / identifier (see
    /// [`WriteEndpoint::path`]).
    pub path: PathBuf,
    /// Negotiated format the reader will receive.
    pub format: AudioFormat,
    /// Suggested read-side buffer capacity in audio frames.
    pub buffer_frames: u32,
}

/// Endpoint pair handed to a composition plugin. The
/// composition reads from `input` and writes to `output`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionEndpoints {
    /// Input endpoint — the format the upstream stage produces.
    pub input: ReadEndpoint,
    /// Output endpoint — the format the downstream stage
    /// expects.
    pub output: WriteEndpoint,
}

/// Callback fired when the framework rewires the audio chain
/// topology (source change, composition mode change, delivery
/// target change, hot-plug). Plugins register a callback via
/// [`AudioRouting::on_route_change`]; the framework invokes it
/// on the rewire path, before the new endpoints are made
/// available via [`AudioRouting::write_endpoint`] /
/// [`AudioRouting::read_endpoint`] /
/// [`AudioRouting::composition_endpoints`].
///
/// The callback is `Arc<dyn Fn(...) + Send + Sync>` so plugins
/// can store the same callback handle in multiple call sites
/// (e.g. a player struct's owned reference + the framework's
/// registry) without the framework requiring `Box<dyn
/// FnOnce>` semantics — route changes can fire many times
/// over the lifetime of one callback.
pub type RouteChangeCallback =
    Arc<dyn Fn(&RouteChange) + Send + Sync + 'static>;

/// Full resolved routing snapshot for one plugin's chain
/// stage. Mirror of the framework's
/// `evo::audio_routing::ResolvedRouting` shaped for the SDK
/// surface — both ends of the OOP wire-proxy work against
/// this canonical type. The framework's wire host constructs
/// this from the local `ResolvedRouting` on every rewire and
/// pushes it via
/// [`crate::wire::WireFrame::AudioRoutingStateChanged`]; the
/// SDK's `WireAudioRouting` proxy caches it and serves the
/// four sync read methods on [`AudioRouting`] from the cache.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedRouting {
    /// Source-side write endpoint. `Some` when the plugin is
    /// a source or composition output-side; `None` for
    /// delivery-only plugins or before the first publish.
    pub write: Option<WriteEndpoint>,
    /// Delivery-side read endpoint. `Some` when the plugin is
    /// a delivery or composition input-side; `None` for
    /// source-only plugins or before the first publish.
    pub read: Option<ReadEndpoint>,
    /// Negotiated audio format for the plugin's chain stage.
    pub format: AudioFormat,
}

/// Description of one route-change event. The plugin's
/// callback receives this as its single argument; the typed
/// fields let the plugin react without re-fetching every
/// endpoint via the trait methods.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteChange {
    /// Negotiated format after the rewire. Plugins that drive
    /// downstream hardware reconfigure to this format; plugins
    /// that pass bytes through reopen their endpoint at the
    /// new format.
    pub new_format: AudioFormat,
    /// Free-form operator-readable reason
    /// (`"source plugin changed"` / `"hot-plug detected new
    /// DAC"` / `"operator pinned topology"`). Surfaced in the
    /// active-topology subject for diagnostics; treated
    /// opaquely by the framework.
    pub reason: String,
}

/// Plugin-side audio-data-plane routing surface. Source /
/// composition / delivery plugins consume their endpoint via
/// the corresponding trait method.
///
/// Every method returns [`Result`] because the framework may
/// not have a topology configured for this plugin yet — for
/// example, at admission before the reconciliation engine has
/// run, or after a rewire has been requested but not yet
/// completed. Plugins must handle [`AudioRoutingError`] gracefully:
/// retry briefly, then surface a structured load failure to
/// the operator.
pub trait AudioRouting: Send + Sync + std::fmt::Debug {
    /// Returns the endpoint a source plugin (or composition's
    /// output stage) should write audio bytes into. Returns
    /// [`AudioRoutingError::EndpointNotConfigured`] when no
    /// topology has been published for this plugin yet.
    fn write_endpoint(&self) -> Result<WriteEndpoint, AudioRoutingError>;

    /// Returns the endpoint a delivery plugin (or
    /// composition's input stage) should read audio bytes
    /// from. Same not-configured semantics as
    /// [`Self::write_endpoint`].
    fn read_endpoint(&self) -> Result<ReadEndpoint, AudioRoutingError>;

    /// Returns the input + output endpoint pair handed to a
    /// composition plugin. Composition plugins use this in
    /// preference to calling [`Self::read_endpoint`] +
    /// [`Self::write_endpoint`] separately so the two
    /// endpoints are returned coherently from the same
    /// topology snapshot.
    fn composition_endpoints(
        &self,
    ) -> Result<CompositionEndpoints, AudioRoutingError>;

    /// Returns the negotiated audio format for the chain
    /// stage. Same not-configured semantics; equal to the
    /// `format` field on the endpoint structs when the
    /// topology IS configured.
    fn current_format(&self) -> Result<AudioFormat, AudioRoutingError>;

    /// Register a callback invoked on every topology rewire.
    /// The framework holds the callback for the lifetime of
    /// the plugin admission; calling this method again
    /// replaces the previously-registered callback. Plugins
    /// may pass [`None`] to clear the callback.
    fn on_route_change(&self, callback: Option<RouteChangeCallback>);
}

/// Errors raised by [`AudioRouting`] methods.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AudioRoutingError {
    /// No topology has been published for this plugin yet.
    /// Plugins should retry briefly, then surface a structured
    /// load failure to the operator.
    #[error("audio routing: no topology configured for this plugin yet")]
    EndpointNotConfigured,
    /// The plugin's manifest does not declare a capability
    /// that would receive an endpoint of the requested kind
    /// (e.g. `read_endpoint` called from a source plugin
    /// whose manifest only declares output). Indicates a
    /// plugin bug — the SDK trait is being called against the
    /// wrong stage.
    #[error(
        "audio routing: this plugin's manifest does not declare a {kind:?} \
         endpoint role"
    )]
    WrongStage {
        /// What was requested.
        kind: AudioRoutingMethod,
    },
    /// The plugin asked for the composition endpoint pair but
    /// is not a composition plugin (or vice versa). Same
    /// shape as [`Self::WrongStage`] but for composition
    /// specifically.
    #[error("audio routing: this plugin is not a composition plugin")]
    NotCompositionPlugin,
}

/// Trait method discriminator carried by
/// [`AudioRoutingError::WrongStage`] for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioRoutingMethod {
    /// `write_endpoint()` was called.
    WriteEndpoint,
    /// `read_endpoint()` was called.
    ReadEndpoint,
    /// `composition_endpoints()` was called.
    CompositionEndpoints,
    /// `current_format()` was called.
    CurrentFormat,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{AudioFormat, PcmCodec};

    fn pcm() -> AudioFormat {
        AudioFormat::Pcm {
            codec: PcmCodec::PcmS24Le,
            rate_hz: 192_000,
            channels: 2,
        }
    }

    #[test]
    fn endpoint_kind_serialises_as_snake_case() {
        let json = serde_json::to_string(&EndpointKind::AlsaPcm).unwrap();
        assert_eq!(json, "\"alsa_pcm\"");
        let json = serde_json::to_string(&EndpointKind::SharedMemory).unwrap();
        assert_eq!(json, "\"shared_memory\"");
        let json = serde_json::to_string(&EndpointKind::JackPort).unwrap();
        assert_eq!(json, "\"jack_port\"");
    }

    #[test]
    fn write_endpoint_round_trips_through_serde() {
        let ep = WriteEndpoint {
            kind: EndpointKind::AlsaPcm,
            path: PathBuf::from("hw:0,0"),
            format: pcm(),
            buffer_frames: 1024,
        };
        let json = serde_json::to_string(&ep).unwrap();
        let parsed: WriteEndpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ep);
    }

    #[test]
    fn composition_endpoints_round_trips_through_serde() {
        let ep = CompositionEndpoints {
            input: ReadEndpoint {
                kind: EndpointKind::AlsaPcm,
                path: PathBuf::from("loopback:0,0"),
                format: pcm(),
                buffer_frames: 1024,
            },
            output: WriteEndpoint {
                kind: EndpointKind::AlsaPcm,
                path: PathBuf::from("loopback:0,1"),
                format: pcm(),
                buffer_frames: 1024,
            },
        };
        let json = serde_json::to_string(&ep).unwrap();
        let parsed: CompositionEndpoints = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ep);
    }

    #[test]
    fn route_change_round_trips_through_serde() {
        let rc = RouteChange {
            new_format: pcm(),
            reason: "hot-plug detected new DAC".into(),
        };
        let json = serde_json::to_string(&rc).unwrap();
        let parsed: RouteChange = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, rc);
    }

    #[test]
    fn audio_routing_error_renders_actionable_messages() {
        let err = AudioRoutingError::EndpointNotConfigured;
        assert!(err.to_string().contains("no topology configured"));
        let err = AudioRoutingError::WrongStage {
            kind: AudioRoutingMethod::ReadEndpoint,
        };
        // The format string uses Debug formatting on the enum
        // variant — `ReadEndpoint` rather than the serde
        // `read_endpoint` token.
        assert!(err.to_string().contains("ReadEndpoint"));
        let err = AudioRoutingError::NotCompositionPlugin;
        assert!(err.to_string().contains("composition"));
    }

    #[test]
    fn audio_routing_method_serialises_as_snake_case() {
        let json =
            serde_json::to_string(&AudioRoutingMethod::WriteEndpoint).unwrap();
        assert_eq!(json, "\"write_endpoint\"");
        let json =
            serde_json::to_string(&AudioRoutingMethod::ReadEndpoint).unwrap();
        assert_eq!(json, "\"read_endpoint\"");
        let json =
            serde_json::to_string(&AudioRoutingMethod::CompositionEndpoints)
                .unwrap();
        assert_eq!(json, "\"composition_endpoints\"");
    }
}
