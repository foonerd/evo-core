// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Audio data plane typed contracts.
//!
//! Foundation types for the bit-perfect audio data plane: the
//! formats source / composition / delivery plugins declare in
//! their manifests, the rate / channel descriptors that pin
//! every stage's contract, and the route-change semantics
//! plugins consume through [`crate::contract::LoadContext`].
//!
//! Module shape:
//!
//! - [`AudioFormat`]: the negotiated format at one stage of the
//!   audio chain. Three variants — PCM (rate × bit-depth ×
//!   channels), DSD (rate × transport × channels), and
//!   encoded-passthrough (HDMI-to-AVR bitstream cases where the
//!   chain does not decode).
//! - [`PcmCodec`]: PCM bit-depth + container variants
//!   (s16le / s24le / s32le / f32) the framework's data plane
//!   carries through.
//! - [`DsdRate`] + [`DsdTransport`]: DSD rate ladder
//!   (DSD64 → DSD512) and transport carrier (DoP packs DSD
//!   bits into PCM frames; NativeUsb is the raw USB-DSD
//!   protocol).
//! - [`AudioFormatDecl`]: the manifest-side declaration shape.
//!   Source / delivery / composition manifests list these in
//!   their respective `[capabilities.X]` blocks; the
//!   reconciliation engine intersects declarations during
//!   topology negotiation to pick the negotiated [`AudioFormat`].
//! - [`PreferredTopology`]: source-side hint to the
//!   reconciliation engine for whether the source prefers a
//!   chain with no intermediate composition stage.
//! - [`CompositionAudioMode`]: per-mode declaration on
//!   composition plugins — passthrough / equaliser /
//!   resampler / DSD-to-PCM converter / etc. — with the
//!   `preserves_bit_perfect` invariant flag the topology
//!   validator consumes.
//!
//! Audio data NEVER traverses the wire protocol. The framework
//! configures topology (endpoints + format) via OS-native
//! primitives; bytes flow through those primitives from source
//! to delivery without ever entering evo's process. The wire
//! primitive carries control-plane streams (metering, spectrum
//! visualisation, progress) — derived from the audio data, not
//! the data itself.

use serde::{Deserialize, Serialize};

/// Negotiated audio format at one stage of the chain.
///
/// The reconciliation engine picks one [`AudioFormat`] for the
/// source → delivery chain after intersecting every plugin's
/// declared formats with the delivery target's probed hardware
/// capability. The chosen format is the contract every stage
/// honours; a stage that cannot honour it refuses admission /
/// route-change rather than silently converting.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AudioFormat {
    /// PCM frames at a specific rate, bit-depth, and channel
    /// count.
    Pcm {
        /// Bit-depth + container variant.
        codec: PcmCodec,
        /// Sample rate in Hz (e.g. 44100, 192000, 384000).
        rate_hz: u32,
        /// Channel count (1 = mono, 2 = stereo, 6 = 5.1, 8 = 7.1).
        /// At least one channel; the data plane caps at 32 to
        /// match practical USB / HDMI / I2S / AES67 hardware.
        channels: u8,
    },
    /// DSD bitstream at a specific rate, transport, and channel
    /// count.
    Dsd {
        /// DSD rate (DSD64..DSD512).
        rate: DsdRate,
        /// Transport carrier — DoP wraps DSD bits in PCM
        /// frames so it traverses generic USB-Audio Class 1
        /// interfaces; NativeUsb is the dedicated DSD-over-USB
        /// protocol most audiophile DACs implement.
        transport: DsdTransport,
        /// Channel count (typically 2; multi-channel SACD
        /// rip rare but supported).
        channels: u8,
    },
    /// Encoded-bitstream passthrough — HDMI-to-AVR, S/PDIF
    /// Dolby/DTS bitstream, etc. The chain does not decode;
    /// the downstream device does. Bit-perfect by definition
    /// (no transformation in the chain).
    EncodedPassthrough {
        /// Lowercase codec token (`"ac3"` / `"dts"` / `"flac"`
        /// / `"truehd"` / `"dts-hd-ma"`). Free-form to allow
        /// the catalogue / vendor distributions to extend
        /// without framework changes; the framework treats the
        /// string opaquely beyond round-trip equality.
        codec: String,
        /// Sample rate of the encoded stream (44100 / 48000 /
        /// 96000 / 192000).
        rate_hz: u32,
        /// Channel count after decode (the chain still
        /// reports the post-decode shape so UI displays
        /// correctly even though decode happens downstream).
        channels: u8,
        /// Data rate of the encoded stream in kilobits per
        /// second, with CBR / VBR / unknown discrimination.
        /// `None` when no producer has surfaced the value
        /// (e.g. an admission-time AudioFormat for chain
        /// negotiation — rate negotiation is sample-rate-
        /// based, not bitrate-based; the field is
        /// source-introspection metadata for UI rendering).
        ///
        /// Audiophile-grade UIs render bitrate alongside
        /// sample rate for every lossy codec (Roon, Volumio,
        /// foobar, JRiver). Empty until a producer with file-
        /// side header access (e.g. the playback warden's
        /// source probe) fills it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bitrate_kbps: Option<EncodedBitrate>,
    },
}

/// Encoded-codec data-rate descriptor, carrying the kbps value
/// and the CBR / VBR / unknown discriminant audiophile UIs
/// render alongside the sample rate for every lossy codec.
///
/// Each variant carries exactly the values a UI label needs:
///
/// - `Cbr { kbps }`           → "44.1 kHz / 320 kbps"
/// - `Vbr { avg_kbps }`       → "44.1 kHz / 245 kbps VBR"
/// - `Unknown`                → "44.1 kHz / VBR" (or codec-specific
///   fallback label when no average is recoverable)
///
/// Producers that cannot honestly determine a value use
/// `Unknown` rather than fabricating an average — matches the
/// honesty contract the framework's source-side parsers
/// already follow when a file header doesn't carry a shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EncodedBitrate {
    /// Constant bitrate. `kbps` is the file's actual data rate.
    Cbr {
        /// Bitrate in kilobits per second.
        kbps: u32,
    },
    /// Variable bitrate. `avg_kbps` is the file-declared or
    /// computed average (Xing/LAME tag for MP3, ASF File
    /// Properties for WMA, Vorbis identification header's
    /// `nominal_bitrate`, M4A esds `avgBitrate`).
    Vbr {
        /// Average bitrate in kilobits per second.
        avg_kbps: u32,
    },
    /// Variable bitrate with no recoverable average. Honest
    /// fallback for parsers that cannot compute the value
    /// from a file-head probe (Opus without a span scan,
    /// truncated VBR MP3 lacking an Xing tag).
    Unknown,
}

impl AudioFormat {
    /// Validate the framework's basic shape invariants:
    /// channels in `[1, 32]`, PCM rate above 0 Hz,
    /// encoded-passthrough codec non-empty.
    ///
    /// Returns `Ok(())` on success or a [`AudioFormatError`]
    /// naming the first invariant violated. Callers (manifest
    /// validation, wire-op handlers) propagate the error
    /// upstream as a structured admission / contract refusal.
    pub fn validate(&self) -> Result<(), AudioFormatError> {
        match self {
            AudioFormat::Pcm {
                rate_hz, channels, ..
            } => {
                if *rate_hz == 0 {
                    return Err(AudioFormatError::Invalid(
                        "pcm rate_hz must be > 0".into(),
                    ));
                }
                validate_channels(*channels)?;
            }
            AudioFormat::Dsd { channels, .. } => {
                validate_channels(*channels)?;
            }
            AudioFormat::EncodedPassthrough {
                codec,
                rate_hz,
                channels,
                bitrate_kbps,
            } => {
                if codec.trim().is_empty() {
                    return Err(AudioFormatError::Invalid(
                        "encoded_passthrough codec must not be empty".into(),
                    ));
                }
                if *rate_hz == 0 {
                    return Err(AudioFormatError::Invalid(
                        "encoded_passthrough rate_hz must be > 0".into(),
                    ));
                }
                validate_channels(*channels)?;
                if let Some(b) = bitrate_kbps {
                    validate_bitrate(b)?;
                }
            }
        }
        Ok(())
    }

    /// Returns `true` when the two formats describe the same
    /// audio shape: same kind, same codec / rate / channels.
    /// The data plane treats two formats as bit-perfect-
    /// compatible iff this returns `true` — PCM at the same
    /// (rate, bit-depth, channel layout) is the only chain
    /// shape the framework's bit-perfect validator accepts as
    /// passthrough.
    pub fn is_compatible_with(&self, other: &AudioFormat) -> bool {
        self == other
    }
}

fn validate_channels(channels: u8) -> Result<(), AudioFormatError> {
    if channels == 0 {
        return Err(AudioFormatError::Invalid("channels must be > 0".into()));
    }
    if channels > 32 {
        return Err(AudioFormatError::Invalid(format!(
            "channels {channels} exceeds the framework cap of 32"
        )));
    }
    Ok(())
}

fn validate_bitrate(b: &EncodedBitrate) -> Result<(), AudioFormatError> {
    match b {
        EncodedBitrate::Cbr { kbps } => {
            if *kbps == 0 {
                return Err(AudioFormatError::Invalid(
                    "encoded_passthrough bitrate_kbps cbr kbps must be > 0"
                        .into(),
                ));
            }
        }
        EncodedBitrate::Vbr { avg_kbps } => {
            if *avg_kbps == 0 {
                return Err(AudioFormatError::Invalid(
                    "encoded_passthrough bitrate_kbps vbr avg_kbps must be > 0"
                        .into(),
                ));
            }
        }
        EncodedBitrate::Unknown => {}
    }
    Ok(())
}

/// PCM bit-depth + container variant.
///
/// The four variants cover the framework's PCM range. Vendor
/// distributions or plugins requiring a non-listed container
/// (e.g. 20-bit packed) must add the variant via an SDK update
/// — the framework's data plane refuses formats outside this
/// set so the topology validator's bit-perfect contract is
/// definite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PcmCodec {
    /// 16-bit signed little-endian (the CD baseline; common
    /// on USB-Audio Class 1 interfaces and on-chip codecs).
    PcmS16Le,
    /// 24-bit signed little-endian packed in 24-bit frames
    /// (audiophile-grade content, no left-shift to 32-bit).
    PcmS24Le,
    /// 32-bit signed little-endian (24-bit content shifted
    /// left into 32-bit frames; the most common shape on
    /// USB-Audio Class 2 interfaces).
    PcmS32Le,
    /// 32-bit IEEE float (some professional / studio gear
    /// presents float natively; rarely the negotiated chain
    /// format because most DACs convert float to fixed
    /// internally and the conversion is lossy).
    PcmF32,
}

/// DSD rate ladder.
///
/// The framework supports the four standard DSD rates
/// audiophile content uses today; higher rates (DSD1024) are
/// rare and not in the current substrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum DsdRate {
    /// DSD64 (2.8224 MHz; the SACD baseline).
    Dsd64,
    /// DSD128 (5.6448 MHz).
    Dsd128,
    /// DSD256 (11.2896 MHz).
    Dsd256,
    /// DSD512 (22.5792 MHz; high-end audiophile DAC ceiling).
    Dsd512,
}

/// DSD transport carrier — how DSD bits reach the DAC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DsdTransport {
    /// DSD-over-PCM. DSD bits are packed inside PCM frames so
    /// the stream traverses generic USB-Audio Class
    /// interfaces. Standard for DACs without a dedicated DSD
    /// USB endpoint.
    Dop,
    /// Native USB-DSD protocol. Dedicated USB endpoint with
    /// direct DSD bit transport — the audiophile baseline for
    /// high-end DACs.
    NativeUsb,
}

/// Manifest-side audio-format declaration. Sources / delivery
/// targets / composition modes list these in their respective
/// `[capabilities.X]` blocks; the reconciliation engine
/// intersects the declarations during topology negotiation to
/// pick a single concrete [`AudioFormat`] for the chain.
///
/// Each declaration enumerates the rates the plugin supports
/// for one (codec, channels) combination. The intersection
/// algorithm pairs declarations across stages; the negotiated
/// format is the (codec, rate, channels) triple every stage in
/// the chain accepts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AudioFormatDecl {
    /// PCM declaration: one codec, one channel count, an
    /// enumerated set of supported rates.
    Pcm {
        /// Bit-depth + container variant.
        codec: PcmCodec,
        /// Supported rates in Hz. Order is informational; the
        /// reconciliation engine treats the list as a set.
        rate_hz: Vec<u32>,
        /// Channel count.
        channels: u8,
    },
    /// DSD declaration: one transport, an enumerated set of
    /// rates, one channel count.
    Dsd {
        /// Transport carrier.
        transport: DsdTransport,
        /// Supported DSD rates.
        rates: Vec<DsdRate>,
        /// Channel count.
        channels: u8,
    },
    /// Encoded-passthrough declaration: one codec token, an
    /// enumerated set of rates, one channel count.
    EncodedPassthrough {
        /// Lowercase codec token (`"ac3"` / `"dts"` / etc.).
        codec: String,
        /// Supported rates.
        rate_hz: Vec<u32>,
        /// Channel count after decode.
        channels: u8,
    },
}

impl AudioFormatDecl {
    /// Validate the declaration's basic shape: non-empty rate
    /// list, channels in `[1, 32]`, codec non-empty for
    /// encoded-passthrough.
    pub fn validate(&self) -> Result<(), AudioFormatError> {
        match self {
            AudioFormatDecl::Pcm {
                rate_hz, channels, ..
            } => {
                if rate_hz.is_empty() {
                    return Err(AudioFormatError::Invalid(
                        "pcm declaration rate_hz must list at least one rate"
                            .into(),
                    ));
                }
                if rate_hz.contains(&0) {
                    return Err(AudioFormatError::Invalid(
                        "pcm declaration rate_hz entries must be > 0".into(),
                    ));
                }
                validate_channels(*channels)?;
            }
            AudioFormatDecl::Dsd {
                rates, channels, ..
            } => {
                if rates.is_empty() {
                    return Err(AudioFormatError::Invalid(
                        "dsd declaration rates must list at least one rate"
                            .into(),
                    ));
                }
                validate_channels(*channels)?;
            }
            AudioFormatDecl::EncodedPassthrough {
                codec,
                rate_hz,
                channels,
            } => {
                if codec.trim().is_empty() {
                    return Err(AudioFormatError::Invalid(
                        "encoded_passthrough declaration codec must not be \
                         empty"
                            .into(),
                    ));
                }
                if rate_hz.is_empty() {
                    return Err(AudioFormatError::Invalid(
                        "encoded_passthrough declaration rate_hz must list at \
                         least one rate"
                            .into(),
                    ));
                }
                if rate_hz.contains(&0) {
                    return Err(AudioFormatError::Invalid(
                        "encoded_passthrough declaration rate_hz entries must \
                         be > 0"
                            .into(),
                    ));
                }
                validate_channels(*channels)?;
            }
        }
        Ok(())
    }

    /// Returns `true` when the supplied [`AudioFormat`] matches
    /// the declaration's (kind, codec, channels) shape AND the
    /// format's rate appears in the declaration's rate list.
    /// Used by the reconciliation engine's intersection
    /// algorithm.
    pub fn covers(&self, fmt: &AudioFormat) -> bool {
        match (self, fmt) {
            (
                AudioFormatDecl::Pcm {
                    codec: dc,
                    rate_hz: drs,
                    channels: dch,
                },
                AudioFormat::Pcm {
                    codec: fc,
                    rate_hz: fr,
                    channels: fch,
                },
            ) => dc == fc && dch == fch && drs.contains(fr),
            (
                AudioFormatDecl::Dsd {
                    transport: dt,
                    rates: drs,
                    channels: dch,
                },
                AudioFormat::Dsd {
                    transport: ft,
                    rate: fr,
                    channels: fch,
                },
            ) => dt == ft && dch == fch && drs.contains(fr),
            (
                AudioFormatDecl::EncodedPassthrough {
                    codec: dc,
                    rate_hz: drs,
                    channels: dch,
                },
                AudioFormat::EncodedPassthrough {
                    codec: fc,
                    rate_hz: fr,
                    channels: fch,
                    bitrate_kbps: _,
                },
            ) => dc == fc && dch == fch && drs.contains(fr),
            _ => false,
        }
    }
}

/// Source-side topology preference. Hint to the reconciliation
/// engine for whether the source plugin is best served by a
/// chain with no intermediate composition stage (the default
/// for high-resolution audiophile sources) or accepts an
/// intermediate stage when one is required.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum PreferredTopology {
    /// Source declares no preference; the topology scorer
    /// picks based on hardware profile + operator policy.
    /// Default.
    #[default]
    Any,
    /// Source prefers a chain with no composition stage when
    /// the delivery target accepts the source's format
    /// directly. Hint only — operator policy or format
    /// mismatch may override.
    NoIntermediate,
    /// Source prefers a passthrough composition stage (e.g.
    /// the source needs an ALSA loopback for buffering even
    /// when no transformation is required).
    Passthrough,
}

/// Composition-mode declaration. A composition plugin lists
/// every mode it supports; the reconciliation engine picks one
/// per topology based on the source / delivery format pair and
/// operator policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositionAudioMode {
    /// Mode name (`"passthrough"` / `"eq_only"` /
    /// `"resampled"` / `"dsd_to_pcm"` / `"upsampled"`). The
    /// framework treats the string as opaque except for
    /// `"passthrough"` which the topology scorer treats as
    /// the minimum-signal-path baseline.
    pub name: String,
    /// `true` when this mode preserves bit-perfect
    /// (passthrough; analog volume; format-preserving filter
    /// chains). The topology validator refuses chains that
    /// claim bit-perfect with a stage whose mode declares
    /// `false`.
    pub preserves_bit_perfect: bool,
    /// Optional input-format constraint. When `None`, the
    /// mode accepts any format the source produces; when set,
    /// only chains whose source format is in this list select
    /// the mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formats_in: Option<Vec<AudioFormatDecl>>,
    /// Optional output-format constraint. When `None`, the
    /// mode preserves the input format (passthrough or
    /// transparent transform); when set, the mode produces
    /// formats from this list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formats_out: Option<Vec<AudioFormatDecl>>,
}

impl CompositionAudioMode {
    /// Validate basic shape: name non-empty, every declared
    /// format in `formats_in` / `formats_out` valid in
    /// isolation. The reconciliation engine validates the
    /// chain-composition rules at topology selection time.
    pub fn validate(&self) -> Result<(), AudioFormatError> {
        if self.name.trim().is_empty() {
            return Err(AudioFormatError::Invalid(
                "composition mode name must not be empty".into(),
            ));
        }
        if let Some(decls) = &self.formats_in {
            for d in decls {
                d.validate()?;
            }
        }
        if let Some(decls) = &self.formats_out {
            for d in decls {
                d.validate()?;
            }
        }
        Ok(())
    }
}

/// Errors raised by the audio-format type validators. Wraps
/// every shape violation under a single variant — callers
/// (manifest validation, wire-op handlers) format the contained
/// message into a structured admission / contract refusal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AudioFormatError {
    /// Shape / range / coherence violation.
    #[error("audio format invalid: {0}")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_format_validates_shape() {
        let f = AudioFormat::Pcm {
            codec: PcmCodec::PcmS24Le,
            rate_hz: 192_000,
            channels: 2,
        };
        f.validate().expect("typical audiophile PCM is valid");
    }

    #[test]
    fn pcm_format_refuses_zero_rate() {
        let f = AudioFormat::Pcm {
            codec: PcmCodec::PcmS16Le,
            rate_hz: 0,
            channels: 2,
        };
        let err = f.validate().expect_err("zero rate is invalid");
        assert!(matches!(err, AudioFormatError::Invalid(_)));
    }

    #[test]
    fn channel_count_caps_at_32() {
        let f = AudioFormat::Pcm {
            codec: PcmCodec::PcmS16Le,
            rate_hz: 48_000,
            channels: 33,
        };
        let err = f.validate().expect_err("33 channels exceeds cap");
        assert!(
            matches!(err, AudioFormatError::Invalid(msg) if msg.contains("32"))
        );
    }

    #[test]
    fn dsd_format_validates_shape() {
        let f = AudioFormat::Dsd {
            rate: DsdRate::Dsd512,
            transport: DsdTransport::NativeUsb,
            channels: 2,
        };
        f.validate().expect("DSD512 native USB is valid");
    }

    #[test]
    fn encoded_passthrough_refuses_empty_codec() {
        let f = AudioFormat::EncodedPassthrough {
            codec: "".into(),
            rate_hz: 48_000,
            channels: 6,
            bitrate_kbps: None,
        };
        let err = f.validate().expect_err("empty codec token is invalid");
        assert!(
            matches!(err, AudioFormatError::Invalid(msg) if msg.contains("codec"))
        );
    }

    #[test]
    fn pcm_format_round_trips_through_serde() {
        let original = AudioFormat::Pcm {
            codec: PcmCodec::PcmS32Le,
            rate_hz: 384_000,
            channels: 2,
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: AudioFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn pcm_codec_serialises_as_snake_case() {
        let codec = PcmCodec::PcmS24Le;
        let json = serde_json::to_string(&codec).unwrap();
        assert_eq!(json, "\"pcm_s24_le\"");
    }

    #[test]
    fn dsd_rate_serialises_as_uppercase() {
        let rate = DsdRate::Dsd256;
        let json = serde_json::to_string(&rate).unwrap();
        assert_eq!(json, "\"DSD256\"");
    }

    #[test]
    fn declaration_validates_non_empty_rates() {
        let bad = AudioFormatDecl::Pcm {
            codec: PcmCodec::PcmS16Le,
            rate_hz: vec![],
            channels: 2,
        };
        let err = bad.validate().expect_err("empty rate list is invalid");
        assert!(
            matches!(err, AudioFormatError::Invalid(msg) if msg.contains("rate_hz"))
        );
    }

    #[test]
    fn declaration_covers_concrete_format() {
        let decl = AudioFormatDecl::Pcm {
            codec: PcmCodec::PcmS24Le,
            rate_hz: vec![44_100, 48_000, 96_000, 192_000],
            channels: 2,
        };
        let fmt = AudioFormat::Pcm {
            codec: PcmCodec::PcmS24Le,
            rate_hz: 96_000,
            channels: 2,
        };
        assert!(decl.covers(&fmt), "declaration must cover an in-list rate");

        let off_rate = AudioFormat::Pcm {
            codec: PcmCodec::PcmS24Le,
            rate_hz: 88_200,
            channels: 2,
        };
        assert!(
            !decl.covers(&off_rate),
            "declaration must not cover an out-of-list rate"
        );

        let off_codec = AudioFormat::Pcm {
            codec: PcmCodec::PcmS16Le,
            rate_hz: 96_000,
            channels: 2,
        };
        assert!(
            !decl.covers(&off_codec),
            "declaration must not cover a different codec"
        );
    }

    #[test]
    fn composition_mode_validates_name() {
        let bad = CompositionAudioMode {
            name: "".into(),
            preserves_bit_perfect: true,
            formats_in: None,
            formats_out: None,
        };
        let err = bad.validate().expect_err("empty name is invalid");
        assert!(
            matches!(err, AudioFormatError::Invalid(msg) if msg.contains("name"))
        );
    }

    #[test]
    fn composition_mode_round_trips_through_toml() {
        let mode = CompositionAudioMode {
            name: "passthrough".into(),
            preserves_bit_perfect: true,
            formats_in: None,
            formats_out: None,
        };
        let toml = toml::to_string(&mode).unwrap();
        let parsed: CompositionAudioMode = toml::from_str(&toml).unwrap();
        assert_eq!(parsed, mode);
    }

    #[test]
    fn preferred_topology_default_is_any() {
        let p: PreferredTopology = Default::default();
        assert_eq!(p, PreferredTopology::Any);
    }
}
