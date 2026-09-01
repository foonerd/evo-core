// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

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

    /// The manifest's `[dependencies]` section listed the plugin's
    /// own canonical name in `required` / `recommended` /
    /// `conflicts_with`. Self-dependency is structurally
    /// nonsensical and refuses validation.
    #[error("manifest declares self-dependency on {plugin_name:?} in {list}")]
    SelfDependency {
        /// The plugin's own name.
        plugin_name: String,
        /// Which list the self-reference appeared in
        /// (`"required"` / `"recommended"` /
        /// `"conflicts_with"`).
        list: &'static str,
    },

    /// The manifest's `[dependencies]` section declared a version
    /// requirement string that does not parse as a semver
    /// requirement.
    #[error(
        "manifest dependency on {plugin_name:?} declares unparseable \
         version requirement {requirement:?}: {detail}"
    )]
    InvalidDependencyVersion {
        /// Canonical name of the dependency entry.
        plugin_name: String,
        /// The raw requirement string that failed to parse.
        requirement: String,
        /// Parse-error detail.
        detail: String,
    },

    /// The manifest's `[dependencies]` section declared an empty
    /// canonical name in one of its lists.
    #[error("manifest dependency entry has empty plugin_name in {list}")]
    EmptyDependencyName {
        /// Which list the empty entry appeared in.
        list: &'static str,
    },

    /// The manifest's `[[ui.stocks]]` array contained an entry that
    /// failed SDK-level shape validation (empty shelf id, empty
    /// widget kind, or schema_version < 1). Registry-level
    /// validation (does the shelf exist, is the widget kind known,
    /// is the size within envelope, does cardinality permit a
    /// further stocking) happens at admission against the
    /// framework's runtime registries, not here.
    #[error("manifest [[ui.stocks]] entry {index}: invalid {field}: {detail}")]
    InvalidUiStocking {
        /// Zero-based index of the offending entry within the
        /// `[[ui.stocks]]` array.
        index: usize,
        /// Field that failed (`"ui_shelf"`, `"widget"`,
        /// `"schema_version"`).
        field: &'static str,
        /// Human-readable detail.
        detail: String,
    },

    /// The manifest declared `plugin.kind` for a UI artefact
    /// shape (`theme`, `ui_shell`, or `widget_kind_pack`) but
    /// did NOT include the matching `[<kind>]` section
    /// describing the artefact payload.
    #[error("manifest plugin.kind = {kind} requires the [{section}] section")]
    MissingArtefactSection {
        /// The `plugin.kind` value the manifest declared.
        kind: &'static str,
        /// The manifest section that was expected
        /// (`"theme"` / `"ui_shell"` / `"widgets"`).
        section: &'static str,
    },

    /// The manifest declared a UI artefact section
    /// (`[theme]`, `[ui_shell]`, `[widgets]`) that does NOT
    /// match the manifest's `plugin.kind`. Functional
    /// plugins MUST NOT carry artefact sections; artefact
    /// kinds must carry only their own section.
    #[error(
        "manifest plugin.kind = {kind} cannot carry [{section}] \
         section"
    )]
    UnexpectedArtefactSection {
        /// The `plugin.kind` value the manifest declared.
        kind: &'static str,
        /// The unexpected manifest section
        /// (`"theme"` / `"ui_shell"` / `"widgets"`).
        section: &'static str,
    },

    /// The manifest declared BOTH the legacy `[target]` block AND
    /// the multi-stocking `[[stockings]]` array. The two file forms
    /// are mutually exclusive; the plugin author must choose one
    /// shape per the Stocking primitive contract.
    #[error(
        "manifest declares both [target] and [[stockings]]; choose \
         exactly one form (legacy single-stocking [target] OR \
         multi-stocking [[stockings]] per the Stocking primitive)"
    )]
    MixedStockingForms,

    /// The manifest declared neither `[target]` nor `[[stockings]]`.
    /// Every functional plugin must declare at least one shelf
    /// occupancy.
    #[error(
        "manifest declares no shelf occupancy: either [target] or \
         [[stockings]] is required for functional plugins"
    )]
    NoStocking,

    /// A `[[stockings]]` entry declares a role inconsistent with
    /// the plugin's `[kind].interaction`. A `warden` stocking
    /// requires the plugin's interaction to be `warden`; a
    /// `respondent` stocking is permitted on `warden` plugins too
    /// (a warden plugin may stock respondent surfaces on adjacent
    /// shelves whose substrate it owns).
    #[error(
        "manifest [[stockings]] entry {index} on shelf {shelf:?}: \
         role {stocking_role:?} is incompatible with plugin \
         interaction {plugin_interaction:?}"
    )]
    StockingRoleMismatch {
        /// Zero-based index of the offending stocking.
        index: usize,
        /// The shelf the offending stocking names.
        shelf: String,
        /// The stocking's declared role.
        stocking_role: &'static str,
        /// The plugin's declared interaction.
        plugin_interaction: &'static str,
    },

    /// Two `[[stockings]]` entries name the same shelf. A plugin
    /// stocks each of its shelves exactly once.
    #[error(
        "manifest declares stockings on shelf {shelf:?} more than \
         once; each stocking occupies one distinct shelf"
    )]
    StockingShelfDuplicate {
        /// The duplicated shelf name.
        shelf: String,
    },

    /// A `[[stockings]]` entry declares a `request_types` entry
    /// that is not present in the plugin's
    /// `capabilities.respondent.request_types` set. Every stocked
    /// verb must first be declared at the plugin level.
    #[error(
        "manifest [[stockings]] entry {index} on shelf {shelf:?} \
         declares verb {verb:?} that is not in \
         capabilities.respondent.request_types"
    )]
    StockingVerbNotDeclared {
        /// Zero-based index of the offending stocking.
        index: usize,
        /// The shelf the offending stocking names.
        shelf: String,
        /// The verb that is not declared at the plugin level.
        verb: String,
    },

    /// One verb appears in two different stockings' `request_types`
    /// arrays. The dispatcher routes each verb via the stocking
    /// whose shelf the request names; a verb stocked twice has
    /// ambiguous routing.
    #[error(
        "manifest declares verb {verb:?} in two stockings; each \
         verb belongs to exactly one stocking"
    )]
    StockingVerbOverlap {
        /// The verb that appears in two stockings.
        verb: String,
    },

    /// The plugin's `capabilities.respondent.request_types`
    /// declares verbs that no stocking carries. Every declared
    /// verb must be stocked on exactly one shelf so the dispatcher
    /// has a routing target.
    #[error(
        "manifest declares respondent verbs {verbs:?} that no \
         stocking carries; every verb must be partitioned across \
         the [[stockings]] entries"
    )]
    StockingVerbsUnstocked {
        /// The verbs left unstocked.
        verbs: Vec<String>,
    },

    /// The manifest declared `plugin.kind = "functional"`
    /// (the historical executable plugin shape) but omitted
    /// one of the four sections that an executable plugin
    /// requires: `[kind]` / `[transport]` / `[resources]` /
    /// `[lifecycle]`.
    #[error(
        "manifest plugin.kind = {kind} requires the \
         [{section}] section"
    )]
    MissingExecutableSection {
        /// The `plugin.kind` value the manifest declared
        /// (always `"functional"` today; future executable
        /// kinds may share the same constraint).
        kind: &'static str,
        /// The missing manifest section (`"kind"` /
        /// `"transport"` / `"resources"` / `"lifecycle"`).
        section: &'static str,
    },

    /// The manifest declared `plugin.kind` for a UI
    /// artefact (`theme`, `ui_shell`, or `widget_kind_pack`)
    /// but ALSO carried one of the executable-only sections
    /// (`[kind]` / `[transport]` / `[resources]` /
    /// `[lifecycle]`). Artefacts have no instance shape, no
    /// transport, no resource ceiling, and no lifecycle
    /// policy: they are static asset bundles loaded by the
    /// artefact registries rather than dispatched to.
    #[error(
        "manifest plugin.kind = {kind} cannot carry [{section}] \
         section"
    )]
    UnexpectedExecutableSection {
        /// The `plugin.kind` value the manifest declared.
        kind: &'static str,
        /// The unexpected manifest section (`"kind"` /
        /// `"transport"` / `"resources"` / `"lifecycle"`).
        section: &'static str,
    },
}
