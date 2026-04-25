//! Error types for the evo plugin SDK.
//!
//! At this stage only manifest-related errors are defined. Wire-protocol and
//! contract errors are added as those subsystems land.

use thiserror::Error;

/// Errors produced when parsing, validating, or serialising a plugin manifest.
///
/// Every variant carries either a wrapped parser error or a descriptive
/// message naming the offending field. Variants are `non_exhaustive` so
/// additive refinements to manifest validation do not constitute breaking
/// changes to this enum.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ManifestError {
    /// The TOML input could not be parsed.
    #[error("manifest TOML parse error: {0}")]
    ParseError(#[from] toml::de::Error),

    /// The TOML could not be serialised back to a string.
    #[error("manifest TOML serialisation error: {0}")]
    SerializeError(#[from] toml::ser::Error),

    /// The manifest parsed as TOML but violated the documented schema.
    /// The wrapped string names the offending section and field.
    #[error("manifest schema violation: {0}")]
    SchemaViolation(String),

    /// The manifest declared a plugin-contract version the SDK does not
    /// support. The u32 is the version the manifest requested.
    #[error("unsupported plugin contract version: {0}")]
    UnsupportedContractVersion(u32),

    /// The manifest's `plugin.name` field did not match the required
    /// reverse-DNS pattern. The string is the offending name.
    #[error("invalid plugin name: {0}")]
    InvalidName(String),

    /// The manifest declared `kind` fields that are inconsistent with the
    /// capability sections present. For example, `interaction = "warden"`
    /// without a `[capabilities.warden]` table.
    #[error("inconsistent capabilities: {0}")]
    InconsistentCapabilities(String),

    /// The manifest's `prerequisites.evo_min_version` is strictly greater
    /// than the evo steward's own version. The plugin requires a newer
    /// framework than is running.
    #[error("manifest requires evo >= {required}, running {running}")]
    EvoVersionTooLow {
        /// Minimum evo version the manifest declares.
        required: semver::Version,
        /// Version of the running evo steward.
        running: semver::Version,
    },

    /// The manifest's `prerequisites.os_family` is neither `"any"` nor
    /// a match for the host operating system (see `std::env::consts::OS`).
    #[error(
        "manifest requires os_family = {required:?}, running on {running:?}"
    )]
    OsFamilyMismatch {
        /// OS family string declared by the manifest.
        required: String,
        /// OS family string of the running host.
        running: String,
    },
}
