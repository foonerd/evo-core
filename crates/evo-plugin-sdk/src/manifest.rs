// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Plugin manifest types.
//!
//! The types in this module mirror the manifest schema documented in
//! `docs/engineering/PLUGIN_PACKAGING.md` section 2. Every plugin ships with a
//! `manifest.toml` that deserialises into [`Manifest`]; the steward validates
//! the resulting value before admitting the plugin.
//!
//! ## Validation layers
//!
//! Manifest validation happens in three layers:
//!
//! 1. **TOML parse**: Handled by `toml` + `serde`. Missing required fields and
//!    malformed types are rejected here as [`ManifestError::ParseError`].
//! 2. **Schema validation**: [`Manifest::validate`] checks constraints that
//!    cannot be expressed in the type system alone: reverse-DNS name format,
//!    supported contract version, capability-vs-kind consistency.
//! 3. **Shelf-shape validation**: Performed by the steward, not by this SDK.
//!    The steward loads the published shelf-shape schema for the manifest's
//!    `target.shelf` and validates additional domain-specific constraints
//!    declared in that schema. This SDK does not know about specific shelves.
//!
//! [`Manifest::from_toml`] performs layers 1 and 2 in sequence.

use crate::error::ManifestError;
use crate::ui::UiStocking;
use once_cell::sync::Lazy;
use regex::Regex;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The plugin contract version this SDK supports.
///
/// The SDK admits manifests declaring `plugin.contract = 1`. Manifests
/// declaring any other value are rejected with
/// [`ManifestError::UnsupportedContractVersion`].
pub const SUPPORTED_CONTRACT_VERSION: u32 = 1;

/// Default Fast Path per-warden dispatch budget in milliseconds.
///
/// Used by [`WardenCapabilities::fast_path_budget_ms`] when the
/// manifest leaves the field unset. Bounds the steward's
/// dispatch + serialise overhead plus the warden's
/// `course_correct` execution on the Fast Path channel; calls
/// exceeding budget refuse with the structured
/// `unavailable / fast_path_budget_exceeded` error taxonomy.
pub const FAST_PATH_BUDGET_MS_DEFAULT: u32 = 50;

/// Maximum Fast Path per-warden dispatch budget the framework
/// will accept.
///
/// Manifests declaring a higher value are clamped at admission
/// to this maximum. The cap exists to keep Fast Path's
/// latency-bounded contract honest: a warden declaring a 5-second
/// budget would defeat the whole point of the channel. 200ms is
/// the bound documented in the design — operator-finger ↔
/// device-feedback loops with a <50ms-tactile-latency
/// expectation tolerate occasional outliers up to roughly 4x
/// before the channel's distinct-from-slow-path guarantee
/// becomes meaningless.
pub const FAST_PATH_BUDGET_MS_MAX: u32 = 200;

/// Regex matching a valid plugin canonical name.
///
/// The pattern is taken verbatim from `PLUGIN_PACKAGING.md` section 4:
/// `^[a-z][a-z0-9]*(\.[a-z][a-z0-9-]*)+$`.
static NAME_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^[a-z][a-z0-9]*(\.[a-z][a-z0-9-]*)+$")
        .expect("plugin name regex must compile")
});

/// A complete plugin manifest, modelling every section of the schema in
/// `PLUGIN_PACKAGING.md` section 2.
// `Eq` is not implemented because `UiSection`'s stockings carry
// `toml::Value` parameters whose `Float` variant is not `Eq`.
// `PartialEq` is sufficient for equality comparisons; downstream
// callers do not key on `Manifest` in HashMap/HashSet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    /// Identity and version of the plugin.
    pub plugin: Plugin,
    /// The shelf this plugin targets (legacy single-stocking form).
    ///
    /// Populated either from the file's `[target]` block or
    /// synthesized from the primary stocking when the file declares
    /// `[[stockings]]`. Always non-sentinel post-normalisation; the
    /// `[target]` block is permitted to be absent in the file when
    /// `[[stockings]]` is present.
    ///
    /// **Serialised form.** `to_toml` emits the canonical
    /// `[[stockings]]` form for every manifest; the `[target]`
    /// block is input-only and is not re-emitted. Round-trip
    /// (`from_toml` → `to_toml` → `from_toml`) is preserved because
    /// `normalize_stockings` re-derives this field deterministically
    /// from the primary stocking on every parse. Legacy
    /// hand-written manifests using `[target]` continue to admit
    /// unchanged; tools that round-trip a manifest through the SDK
    /// will produce `[[stockings]]` on output.
    #[serde(default, skip_serializing)]
    pub target: Target,
    /// Multi-stocking declaration: this plugin stocks one or more
    /// shelves with per-shelf shape, role, and verb partition. See
    /// `PLUGIN_PACKAGING.md` §2 "Multi-stocking plugins" and
    /// See the Stocking primitive for the contract.
    ///
    /// **Bucket: Enforced.** When the file declares `[target]` only,
    /// [`Manifest::normalize_stockings`] synthesizes a one-element
    /// `stockings` vector covering the plugin's entire request_types
    /// set under a role derived from `[kind].interaction`. When the
    /// file declares `[[stockings]]` only, the parser populates
    /// `target` from the primary stocking. The file forms are
    /// mutually exclusive; a manifest carrying both raises
    /// [`ManifestError::MixedStockingForms`]. A manifest carrying
    /// neither raises [`ManifestError::NoStocking`]. Post-normalise
    /// every manifest has at least one stocking.
    #[serde(default)]
    pub stockings: Vec<Stocking>,
    /// The plugin's instance and interaction shapes.
    ///
    /// Required for [`ArtefactKind::Functional`] plugins —
    /// the historical admission-typed plugin shape that
    /// declares an interaction model the steward dispatches
    /// to. Forbidden for UI artefact kinds
    /// ([`ArtefactKind::Theme`] / [`ArtefactKind::UiShell`] /
    /// [`ArtefactKind::WidgetKindPack`]) — those render
    /// rather than execute and have no respondent / warden
    /// shape. Validation enforces presence-by-kind in
    /// [`Manifest::validate`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
    /// How the steward loads this plugin.
    ///
    /// Required for [`ArtefactKind::Functional`]; forbidden
    /// for UI artefact kinds (artefact bundles are loaded
    /// from the plugin directory by the artefact registries,
    /// not via the in-process / out-of-process transport
    /// machinery).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
    /// Declared trust class.
    pub trust: Trust,
    /// Environmental prerequisites for admission.
    pub prerequisites: Prerequisites,
    /// Declared resource ceilings.
    ///
    /// Required for [`ArtefactKind::Functional`]; forbidden
    /// for UI artefact kinds (artefacts are static asset
    /// bundles with no per-instance resource budget).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
    /// Lifecycle policy (hot reload, restart, autostart).
    ///
    /// Required for [`ArtefactKind::Functional`]; forbidden
    /// for UI artefact kinds (artefact lifecycle is driven
    /// by activation verbs in sub-primitive C, not by the
    /// hot-reload / autostart / restart-on-crash policy
    /// applicable to executable plugins).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<Lifecycle>,
    /// Kind-specific capability declarations.
    ///
    /// The sub-tables populated here must be consistent with [`Kind`]: a
    /// warden plugin must populate `warden`, a factory must populate
    /// `factory`, a respondent must populate `respondent`. Consistency is
    /// checked by [`Manifest::validate`].
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Inter-plugin dependency declarations. Empty by default
    /// (no required, no recommended, no conflicts). When
    /// populated, the framework's admission gate refuses to
    /// admit a plugin whose `required` dependencies are not
    /// already admitted, and refuses to admit a plugin whose
    /// `conflicts_with` set has an admitted member.
    #[serde(default)]
    pub dependencies: Dependencies,

    /// Explicit UI stockings. Empty by default — most plugins
    /// rely on the convergence default that auto-derives a
    /// stocking from the catalogue [`Target`]. Plugins declare
    /// `[[ui.stocks]]` only when they need surfaces beyond the
    /// convergence default, non-default widget kinds /
    /// parameters, or shelves the catalogue model does not
    /// cover (`now_playing.context`, `prompts.active`).
    ///
    /// SDK-level validation enforces field shape only (each
    /// stocking has a non-empty shelf id, a non-empty widget
    /// kind, and a `schema_version >= 1`). Registry-level
    /// validation — does the shelf exist, is the widget kind
    /// admissible, is the size within envelope, does
    /// cardinality permit a further stocking — happens at
    /// admission against the framework's runtime registries,
    /// not here.
    #[serde(default)]
    pub ui: UiSection,
    /// Theme artefact section. Required when
    /// `plugin.kind = "theme"`; refused when present on
    /// any other artefact kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<ThemeSection>,
    /// UI shell artefact section. Required when
    /// `plugin.kind = "ui_shell"`; refused when present on
    /// any other artefact kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_shell: Option<UiShellSection>,
    /// Widget kind pack artefact section. Required when
    /// `plugin.kind = "widget_kind_pack"`; refused when
    /// present on any other artefact kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widgets: Option<WidgetKindPackSection>,
}

impl Manifest {
    /// Parse a manifest from a TOML string and fully validate it.
    ///
    /// Performs layers 1 and 2 of the validation cascade documented at module
    /// level: TOML parsing followed by schema validation.
    pub fn from_toml(input: &str) -> Result<Self, ManifestError> {
        // Per LOGGING.md §2 ("individual message parse steps" fire at
        // trace): manifest parse step. Trace-level so it doesn't
        // pollute info/debug streams during normal admission.
        tracing::trace!(
            input_bytes = input.len(),
            "manifest: from_toml parsing"
        );
        let mut manifest: Manifest = toml::from_str(input)?;
        manifest.normalize_stockings()?;
        manifest.validate()?;
        tracing::trace!(
            plugin = %manifest.plugin.name,
            stockings = manifest.stockings.len(),
            "manifest: from_toml parsed"
        );
        Ok(manifest)
    }

    /// Normalise the manifest's shelf-occupancy declaration. After
    /// parsing the TOML, the manifest may carry either the legacy
    /// `[target]` block, the new `[[stockings]]` array, or
    /// (erroneously) both or neither. This method:
    ///
    /// - Refuses a manifest declaring BOTH forms with
    ///   [`ManifestError::MixedStockingForms`].
    /// - Refuses a functional manifest declaring NEITHER form with
    ///   [`ManifestError::NoStocking`].
    /// - Synthesizes the missing form when only one is declared, so
    ///   the post-normalisation manifest carries BOTH a canonical
    ///   `target` (the primary stocking's shelf + shape) AND a
    ///   non-empty `stockings` Vec (the canonical multi-stocking
    ///   view).
    ///
    /// For UI artefact kinds (`Theme` / `UiShell` / `WidgetKindPack`)
    /// neither block is required — artefact manifests stock UI
    /// shelves via `[[ui.stocks]]` instead and may omit `[target]`
    /// and `[[stockings]]` entirely. This method is a no-op on UI
    /// artefact manifests.
    ///
    /// The primary-stocking selector prefers a stocking whose `role`
    /// is `Warden` (a plugin whose `[kind].interaction = warden`
    /// has its primary surface on the warden shelf); if no warden
    /// stocking is present, the first stocking in declaration order
    /// is the primary.
    fn normalize_stockings(&mut self) -> Result<(), ManifestError> {
        // UI artefact manifests do not declare a data-plane stocking.
        if self.plugin.kind != ArtefactKind::Functional {
            return Ok(());
        }

        let has_legacy_target = !self.target.is_sentinel();
        let has_stockings = !self.stockings.is_empty();

        match (has_legacy_target, has_stockings) {
            (true, true) => Err(ManifestError::MixedStockingForms),
            (false, false) => Err(ManifestError::NoStocking),
            (true, false) => {
                // Legacy single-stocking form: synthesize a
                // one-element stockings vector from [target]. The
                // role is derived from [kind].interaction; the verb
                // set is the plugin's full
                // capabilities.respondent.request_types (every verb
                // dispatches via the single shelf).
                let role = match self.kind.as_ref() {
                    Some(k) => StockingRole::from_interaction(k.interaction),
                    None => StockingRole::Respondent,
                };
                let request_types = self
                    .capabilities
                    .respondent
                    .as_ref()
                    .map(|r| r.request_types.clone())
                    .unwrap_or_default();
                self.stockings.push(Stocking {
                    shelf: self.target.shelf.clone(),
                    shape: self.target.shape,
                    role,
                    request_types,
                });
                Ok(())
            }
            (false, true) => {
                // Multi-stocking form: synthesize target from the
                // primary stocking (warden role preferred, else
                // first stocking).
                let primary_idx = self
                    .stockings
                    .iter()
                    .position(|s| s.role == StockingRole::Warden)
                    .unwrap_or(0);
                self.target = Target {
                    shelf: self.stockings[primary_idx].shelf.clone(),
                    shape: self.stockings[primary_idx].shape,
                };
                Ok(())
            }
        }
    }

    /// Validate the stocking partition discipline against the
    /// plugin's `[kind]` and `capabilities.respondent.request_types`:
    ///
    /// - Every stocking's `role` is consistent with
    ///   `[kind].interaction`.
    /// - Each shelf appears in at most one stocking
    ///   (no intra-plugin shelf duplication).
    /// - Every stocking's `request_types` is a subset of the
    ///   plugin's full `capabilities.respondent.request_types`.
    /// - The union of every stocking's `request_types` equals the
    ///   plugin's full set (no verb is unstocked).
    /// - The intersection across any pair of stockings is empty
    ///   (no verb stocked by two shelves).
    ///
    /// Single-stocking manifests synthesized from the legacy
    /// `[target]` form trivially satisfy every partition invariant
    /// (one stocking owns every verb). Multi-stocking manifests
    /// must explicitly partition.
    fn validate_stocking_partition(&self) -> Result<(), ManifestError> {
        if self.plugin.kind != ArtefactKind::Functional {
            return Ok(());
        }
        if self.stockings.is_empty() {
            // normalize_stockings raises NoStocking BEFORE this
            // path runs on functional manifests; defensive guard.
            return Err(ManifestError::NoStocking);
        }

        // Role consistency.
        if let Some(kind) = self.kind.as_ref() {
            for (idx, stocking) in self.stockings.iter().enumerate() {
                if !stocking
                    .role
                    .is_compatible_with_interaction(kind.interaction)
                {
                    return Err(ManifestError::StockingRoleMismatch {
                        index: idx,
                        shelf: stocking.shelf.clone(),
                        stocking_role: stocking.role.as_str(),
                        plugin_interaction: match kind.interaction {
                            InteractionShape::Respondent => "respondent",
                            InteractionShape::Warden => "warden",
                        },
                    });
                }
            }
        }

        // Intra-plugin shelf uniqueness.
        let mut seen_shelves: std::collections::BTreeSet<&str> =
            std::collections::BTreeSet::new();
        for stocking in &self.stockings {
            if !seen_shelves.insert(stocking.shelf.as_str()) {
                return Err(ManifestError::StockingShelfDuplicate {
                    shelf: stocking.shelf.clone(),
                });
            }
        }

        // Partition against capabilities.respondent.request_types.
        let plugin_verbs: std::collections::BTreeSet<&str> = self
            .capabilities
            .respondent
            .as_ref()
            .map(|r| r.request_types.iter().map(String::as_str).collect())
            .unwrap_or_default();
        let mut stocked_verbs: std::collections::BTreeSet<&str> =
            std::collections::BTreeSet::new();
        for (idx, stocking) in self.stockings.iter().enumerate() {
            for verb in &stocking.request_types {
                // Subset check.
                if !plugin_verbs.contains(verb.as_str()) {
                    return Err(ManifestError::StockingVerbNotDeclared {
                        index: idx,
                        shelf: stocking.shelf.clone(),
                        verb: verb.clone(),
                    });
                }
                // Empty-intersection check (no verb in two stockings).
                if !stocked_verbs.insert(verb.as_str()) {
                    return Err(ManifestError::StockingVerbOverlap {
                        verb: verb.clone(),
                    });
                }
            }
        }
        // Union-equality check (no verb left unstocked) — only when
        // the plugin declares respondent verbs at all. A
        // capabilities.respondent set may be empty for warden-only
        // plugins whose primary surface is custody verbs (not
        // respondent-shape request_types). In that case
        // plugin_verbs.is_empty() AND stocked_verbs.is_empty()
        // trivially partition.
        if plugin_verbs != stocked_verbs {
            let unstocked: Vec<String> = plugin_verbs
                .difference(&stocked_verbs)
                .map(|s| (*s).to_string())
                .collect();
            return Err(ManifestError::StockingVerbsUnstocked {
                verbs: unstocked,
            });
        }
        Ok(())
    }

    /// Serialise this manifest as a TOML string.
    ///
    /// Does not re-validate. Callers that mutate a manifest in memory and
    /// want to ensure the output is still valid should call [`validate`]
    /// before serialising.
    ///
    /// [`validate`]: Manifest::validate
    pub fn to_toml(&self) -> Result<String, ManifestError> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Validate this manifest against the schema-level constraints that
    /// cannot be expressed as serde-level types.
    ///
    /// Checks:
    /// - `plugin.name` matches the reverse-DNS regex.
    /// - `plugin.contract` equals [`SUPPORTED_CONTRACT_VERSION`].
    /// - The four executable-only sections (`[kind]` / `[transport]` /
    ///   `[resources]` / `[lifecycle]`) are present iff
    ///   `plugin.kind = "functional"` and absent iff
    ///   `plugin.kind` is one of the UI artefact variants.
    /// - The artefact section matching `plugin.kind`
    ///   (`[theme]` / `[ui_shell]` / `[widgets]`) is present
    ///   when the kind is an artefact, absent for functional.
    /// - `capabilities` sub-tables are consistent with `kind`
    ///   (functional plugins only — artefacts have no
    ///   capability sub-tables).
    pub fn validate(&self) -> Result<(), ManifestError> {
        if !NAME_REGEX.is_match(&self.plugin.name) {
            return Err(ManifestError::InvalidName(self.plugin.name.clone()));
        }

        if self.plugin.contract != SUPPORTED_CONTRACT_VERSION {
            return Err(ManifestError::UnsupportedContractVersion(
                self.plugin.contract,
            ));
        }

        self.validate_executable_sections()?;
        self.validate_artefact_sections()?;

        if self.plugin.kind == ArtefactKind::Functional {
            // Capabilities are only meaningful against an
            // executable plugin's `[kind]`; artefact manifests
            // declare no capability sub-tables and the
            // executable-section check above has already
            // confirmed `[kind]` is present.
            let kind = self.kind.as_ref().expect(
                "functional manifest passes executable-section \
                 check with [kind] populated",
            );
            self.capabilities.validate(kind)?;
        }

        self.dependencies.validate(&self.plugin.name)?;
        self.ui.validate()?;
        self.validate_stocking_partition()?;

        Ok(())
    }

    /// Enforce presence-by-kind for the four executable-only
    /// sections (`[kind]` / `[transport]` / `[resources]` /
    /// `[lifecycle]`).
    ///
    /// Functional plugins MUST declare all four. UI artefact
    /// plugins MUST declare none of them — themes, shells,
    /// and widget-kind packs are static asset bundles that
    /// carry no instance / interaction shape, no transport,
    /// no resource ceiling, and no lifecycle policy.
    fn validate_executable_sections(&self) -> Result<(), ManifestError> {
        let kind = self.plugin.kind;
        let kind_present = self.kind.is_some();
        let transport_present = self.transport.is_some();
        let resources_present = self.resources.is_some();
        let lifecycle_present = self.lifecycle.is_some();

        match kind {
            ArtefactKind::Functional => {
                if !kind_present {
                    return Err(ManifestError::MissingExecutableSection {
                        kind: kind.as_str(),
                        section: "kind",
                    });
                }
                if !transport_present {
                    return Err(ManifestError::MissingExecutableSection {
                        kind: kind.as_str(),
                        section: "transport",
                    });
                }
                if !resources_present {
                    return Err(ManifestError::MissingExecutableSection {
                        kind: kind.as_str(),
                        section: "resources",
                    });
                }
                if !lifecycle_present {
                    return Err(ManifestError::MissingExecutableSection {
                        kind: kind.as_str(),
                        section: "lifecycle",
                    });
                }
            }
            ArtefactKind::Theme
            | ArtefactKind::UiShell
            | ArtefactKind::WidgetKindPack => {
                if kind_present {
                    return Err(ManifestError::UnexpectedExecutableSection {
                        kind: kind.as_str(),
                        section: "kind",
                    });
                }
                if transport_present {
                    return Err(ManifestError::UnexpectedExecutableSection {
                        kind: kind.as_str(),
                        section: "transport",
                    });
                }
                if resources_present {
                    return Err(ManifestError::UnexpectedExecutableSection {
                        kind: kind.as_str(),
                        section: "resources",
                    });
                }
                if lifecycle_present {
                    return Err(ManifestError::UnexpectedExecutableSection {
                        kind: kind.as_str(),
                        section: "lifecycle",
                    });
                }
            }
        }
        Ok(())
    }

    /// Borrow the `[kind]` section, panicking if the manifest
    /// is not a functional plugin.
    ///
    /// Convenience for code paths that have already filtered
    /// out artefact-kind manifests (typically via the
    /// `refuse_non_functional` guard at admission entry). The
    /// SDK never calls this; the framework's functional
    /// admission paths do, having already validated the kind
    /// at entry.
    ///
    /// # Panics
    ///
    /// Panics if `plugin.kind` is not
    /// [`ArtefactKind::Functional`]. Use [`Self::kind`]
    /// (returns `Option<&Kind>`) on code paths that may
    /// receive either functional or artefact manifests.
    pub fn require_kind(&self) -> &Kind {
        self.kind.as_ref().unwrap_or_else(|| {
            panic!(
                "require_kind on artefact manifest {:?} (kind = {:?})",
                self.plugin.name, self.plugin.kind
            )
        })
    }

    /// Borrow the `[transport]` section, panicking on
    /// artefact manifests. See [`Self::require_kind`].
    pub fn require_transport(&self) -> &Transport {
        self.transport.as_ref().unwrap_or_else(|| {
            panic!(
                "require_transport on artefact manifest {:?} (kind = {:?})",
                self.plugin.name, self.plugin.kind
            )
        })
    }

    /// Borrow the `[resources]` section, panicking on
    /// artefact manifests. See [`Self::require_kind`].
    pub fn require_resources(&self) -> &Resources {
        self.resources.as_ref().unwrap_or_else(|| {
            panic!(
                "require_resources on artefact manifest {:?} (kind = {:?})",
                self.plugin.name, self.plugin.kind
            )
        })
    }

    /// Borrow the `[lifecycle]` section, panicking on
    /// artefact manifests. See [`Self::require_kind`].
    pub fn require_lifecycle(&self) -> &Lifecycle {
        self.lifecycle.as_ref().unwrap_or_else(|| {
            panic!(
                "require_lifecycle on artefact manifest {:?} (kind = {:?})",
                self.plugin.name, self.plugin.kind
            )
        })
    }

    /// Mutably borrow the `[lifecycle]` section, panicking
    /// on artefact manifests. See [`Self::require_kind`].
    pub fn require_lifecycle_mut(&mut self) -> &mut Lifecycle {
        let kind = self.plugin.kind;
        let plugin_name = self.plugin.name.clone();
        self.lifecycle.as_mut().unwrap_or_else(|| {
            panic!(
                "require_lifecycle_mut on artefact manifest {plugin_name:?} \
                 (kind = {kind:?})"
            )
        })
    }

    /// Enforce the artefact-kind / artefact-section
    /// invariant: when `plugin.kind` is one of the
    /// artefact variants ([`ArtefactKind::Theme`] /
    /// [`ArtefactKind::UiShell`] /
    /// [`ArtefactKind::WidgetKindPack`]), the matching
    /// per-kind section MUST be present; functional
    /// plugins MUST NOT declare any artefact section. The
    /// admission gate refuses on either violation with a
    /// structured diagnostic so the operator sees the
    /// specific shape error rather than a downstream
    /// behavioural surprise.
    fn validate_artefact_sections(&self) -> Result<(), ManifestError> {
        let kind = self.plugin.kind;
        let theme_present = self.theme.is_some();
        let ui_shell_present = self.ui_shell.is_some();
        let widgets_present = self.widgets.is_some();

        match kind {
            ArtefactKind::Functional => {
                if theme_present {
                    return Err(ManifestError::UnexpectedArtefactSection {
                        kind: kind.as_str(),
                        section: "theme",
                    });
                }
                if ui_shell_present {
                    return Err(ManifestError::UnexpectedArtefactSection {
                        kind: kind.as_str(),
                        section: "ui_shell",
                    });
                }
                if widgets_present {
                    return Err(ManifestError::UnexpectedArtefactSection {
                        kind: kind.as_str(),
                        section: "widgets",
                    });
                }
            }
            ArtefactKind::Theme => {
                if !theme_present {
                    return Err(ManifestError::MissingArtefactSection {
                        kind: kind.as_str(),
                        section: "theme",
                    });
                }
                if ui_shell_present {
                    return Err(ManifestError::UnexpectedArtefactSection {
                        kind: kind.as_str(),
                        section: "ui_shell",
                    });
                }
                if widgets_present {
                    return Err(ManifestError::UnexpectedArtefactSection {
                        kind: kind.as_str(),
                        section: "widgets",
                    });
                }
            }
            ArtefactKind::UiShell => {
                if !ui_shell_present {
                    return Err(ManifestError::MissingArtefactSection {
                        kind: kind.as_str(),
                        section: "ui_shell",
                    });
                }
                if theme_present {
                    return Err(ManifestError::UnexpectedArtefactSection {
                        kind: kind.as_str(),
                        section: "theme",
                    });
                }
                if widgets_present {
                    return Err(ManifestError::UnexpectedArtefactSection {
                        kind: kind.as_str(),
                        section: "widgets",
                    });
                }
            }
            ArtefactKind::WidgetKindPack => {
                if !widgets_present {
                    return Err(ManifestError::MissingArtefactSection {
                        kind: kind.as_str(),
                        section: "widgets",
                    });
                }
                if theme_present {
                    return Err(ManifestError::UnexpectedArtefactSection {
                        kind: kind.as_str(),
                        section: "theme",
                    });
                }
                if ui_shell_present {
                    return Err(ManifestError::UnexpectedArtefactSection {
                        kind: kind.as_str(),
                        section: "ui_shell",
                    });
                }
            }
        }
        Ok(())
    }

    /// Check the manifest's `[prerequisites]` against the running
    /// environment.
    ///
    /// This is the in-scope half of `[prerequisites]`. Two fields
    /// are enforceable from core with no distribution-level
    /// machinery and are checked here:
    ///
    /// - `evo_min_version` is compared against the running evo
    ///   steward's own version (typically `env!("CARGO_PKG_VERSION")`
    ///   parsed as a [`Version`]). If the manifest demands a newer
    ///   framework than is running, admission is refused with
    ///   [`ManifestError::EvoVersionTooLow`].
    /// - `os_family` is compared against the host OS string
    ///   (typically [`std::env::consts::OS`] at the call site). The
    ///   special value `"any"` always matches; otherwise exact
    ///   equality is required. Mismatch produces
    ///   [`ManifestError::OsFamilyMismatch`].
    ///
    /// The remaining `[prerequisites]` and `[resources]` fields
    /// (`outbound_network`, `filesystem_scopes`, `max_memory_mb`,
    /// `max_cpu_percent`) are explicitly out of scope for core
    /// enforcement: they require cgroups, network namespaces, bind
    /// mounts, or LSM policy that the steward does not own. Those
    /// fields remain documented in the manifest so distributions
    /// can enforce them via systemd unit directives, cgroup manager
    /// orchestration, or image-level policy. See
    /// `PLUGIN_PACKAGING.md` section 2 ("Enforcement scope") for
    /// the full split.
    ///
    /// This method is independent of [`Manifest::validate`] because
    /// the environment parameters are not intrinsic to the manifest
    /// itself; the steward supplies them at admission time. Callers
    /// that want the complete admission precheck should call
    /// [`Manifest::validate`] first, then this method.
    pub fn check_prerequisites(
        &self,
        evo_version: &Version,
        host_os: &str,
    ) -> Result<(), ManifestError> {
        if self.prerequisites.evo_min_version > *evo_version {
            return Err(ManifestError::EvoVersionTooLow {
                required: self.prerequisites.evo_min_version.clone(),
                running: evo_version.clone(),
            });
        }
        let required_os = self.prerequisites.os_family.as_str();
        if required_os != "any" && required_os != host_os {
            return Err(ManifestError::OsFamilyMismatch {
                required: self.prerequisites.os_family.clone(),
                running: host_os.to_string(),
            });
        }
        Ok(())
    }
}

/// Artefact kind a plugin ships. Default is
/// [`ArtefactKind::Functional`] — the historical
/// admission-typed plugin (singleton / factory + respondent
/// / warden, with code that loads and responds to verbs).
///
/// The other variants ship UI artefacts that flow through
/// the same admission gate, distribution paths, lifecycle
/// states, and signature verification as functional plugins,
/// but render rather than execute.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ArtefactKind {
    /// Code that loads and responds to verbs. The historical
    /// shape; every plugin shipped before the artefact-kind
    /// extension is functional.
    #[default]
    Functional,
    /// A theme — colour / font / spacing / animation tokens
    /// plus an asset bundle (logos, icons, fonts, sounds)
    /// and optional component overrides. Renders the
    /// device's brand without rewriting widget code.
    Theme,
    /// A complete UI shell — the web bundle / native bundle
    /// the operator interacts with. Multiple shells may be
    /// admitted; the operator picks one as active per UI
    /// client.
    UiShell,
    /// A bundle of one or more widget renderers. Plugins
    /// that contribute UI surfaces beyond the framework's
    /// Tier 1 widget set ship a widget-kind-pack so the
    /// framework registers their kinds in the
    /// [`crate::ui::WidgetKindEnvelope`] registry.
    WidgetKindPack,
}

impl ArtefactKind {
    /// Stable on-wire snake_case form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Functional => "functional",
            Self::Theme => "theme",
            Self::UiShell => "ui_shell",
            Self::WidgetKindPack => "widget_kind_pack",
        }
    }
}

/// The `[plugin]` section: identity and version.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Plugin {
    /// Canonical reverse-DNS name, e.g. `com.fiio.dacs`.
    pub name: String,
    /// Plugin version. Semver.
    pub version: Version,
    /// Plugin contract version this manifest targets. Currently only `1` is
    /// supported by this SDK.
    pub contract: u32,
    /// Artefact kind. `functional` (the default) is the
    /// historical admission-typed plugin. `theme` /
    /// `ui_shell` / `widget_kind_pack` are UI artefact
    /// shapes that ship through the same admission gate
    /// as functional plugins but render rather than
    /// execute. Each artefact kind requires the matching
    /// per-kind manifest section to be present
    /// ([`Manifest::theme`] / [`Manifest::ui_shell`] /
    /// [`Manifest::widgets`]); functional plugins must NOT
    /// declare any artefact section.
    ///
    /// Functional manifests MUST also declare the four
    /// executable-only sections (`[kind]` / `[transport]` /
    /// `[resources]` / `[lifecycle]`); artefact manifests
    /// MUST omit them. The dual presence-by-kind invariant
    /// is enforced by [`Manifest::validate`].
    #[serde(default)]
    pub kind: ArtefactKind,
}

/// The `[target]` section: which shelf this plugin stocks (legacy
/// single-stocking form).
///
/// Preserved for back-compat. Plugins authoring against the
/// single-shelf shape continue to declare `[target] shelf=... shape=...`;
/// the parser auto-derives a one-element `stockings` vector on
/// [`Manifest::normalize_stockings`]. New multi-shelf plugins declare
/// `[[stockings]]` directly; the parser populates `target` from the
/// primary stocking. The two forms are mutually exclusive at the
/// file layer; [`Manifest::normalize_stockings`] refuses a manifest
/// that declares both.
///
/// **Bucket: Enforced.** See `PLUGIN_PACKAGING.md` §2.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Target {
    /// Fully qualified shelf name, e.g. `metadata.providers`.
    #[serde(default)]
    pub shelf: String,
    /// Shelf shape version this plugin satisfies.
    #[serde(default)]
    pub shape: u32,
}

impl Target {
    /// Sentinel-empty target produced by the `Default` derive.
    /// Used by [`Manifest::normalize_stockings`] to detect whether
    /// the manifest's `[target]` block was present in the file.
    fn is_sentinel(&self) -> bool {
        self.shelf.is_empty() && self.shape == 0
    }
}

/// One entry in the `[[stockings]]` array — a typed declaration that
/// this plugin stocks one named shelf with one shape, role, and
/// verb partition.
///
/// A plugin's manifest may declare a single stocking via the legacy
/// `[target]` block (the parser auto-derives a one-element
/// `stockings` vector) OR multiple stockings via repeated
/// `[[stockings]]` blocks (per the multi-stocking shape introduced
/// for plugins whose runtime substrate carries multiple
/// operator-facing shelves — see `PLUGIN_PACKAGING.md` §2
/// "Multi-stocking plugins"). The two forms are mutually exclusive
/// at the file layer.
///
/// **Bucket: Enforced.** Each stocking's `shelf` must resolve in the
/// loaded catalogue at admission AND be free of cross-plugin
/// occupancy. `shape` strict-equals the catalogue shelf's current
/// `shape` OR appears in its `shape_supports` list. `role` must be
/// consistent with the plugin's `[kind].interaction`. The union of
/// every stocking's `request_types` equals the plugin's
/// `capabilities.respondent.request_types`; the intersection across
/// any two stockings is empty.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Stocking {
    /// Fully qualified shelf name (`<rack>.<shelf>`) this stocking
    /// occupies.
    pub shelf: String,
    /// Shelf shape version this stocking satisfies.
    pub shape: u32,
    /// Stocking role. Must be consistent with the plugin's
    /// `[kind].interaction`.
    pub role: StockingRole,
    /// The subset of the plugin's
    /// `capabilities.respondent.request_types` that dispatch under
    /// this stocking. Empty for a stocking whose role is
    /// `composer` or `factory` (those interactions do not carry
    /// respondent verbs); non-empty for `warden` and `respondent`
    /// roles unless the plugin declares no respondent verbs at all.
    #[serde(default)]
    pub request_types: Vec<String>,
}

/// Stocking role — which interaction shape this plugin presents on
/// this stocking's shelf.
///
/// A plugin's `[kind].interaction` determines which stocking roles
/// it may declare. The validator refuses a stocking whose role is
/// inconsistent with the plugin's interaction.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum StockingRole {
    /// Discrete request-response surface.
    Respondent,
    /// Sustained-custody surface.
    Warden,
    /// Composer surface (substrate-aware respondent dispatching
    /// composition modes against typed shelf shapes).
    Composer,
    /// Factory surface (variable instances over time).
    Factory,
}

impl StockingRole {
    /// Derive a stocking role from the plugin's
    /// `[kind].interaction`. Used by
    /// [`Manifest::normalize_stockings`] when synthesizing the
    /// single-stocking representation from the legacy `[target]`
    /// form.
    pub fn from_interaction(interaction: InteractionShape) -> Self {
        match interaction {
            InteractionShape::Respondent => StockingRole::Respondent,
            InteractionShape::Warden => StockingRole::Warden,
        }
    }

    /// True iff this stocking role is consistent with the plugin's
    /// declared `[kind].interaction`. A `Warden` plugin may carry
    /// `Respondent` stockings on its non-custody shelves; a
    /// `Respondent` plugin may not declare a `Warden` stocking.
    /// `Composer` and `Factory` stocking roles are compatible with
    /// any plugin interaction the framework currently admits — those
    /// roles describe the shelf-side interaction the stocking
    /// presents, not the plugin's custody/respondent shape.
    fn is_compatible_with_interaction(
        self,
        interaction: InteractionShape,
    ) -> bool {
        match (self, interaction) {
            (StockingRole::Warden, InteractionShape::Warden) => true,
            (StockingRole::Warden, InteractionShape::Respondent) => false,
            (StockingRole::Respondent, _) => true,
            (StockingRole::Composer, _) => true,
            (StockingRole::Factory, _) => true,
        }
    }

    /// Human-readable role token used in diagnostic messages.
    pub fn as_str(self) -> &'static str {
        match self {
            StockingRole::Respondent => "respondent",
            StockingRole::Warden => "warden",
            StockingRole::Composer => "composer",
            StockingRole::Factory => "factory",
        }
    }
}

/// The `[kind]` section: the plugin's instance and interaction shapes.
///
/// These two axes are orthogonal per `CONCEPT.md` section 5 and
/// `PLUGIN_CONTRACT.md` section 1. Their combination determines which
/// capability sub-tables must be present.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Kind {
    /// How many instances of this plugin can exist over time.
    pub instance: InstanceShape,
    /// How the plugin interacts with the steward.
    pub interaction: InteractionShape,
}

/// Instance shape: whether the plugin provides one contribution or many over
/// time.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum InstanceShape {
    /// One contribution for the life of the plugin.
    Singleton,
    /// Variable contributions over time, driven by world events.
    Factory,
}

/// Interaction shape: whether the plugin handles discrete requests or takes
/// sustained custody of work.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum InteractionShape {
    /// Handles discrete request-response exchanges.
    Respondent,
    /// Takes custody of sustained work.
    Warden,
}

/// The `[transport]` section: how the steward loads this plugin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Transport {
    /// Whether the plugin runs in-process or out-of-process.
    #[serde(rename = "type")]
    pub kind: TransportKind,
    /// Path to the artefact file, relative to the plugin directory.
    /// For example, `plugin.bin`, `plugin.so`, or `plugin.wasm`.
    pub exec: String,
}

/// Plugin transport kinds per `PLUGIN_CONTRACT.md` sections 7 and 8.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum TransportKind {
    /// Loaded into the steward process at build time.
    ///
    /// In this codebase in-process means compiled-in: the admission
    /// engine accepts these only through typed Rust API calls
    /// (`admit_singleton_respondent` / `admit_singleton_warden`),
    /// never from disk-based discovery. Runtime dynamic-library
    /// loading (cdylib via `dlopen`) is not supported: the steward
    /// declares `#![forbid(unsafe_code)]`, which would be required
    /// to wrap `dlopen` safely. A `manifest.toml` on disk declaring
    /// `transport.type = "in-process"` is therefore never admitted
    /// by the shipped binary and is skipped by `plugin_discovery`
    /// with a warning.
    InProcess,
    /// Runs as a separate process; communicates with the steward
    /// over a Unix domain socket speaking the wire protocol defined
    /// in `PLUGIN_CONTRACT.md` sections 6 through 11. Plugin
    /// discovery admits this kind at runtime from bundles under
    /// `plugins.search_roots`.
    OutOfProcess,
}

/// The `[trust]` section: declared trust class.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Trust {
    /// Declared trust class. The steward may admit at a lower class if the
    /// signing key does not authorise the declared class.
    pub class: TrustClass,
}

/// Trust classes per `PLUGIN_PACKAGING.md` section 5.
///
/// `#[non_exhaustive]` so downstream code allows for new trust classes
/// to be introduced without a SemVer break. Add a wildcard arm with a
/// structured error or `unreachable!` when matching on this enum across
/// crate boundaries.
#[derive(
    Debug,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum TrustClass {
    /// Highest class. In-process residency. System-wide custody.
    Platform,
    /// Separate process with elevated OS capabilities.
    Privileged,
    /// Separate process as the evo service user.
    Standard,
    /// Restricted user or namespace, no outbound network unless declared.
    Unprivileged,
    /// Sandbox (seccomp, namespace, or Wasm). No direct syscalls.
    Sandbox,
}

/// The `[prerequisites]` section: environmental requirements for admission.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Prerequisites {
    /// Minimum evo workspace version.
    pub evo_min_version: Version,
    /// Required OS family: `linux` or `any`.
    #[serde(default = "default_os_family")]
    pub os_family: String,
    /// Does this plugin make outbound network calls?
    #[serde(default)]
    pub outbound_network: bool,
    /// Scoped filesystem paths needed. Empty means no filesystem access.
    #[serde(default)]
    pub filesystem_scopes: Vec<String>,
}

fn default_os_family() -> String {
    "linux".to_string()
}

/// The `[resources]` section: declared resource ceilings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Resources {
    /// Maximum memory the plugin will use, in megabytes.
    pub max_memory_mb: u32,
    /// Maximum CPU share the plugin will use, expressed as a percentage.
    pub max_cpu_percent: u32,
}

/// The `[lifecycle]` section: hot-reload policy and restart behaviour.
///
/// # Field enforcement bucket
///
/// Every parsed manifest field falls into exactly one of three
/// buckets, declared in `PLUGIN_PACKAGING.md`:
///
/// - **Enforced**: the steward acts on the field at runtime. Any
///   change to the field changes observable steward behaviour.
/// - **Distribution-owned**: explicitly out of scope for the
///   framework; the field is parsed only because it lives in the
///   manifest for distribution-side use (e.g. resource limits).
/// - **Reserved**: the field exists for an in-flight feature not
///   yet implemented. The field is parsed and its absence is
///   permitted; its presence is also permitted but does not yet
///   cause behaviour. Each Reserved field has an open tracking
///   item; closing the tracker promotes the field to Enforced.
///
/// Per-field bucket annotations follow on each declaration below.
// `Eq` is intentionally omitted: the `defaults` field carries
// `toml::Value` payloads whose `Float` variant is not `Eq`.
// `PartialEq` is sufficient for equality comparisons; no
// downstream caller keys on `Lifecycle` in `HashMap` / `HashSet`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Lifecycle {
    /// Legacy hot-reload policy. Superseded by [`Lifecycle::mode`]
    /// as the canonical lifecycle-dispatch declaration; the
    /// steward's reload path no longer consults this field. Kept
    /// in the schema as an optional parse-and-ignore field so
    /// manifests authored against the previous shape continue
    /// to admit cleanly during the migration window. Removed in
    /// the next major release cycle.
    ///
    /// **Bucket: Reserved (legacy).** Parsing accepted for
    /// backward compatibility; no actuator consumes the value.
    #[serde(default)]
    pub hot_reload: HotReloadPolicy,
    /// Whether the steward should start this plugin at steward startup.
    ///
    /// **Bucket: Enforced.** At boot, the steward iterates the discovery
    /// path, parses each manifest, and admits plugins whose `autostart`
    /// is true. `false` skips admission until an operator-initiated
    /// admit verb (a future surface) targets the plugin.
    #[serde(default = "default_true")]
    pub autostart: bool,
    /// Whether the steward should restart this plugin after a crash.
    ///
    /// **Bucket: Reserved.** Parsed for forward-compat; the steward's
    /// out-of-process supervisor today reaps a crashed child and
    /// deregisters its plugin without attempting a restart, regardless
    /// of this flag's value. A future change adds a per-plugin restart
    /// supervisor that consults this field plus `restart_budget`. Until
    /// that lands the field is parsed but not acted on; the default
    /// (true) is harmless under the current implementation.
    #[serde(default = "default_true")]
    pub restart_on_crash: bool,
    /// Maximum number of restarts permitted within a rolling one-hour window
    /// before the steward gives up and de-registers the plugin.
    ///
    /// **Bucket: Reserved.** Bounded with `restart_on_crash`; both
    /// fields graduate together when the restart supervisor lands.
    /// Today the field is parsed but does not influence supervisor
    /// behaviour because no restart attempt is made.
    #[serde(default = "default_restart_budget")]
    pub restart_budget: u32,
    /// Plugin author's signal that this plugin is normally
    /// essential to a device that uses it. **Advisory only.**
    /// The framework does NOT consult this field at admission;
    /// the catalogue's `[[racks.shelves]] required = true` is
    /// the authoritative source for refusing operator
    /// disable / uninstall actions. The manifest field exists
    /// so tooling can warn operators trying to disable a
    /// `recommended_essential` plugin even when the catalogue
    /// has not (yet) marked the shelf required, and so the
    /// diagnose surface can render the author's intent
    /// alongside the operator's actual enabled bit.
    ///
    /// **Bucket: Advisory.** Parsed and surfaced; never
    /// influences admission or refusal decisions.
    #[serde(default)]
    pub recommended_essential: bool,
    /// Per-plugin override for the framework's live-reload state
    /// blob size cap, in bytes.
    ///
    /// When a plugin returns a state blob from
    /// `prepare_for_live_reload` whose payload exceeds the
    /// effective cap, the framework refuses the live-reload and
    /// leaves the previous instance running. Resolution order:
    ///
    /// - When `live_blob_max` is set, the framework uses it as the
    ///   per-plugin cap, clamped to the absolute hard ceiling
    ///   ([`crate::contract::MAX_LIVE_RELOAD_BLOB_BYTES`], 64 MiB).
    /// - When `live_blob_max` is unset, the framework uses the
    ///   default soft cap
    ///   ([`crate::contract::DEFAULT_LIVE_RELOAD_BLOB_BYTES`], 16 MiB).
    /// - Regardless of declaration, blobs above the hard ceiling
    ///   are always refused — at that scale the plugin should be
    ///   using durable persistence rather than live-reload state
    ///   transfer.
    ///
    /// **Bucket: Enforced.** The steward consults this at every
    /// `Live` reload attempt to compute the effective cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_blob_max: Option<u64>,
    /// Per-plugin lifecycle mode. Declares how the framework
    /// handles operator-config changes for this plugin:
    ///
    /// - `reactive-only`: plugin subscribes to substrate state
    ///   and reconfigures internally in place; never torn down
    ///   post-admit. Required for plugins targeting MCU tier.
    ///   The plugin's `load()` registers substrate
    ///   subscriptions and never blocks waiting for input.
    /// - `reload-cleanable`: framework runs a full teardown +
    ///   re-admit cycle on every operator-gestured reload (TOML
    ///   edit + file watcher, or explicit `plugin reload <name>`
    ///   wire op). Plugin honours the teardown contract:
    ///   drop subscriptions, release devices, abort spawned
    ///   tasks within `teardown_deadline_ms`.
    /// - `frozen`: plugin reads TOML at admit and never
    ///   reconfigures; operator reload gestures return a
    ///   `PluginIsFrozen` structured error. Process restart is
    ///   the only mutation vector.
    ///
    /// Defaults to `frozen` when the manifest does not declare
    /// a value. Plugins must opt into `reactive-only` or
    /// `reload-cleanable` explicitly — both modes carry
    /// contracts the plugin author must honour.
    ///
    /// Supersedes [`Lifecycle::hot_reload`] going forward.
    /// `hot_reload` remains parsed for backward compatibility
    /// with manifests authored against the previous shape and
    /// is removed in the next major release cycle.
    ///
    /// **Bucket: Enforced.** The framework consults this at
    /// every reload gesture (file watcher, wire op, CLI) to
    /// choose the appropriate path.
    #[serde(default)]
    pub mode: LifecycleMode,
    /// Per-plugin teardown deadline in milliseconds. The
    /// framework gives the plugin this long to drop
    /// subscriptions, release devices, abort spawned tasks
    /// before hard-aborting the plugin task tree. Defaults
    /// to 5000 ms; per-plugin override for plugins whose
    /// teardown work cannot complete in the default window
    /// (e.g. large blob persistence on shutdown).
    ///
    /// **Bucket: Enforced.** The framework's reload path
    /// observes this on every teardown attempt.
    #[serde(default = "default_teardown_deadline_ms")]
    pub teardown_deadline_ms: u64,
    /// Per-plugin admit deadline in milliseconds. The
    /// framework gives the plugin this long for `load()` to
    /// return success before aborting the admission and
    /// rolling back to the prior config. Defaults to 5000 ms;
    /// per-plugin override for plugins whose admit work
    /// (substrate rehydration, device probing) cannot
    /// complete in the default window.
    ///
    /// **Bucket: Enforced.** The framework's admission path
    /// observes this on every admit attempt.
    #[serde(default = "default_admit_deadline_ms")]
    pub admit_deadline_ms: u64,
    /// Plugin-author-declared minimal configuration the
    /// framework substitutes when both the operator-supplied
    /// TOML and the prior-known-good TOML fail to admit. The
    /// block carries free-form fields the plugin's `load()`
    /// reads to enter a non-disruptive baseline state (for
    /// example, multi-room would declare `role = "auto"` so the
    /// degraded-fall-back instance admits without engaging any
    /// audio-plane device or group membership).
    ///
    /// The block is reviewed at manifest sign-time: a plugin
    /// whose declared defaults cannot themselves admit is a
    /// manifest authoring error and is refused. The block is
    /// optional at the schema layer (plugins that do not need
    /// a defaults fall-back may omit it) — when absent and the
    /// fall-back chain reaches this step, the framework
    /// transitions the plugin to the degraded state without
    /// attempting the defaults admit.
    ///
    /// **Bucket: Enforced.** The framework's fall-back chain
    /// consults this on every admit failure that exhausts the
    /// prior-config retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<LifecycleDefaults>,
}

/// Plugin lifecycle mode per [`Lifecycle::mode`]. Three modes
/// the framework supports natively:
///
/// - `ReactiveOnly`: substrate-subscription contract; never
///   torn down post-admit.
/// - `ReloadCleanable`: teardown + re-admit on reload gesture.
/// - `Frozen`: TOML-at-admit only; reload gestures refused.
///
/// Default is `Frozen` so plugins authored without an
/// explicit mode declaration preserve their existing
/// behaviour (no operator-config reload).
#[derive(
    Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq,
)]
#[serde(rename_all = "kebab-case")]
pub enum LifecycleMode {
    /// Subscribe to substrate state; reconfigure in place;
    /// never torn down post-admit. Required for MCU tier.
    ReactiveOnly,
    /// Full teardown + re-admit on reload gesture. Linux
    /// full-participant + embedded Linux tiers only; MCU
    /// admission refuses this mode.
    ReloadCleanable,
    /// TOML at admit; no reload; process restart is the only
    /// mutation vector.
    #[default]
    Frozen,
}

/// Plugin-author-declared defaults block per
/// [`Lifecycle::defaults`]. The block holds free-form
/// configuration the framework feeds into the plugin's `load()`
/// path when the fall-back chain has exhausted both the
/// operator-supplied TOML and the prior-known-good TOML. The
/// shape of the inner fields is plugin-author-defined; the
/// framework treats the block as opaque and surfaces it to the
/// plugin in the same shape the plugin would receive from a
/// regular TOML load.
///
/// `Eq` is intentionally omitted: `toml::Value::Float` is not
/// `Eq`. `PartialEq` is sufficient for round-trip equality
/// comparisons in tests.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LifecycleDefaults {
    /// Free-form fields the plugin's `load()` reads when the
    /// framework falls back to the manifest defaults. The shape
    /// of the inner table is plugin-specific.
    #[serde(flatten)]
    pub fields: BTreeMap<String, toml::Value>,
}

/// The `[dependencies]` section: inter-plugin admission
/// requirements.
///
/// The framework's admission gate consults this section before
/// admitting a plugin:
///
/// - `required` plugins must be admitted (and enabled) at the
///   target's admit time. Missing required dependencies refuse
///   admission with `MissingRequiredDependency`.
/// - `conflicts_with` plugins must NOT be admitted at the
///   target's admit time. A live conflict refuses admission
///   with `ConflictingPluginPresent`.
/// - `recommended` plugins are advisory only. Admission proceeds
///   regardless; the diagnose surface can surface absence as a
///   warning.
///
/// Each list entry is a [`DependencySpec`] — a plugin canonical
/// name plus an optional semver requirement. The plain-string
/// shorthand (parsed from a TOML string per Serde's untagged
/// matching) is supported for the common
/// `[ui]` section — the plugin's explicit UI stockings.
///
/// Empty by default. Most plugins rely on the convergence
/// default that auto-derives a UI stocking from the catalogue
/// [`Target`]; the framework's admission gate inserts the
/// derived stocking transparently. A plugin populates
/// `[[ui.stocks]]` only when the convergence default is
/// insufficient (richer surfaces, non-default widget kinds /
/// parameters, or shelves the catalogue model does not
/// cover).
///
/// Field shape:
///
/// ```toml
/// [[ui.stocks]]
/// ui_shelf       = "library.sources"
/// widget         = "audio.browse.tree.entry"
/// size           = "third"
/// schema_version = 1
///
/// [[ui.stocks]]
/// ui_shelf   = "now_playing.context"
/// widget     = "audio.metering.spectrum"
/// size       = "full"
/// mode       = "inline"
/// parameters = { fft_size = 4096, smoothing = 0.6 }
/// schema_version = 1
/// ```
///
/// SDK-level validation enforces field shape only. Registry-
/// level validation (does the shelf exist, is the widget kind
/// admissible, is the size within envelope, does cardinality
/// permit a further stocking) happens at admission against
/// the framework's runtime registries.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UiSection {
    /// Explicit UI stockings declared by the plugin.
    #[serde(default)]
    pub stocks: Vec<UiStocking>,
}

impl UiSection {
    /// SDK-level validation. Refuses entries with empty
    /// `ui_shelf`, empty `widget`, or `schema_version < 1`.
    /// Registry-aware checks happen elsewhere.
    pub fn validate(&self) -> Result<(), ManifestError> {
        for (index, stocking) in self.stocks.iter().enumerate() {
            if stocking.ui_shelf.trim().is_empty() {
                return Err(ManifestError::InvalidUiStocking {
                    index,
                    field: "ui_shelf",
                    detail: "empty shelf id".to_string(),
                });
            }
            if stocking.widget.trim().is_empty() {
                return Err(ManifestError::InvalidUiStocking {
                    index,
                    field: "widget",
                    detail: "empty widget kind".to_string(),
                });
            }
            if stocking.schema_version < 1 {
                return Err(ManifestError::InvalidUiStocking {
                    index,
                    field: "schema_version",
                    detail: format!(
                        "schema_version must be >= 1, got {}",
                        stocking.schema_version
                    ),
                });
            }
        }
        Ok(())
    }
}

/// `[theme]` section — the manifest payload for a plugin
/// whose `plugin.kind = "theme"`.
///
/// Themes ship the device's brand surface: colour / font /
/// spacing / animation / border tokens, an asset bundle
/// (logos / icons / fonts / sounds), an optional set of
/// per-widget render overrides, and a declared variant set
/// (light / dark / auto, etc.).
///
/// Token + asset + override values are TOML-typed
/// ([`toml::Value`]) so vendors can mix strings, integers,
/// and floats without the SDK enumerating every shape.
/// Registry-level token validation (does the token name
/// match the framework's vocabulary, do contrast ratios
/// meet the declared WCAG conformance level) happens at
/// admission against the framework's runtime tier.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ThemeSection {
    /// Operator-readable theme name. Surfaces in the
    /// theme picker.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Variants the theme ships (e.g. `["light", "dark",
    /// "auto"]`). The active variant is per-operator
    /// state; the theme declares which variants it can
    /// render. Empty means a single fixed variant.
    #[serde(default)]
    pub variants: Vec<String>,
    /// `[theme.tokens]` table — colour / typography /
    /// spacing / animation / border tokens. Vocabulary
    /// validated at admission.
    #[serde(default)]
    pub tokens: BTreeMap<String, toml::Value>,
    /// `[theme.assets]` table — bundle-relative paths to
    /// asset files (logos, icons, fonts, sounds, splash
    /// imagery). Path existence validated at admission.
    #[serde(default)]
    pub assets: BTreeMap<String, String>,
    /// `[theme.overrides]` table — per-widget-kind render
    /// overrides (CSS files, etc.). Keys are widget kind
    /// ids; values are bundle-relative paths.
    #[serde(default)]
    pub overrides: BTreeMap<String, String>,
    /// `[theme.compliance]` block — accessibility /
    /// regulatory metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compliance: Option<ThemeCompliance>,
}

/// `[theme.compliance]` block carrying the theme's
/// declared accessibility conformance level. Today only
/// the WCAG conformance level is captured; richer
/// regulatory metadata (region-specific compliance,
/// energy-class declarations) lands in follow-on
/// iterations.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ThemeCompliance {
    /// Declared WCAG conformance level (`A`, `AA`, `AAA`).
    /// Validated against the theme's actual contrast
    /// ratios at admission per the lock-24 accessibility
    /// enforcement ADR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wcag_conformance: Option<String>,
}

/// `[ui_shell]` section — the manifest payload for a
/// plugin whose `plugin.kind = "ui_shell"`.
///
/// UI shells ship the operator-facing UI app — a web
/// bundle the steward serves to browser clients, or a
/// native bundle the device runs on its panel. Multiple
/// shells may be admitted; the operator picks one as
/// active per UI client.
// `Default` is not derived because `semver::Version` does not
// implement `Default` (a "default version" would be ambiguous).
// UI shells declare `min_evo_version` explicitly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UiShellSection {
    /// Operator-readable shell name. Surfaces in the
    /// shell picker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Shell-bundle shape: `web_bundle` for a static web
    /// app the steward serves, `tauri_native` for a
    /// platform-native bundle the device runs locally.
    /// Future variants ride additive enum extensions.
    pub shell_type: String,
    /// Bundle-relative path to the shell's entry HTML /
    /// executable.
    pub entry_point: String,
    /// Optional bundle-relative path to a PWA-style
    /// `manifest.json` the shell uses for installability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_assets: Option<String>,
    /// Optional bundle-relative path to a service worker
    /// the shell registers for offline support.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_worker: Option<String>,
    /// Widget kind globs the shell renders. Entries
    /// follow the same prefix-glob shape as
    /// [`crate::ui::AcceptedWidgets::Allowed`] entries
    /// — `evo.*` means "every framework widget kind",
    /// `audio.*` means "every audio reference-device
    /// widget kind". Admission verifies every required
    /// kind has a registered envelope.
    #[serde(default)]
    pub required_widget_kinds: Vec<String>,
    /// Whether the shell honours the active theme's
    /// tokens + asset overrides. Operator surfaces hide
    /// the theme picker for shells that opt out.
    #[serde(default)]
    pub supports_themes: bool,
    /// Whether the shell renders meaningfully when the
    /// device is offline.
    #[serde(default)]
    pub supports_offline: bool,
    /// Minimum evo steward version the shell requires.
    /// Validated at admission.
    pub min_evo_version: semver::Version,
}

/// `[widgets]` section — the manifest payload for a
/// plugin whose `plugin.kind = "widget_kind_pack"`.
///
/// Widget kind packs ship one or more widget renderers.
/// Each declared kind registers in the framework's
/// [`crate::ui::WidgetKindEnvelope`] registry so plugin
/// stockings can target the kind. If two packs declare
/// the same kind, admission of the second refuses with a
/// structured collision error.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WidgetKindPackSection {
    /// Widget kind ids the pack provides. Each id must be
    /// unique across admitted packs; admission refuses
    /// duplicates.
    pub provides: Vec<String>,
    /// Bundle-relative path to a TOML file declaring the
    /// per-kind size envelopes (min / ideal / max sizes,
    /// aspect ratio, mode, responsive overrides). The
    /// framework reads this at admission to populate the
    /// envelope registry.
    pub size_envelopes_path: String,
    /// Bundle-relative path to a TOML file declaring the
    /// per-kind accessibility semantics (ARIA roles,
    /// keyboard interactions, screen-reader hints).
    /// Validated at admission per the lock-24 ADR.
    pub accessibility_declarations_path: String,
}

/// `["org.example.plugin"]` form; the full struct form
/// `{ plugin_name = "...", version = ">=0.1.0" }` is supported
/// for entries with version constraints.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Dependencies {
    /// Plugins that MUST be admitted+enabled before this plugin
    /// can admit. Missing required dependencies refuse admission.
    #[serde(default)]
    pub required: Vec<DependencySpec>,
    /// Plugins the author RECOMMENDS but does not REQUIRE.
    /// Advisory only.
    #[serde(default)]
    pub recommended: Vec<DependencySpec>,
    /// Plugins this plugin CANNOT coexist with. A live conflict
    /// refuses admission.
    #[serde(default)]
    pub conflicts_with: Vec<DependencySpec>,
}

/// One dependency entry. Carries a plugin canonical name plus
/// an optional semver requirement against that plugin's
/// declared version.
///
/// Two on-the-wire forms supported via Serde untagged matching:
///
/// 1. Plain string: `"org.evoframework.composition.alsa"` —
///    the canonical plugin name with no version constraint.
/// 2. Inline table:
///    `{ plugin_name = "...", version = ">=0.1.0" }` —
///    the explicit form with a semver requirement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum DependencySpec {
    /// Plain canonical name; no version constraint.
    Name(String),
    /// Explicit form with optional version requirement.
    WithVersion {
        /// Canonical reverse-DNS plugin name.
        plugin_name: String,
        /// Optional semver requirement (e.g. `">=0.1.0"`,
        /// `"^1.2"`, `"~0.3"`). Parsed eagerly when the
        /// manifest is validated; an unparseable string is a
        /// schema-validation error.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
}

impl DependencySpec {
    /// Borrowed canonical plugin name.
    pub fn plugin_name(&self) -> &str {
        match self {
            Self::Name(n) => n,
            Self::WithVersion { plugin_name, .. } => plugin_name,
        }
    }

    /// Borrowed raw version-requirement string, if any.
    pub fn version_str(&self) -> Option<&str> {
        match self {
            Self::Name(_) => None,
            Self::WithVersion { version, .. } => version.as_deref(),
        }
    }

    /// Parse the version requirement (if any) into a typed
    /// [`VersionReq`]. `Ok(None)` means the entry has no
    /// constraint; `Ok(Some(req))` means a parseable
    /// constraint; `Err(_)` means the operator declared a
    /// constraint string that does not parse.
    pub fn parsed_version(&self) -> Result<Option<VersionReq>, semver::Error> {
        match self.version_str() {
            None => Ok(None),
            Some(s) => VersionReq::parse(s).map(Some),
        }
    }
}

impl Dependencies {
    /// Validate the dependency declaration against the host
    /// plugin's name. Refuses self-references in any list,
    /// empty canonical names, and unparseable version
    /// requirements. Called from [`Manifest::validate`]; an
    /// out-of-tree caller wanting the same validation against
    /// a borrowed [`Dependencies`] body can call this directly.
    pub fn validate(&self, plugin_name: &str) -> Result<(), ManifestError> {
        validate_list(plugin_name, &self.required, "required")?;
        validate_list(plugin_name, &self.recommended, "recommended")?;
        validate_list(plugin_name, &self.conflicts_with, "conflicts_with")?;
        Ok(())
    }
}

fn validate_list(
    plugin_name: &str,
    entries: &[DependencySpec],
    list: &'static str,
) -> Result<(), ManifestError> {
    for entry in entries {
        let name = entry.plugin_name();
        if name.is_empty() {
            return Err(ManifestError::EmptyDependencyName { list });
        }
        if name == plugin_name {
            return Err(ManifestError::SelfDependency {
                plugin_name: plugin_name.to_string(),
                list,
            });
        }
        if let Some(version_str) = entry.version_str() {
            VersionReq::parse(version_str).map_err(|e| {
                ManifestError::InvalidDependencyVersion {
                    plugin_name: name.to_string(),
                    requirement: version_str.to_string(),
                    detail: e.to_string(),
                }
            })?;
        }
    }
    Ok(())
}

fn default_true() -> bool {
    true
}

fn default_restart_budget() -> u32 {
    5
}

/// Default value for [`Lifecycle::teardown_deadline_ms`].
/// 5000 ms — enough for routine drop-subscription / release-
/// device / abort-task work on every device tier, bounded
/// enough to prevent stuck teardown from blocking the
/// framework.
fn default_teardown_deadline_ms() -> u64 {
    5000
}

/// Default value for [`Lifecycle::admit_deadline_ms`].
/// 5000 ms — enough for routine rehydrate / probe / subscribe
/// work, bounded enough to prevent stuck admission from
/// blocking other plugins or the framework's drain path.
fn default_admit_deadline_ms() -> u64 {
    5000
}

/// Legacy hot-reload policy. Superseded by [`LifecycleMode`] as
/// the canonical lifecycle-dispatch declaration; preserved for
/// back-compat parsing on manifests authored against the
/// previous schema.
#[derive(
    Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash,
)]
#[serde(rename_all = "lowercase")]
pub enum HotReloadPolicy {
    /// Full unload-reload cycle on update.
    #[default]
    None,
    /// Process restart (or re-instantiation) on update without wider
    /// disruption.
    Restart,
    /// Plugin accepts a `reload_in_place` verb; custody retained across
    /// update.
    Live,
}

/// The `[capabilities]` section, with kind-specific sub-tables.
///
/// All sub-tables are optional at the serde layer; consistency with the
/// plugin's [`Kind`] is enforced by [`Capabilities::validate`] which is
/// called from [`Manifest::validate`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Capabilities {
    /// Respondent-specific capabilities. Required iff
    /// `kind.interaction == Respondent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub respondent: Option<RespondentCapabilities>,

    /// Warden-specific capabilities. Required iff
    /// `kind.interaction == Warden`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warden: Option<WardenCapabilities>,

    /// Factory-specific capabilities. Required iff
    /// `kind.instance == Factory`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub factory: Option<FactoryCapabilities>,

    /// Source-plugin capabilities. Present iff this plugin owns
    /// one or more URI schemes — i.e. it's a source for items
    /// addressable by those schemes (a music streaming service,
    /// a local-file source, an RTSP / Icecast stream source).
    /// The framework registers each declared scheme with the
    /// URI-scheme registry at admission time so dispatched
    /// verbs (play_now / play_now_collection / etc.) targeting
    /// those URIs route to this plugin's shelf. On plugin
    /// unload, the framework unregisters the schemes so a
    /// follow-up admission of a different plugin claiming the
    /// same scheme can succeed.
    ///
    /// A source plugin is always also a respondent (the
    /// dispatched verb arrives via `handle_request`). Manifest
    /// validation refuses a `source` declaration without a
    /// `respondent` declaration, and refuses a respondent
    /// `source` whose `request_types` does not include at
    /// least one play-control verb (`play_now`,
    /// `play_now_collection`, `stop`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceCapabilities>,

    /// Delivery-plugin capabilities. Present iff this plugin
    /// owns a delivery device (an ALSA pcm, a JACK port, a
    /// shared-memory region) the framework wires the audio
    /// data plane's terminating stage through. The
    /// reconciliation engine probes the declared device at
    /// admission to populate the [`crate::audio::AudioFormat`]
    /// negotiated for the chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliveryCapabilities>,

    /// Composition-plugin capabilities. Present iff this
    /// plugin sits between source and delivery in the audio
    /// chain (an equaliser, resampler, DSD-to-PCM converter,
    /// passthrough buffer). The framework picks one of the
    /// declared [`crate::audio::CompositionAudioMode`]s per
    /// topology selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition: Option<CompositionCapabilities>,

    /// Gateway-plugin declaration. Present iff this plugin
    /// bridges between an external ecosystem (AirPlay 2,
    /// Cast, Spotify Connect, Roon Ready, MusicCast, HEOS,
    /// Sonos, etc.) and the multi-room native protocol.
    /// The framework registers gateway plugins in a per-
    /// boot registry and exposes them through the operator
    /// surface. Gateway implementations are vendor-
    /// distribution scope — the framework's reference
    /// generic-device build ships only legally-clean
    /// plugins; vendor distributions that hold the upstream
    /// ecosystem's certification carry the gateway plugins
    /// for their target ecosystems.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<GatewayCapabilities>,

    /// Admin-capability flag.
    ///
    /// When `true`, the plugin declares that it wants access to
    /// the administration surface: the `SubjectAdmin` and
    /// `RelationAdmin` callback traits in its
    /// [`LoadContext`](crate::contract::LoadContext). Admission
    /// refuses `admin = true` plugins whose effective trust class
    /// is below the admin minimum (currently `Privileged`);
    /// accepted admin plugins see `Some(Arc<dyn SubjectAdmin>)`
    /// and `Some(Arc<dyn RelationAdmin>)` in their `LoadContext`.
    ///
    /// Non-admin plugins leave this at the default (`false`) and
    /// see `None` for both admin callbacks. The flag is
    /// orthogonal to [`Kind`]: an admin plugin is typically a
    /// respondent (it stocks the `administration.*` shelves from
    /// `CATALOGUE.md`) but may be a warden or a factory for
    /// advanced operator tooling.
    ///
    /// Default is `false` so existing manifests remain valid
    /// without modification.
    #[serde(default)]
    pub admin: bool,

    /// Fast Path sender flag.
    ///
    /// When `true`, the plugin declares that its `load` body or
    /// any of its callbacks may invoke
    /// `LoadContext::fast_path_dispatch`. Hardware-input plugins
    /// (IR receivers, Bluetooth controllers, keyboard listeners,
    /// touch handlers) declare this; pure source / library /
    /// metadata plugins leave it at the default. Admission
    /// refuses Fast Path dispatches from plugins whose manifest
    /// does not declare it.
    ///
    /// The flag gates dispatching ON Fast Path; declaring which
    /// verbs a warden can serve ON Fast Path is the orthogonal
    /// [`WardenCapabilities::fast_path_verbs`] field. A plugin
    /// can be a Fast Path target without being a Fast Path
    /// sender (a warden serving operator-issued volume-set
    /// frames) and vice versa (an input plugin emitting Fast
    /// Path frames to a warden it does not itself host).
    ///
    /// Default is `false` so existing manifests remain valid
    /// without modification. The flag is independent of trust
    /// class and admin capability.
    #[serde(default)]
    pub fast_path: bool,

    /// Appointments-creation flag.
    ///
    /// When `true`, the plugin declares that it wants the
    /// `LoadContext.appointments` handle populated. The
    /// admission engine populates the handle only for plugins
    /// declaring this; non-declaring plugins see `None` and
    /// cannot create appointments through the SDK surface.
    ///
    /// Default `false`. Independent of trust class and admin
    /// capability.
    #[serde(default)]
    pub appointments: bool,

    /// Watches-creation flag.
    ///
    /// When `true`, the plugin declares that it wants the
    /// `LoadContext.watches` handle populated. The admission
    /// engine populates the handle only for plugins declaring
    /// this; non-declaring plugins see `None` and cannot create
    /// watches through the SDK surface.
    ///
    /// Default `false`. Independent of trust class and admin
    /// capability.
    #[serde(default)]
    pub watches: bool,

    /// Streaming-producer flag.
    ///
    /// When `true`, the plugin declares that it wants the
    /// `LoadContext.streams` handle populated so it can open,
    /// emit on, and close streams against the framework's stream
    /// coordinator (the producer-side surface of the streaming
    /// wire primitive). Consumers (UI clients, other plugins)
    /// reach the coordinator through the wire layer and do not
    /// need this flag.
    ///
    /// The admission engine populates the handle only for plugins
    /// declaring this AND only when the engine was configured
    /// with a stream coordinator handle. Non-declaring plugins see
    /// `None` and cannot emit streams through the SDK surface.
    ///
    /// Default `false`. Independent of trust class and admin
    /// capability.
    #[serde(default)]
    pub streams: bool,

    /// Multi-room audio-plane participation flag.
    ///
    /// When `true`, the plugin declares that it wants the
    /// `LoadContext.audio_plane` handle populated so it can
    /// fan audio frames out to multi-room receivers AND
    /// subscribe to incoming audio frames from a source-host
    /// peer. Used by the open-source evo-native multi-room
    /// gateway plugin to bridge the local audio chain to the
    /// framework's audio-plane TCP transport.
    ///
    /// The admission engine populates the handle only for
    /// plugins declaring this AND only when the engine was
    /// configured with an audio-plane runtime handle. Non-
    /// declaring plugins see `None` and cannot exchange audio
    /// frames across the multi-room substrate.
    ///
    /// Default `false`. Independent of trust class and admin
    /// capability.
    #[serde(default)]
    pub audio_plane: bool,

    /// Subject-state subscription flag.
    ///
    /// When `true`, the plugin declares that it wants the
    /// `LoadContext.subject_state_subscriber` handle populated
    /// so it can subscribe to push-mode state changes of
    /// subjects announced by other plugins.
    ///
    /// The admission engine populates this handle for every
    /// in-process plugin today (the registry's broadcast is
    /// the same regardless of declaration); the manifest flag
    /// is the declarative seam future revisions will gate on
    /// when capability-based filtering lands.
    ///
    /// Default `false`. Independent of trust class and admin
    /// capability.
    #[serde(default)]
    pub subscribe_subjects: bool,

    /// Notification-emitter flag.
    ///
    /// When `true`, the plugin declares that it wants the
    /// `LoadContext.notifications` handle populated so it can
    /// send notifications to the operator (display banners,
    /// audible chimes, voice prompts) and cancel them when the
    /// triggering condition clears. The framework wrapper
    /// enforces source-plugin attribution: every notification's
    /// `source_plugin` field is overwritten with the plugin's
    /// canonical name before reaching the dispatcher, so a
    /// plugin cannot spoof another plugin's name on the
    /// operator's notification surface.
    ///
    /// The admission engine populates the handle only for plugins
    /// declaring this AND only when the engine was configured
    /// with a notification dispatcher handle. Non-declaring
    /// plugins see `None` and cannot send notifications through
    /// the SDK surface.
    ///
    /// Default `false`. Independent of trust class and admin
    /// capability.
    #[serde(default)]
    pub notifications: bool,

    /// Metadata-consumer flag.
    ///
    /// When `true`, the plugin declares that it wants the
    /// `LoadContext.metadata` handle populated so it can consult
    /// the framework's metadata chain — execute structured
    /// queries across registered providers, fetch a single item
    /// by URI from a known owning provider, or enrich a batch of
    /// references with fields from any provider that declares
    /// them. Distinct from being a metadata *provider*: a plugin
    /// implements [`MetadataProvider`] when it answers queries;
    /// the consumer flag governs whether the plugin can issue
    /// queries.
    ///
    /// A plugin can be both a provider (registered with the
    /// chain at admission time) and a consumer (declares this
    /// flag) — common case for source plugins that answer
    /// queries about their own URIs and also enrich queue items
    /// they did not originate.
    ///
    /// The admission engine populates the handle only for plugins
    /// declaring this AND only when the engine was configured
    /// with a metadata-chain handle. Non-declaring plugins see
    /// `None` and cannot consult the chain through the SDK
    /// surface.
    ///
    /// Default `false`. Independent of trust class and admin
    /// capability.
    ///
    /// [`MetadataProvider`]: crate::contract::MetadataProvider
    #[serde(default)]
    pub metadata: bool,

    /// Background-scheduler flag.
    ///
    /// When `true`, the plugin declares that it wants the
    /// `LoadContext.scheduler` handle populated so it can
    /// register plugin-internal background work (OAuth refresh
    /// cycles, cache TTL pruning, heartbeats, polls, one-shot
    /// delayed work) through the framework's scheduling
    /// primitive. Distinct from `appointments` (operator-facing
    /// alarms) and `watches` (condition-driven instructions);
    /// scheduler tasks are plugin-author-controlled background
    /// work the operator does not see unless the plugin
    /// surfaces them.
    ///
    /// The admission engine populates the handle only for
    /// plugins declaring this AND only when the engine was
    /// configured with a scheduler-runtime handle. Non-declaring
    /// plugins see `None` and cannot register schedules through
    /// the SDK surface.
    ///
    /// Default `false`. Independent of trust class and admin
    /// capability.
    #[serde(default)]
    pub scheduler: bool,
}

impl Capabilities {
    /// Check that capability sub-tables are consistent with the plugin's
    /// declared [`Kind`].
    ///
    /// Rules:
    /// - `respondent` must be present when
    ///   `kind.interaction == Respondent`.
    /// - `warden` must be present when
    ///   `kind.interaction == Warden`. A warden plugin MAY
    ///   additionally declare `[capabilities.respondent]`
    ///   to handle request-type dispatch alongside its
    ///   `course_correct` surface — typical for plugins
    ///   that own a resource (custody) AND respond to
    ///   URI-targeted source verbs (e.g. an audio playback
    ///   warden that also owns one or more music URI
    ///   schemes).
    /// - A respondent plugin CANNOT additionally declare
    ///   `[capabilities.warden]`: holding custody is the
    ///   defining warden role, and a respondent claiming
    ///   it would be functionally a warden under a
    ///   misnamed `kind.interaction`.
    /// - `factory` must be present iff `kind.instance == Factory`.
    pub fn validate(&self, kind: &Kind) -> Result<(), ManifestError> {
        match kind.interaction {
            InteractionShape::Respondent => {
                if self.respondent.is_none() {
                    return Err(ManifestError::InconsistentCapabilities(
                        "kind.interaction is respondent but \
                         [capabilities.respondent] is missing"
                            .to_string(),
                    ));
                }
                if self.warden.is_some() {
                    return Err(ManifestError::InconsistentCapabilities(
                        "kind.interaction is respondent but \
                         [capabilities.warden] is present — holding \
                         custody is the defining warden role; a \
                         respondent that claims it should declare \
                         interaction = warden instead"
                            .to_string(),
                    ));
                }
            }
            InteractionShape::Warden => {
                if self.warden.is_none() {
                    return Err(ManifestError::InconsistentCapabilities(
                        "kind.interaction is warden but \
                         [capabilities.warden] is missing"
                            .to_string(),
                    ));
                }
                // Warden plugins MAY additionally declare
                // [capabilities.respondent] to expose a
                // request-type surface alongside their
                // course_correct surface. The two dispatch
                // paths are independent: course_correct
                // verbs route through the custody-aware
                // dispatcher; request_types route through
                // the request-handler dispatcher. A typical
                // example is an audio playback warden that
                // also owns one or more URI schemes and
                // handles source verbs (play_now, stop,
                // etc.) targeting them.
                // Validate course_correct_verbs shape: when set,
                // it must be non-empty (a warden with zero
                // declared verbs cannot perform any course
                // corrections, which is a manifest authoring
                // error rather than a legitimate state) and
                // contain no duplicates.
                if let Some(verbs) = self
                    .warden
                    .as_ref()
                    .and_then(|w| w.course_correct_verbs.as_ref())
                {
                    if verbs.is_empty() {
                        return Err(ManifestError::InconsistentCapabilities(
                            "[capabilities.warden].course_correct_verbs \
                             is set to an empty list; a warden with no \
                             declared verbs cannot dispatch any course \
                             corrections (omit the field for legacy \
                             plugins; otherwise list at least one verb)"
                                .to_string(),
                        ));
                    }
                    let mut seen = std::collections::BTreeSet::new();
                    for v in verbs {
                        if !seen.insert(v.as_str()) {
                            return Err(
                                ManifestError::InconsistentCapabilities(
                                    format!(
                                "[capabilities.warden].course_correct_verbs \
                                 contains duplicate entry {v:?}"
                            ),
                                ),
                            );
                        }
                    }
                }

                self.validate_warden_fast_path()?;
            }
        }

        match kind.instance {
            InstanceShape::Factory => {
                if self.factory.is_none() {
                    return Err(ManifestError::InconsistentCapabilities(
                        "kind.instance is factory but \
                         [capabilities.factory] is missing"
                            .to_string(),
                    ));
                }
            }
            InstanceShape::Singleton => {
                if self.factory.is_some() {
                    return Err(ManifestError::InconsistentCapabilities(
                        "kind.instance is singleton but \
                         [capabilities.factory] is present"
                            .to_string(),
                    ));
                }
            }
        }

        // Validate source-plugin shape. A source plugin owns
        // URI schemes; the dispatched verbs targeting those
        // URIs arrive via handle_request, which means the
        // plugin must also be a respondent and must declare
        // at least one play-control verb.
        if let Some(source) = &self.source {
            if source.uri_schemes.is_empty() {
                return Err(ManifestError::InconsistentCapabilities(
                    "[capabilities.source].uri_schemes is empty; \
                     omit the [capabilities.source] section if the \
                     plugin does not own any URI schemes"
                        .to_string(),
                ));
            }
            // No duplicates in the URI scheme list.
            let mut seen = std::collections::BTreeSet::new();
            for s in &source.uri_schemes {
                if s.is_empty() {
                    return Err(ManifestError::InconsistentCapabilities(
                        "[capabilities.source].uri_schemes contains \
                         an empty string"
                            .to_string(),
                    ));
                }
                if !seen.insert(s.as_str()) {
                    return Err(ManifestError::InconsistentCapabilities(
                        format!(
                            "[capabilities.source].uri_schemes \
                             contains duplicate entry {s:?}"
                        ),
                    ));
                }
            }
            // Source plugin must also be a respondent (the
            // dispatch arrives via handle_request).
            let Some(resp) = &self.respondent else {
                return Err(ManifestError::InconsistentCapabilities(
                    "[capabilities.source] is set but \
                     [capabilities.respondent] is missing — a source \
                     plugin must also be a respondent so dispatched \
                     verbs arrive via handle_request"
                        .to_string(),
                ));
            };
            // Must declare at least one play-control verb. The
            // closed set of play-control verb stable strings
            // matches `evo_plugin_sdk::contract::SourceVerb::as_str`
            // for the play_now / play_now_collection / stop /
            // pause / resume / etc. verbs. We only require ONE
            // here so authors can ship an early plugin that
            // handles only play_now without needing the full set.
            const PLAY_CONTROL_VERBS: &[&str] = &[
                "play_now",
                "play_now_collection",
                "play_next",
                "enqueue",
                "enqueue_and_start",
                "replace_queue",
                "save",
                "pause",
                "resume",
                "stop",
                "seek",
                "next",
                "previous",
            ];
            let has_play_control = resp
                .request_types
                .iter()
                .any(|rt| PLAY_CONTROL_VERBS.contains(&rt.as_str()));
            if !has_play_control {
                return Err(ManifestError::InconsistentCapabilities(
                    "[capabilities.source] is set but \
                     [capabilities.respondent].request_types declares \
                     no play-control verb (play_now, \
                     play_now_collection, stop, etc.) — a source \
                     plugin must handle at least one source verb"
                        .to_string(),
                ));
            }
            // Audio-source coherence: when output_kind names an
            // audio class, audio_formats must list at least one
            // declaration AND each declaration must be valid in
            // isolation. Sources that do not produce audio leave
            // output_kind absent and audio_formats empty.
            if let Some(kind) = source.output_kind.as_deref() {
                if is_audio_kind(kind) {
                    if source.audio_formats.is_empty() {
                        return Err(ManifestError::InconsistentCapabilities(
                            format!(
                                "[capabilities.source].output_kind = \
                                 {kind:?} but \
                                 [capabilities.source].audio_formats is \
                                 empty — audio sources must list at least \
                                 one format declaration"
                            ),
                        ));
                    }
                    for decl in &source.audio_formats {
                        decl.validate().map_err(|e| {
                            ManifestError::InconsistentCapabilities(format!(
                                "[capabilities.source].audio_formats: {e}"
                            ))
                        })?;
                    }
                }
            }
        }

        // Delivery-plugin shape. Device path non-empty, audio
        // formats valid in isolation when input_kind names an
        // audio class.
        if let Some(delivery) = &self.delivery {
            if delivery.input_kind.trim().is_empty() {
                return Err(ManifestError::InconsistentCapabilities(
                    "[capabilities.delivery].input_kind must not be empty"
                        .to_string(),
                ));
            }
            if delivery.device.trim().is_empty() {
                return Err(ManifestError::InconsistentCapabilities(
                    "[capabilities.delivery].device must not be empty"
                        .to_string(),
                ));
            }
            if is_audio_kind(&delivery.input_kind)
                && delivery.audio_formats.is_empty()
                && delivery.formats_query.is_none()
            {
                return Err(ManifestError::InconsistentCapabilities(format!(
                    "[capabilities.delivery].input_kind = \
                         {kind:?} but neither audio_formats nor \
                         formats_query is set — audio delivery plugins \
                         must declare at least one of (manifest-listed \
                         formats, runtime probe mechanism)",
                    kind = delivery.input_kind
                )));
            }
            for decl in &delivery.audio_formats {
                decl.validate().map_err(|e| {
                    ManifestError::InconsistentCapabilities(format!(
                        "[capabilities.delivery].audio_formats: {e}"
                    ))
                })?;
            }
            // bit_perfect_capable requires exclusive_mode — a
            // shared device cannot guarantee bit-perfect because
            // the OS mixer may resample / dither.
            if delivery.bit_perfect_capable && !delivery.exclusive_mode {
                return Err(ManifestError::InconsistentCapabilities(
                    "[capabilities.delivery].bit_perfect_capable = true \
                     requires exclusive_mode = true — a shared device \
                     cannot guarantee bit-perfect output"
                        .to_string(),
                ));
            }
        }

        // Composition-plugin shape. input/output kinds non-empty,
        // modes non-empty, default_mode references a declared mode,
        // each mode valid in isolation.
        if let Some(composition) = &self.composition {
            if composition.input_kind.trim().is_empty() {
                return Err(ManifestError::InconsistentCapabilities(
                    "[capabilities.composition].input_kind must not be empty"
                        .to_string(),
                ));
            }
            if composition.output_kind.trim().is_empty() {
                return Err(ManifestError::InconsistentCapabilities(
                    "[capabilities.composition].output_kind must not be \
                     empty"
                        .to_string(),
                ));
            }
            if composition.modes.is_empty() {
                return Err(ManifestError::InconsistentCapabilities(
                    "[capabilities.composition].modes must list at least \
                     one mode"
                        .to_string(),
                ));
            }
            let mut seen_names = std::collections::BTreeSet::new();
            for mode in &composition.modes {
                mode.validate().map_err(|e| {
                    ManifestError::InconsistentCapabilities(format!(
                        "[capabilities.composition].modes: {e}"
                    ))
                })?;
                if !seen_names.insert(mode.name.as_str()) {
                    return Err(ManifestError::InconsistentCapabilities(
                        format!(
                            "[capabilities.composition].modes contains \
                             duplicate name {name:?}",
                            name = mode.name
                        ),
                    ));
                }
            }
            if !seen_names.contains(composition.default_mode.as_str()) {
                return Err(ManifestError::InconsistentCapabilities(format!(
                    "[capabilities.composition].default_mode = \
                         {default:?} does not match any declared mode \
                         name",
                    default = composition.default_mode
                )));
            }
        }

        if let Some(gateway) = &self.gateway {
            gateway.validate().map_err(|e| {
                ManifestError::InconsistentCapabilities(format!(
                    "[capabilities.gateway]: {e}"
                ))
            })?;
        }

        // Per-verb capability gate consistency: every key in a
        // capability map MUST appear in the corresponding verb
        // list (request_types for respondents, course_correct_verbs
        // for wardens). A key absent from the verb list is a
        // manifest authoring error — the framework cannot route a
        // gate to a verb the plugin does not declare.
        if let Some(resp) = &self.respondent {
            let declared: std::collections::BTreeSet<&str> =
                resp.request_types.iter().map(String::as_str).collect();
            for (verb, capability) in &resp.verb_capabilities {
                if !declared.contains(verb.as_str()) {
                    return Err(ManifestError::InconsistentCapabilities(
                        format!(
                            "[capabilities.respondent].verb_capabilities \
                             declares verb {verb:?} which does not appear \
                             in request_types; every verb_capabilities \
                             key MUST be a member of request_types"
                        ),
                    ));
                }
                if let Some(scope) = capability.scope() {
                    if scope.trim().is_empty() {
                        return Err(ManifestError::InconsistentCapabilities(
                            format!(
                                "[capabilities.respondent].verb_capabilities[\
                                 {verb:?}] declares an empty capability \
                                 scope; scope names must not be empty"
                            ),
                        ));
                    }
                }
            }
        }
        if let Some(warden) = &self.warden {
            match &warden.course_correct_verbs {
                Some(verbs) => {
                    let declared: std::collections::BTreeSet<&str> =
                        verbs.iter().map(String::as_str).collect();
                    for (verb, capability) in &warden.verb_capabilities {
                        if !declared.contains(verb.as_str()) {
                            return Err(
                                ManifestError::InconsistentCapabilities(
                                    format!(
                                "[capabilities.warden].verb_capabilities \
                                 declares verb {verb:?} which does not appear \
                                 in course_correct_verbs; every \
                                 verb_capabilities key MUST be a member of \
                                 course_correct_verbs"
                            ),
                                ),
                            );
                        }
                        if let Some(scope) = capability.scope() {
                            if scope.trim().is_empty() {
                                return Err(
                                    ManifestError::InconsistentCapabilities(
                                        format!(
                                    "[capabilities.warden].verb_capabilities[\
                                     {verb:?}] declares an empty capability \
                                     scope; scope names must not be empty"
                                ),
                                    ),
                                );
                            }
                        }
                    }
                }
                None => {
                    if !warden.verb_capabilities.is_empty() {
                        return Err(ManifestError::InconsistentCapabilities(
                            "[capabilities.warden].verb_capabilities is \
                             populated but course_correct_verbs is absent; \
                             declare course_correct_verbs before declaring \
                             per-verb capabilities"
                                .to_string(),
                        ));
                    }
                }
            }
        }

        Ok(())
    }

    /// Validate the warden-side Fast Path declarations.
    ///
    /// Caller has already established `kind.interaction == Warden`
    /// and that `self.warden` is `Some`.
    ///
    /// Rules:
    /// - `fast_path_verbs`, when set, must be non-empty and
    ///   duplicate-free (mirrors the `course_correct_verbs`
    ///   shape).
    /// - `fast_path_verbs ⊆ course_correct_verbs` whenever both
    ///   are set: Fast Path is a latency-bounded variant of the
    ///   same dispatch surface, not a different one.
    /// - `fast_path_budget_ms`, when set, must be `> 0`. A
    ///   zero-millisecond budget is unreachable. Values exceeding
    ///   [`FAST_PATH_BUDGET_MS_MAX`] are accepted at parse time
    ///   but clamped at admission with a warning trace; surfacing
    ///   a hard parse error here would force every distribution
    ///   to copy the constant into its CI lints, while clamping
    ///   keeps the framework's contract centralised.
    /// - Every key in `fast_path_coalesce_ms` must appear in
    ///   `fast_path_verbs`. Per-verb coalescing on a verb the
    ///   warden does not declare on Fast Path is a manifest
    ///   authoring error.
    /// - Every value in `fast_path_coalesce_ms` must be `> 0`.
    fn validate_warden_fast_path(&self) -> Result<(), ManifestError> {
        let warden = match self.warden.as_ref() {
            Some(w) => w,
            // Caller has already produced a structured error if a
            // warden plugin is missing this block.
            None => return Ok(()),
        };

        if let Some(verbs) = warden.fast_path_verbs.as_ref() {
            if verbs.is_empty() {
                return Err(ManifestError::InconsistentCapabilities(
                    "[capabilities.warden].fast_path_verbs is set to an \
                     empty list; a warden with no Fast Path verbs cannot \
                     serve Fast Path frames (omit the field to mark the \
                     warden Fast-Path-ineligible; otherwise list at least \
                     one verb)"
                        .to_string(),
                ));
            }
            let mut seen = std::collections::BTreeSet::new();
            for v in verbs {
                if !seen.insert(v.as_str()) {
                    return Err(ManifestError::InconsistentCapabilities(
                        format!(
                            "[capabilities.warden].fast_path_verbs \
                             contains duplicate entry {v:?}"
                        ),
                    ));
                }
            }

            if let Some(course_correct) = warden.course_correct_verbs.as_ref() {
                let cc: std::collections::BTreeSet<&str> =
                    course_correct.iter().map(String::as_str).collect();
                for v in verbs {
                    if !cc.contains(v.as_str()) {
                        return Err(ManifestError::InconsistentCapabilities(
                            format!(
                                "[capabilities.warden].fast_path_verbs \
                                 entry {v:?} is not in course_correct_verbs; \
                                 Fast Path verbs must be a subset of the \
                                 declared course_correct surface"
                            ),
                        ));
                    }
                }
            }
        }

        if let Some(budget) = warden.fast_path_budget_ms {
            if budget == 0 {
                return Err(ManifestError::InconsistentCapabilities(
                    "[capabilities.warden].fast_path_budget_ms must be > 0; \
                     a zero-millisecond Fast Path budget is unreachable"
                        .to_string(),
                ));
            }
        }

        if let Some(coalesce) = warden.fast_path_coalesce_ms.as_ref() {
            let declared_verbs: std::collections::BTreeSet<&str> = warden
                .fast_path_verbs
                .as_ref()
                .map(|v| v.iter().map(String::as_str).collect())
                .unwrap_or_default();
            for (verb, window) in coalesce {
                if !declared_verbs.contains(verb.as_str()) {
                    return Err(ManifestError::InconsistentCapabilities(
                        format!(
                            "[capabilities.warden].fast_path_coalesce_ms \
                             references verb {verb:?} which is not in \
                             fast_path_verbs; per-verb coalescing on a \
                             verb the warden does not declare on Fast \
                             Path is a manifest authoring error"
                        ),
                    ));
                }
                if *window == 0 {
                    return Err(ManifestError::InconsistentCapabilities(
                        format!(
                            "[capabilities.warden].fast_path_coalesce_ms \
                             entry for verb {verb:?} is 0; a zero-\
                             millisecond coalesce window is meaningless \
                             (omit the entry to disable coalescing for \
                             that verb)"
                        ),
                    ));
                }
            }
        }

        Ok(())
    }
}

/// Capability scope a caller must hold to invoke one plugin-route
/// verb (a respondent's `request_type` or a warden's
/// `course_correct` verb).
///
/// The wire shape mirrors the framework-internal
/// `evo_projection_core::CapabilityRequirement` — same
/// `kind`-tagged serde representation, same lattice
/// (`none` / `read` / `write` / `step_up`). The two types are
/// declared in separate crates because the SDK ships under
/// Apache-2.0 (so plugin authors compile against the SDK alone
/// without picking up a copyleft-adjacent licence) while the
/// framework-internal type lives in the BUSL-1.1 framework
/// crate. The steward converts `VerbCapability` to the
/// framework-internal requirement at admission time; the on-the-
/// wire TOML / JSON encoding is byte-identical so no migration is
/// required when a plugin's manifest crosses the boundary.
///
/// Variants form a strict lattice:
///
/// - [`Self::None`] — anonymous-OK; the verb accepts dispatches
///   without a capability check. Default for verbs declared in
///   `request_types` / `course_correct_verbs` but absent from
///   the `verb_capabilities` map, and the default for legacy
///   manifests authored before this field existed.
/// - [`Self::Read`] — caller's token must bear the named read
///   scope.
/// - [`Self::Write`] — caller's token must bear the named write
///   scope. Reserved for verbs that mutate observable state
///   visible to other principals.
/// - [`Self::StepUp`] — caller's token must bear the named write
///   scope AND have completed step-up auth within the session's
///   step-up TTL. Reserved for privileged operations: power
///   management, network admin, hardware reconfiguration, plugin
///   admission, capability grants, update apply, etc.
///
/// The scope strings (`"system_admin"`, `"network_admin"`,
/// `"audio_admin"`, `"updates_admin"`, etc.) are opaque to the
/// SDK; the framework's capability registry registers the legal
/// set at admission time. Vendor distributions that introduce
/// new scopes register them with the registry; the SDK does not
/// enumerate a closed set so vendor plugins can declare scopes
/// their distribution's registry knows about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerbCapability {
    /// Anonymous-OK. The verb accepts dispatches without a
    /// capability check.
    None,
    /// Caller's token must bear the named read scope.
    Read {
        /// The capability scope the caller must hold for read.
        scope: String,
    },
    /// Caller's token must bear the named write scope.
    Write {
        /// The capability scope the caller must hold for write.
        scope: String,
    },
    /// Caller's token must bear the named write scope AND have
    /// completed step-up auth within the session's step-up TTL.
    StepUp {
        /// The capability scope the caller must hold for
        /// step-up write.
        scope: String,
    },
}

impl VerbCapability {
    /// Borrow the scope string if this variant carries one.
    /// `None` returns `None`.
    pub fn scope(&self) -> Option<&str> {
        match self {
            Self::None => Option::None,
            Self::Read { scope }
            | Self::Write { scope }
            | Self::StepUp { scope } => Some(scope.as_str()),
        }
    }

    /// Whether this declaration demands an active step-up auth
    /// session. Only [`Self::StepUp`] returns `true`.
    pub fn requires_step_up(&self) -> bool {
        matches!(self, Self::StepUp { .. })
    }

    /// Whether this declaration gates the verb behind any
    /// capability check. `None` returns `false`; every other
    /// variant returns `true`.
    pub fn is_gated(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Respondent-specific capabilities per `PLUGIN_PACKAGING.md` section 2.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RespondentCapabilities {
    /// Shelf-shape-declared request types this plugin handles.
    ///
    /// **Bucket: Enforced.** Admission rejects a respondent whose
    /// `request_types` does not cover at least one verb the target
    /// shelf shape exposes; subsequent dispatch refuses any
    /// `handle_request` whose `request_type` is not in this list.
    /// The two checks together pin the contract: a respondent serves
    /// exactly the verbs it advertises.
    pub request_types: Vec<String>,
    /// Optional per-verb capability requirements.
    ///
    /// **Bucket: Enforced.** Keys MUST be a subset of
    /// [`Self::request_types`]; manifest validation rejects a key
    /// absent from the verb list as an authoring error. Verbs
    /// declared in `request_types` but absent from this map default
    /// to [`VerbCapability::None`] — anonymous-OK. The framework
    /// dispatcher consults this map on every plugin-route request
    /// and enforces the declared capability against the principal's
    /// granted-capabilities set BEFORE forwarding the request to
    /// the plugin's `handle_request`; a failed check refuses with
    /// the structured `permission_denied` error and the plugin
    /// never sees the call. Plugins MUST NOT re-check the principal
    /// in `handle_request` — the framework dispatcher's check is
    /// the authoritative gate; re-checking inside the plugin would
    /// split the policy across two layers.
    ///
    /// Optional + backwards-compatible: legacy manifests authored
    /// before this field existed remain valid; their verbs default
    /// to `None`. Plugins declaring privileged or admin verbs
    /// (anything that mutates host state, hardware, or operator-
    /// visible configuration) are expected to populate this map.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub verb_capabilities: BTreeMap<String, VerbCapability>,
    /// Deadline after which the steward declares a timeout.
    ///
    /// **Bucket: Enforced.** Used by the router to populate
    /// `EnforcementPolicy::default_request_deadline_ms` at admission.
    /// Every `handle_request` whose own `Request::deadline` is `None`
    /// inherits this deadline; the dispatch wraps the call in a
    /// `tokio::time::timeout` and surfaces a `PluginError::Timeout`
    /// on expiry.
    pub response_budget_ms: u32,
}

/// Source-plugin capabilities. Declares the URI schemes the
/// plugin owns; the framework registers them with its
/// URI-scheme registry at admission time so dispatched verbs
/// targeting URIs of those schemes route to this plugin.
///
/// A scheme is owned by exactly one plugin at a time. Two
/// admitted plugins cannot share a scheme; admission refuses
/// the second plugin with a structured error and the operator
/// resolves by removing one of the conflicting plugins.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceCapabilities {
    /// URI schemes this plugin owns. Each scheme is the prefix
    /// before the first `:` in addressable URIs — e.g.
    /// `["spotify", "spotify-playlist"]` means the plugin
    /// handles URIs `spotify:track:...` and
    /// `spotify-playlist:...`.
    ///
    /// **Bucket: Enforced.** Admission registers every listed
    /// scheme with the framework's URI-scheme registry; a
    /// conflict (another plugin already owns the scheme)
    /// refuses the admission with a structured error.
    /// Subsequent dispatch refuses any verb targeting a URI
    /// whose scheme is not registered. The two checks together
    /// pin the contract: a source serves exactly the URI
    /// namespace it advertises.
    ///
    /// Empty list is valid but signals "this isn't actually a
    /// source plugin" — manifest validation refuses, since the
    /// `source` capability section's only purpose is to declare
    /// schemes.
    pub uri_schemes: Vec<String>,

    /// Output kind — the abstract data class this source
    /// produces (`"audio.pcm"`, `"audio.encoded"`, etc.). The
    /// framework treats the string opaquely except for routing
    /// the source through the matching data plane. Optional;
    /// sources that do not produce framework-managed media
    /// streams leave it absent.
    ///
    /// **Bucket: Enforced.** Audio sources MUST set this to
    /// `"audio.pcm"` or `"audio.encoded"` for the audio data
    /// plane to wire the source's [`AudioRouting`] endpoint.
    ///
    /// [`AudioRouting`]: crate::audio
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_kind: Option<String>,

    /// Audio formats the source can produce. Non-empty when
    /// `output_kind` is one of the audio kinds; the
    /// reconciliation engine intersects this list with the
    /// downstream stages' declarations to pick a negotiated
    /// format.
    ///
    /// **Bucket: Enforced.** Audio sources whose `output_kind`
    /// names an audio kind MUST list at least one format here;
    /// manifest validation refuses an empty list under that
    /// condition.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio_formats: Vec<crate::audio::AudioFormatDecl>,

    /// `true` when the source promises bit-perfect output —
    /// the produced bytes are identical to the source material
    /// without implicit resampling, dither, or volume scaling.
    /// The framework cannot verify the claim directly; the
    /// topology validator detects when a chain that claims
    /// bit-perfect actually requires implicit conversion and
    /// refuses.
    ///
    /// **Bucket: Self-declared promise.** Default `false` so
    /// existing manifests remain valid without modification.
    #[serde(default)]
    pub bit_perfect_capable: bool,

    /// Source-side topology preference — hint to the
    /// reconciliation engine for whether a chain with no
    /// intermediate composition stage is preferred. Operator
    /// policy and format mismatch may override.
    #[serde(default, skip_serializing_if = "is_default_topology_preference")]
    pub preferred_topology: crate::audio::PreferredTopology,
}

fn is_default_topology_preference(p: &crate::audio::PreferredTopology) -> bool {
    matches!(p, crate::audio::PreferredTopology::Any)
}

/// Returns `true` when the supplied kind string names an audio
/// data class — used by capability validation to gate audio-
/// specific shape rules. Recognised kinds: `"audio.pcm"`
/// (decoded PCM), `"audio.encoded"` (encoded passthrough).
fn is_audio_kind(kind: &str) -> bool {
    matches!(kind, "audio.pcm" | "audio.encoded")
}

/// Composition-plugin audio capabilities. Declares the audio
/// modes (passthrough / equaliser / resampler / DSD-to-PCM /
/// upsampler / etc.) the plugin supports. The reconciliation
/// engine picks one per topology after intersecting the
/// source's produced format with the delivery target's
/// accepted format and applying operator policy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompositionCapabilities {
    /// Abstract input data class (`"audio.pcm"` /
    /// `"audio.encoded"`).
    pub input_kind: String,
    /// Abstract output data class. Typically the same as
    /// `input_kind`; differs only for converters
    /// (`"audio.pcm"` → `"audio.encoded"` is uncommon, the
    /// reverse — DSD-to-PCM converters — is the realistic
    /// case).
    pub output_kind: String,
    /// Modes the plugin offers. The reconciliation engine
    /// picks one per topology selection; operator policy may
    /// pin a specific mode.
    ///
    /// **Bucket: Enforced.** Manifest validation refuses an
    /// empty list — a composition declaration without modes
    /// has no surface to compose.
    pub modes: Vec<crate::audio::CompositionAudioMode>,
    /// Default mode the framework selects when nothing else
    /// constrains the choice. Must reference a `name` in
    /// `modes`; manifest validation refuses dangling
    /// references.
    pub default_mode: String,
}

/// Delivery-plugin audio capabilities. Declares the device
/// path the plugin owns (an ALSA pcm name, a JACK port name,
/// etc.) and the formats it accepts. The reconciliation
/// engine probes the device's actual hardware capability via
/// the declared `formats_query` mechanism at admission so the
/// substrate-published capability set is authoritative even
/// when the manifest's `audio_formats` list is conservative.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeliveryCapabilities {
    /// Abstract input data class (typically `"audio.pcm"` for
    /// most delivery plugins; `"audio.encoded"` for
    /// HDMI-passthrough-to-AVR cases).
    pub input_kind: String,
    /// Device path or identifier — `"alsa:hw:0,0"`,
    /// `"jack:system:playback_1,playback_2"`, etc. The
    /// framework treats the prefix before the first `:` as
    /// the substrate hint (alsa / jack / shm) and the
    /// remainder as the substrate-specific identifier.
    pub device: String,
    /// Probe mechanism the framework uses at admission to
    /// query the device's actual hardware capability —
    /// `"alsa_hw_params"` / `"jack_port_query"` / etc. When
    /// `None`, the framework treats `audio_formats` as
    /// authoritative without probing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formats_query: Option<String>,
    /// Audio formats the delivery plugin can accept. Acts as
    /// the manifest's stated capability; the framework
    /// intersects this with probe results when `formats_query`
    /// is set. Non-empty when `input_kind` names an audio
    /// kind.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio_formats: Vec<crate::audio::AudioFormatDecl>,
    /// `true` when the plugin opens the underlying device in
    /// exclusive / hardware-direct mode (ALSA `hw:` /
    /// JACK direct routing). Required for bit-perfect chains —
    /// the framework refuses to wire a bit-perfect topology
    /// through a delivery plugin that declares
    /// `exclusive_mode = false`.
    #[serde(default)]
    pub exclusive_mode: bool,
    /// `true` when the delivery plugin promises bit-perfect
    /// output to the underlying hardware. Same self-declared-
    /// promise contract as the source-side equivalent; the
    /// topology validator catches chains that claim
    /// bit-perfect but require conversion.
    #[serde(default)]
    pub bit_perfect_capable: bool,
}

/// Direction in which a gateway plugin bridges between an
/// external ecosystem and the multi-room native protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayDirection {
    /// External-system → evo group. The plugin pulls
    /// content originating in the external ecosystem (e.g.
    /// an iPhone running AirPlay) into a multi-room group
    /// for playback. Typically also a source plugin.
    InboundSource,
    /// evo group → external-system. The plugin pushes the
    /// group's audio output to a non-evo receiver
    /// (e.g. a Roon Ready endpoint, a Cast group). Typically
    /// also a delivery plugin.
    OutboundSink,
    /// Bidirectional gateway — the plugin bridges in both
    /// directions for protocols whose contract requires it.
    Bidirectional,
}

/// Gateway-plugin declaration. Present on plugins that
/// bridge between an external ecosystem and the multi-room
/// native protocol.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GatewayCapabilities {
    /// Protocol identifier — vendor-defined. Conventional
    /// values: `airplay2`, `chromecast`, `roon-ready`,
    /// `spotify-connect`, `heos`, `musiccast`, `sonos`. The
    /// framework does not enforce a vocabulary; vendor
    /// distributions decide their own protocol naming
    /// scheme.
    pub protocol: String,

    /// Bridge direction.
    pub direction: GatewayDirection,

    /// Whether the protocol is licensed / certified by the
    /// upstream ecosystem owner (Apple, Roon Labs, Spotify,
    /// etc.). Framework reference distributions reject
    /// licensed-protocol plugins; vendor distributions that
    /// hold the certification accept. Operator surfaces
    /// surface the flag for transparency. Default `false`
    /// (an open / royalty-free protocol).
    #[serde(default)]
    pub licensed: bool,
}

impl GatewayCapabilities {
    /// Validate the declaration. Refuses an empty protocol
    /// identifier; the protocol value must be operator-
    /// readable and nonempty after trim.
    pub fn validate(&self) -> Result<(), String> {
        if self.protocol.trim().is_empty() {
            return Err("gateway.protocol must not be empty".to_string());
        }
        Ok(())
    }
}

/// Warden-specific capabilities per `PLUGIN_PACKAGING.md` section 2.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WardenCapabilities {
    /// What this warden takes custody of, e.g. `playback`.
    ///
    /// **Bucket: Distribution-owned.** The catalogue and shelf
    /// shapes determine which custody domains are valid; the
    /// steward consults the field for diagnostics and for the
    /// `custody_exclusive` interlock but does not itself enumerate
    /// or constrain the domain string.
    pub custody_domain: String,
    /// Whether another warden of the same domain may coexist.
    ///
    /// **Bucket: Enforced.** Admission refuses a second warden
    /// declaring the same `custody_domain` when either declares
    /// `custody_exclusive = true`. The interlock prevents two
    /// wardens from racing for the same custody.
    pub custody_exclusive: bool,
    /// Deadline for fast-path course corrections.
    ///
    /// **Bucket: Enforced.** Used by the router to populate
    /// `EnforcementPolicy::course_correction_deadline_ms` at
    /// admission. Every `course_correct` call is wrapped in a
    /// `tokio::time::timeout` of this duration; expiry surfaces
    /// the configured `custody_failure_mode` to the operator.
    pub course_correction_budget_ms: u32,
    /// Behaviour when a custody operation fails.
    ///
    /// **Bucket: Enforced.** Used by the router to populate
    /// `EnforcementPolicy::custody_failure_mode`. On any failure of
    /// the `course_correct` dispatch path (warden-returned error or
    /// `course_correction_budget_ms` deadline expiry), the router
    /// branches on this value before propagating the dispatch error:
    ///
    /// - **`Abort`** marks the custody record as `aborted` on the
    ///   ledger and emits a `custody_aborted` happening on the
    ///   durable bus carrying the steward-recorded failure reason.
    ///   The custody is over from the steward's point of view; the
    ///   warden is expected to release the handle on its next
    ///   opportunity. Consumers acting on the handle should treat
    ///   this as a hard stop signal.
    /// - **`PartialOk`** marks the custody record as `degraded` on
    ///   the ledger and emits a `custody_degraded` happening on the
    ///   durable bus carrying the steward-recorded failure reason.
    ///   The warden is not released and may continue to report
    ///   state on the same handle; consumers decide whether to keep
    ///   consuming partial results or to stop.
    ///
    /// In both cases the dispatch error is propagated to the caller
    /// unchanged; the differential is the ledger transition and the
    /// happening variant. Take-custody and release-custody paths
    /// today behave uniformly as if `Abort` were declared (they
    /// have no failure-mode hook); when those paths grow a
    /// failure-mode-aware surface, this field will gate them in
    /// the same way.
    pub custody_failure_mode: CustodyFailureMode,
    /// Verb names this warden's `course_correct` accepts.
    ///
    /// **Bucket: Enforced.** Parallel to a respondent's
    /// `request_types`. When set, the router refuses any
    /// `course_correct` whose `correction_type` is not in this
    /// list with a structured `StewardError::Dispatch` naming the
    /// offending verb and the declared set; the warden's
    /// `course_correct` body never sees undeclared verbs.
    ///
    /// `None` (legacy plugins authored before this field existed):
    /// no verb gating; the warden handles unknown verbs in its
    /// own implementation. Plugins targeting a recent
    /// `prerequisites.evo_min_version` are expected to declare
    /// the field; the framework's admission-time skew policy
    /// decides whether to enforce, warn, or refuse based on the
    /// declared minimum.
    ///
    /// Empty `Some(vec![])` is invalid: a warden with zero
    /// declared verbs cannot perform any course corrections, so
    /// declaring zero is a manifest error caught at validate
    /// time.
    ///
    /// Future-compat: when fast-path dispatch ships its own
    /// `fast_path_verbs` list, manifest validation will require
    /// `fast_path_verbs ⊆ course_correct_verbs` (Fast Path is a
    /// latency-bounded variant of the same dispatch surface, not
    /// a different one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub course_correct_verbs: Option<Vec<String>>,

    /// Optional per-verb capability requirements for warden
    /// course-correct verbs.
    ///
    /// **Bucket: Enforced.** Parallel to
    /// [`RespondentCapabilities::verb_capabilities`] but scoped to
    /// the warden's `course_correct` verb surface. Keys MUST be a
    /// subset of [`Self::course_correct_verbs`] when that field is
    /// `Some(_)`; when `course_correct_verbs` is `None` (legacy)
    /// the map MUST be empty (a legacy warden cannot declare
    /// per-verb capability requirements without first declaring
    /// its verb list). Verbs declared in `course_correct_verbs`
    /// but absent from this map default to [`VerbCapability::None`]
    /// — anonymous-OK.
    ///
    /// The framework dispatcher consults this map on every
    /// course-correct dispatch (both slow path and Fast Path) and
    /// enforces the declared capability against the principal's
    /// granted-capabilities set BEFORE forwarding the verb to the
    /// warden's `course_correct`. A failed check refuses with the
    /// structured `permission_denied` error; the warden never
    /// sees the call. Wardens MUST NOT re-check the principal in
    /// `course_correct` — the dispatcher's check is the
    /// authoritative gate.
    ///
    /// Optional + backwards-compatible: legacy manifests authored
    /// before this field existed remain valid; their verbs default
    /// to `None`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub verb_capabilities: BTreeMap<String, VerbCapability>,

    /// Verb names this warden serves on the Fast Path channel.
    ///
    /// **Bucket: Enforced.** Subset of
    /// [`Self::course_correct_verbs`] — Fast Path is a
    /// latency-bounded variant of the same dispatch surface, not
    /// a different one. A `course_correct` verb omitted here is
    /// reachable only on the slow path; a verb listed here MUST
    /// also appear in `course_correct_verbs` (validation
    /// enforces the subset). Operators issuing Fast Path frames
    /// against a verb absent from this list see
    /// `not_found / not_fast_path_eligible`.
    ///
    /// `None` (default) means this warden is unreachable on
    /// Fast Path; `Some(vec![...])` opts in a specific verb
    /// list. Empty `Some(vec![])` is invalid: a warden that
    /// declares zero Fast Path verbs cannot serve any, so
    /// declaring zero is a manifest error caught at validate
    /// time (mirrors the `course_correct_verbs` shape).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_path_verbs: Option<Vec<String>>,

    /// Per-warden Fast Path dispatch budget in milliseconds.
    ///
    /// **Bucket: Enforced.** Bounds the steward's dispatch +
    /// serialise overhead plus the warden's `course_correct`
    /// execution on the Fast Path channel; calls exceeding budget
    /// refuse with the structured
    /// `unavailable / fast_path_budget_exceeded` error taxonomy.
    /// `None` defaults to
    /// [`FAST_PATH_BUDGET_MS_DEFAULT`] at admission. Values above
    /// [`FAST_PATH_BUDGET_MS_MAX`] are clamped at admission with
    /// a warning trace; the framework refuses to grant a Fast
    /// Path budget greater than the max because a 5-second-budget
    /// warden would defeat the whole point of the channel.
    ///
    /// Distinct from
    /// [`Self::course_correction_budget_ms`] — that field bounds
    /// slow-path course-correct dispatch and may legitimately be
    /// in the multi-second range; this field bounds Fast Path
    /// dispatch and is capped at 200ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_path_budget_ms: Option<u32>,

    /// Per-verb Fast Path coalesce windows in milliseconds.
    ///
    /// **Bucket: Distribution-owned.** Map of verb-name -> debounce
    /// window. Multiple Fast Path frames for the same verb arriving
    /// within the window collapse to a single dispatch (the most
    /// recent payload wins). `None` (default) and missing keys mean
    /// no coalescing for the corresponding verb — every frame
    /// dispatches.
    ///
    /// Used by the warden as a safety net against badly-behaved
    /// input plugins (a touch slider emitting 1000 Hz volume
    /// changes coalesces to one dispatch every 20 ms). The right
    /// per-verb cap is application-specific; a volume slider
    /// might want 20 ms while a pause/resume verb wants 0 (every
    /// frame matters).
    ///
    /// Validation: every key in this map MUST appear in
    /// [`Self::fast_path_verbs`]. Per-verb coalescing on a verb
    /// the warden does not declare on Fast Path is a manifest
    /// authoring error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_path_coalesce_ms: Option<std::collections::BTreeMap<String, u32>>,
}

/// Custody failure modes per `PLUGIN_PACKAGING.md` section 2.
///
/// Selects the steward's bookkeeping and signalling behaviour when a
/// custody operation on a warden's handle fails. The semantic is
/// pinned by the `course_correct` dispatch path; see
/// [`WardenCapabilities::custody_failure_mode`] for the full
/// description of what each mode causes the steward to record and
/// emit.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CustodyFailureMode {
    /// A custody operation failure terminates the work under
    /// custody. The steward marks the custody record as `aborted`,
    /// emits `custody_aborted` on the durable happenings bus, and
    /// propagates the dispatch error. The warden is expected to
    /// release the handle on its next opportunity.
    Abort,
    /// A custody operation failure leaves partial results intact.
    /// The steward marks the custody record as `degraded`, emits
    /// `custody_degraded` on the durable happenings bus, and
    /// propagates the dispatch error. The warden is not released;
    /// further state reports on the same handle remain valid.
    PartialOk,
}

/// Factory-specific capabilities per `PLUGIN_PACKAGING.md` section 2.
///
/// **Bucket-level note**: every field on this struct is currently
/// **Reserved**. Factory plugins are not yet admitted by the v0
/// steward (admission rejects `kind.instance = "factory"` at the
/// pre-admission validation pass). The fields are parsed for
/// forward-compat against the future factory-admission path; their
/// values do not influence behaviour today because no factory
/// reaches admission.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FactoryCapabilities {
    /// Maximum number of concurrent instances the steward admits from this
    /// factory. Announcements beyond this are refused.
    ///
    /// **Bucket: Reserved.** Will become Enforced when factory admission
    /// lands; today the field is parsed but no factory passes admission.
    pub max_instances: u32,
    /// Instance TTL in seconds. `0` means no TTL; instances live until
    /// retracted by the factory.
    ///
    /// **Bucket: Reserved.** Same disposition as `max_instances`.
    pub instance_ttl_seconds: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_singleton_respondent() -> &'static str {
        r#"
[plugin]
name = "com.example.metadata.local"
version = "0.1.0"
contract = 1

[target]
shelf = "metadata.providers"
shape = 2

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "out-of-process"
exec = "plugin.bin"

[trust]
class = "unprivileged"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = false
filesystem_scopes = []

[resources]
max_memory_mb = 64
max_cpu_percent = 5

[lifecycle]
hot_reload = "restart"
autostart = true
restart_on_crash = true
restart_budget = 5

[capabilities.respondent]
request_types = ["metadata.query"]
response_budget_ms = 5000
"#
    }

    fn valid_factory_warden() -> &'static str {
        r#"
[plugin]
name = "com.example.sessions"
version = "1.2.3"
contract = 1

[target]
shelf = "sessions.active"
shape = 1

[kind]
instance = "factory"
interaction = "warden"

[transport]
type = "in-process"
exec = "plugin.so"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = true
filesystem_scopes = ["/var/lib/evo/plugins/com.example.sessions/state"]

[resources]
max_memory_mb = 128
max_cpu_percent = 10

[lifecycle]
hot_reload = "live"
autostart = true
restart_on_crash = true
restart_budget = 3

[capabilities.warden]
custody_domain = "session"
custody_exclusive = false
course_correction_budget_ms = 100
custody_failure_mode = "abort"

[capabilities.factory]
max_instances = 32
instance_ttl_seconds = 0
"#
    }

    #[test]
    fn parses_valid_singleton_respondent() {
        let m = Manifest::from_toml(valid_singleton_respondent())
            .expect("valid manifest should parse");
        assert_eq!(m.plugin.name, "com.example.metadata.local");
        assert_eq!(m.plugin.contract, 1);
        let kind = m.require_kind();
        assert_eq!(kind.instance, InstanceShape::Singleton);
        assert_eq!(kind.interaction, InteractionShape::Respondent);
        assert_eq!(m.require_transport().kind, TransportKind::OutOfProcess);
        assert_eq!(m.trust.class, TrustClass::Unprivileged);
        assert!(m.capabilities.respondent.is_some());
        assert!(m.capabilities.warden.is_none());
        assert!(m.capabilities.factory.is_none());
    }

    #[test]
    fn parses_valid_factory_warden() {
        let m = Manifest::from_toml(valid_factory_warden())
            .expect("valid factory-warden manifest should parse");
        let kind = m.require_kind();
        assert_eq!(kind.instance, InstanceShape::Factory);
        assert_eq!(kind.interaction, InteractionShape::Warden);
        assert_eq!(m.require_transport().kind, TransportKind::InProcess);
        assert_eq!(m.trust.class, TrustClass::Platform);
        assert_eq!(m.require_lifecycle().hot_reload, HotReloadPolicy::Live);
        assert!(m.capabilities.warden.is_some());
        assert!(m.capabilities.factory.is_some());
        assert!(m.capabilities.respondent.is_none());
    }

    #[test]
    fn round_trip_singleton_respondent() {
        let m1 = Manifest::from_toml(valid_singleton_respondent()).unwrap();
        let serialised = m1.to_toml().unwrap();
        let m2 = Manifest::from_toml(&serialised).unwrap();
        assert_eq!(m1, m2);
    }

    #[test]
    fn round_trip_factory_warden() {
        let m1 = Manifest::from_toml(valid_factory_warden()).unwrap();
        let serialised = m1.to_toml().unwrap();
        let m2 = Manifest::from_toml(&serialised).unwrap();
        assert_eq!(m1, m2);
    }

    #[test]
    fn rejects_invalid_name_no_dot() {
        let toml = valid_singleton_respondent()
            .replace("com.example.metadata.local", "notanfqdn");
        match Manifest::from_toml(&toml) {
            Err(ManifestError::InvalidName(name)) => {
                assert_eq!(name, "notanfqdn");
            }
            other => panic!("expected InvalidName, got {other:?}"),
        }
    }

    #[test]
    fn rejects_invalid_name_uppercase() {
        let toml = valid_singleton_respondent()
            .replace("com.example.metadata.local", "Com.Example.Bad");
        assert!(matches!(
            Manifest::from_toml(&toml),
            Err(ManifestError::InvalidName(_))
        ));
    }

    #[test]
    fn rejects_invalid_name_leading_digit() {
        let toml = valid_singleton_respondent()
            .replace("com.example.metadata.local", "1com.example.bad");
        assert!(matches!(
            Manifest::from_toml(&toml),
            Err(ManifestError::InvalidName(_))
        ));
    }

    #[test]
    fn rejects_unsupported_contract_version() {
        let toml = valid_singleton_respondent()
            .replace("contract = 1", "contract = 2");
        match Manifest::from_toml(&toml) {
            Err(ManifestError::UnsupportedContractVersion(v)) => {
                assert_eq!(v, 2)
            }
            other => {
                panic!("expected UnsupportedContractVersion, got {other:?}")
            }
        }
    }

    #[test]
    fn rejects_warden_interaction_without_warden_capabilities() {
        let toml = valid_singleton_respondent().replace(
            r#"interaction = "respondent""#,
            r#"interaction = "warden""#,
        );
        assert!(matches!(
            Manifest::from_toml(&toml),
            Err(ManifestError::InconsistentCapabilities(_))
        ));
    }

    #[test]
    fn rejects_singleton_with_factory_capabilities() {
        // Start from the factory-warden manifest and turn its instance back to
        // singleton without removing the factory capabilities section.
        let toml = valid_factory_warden()
            .replace(r#"instance = "factory""#, r#"instance = "singleton""#);
        assert!(matches!(
            Manifest::from_toml(&toml),
            Err(ManifestError::InconsistentCapabilities(_))
        ));
    }

    #[test]
    fn rejects_factory_without_factory_capabilities() {
        let mut toml = valid_singleton_respondent().to_string();
        // Flip to factory without adding [capabilities.factory].
        toml = toml
            .replace(r#"instance = "singleton""#, r#"instance = "factory""#);
        assert!(matches!(
            Manifest::from_toml(&toml),
            Err(ManifestError::InconsistentCapabilities(_))
        ));
    }

    #[test]
    fn rejects_missing_required_fields() {
        // Drop the [resources] table entirely. As of the
        // executable / artefact section split, the four
        // executable-only sections are typed as Option<T>;
        // omitting [resources] from a functional manifest
        // surfaces the structured `MissingExecutableSection`
        // diagnostic naming the offending block, rather than
        // a serde ParseError on an unknown / missing field.
        let toml = valid_singleton_respondent()
            .lines()
            .filter(|l| {
                !l.starts_with("[resources]")
                    && !l.starts_with("max_memory_mb")
                    && !l.starts_with("max_cpu_percent")
            })
            .collect::<Vec<_>>()
            .join("\n");
        match Manifest::from_toml(&toml) {
            Err(ManifestError::MissingExecutableSection { kind, section }) => {
                assert_eq!(kind, "functional");
                assert_eq!(section, "resources");
            }
            other => {
                panic!("expected MissingExecutableSection, got {other:?}")
            }
        }
    }

    #[test]
    fn trust_class_ordering() {
        // TrustClass derives Ord; the highest trust is Platform and the
        // lowest is Sandbox. Confirm the ordering matches the document.
        assert!(TrustClass::Platform < TrustClass::Privileged);
        assert!(TrustClass::Privileged < TrustClass::Standard);
        assert!(TrustClass::Standard < TrustClass::Unprivileged);
        assert!(TrustClass::Unprivileged < TrustClass::Sandbox);
    }

    #[test]
    fn trust_class_round_trips_under_non_exhaustive() {
        // TrustClass is `#[non_exhaustive]`: downstream pattern matches
        // must include a wildcard arm. The attribute does not affect
        // same-crate construction or serialisation — this test pins that
        // invariant by exercising every variant through TOML serde and
        // confirming round-trip equality.
        for class in [
            TrustClass::Platform,
            TrustClass::Privileged,
            TrustClass::Standard,
            TrustClass::Unprivileged,
            TrustClass::Sandbox,
        ] {
            let trust = Trust { class };
            let toml = toml::to_string(&trust).expect("serialise trust");
            let parsed: Trust =
                toml::from_str(&toml).expect("deserialise trust");
            assert_eq!(parsed.class, class);
        }
    }

    #[test]
    fn instance_and_interaction_serialise_as_lowercase() {
        let toml_snippet = toml::to_string(&Kind {
            instance: InstanceShape::Factory,
            interaction: InteractionShape::Warden,
        })
        .unwrap();
        assert!(toml_snippet.contains(r#"instance = "factory""#));
        assert!(toml_snippet.contains(r#"interaction = "warden""#));
    }

    #[test]
    fn transport_kind_serialises_as_kebab_case() {
        let toml_snippet = toml::to_string(&Transport {
            kind: TransportKind::OutOfProcess,
            exec: "plugin.bin".to_string(),
        })
        .unwrap();
        assert!(toml_snippet.contains(r#"type = "out-of-process""#));
    }

    // -----------------------------------------------------------------
    // check_prerequisites: in-scope half of `[prerequisites]`.
    // -----------------------------------------------------------------

    #[test]
    fn check_prerequisites_accepts_matching_os_and_lower_required_version() {
        let m = Manifest::from_toml(valid_singleton_respondent()).unwrap();
        // Fixture declares evo_min_version = 0.1.0, os_family = linux.
        m.check_prerequisites(&Version::new(0, 1, 8), "linux")
            .expect("should admit: running version >= required, os matches");
    }

    #[test]
    fn check_prerequisites_accepts_equal_required_version() {
        let m = Manifest::from_toml(valid_singleton_respondent()).unwrap();
        // Boundary: running version exactly equals required.
        m.check_prerequisites(&Version::new(0, 1, 0), "linux")
            .expect("equal version must pass; the check is strict >");
    }

    #[test]
    fn check_prerequisites_rejects_required_version_above_running() {
        let m = Manifest::from_toml(valid_singleton_respondent()).unwrap();
        // Fixture requires >= 0.1.0; pretend we are running 0.0.9.
        match m.check_prerequisites(&Version::new(0, 0, 9), "linux") {
            Err(ManifestError::EvoVersionTooLow { required, running }) => {
                assert_eq!(required, Version::new(0, 1, 0));
                assert_eq!(running, Version::new(0, 0, 9));
            }
            other => panic!("expected EvoVersionTooLow, got {other:?}"),
        }
    }

    #[test]
    fn check_prerequisites_rejects_future_major() {
        // A plugin built against a future major must be refused by a
        // current-major steward even if the minor would satisfy.
        let toml = valid_singleton_respondent().replace(
            r#"evo_min_version = "0.1.0""#,
            r#"evo_min_version = "1.0.0""#,
        );
        let m = Manifest::from_toml(&toml).unwrap();
        assert!(matches!(
            m.check_prerequisites(&Version::new(0, 9, 9), "linux"),
            Err(ManifestError::EvoVersionTooLow { .. })
        ));
    }

    #[test]
    fn check_prerequisites_accepts_os_family_any() {
        // os_family = "any" matches every host OS, including ones the
        // steward has never seen before.
        let toml = valid_singleton_respondent()
            .replace(r#"os_family = "linux""#, r#"os_family = "any""#);
        let m = Manifest::from_toml(&toml).unwrap();
        m.check_prerequisites(&Version::new(0, 1, 8), "linux")
            .unwrap();
        m.check_prerequisites(&Version::new(0, 1, 8), "macos")
            .unwrap();
        m.check_prerequisites(&Version::new(0, 1, 8), "freebsd")
            .unwrap();
        // Even an OS string the SDK's schema has no opinion about.
        m.check_prerequisites(&Version::new(0, 1, 8), "plan9")
            .unwrap();
    }

    #[test]
    fn check_prerequisites_accepts_matching_specific_os_family() {
        let toml = valid_singleton_respondent()
            .replace(r#"os_family = "linux""#, r#"os_family = "macos""#);
        let m = Manifest::from_toml(&toml).unwrap();
        m.check_prerequisites(&Version::new(0, 1, 8), "macos")
            .expect("exact match should pass");
    }

    #[test]
    fn check_prerequisites_rejects_mismatched_os_family() {
        // Fixture declares os_family = linux; simulate a macOS host.
        let m = Manifest::from_toml(valid_singleton_respondent()).unwrap();
        match m.check_prerequisites(&Version::new(0, 1, 8), "macos") {
            Err(ManifestError::OsFamilyMismatch { required, running }) => {
                assert_eq!(required, "linux");
                assert_eq!(running, "macos");
            }
            other => panic!("expected OsFamilyMismatch, got {other:?}"),
        }
    }

    #[test]
    fn check_prerequisites_rejects_version_before_os() {
        // When both checks would fail, version is tested first. Pin
        // the order so a future refactor does not silently swap them
        // (which would change the error surface callers observe).
        let toml = valid_singleton_respondent()
            .replace(
                r#"evo_min_version = "0.1.0""#,
                r#"evo_min_version = "9.9.9""#,
            )
            .replace(r#"os_family = "linux""#, r#"os_family = "macos""#);
        let m = Manifest::from_toml(&toml).unwrap();
        assert!(matches!(
            m.check_prerequisites(&Version::new(0, 1, 8), "linux"),
            Err(ManifestError::EvoVersionTooLow { .. })
        ));
    }

    // -----------------------------------------------------------------
    // capabilities.admin manifest flag.
    //
    // The flag is orthogonal to `Kind`: the validator must accept it
    // on any plugin kind (respondent, warden, factory) without
    // requiring a matching [capabilities.admin.*] sub-table, because
    // `admin` is a boolean flag not a sub-table. Tests exercise the
    // parse path, the default, and round-trip.
    // -----------------------------------------------------------------

    #[test]
    fn accepts_manifest_with_admin_true() {
        // admin = true is accepted on any plugin kind. Fixture is a
        // singleton respondent; the flag rides on [capabilities]
        // alongside the respondent capability sub-table. The
        // explicit [capabilities] header is necessary because TOML
        // bare keys belong to the most recently opened table: a
        // stray `admin = true` with no preceding [capabilities]
        // header lands in [lifecycle] and is silently dropped as
        // an unknown field (serde default behaviour).
        let toml = valid_singleton_respondent().replace(
            "[capabilities.respondent]",
            "[capabilities]\nadmin = true\n\n[capabilities.respondent]",
        );
        let m = Manifest::from_toml(&toml)
            .expect("admin = true must be accepted on a respondent");
        assert!(m.capabilities.admin);
    }

    #[test]
    fn defaults_admin_to_false_when_absent() {
        // Omitted admin flag defaults to false. Existing manifests
        // without explicit admin opt-in must remain valid without
        // modification.
        let m = Manifest::from_toml(valid_singleton_respondent()).unwrap();
        assert!(
            !m.capabilities.admin,
            "admin must default to false when absent"
        );
    }

    #[test]
    fn admin_true_round_trips_in_toml() {
        // Deserialise, reserialise, deserialise. The admin = true
        // value must survive the round trip intact. See
        // accepts_manifest_with_admin_true for why the explicit
        // [capabilities] header is required in the input fixture.
        let toml = valid_singleton_respondent().replace(
            "[capabilities.respondent]",
            "[capabilities]\nadmin = true\n\n[capabilities.respondent]",
        );
        let m1 = Manifest::from_toml(&toml).unwrap();
        assert!(m1.capabilities.admin);
        let serialised = m1.to_toml().unwrap();
        let m2 = Manifest::from_toml(&serialised).unwrap();
        assert!(m2.capabilities.admin);
        assert_eq!(m1, m2);
    }

    #[test]
    fn lifecycle_recommended_essential_defaults_to_false() {
        let m = Manifest::from_toml(valid_singleton_respondent())
            .expect("valid manifest should parse");
        assert!(
            !m.require_lifecycle().recommended_essential,
            "default must be false; the field is opt-in advisory"
        );
    }

    #[test]
    fn lifecycle_recommended_essential_round_trips_when_declared() {
        let toml = valid_singleton_respondent().replace(
            "restart_budget = 5",
            "restart_budget = 5\nrecommended_essential = true",
        );
        let m = Manifest::from_toml(&toml).expect("manifest should parse");
        assert!(m.require_lifecycle().recommended_essential);
        let round_tripped = Manifest::from_toml(&m.to_toml().unwrap()).unwrap();
        assert!(round_tripped.require_lifecycle().recommended_essential);
    }

    // ---------------------------------------------------------------
    // Fast Path foundation tests. Cover the manifest fields that gate
    // Fast Path admission: capabilities.fast_path (sender flag),
    // capabilities.warden.fast_path_verbs / .fast_path_budget_ms /
    // .fast_path_coalesce_ms (warden-side declarations).
    // ---------------------------------------------------------------

    /// A warden manifest fragment for Fast Path tests. Includes a
    /// minimal `course_correct_verbs` list so subset checks have
    /// something to refer to.
    fn warden_with_course_correct_verbs() -> String {
        // Same shape as valid_factory_warden but a singleton (the
        // factory-admission path is still under construction; using
        // a singleton avoids tripping the orthogonal factory gate).
        valid_factory_warden()
            .replace(
                r#"instance = "factory""#,
                r#"instance = "singleton""#,
            )
            .replace(
                "[capabilities.factory]\nmax_instances = 32\ninstance_ttl_seconds = 0\n",
                "",
            )
            .replace(
                "custody_failure_mode = \"abort\"",
                r#"custody_failure_mode = "abort"
course_correct_verbs = ["volume_set", "mute", "pause", "resume", "seek"]"#,
            )
    }

    #[test]
    fn warden_may_also_declare_respondent_capabilities() {
        // A warden plugin that ALSO handles request-type
        // dispatch alongside its course_correct surface
        // — typical for source-verb-handling wardens that
        // own one or more URI schemes — declares both
        // [capabilities.warden] and [capabilities.respondent].
        // The interaction-shape exclusivity used to refuse
        // this; it is now allowed asymmetrically (warden
        // may have respondent block; the reverse is still
        // refused since respondents do not hold custody).
        let toml = warden_with_course_correct_verbs()
            + r#"

[capabilities.respondent]
request_types = ["play_now", "stop", "pause", "resume", "seek"]
response_budget_ms = 1000
"#;
        let m = Manifest::from_toml(&toml).expect(
            "warden plugin with [capabilities.respondent] must validate",
        );
        let warden =
            m.capabilities.warden.expect("warden capabilities present");
        assert!(warden.course_correct_verbs.is_some());
        let respondent = m
            .capabilities
            .respondent
            .expect("respondent capabilities present");
        assert_eq!(respondent.request_types.len(), 5);
        assert!(respondent.request_types.contains(&"play_now".to_string()));
    }

    #[test]
    fn respondent_still_cannot_declare_warden_capabilities() {
        // Asymmetric relaxation: warden + respondent is
        // allowed, but respondent + warden is not. A
        // respondent that claims warden capabilities is
        // structurally a warden under a misnamed
        // interaction; the rejection guides the author to
        // declare interaction = "warden" instead.
        let toml = valid_singleton_respondent().to_string()
            + r#"

[capabilities.warden]
custody_domain = "playback"
custody_exclusive = true
course_correction_budget_ms = 1000
custody_failure_mode = "abort"
course_correct_verbs = ["pause"]
"#;
        let err = Manifest::from_toml(&toml).expect_err(
            "respondent plugin with [capabilities.warden] must refuse",
        );
        match err {
            ManifestError::InconsistentCapabilities(msg) => {
                assert!(
                    msg.contains("warden")
                        && msg.contains("interaction = warden"),
                    "rejection message should guide the author: {msg:?}"
                );
            }
            other => panic!("expected InconsistentCapabilities, got {other:?}"),
        }
    }

    #[test]
    fn defaults_fast_path_to_false_when_absent() {
        // Existing manifests without explicit Fast Path opt-in
        // must remain valid without modification.
        let m = Manifest::from_toml(valid_singleton_respondent()).unwrap();
        assert!(
            !m.capabilities.fast_path,
            "fast_path must default to false when absent"
        );
    }

    #[test]
    fn accepts_manifest_with_fast_path_true() {
        let toml = valid_singleton_respondent().replace(
            "[capabilities.respondent]",
            "[capabilities]\nfast_path = true\n\n[capabilities.respondent]",
        );
        let m = Manifest::from_toml(&toml)
            .expect("fast_path = true must be accepted on a respondent");
        assert!(m.capabilities.fast_path);
    }

    #[test]
    fn fast_path_true_round_trips_in_toml() {
        let toml = valid_singleton_respondent().replace(
            "[capabilities.respondent]",
            "[capabilities]\nfast_path = true\n\n[capabilities.respondent]",
        );
        let m1 = Manifest::from_toml(&toml).unwrap();
        assert!(m1.capabilities.fast_path);
        let serialised = m1.to_toml().unwrap();
        let m2 = Manifest::from_toml(&serialised).unwrap();
        assert!(m2.capabilities.fast_path);
        assert_eq!(m1, m2);
    }

    #[test]
    fn defaults_fast_path_warden_fields_to_none_when_absent() {
        let m = Manifest::from_toml(valid_factory_warden()).unwrap();
        let warden = m.capabilities.warden.expect("warden block present");
        assert!(warden.fast_path_verbs.is_none());
        assert!(warden.fast_path_budget_ms.is_none());
        assert!(warden.fast_path_coalesce_ms.is_none());
    }

    #[test]
    fn accepts_warden_with_fast_path_verbs_subset_of_course_correct_verbs() {
        let base = warden_with_course_correct_verbs();
        let toml = base.replace(
            r#"course_correct_verbs = ["volume_set", "mute", "pause", "resume", "seek"]"#,
            r#"course_correct_verbs = ["volume_set", "mute", "pause", "resume", "seek"]
fast_path_verbs = ["volume_set", "mute", "pause", "resume", "seek"]
fast_path_budget_ms = 50

[capabilities.warden.fast_path_coalesce_ms]
volume_set = 20
seek = 10"#,
        );
        let m = Manifest::from_toml(&toml)
            .expect("Fast Path declaration must parse");
        let warden = m.capabilities.warden.expect("warden block present");
        assert_eq!(
            warden.fast_path_verbs.as_deref(),
            Some(
                ["volume_set", "mute", "pause", "resume", "seek"]
                    .map(String::from)
                    .as_slice()
            )
        );
        assert_eq!(warden.fast_path_budget_ms, Some(50));
        let coalesce = warden
            .fast_path_coalesce_ms
            .expect("coalesce block present");
        assert_eq!(coalesce.get("volume_set"), Some(&20));
        assert_eq!(coalesce.get("seek"), Some(&10));
    }

    #[test]
    fn rejects_empty_fast_path_verbs_list() {
        let base = warden_with_course_correct_verbs();
        let toml = base.replace(
            r#"course_correct_verbs = ["volume_set", "mute", "pause", "resume", "seek"]"#,
            r#"course_correct_verbs = ["volume_set", "mute", "pause", "resume", "seek"]
fast_path_verbs = []"#,
        );
        let err = Manifest::from_toml(&toml)
            .expect_err("an empty Fast Path verb list must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("fast_path_verbs") && msg.contains("empty"),
            "error must name the field and the empty-list problem: {msg}"
        );
    }

    #[test]
    fn rejects_duplicate_fast_path_verb_entries() {
        let base = warden_with_course_correct_verbs();
        let toml = base.replace(
            r#"course_correct_verbs = ["volume_set", "mute", "pause", "resume", "seek"]"#,
            r#"course_correct_verbs = ["volume_set", "mute", "pause", "resume", "seek"]
fast_path_verbs = ["volume_set", "mute", "volume_set"]"#,
        );
        let err = Manifest::from_toml(&toml)
            .expect_err("duplicate Fast Path verb entries must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("duplicate") && msg.contains("volume_set"),
            "error must name the duplicate verb: {msg}"
        );
    }

    #[test]
    fn rejects_fast_path_verb_not_in_course_correct_verbs() {
        // The whole point of the subset rule: a Fast Path verb
        // that the warden does not declare on the slow-path
        // dispatch surface either is unreachable (bug) or skirts
        // the slow-path verb gating (security risk). Reject it.
        let base = warden_with_course_correct_verbs();
        let toml = base.replace(
            r#"course_correct_verbs = ["volume_set", "mute", "pause", "resume", "seek"]"#,
            r#"course_correct_verbs = ["volume_set", "mute"]
fast_path_verbs = ["volume_set", "mute", "secret_admin_verb"]"#,
        );
        let err = Manifest::from_toml(&toml)
            .expect_err("Fast Path verb must be in course_correct_verbs");
        let msg = err.to_string();
        assert!(
            msg.contains("secret_admin_verb")
                && msg.contains("course_correct_verbs"),
            "error must name the offending verb and the subset rule: {msg}"
        );
    }

    #[test]
    fn rejects_zero_fast_path_budget() {
        let base = warden_with_course_correct_verbs();
        let toml = base.replace(
            r#"course_correct_verbs = ["volume_set", "mute", "pause", "resume", "seek"]"#,
            r#"course_correct_verbs = ["volume_set", "mute", "pause", "resume", "seek"]
fast_path_verbs = ["volume_set"]
fast_path_budget_ms = 0"#,
        );
        let err = Manifest::from_toml(&toml)
            .expect_err("a zero-millisecond Fast Path budget is unreachable");
        let msg = err.to_string();
        assert!(
            msg.contains("fast_path_budget_ms"),
            "error must name the field: {msg}"
        );
    }

    #[test]
    fn rejects_coalesce_for_undeclared_verb() {
        let base = warden_with_course_correct_verbs();
        let toml = base.replace(
            r#"course_correct_verbs = ["volume_set", "mute", "pause", "resume", "seek"]"#,
            r#"course_correct_verbs = ["volume_set", "mute", "pause", "resume", "seek"]
fast_path_verbs = ["volume_set", "mute"]
fast_path_budget_ms = 50

[capabilities.warden.fast_path_coalesce_ms]
seek = 10"#,
        );
        let err = Manifest::from_toml(&toml)
            .expect_err("coalesce must reference a declared Fast Path verb");
        let msg = err.to_string();
        assert!(
            msg.contains("seek") && msg.contains("fast_path_verbs"),
            "error must name the undeclared verb and the rule: {msg}"
        );
    }

    #[test]
    fn rejects_zero_coalesce_window() {
        let base = warden_with_course_correct_verbs();
        let toml = base.replace(
            r#"course_correct_verbs = ["volume_set", "mute", "pause", "resume", "seek"]"#,
            r#"course_correct_verbs = ["volume_set", "mute", "pause", "resume", "seek"]
fast_path_verbs = ["volume_set"]
fast_path_budget_ms = 50

[capabilities.warden.fast_path_coalesce_ms]
volume_set = 0"#,
        );
        let err = Manifest::from_toml(&toml)
            .expect_err("a zero-millisecond coalesce window is meaningless");
        let msg = err.to_string();
        assert!(
            msg.contains("volume_set"),
            "error must name the offending verb: {msg}"
        );
    }

    #[test]
    fn fast_path_budget_constants_pin_to_documented_values() {
        // Pin the constants so a future contributor cannot quietly
        // raise the budget cap without updating the public
        // engineering documentation. Default 50ms, framework-
        // enforced max 200ms.
        assert_eq!(FAST_PATH_BUDGET_MS_DEFAULT, 50);
        assert_eq!(FAST_PATH_BUDGET_MS_MAX, 200);
    }

    #[test]
    fn fast_path_warden_block_round_trips_in_toml() {
        let base = warden_with_course_correct_verbs();
        let toml = base.replace(
            r#"course_correct_verbs = ["volume_set", "mute", "pause", "resume", "seek"]"#,
            r#"course_correct_verbs = ["volume_set", "mute", "pause", "resume", "seek"]
fast_path_verbs = ["volume_set", "mute"]
fast_path_budget_ms = 75

[capabilities.warden.fast_path_coalesce_ms]
volume_set = 20"#,
        );
        let m1 = Manifest::from_toml(&toml).unwrap();
        let serialised = m1.to_toml().unwrap();
        let m2 = Manifest::from_toml(&serialised).unwrap();
        assert_eq!(m1, m2);
    }

    fn valid_source_respondent() -> String {
        // A respondent that also owns the `evo-test` URI scheme and
        // declares the play-control verbs needed for plan-engine
        // dispatch.
        valid_singleton_respondent()
            .replace(
                r#"request_types = ["metadata.query"]"#,
                r#"request_types = ["play_now", "play_now_collection", "stop"]"#,
            )
            .to_string()
            + r#"
[capabilities.source]
uri_schemes = ["evo-test"]
"#
    }

    #[test]
    fn parses_source_capabilities() {
        let m = Manifest::from_toml(&valid_source_respondent())
            .expect("source-respondent manifest should parse");
        let source = m
            .capabilities
            .source
            .as_ref()
            .expect("source capabilities present");
        assert_eq!(source.uri_schemes, vec!["evo-test".to_string()]);
        // Source plugins must also be respondents.
        assert!(m.capabilities.respondent.is_some());
    }

    #[test]
    fn source_capabilities_round_trip_in_toml() {
        let m1 = Manifest::from_toml(&valid_source_respondent()).unwrap();
        let serialised = m1.to_toml().unwrap();
        let m2 = Manifest::from_toml(&serialised).unwrap();
        assert_eq!(m1, m2);
    }

    #[test]
    fn rejects_source_without_respondent() {
        // Singleton + warden interaction shape, with a bolted-on
        // [capabilities.source] block. Dispatch arrives via
        // handle_request, so a source plugin without a respondent
        // section is unreachable and admission must refuse.
        let toml = valid_factory_warden().to_string()
            + r#"
[capabilities.source]
uri_schemes = ["evo-test"]
"#;
        let err = Manifest::from_toml(&toml)
            .expect_err("source without respondent must be refused");
        assert!(
            matches!(err, ManifestError::InconsistentCapabilities(_)),
            "expected InconsistentCapabilities, got {err:?}",
        );
    }

    #[test]
    fn rejects_source_with_no_play_control_verb() {
        // Respondent declares only a non-source verb. Source plugin
        // without any play-control verb means dispatched verbs have
        // nowhere to land; admission must refuse.
        let toml = valid_singleton_respondent().to_string()
            + r#"
[capabilities.source]
uri_schemes = ["evo-test"]
"#;
        let err = Manifest::from_toml(&toml)
            .expect_err("source without play-control verb must be refused");
        assert!(
            matches!(err, ManifestError::InconsistentCapabilities(_)),
            "expected InconsistentCapabilities, got {err:?}",
        );
    }

    /// Positive-side lock for the source-plugin admission
    /// contract. A minimal source plugin — one URI scheme,
    /// one qualifying play-control verb declared on the
    /// respondent's `request_types` — MUST admit cleanly.
    /// Iterates the entire closed set of play-control verbs
    /// so a future edit that narrows the vocabulary breaks
    /// this test explicitly rather than silently reducing
    /// what device plugins may declare.
    ///
    /// Named test for the operator-facing contract Team B
    /// (device plugins) will build against: a
    /// `[capabilities.source] uri_schemes = ["dlna"]`
    /// (or `file`, `spotify`, any operator-chosen scheme)
    /// admits with, e.g., `play_now` alone.
    #[test]
    fn accepts_source_with_any_single_play_control_verb() {
        const PLAY_CONTROL_VERBS: &[&str] = &[
            "play_now",
            "play_now_collection",
            "play_next",
            "enqueue",
            "enqueue_and_start",
            "replace_queue",
            "save",
            "pause",
            "resume",
            "stop",
            "seek",
            "next",
            "previous",
        ];
        for verb in PLAY_CONTROL_VERBS {
            let toml = valid_singleton_respondent()
                .replace(
                    r#"request_types = ["metadata.query"]"#,
                    &format!(r#"request_types = ["{verb}"]"#),
                )
                .to_string()
                + r#"
[capabilities.source]
uri_schemes = ["dlna"]
"#;
            let parsed = Manifest::from_toml(&toml).unwrap_or_else(|e| {
                panic!(
                    "source manifest declaring only `{verb}` must admit; \
                     got {e:?}"
                )
            });
            assert_eq!(
                parsed
                    .capabilities
                    .source
                    .as_ref()
                    .expect("source section present")
                    .uri_schemes,
                vec!["dlna".to_string()],
                "uri_schemes must round-trip through parse for verb {verb}"
            );
        }
    }

    #[test]
    fn rejects_source_with_empty_uri_schemes() {
        let toml = valid_singleton_respondent()
            .replace(
                r#"request_types = ["metadata.query"]"#,
                r#"request_types = ["play_now"]"#,
            )
            .to_string()
            + r#"
[capabilities.source]
uri_schemes = []
"#;
        let err = Manifest::from_toml(&toml)
            .expect_err("source with empty uri_schemes must be refused");
        assert!(
            matches!(err, ManifestError::InconsistentCapabilities(_)),
            "expected InconsistentCapabilities, got {err:?}",
        );
    }

    #[test]
    fn rejects_source_with_duplicate_uri_schemes() {
        let toml = valid_singleton_respondent()
            .replace(
                r#"request_types = ["metadata.query"]"#,
                r#"request_types = ["play_now"]"#,
            )
            .to_string()
            + r#"
[capabilities.source]
uri_schemes = ["evo-test", "evo-test"]
"#;
        let err = Manifest::from_toml(&toml)
            .expect_err("source with duplicate uri_schemes must be refused");
        assert!(
            matches!(err, ManifestError::InconsistentCapabilities(_)),
            "expected InconsistentCapabilities, got {err:?}",
        );
    }

    #[test]
    fn rejects_source_with_empty_string_uri_scheme() {
        let toml = valid_singleton_respondent()
            .replace(
                r#"request_types = ["metadata.query"]"#,
                r#"request_types = ["play_now"]"#,
            )
            .to_string()
            + r#"
[capabilities.source]
uri_schemes = ["evo-test", ""]
"#;
        let err = Manifest::from_toml(&toml)
            .expect_err("source with empty-string uri scheme must be refused");
        assert!(
            matches!(err, ManifestError::InconsistentCapabilities(_)),
            "expected InconsistentCapabilities, got {err:?}",
        );
    }

    // ----- Dependency declaration tests -----

    fn manifest_with_dependencies(dep_section: &str) -> String {
        let base = valid_singleton_respondent();
        format!("{base}\n{dep_section}\n")
    }

    #[test]
    fn parses_minimal_manifest_without_dependencies_section() {
        let m = Manifest::from_toml(valid_singleton_respondent())
            .expect("valid manifest should parse");
        assert!(m.dependencies.required.is_empty());
        assert!(m.dependencies.recommended.is_empty());
        assert!(m.dependencies.conflicts_with.is_empty());
    }

    #[test]
    fn parses_dependencies_plain_string_form() {
        let toml = manifest_with_dependencies(
            r#"
[dependencies]
required = ["org.evoframework.composition.alsa"]
recommended = ["org.evoframework.metadata.local"]
conflicts_with = ["com.competing.composition"]
"#,
        );
        let m = Manifest::from_toml(&toml).expect("valid");
        assert_eq!(m.dependencies.required.len(), 1);
        assert_eq!(
            m.dependencies.required[0].plugin_name(),
            "org.evoframework.composition.alsa"
        );
        assert_eq!(m.dependencies.required[0].version_str(), None);
        assert_eq!(m.dependencies.recommended.len(), 1);
        assert_eq!(m.dependencies.conflicts_with.len(), 1);
    }

    #[test]
    fn parses_dependencies_explicit_form_with_version() {
        let toml = manifest_with_dependencies(
            r#"
[dependencies]
required = [
    { plugin_name = "org.evoframework.composition.alsa", version = ">=0.1.0" },
]
"#,
        );
        let m = Manifest::from_toml(&toml).expect("valid");
        assert_eq!(m.dependencies.required.len(), 1);
        let entry = &m.dependencies.required[0];
        assert_eq!(entry.plugin_name(), "org.evoframework.composition.alsa");
        assert_eq!(entry.version_str(), Some(">=0.1.0"));
        let parsed = entry
            .parsed_version()
            .unwrap()
            .expect("parseable version req");
        assert!(parsed.matches(&Version::new(0, 1, 0)));
        assert!(parsed.matches(&Version::new(0, 2, 0)));
        assert!(!parsed.matches(&Version::new(0, 0, 9)));
    }

    #[test]
    fn parses_dependencies_mixed_forms() {
        let toml = manifest_with_dependencies(
            r#"
[dependencies]
required = [
    "org.evoframework.composition.alsa",
    { plugin_name = "org.evoframework.metadata.local", version = "^0.1" },
]
"#,
        );
        let m = Manifest::from_toml(&toml).expect("valid");
        assert_eq!(m.dependencies.required.len(), 2);
        assert_eq!(m.dependencies.required[0].version_str(), None);
        assert_eq!(m.dependencies.required[1].version_str(), Some("^0.1"));
    }

    #[test]
    fn validate_refuses_self_dependency_in_required() {
        let toml = manifest_with_dependencies(
            r#"
[dependencies]
required = ["com.example.metadata.local"]
"#,
        );
        let err = Manifest::from_toml(&toml).unwrap_err();
        assert!(
            matches!(
                err,
                ManifestError::SelfDependency {
                    list: "required",
                    ..
                }
            ),
            "expected SelfDependency(required), got {err:?}",
        );
    }

    #[test]
    fn validate_refuses_self_dependency_in_recommended() {
        let toml = manifest_with_dependencies(
            r#"
[dependencies]
recommended = ["com.example.metadata.local"]
"#,
        );
        let err = Manifest::from_toml(&toml).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::SelfDependency {
                list: "recommended",
                ..
            }
        ));
    }

    #[test]
    fn validate_refuses_self_dependency_in_conflicts() {
        let toml = manifest_with_dependencies(
            r#"
[dependencies]
conflicts_with = ["com.example.metadata.local"]
"#,
        );
        let err = Manifest::from_toml(&toml).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::SelfDependency {
                list: "conflicts_with",
                ..
            }
        ));
    }

    #[test]
    fn validate_refuses_unparseable_version_requirement() {
        let toml = manifest_with_dependencies(
            r#"
[dependencies]
required = [
    { plugin_name = "org.evoframework.composition.alsa", version = "not-a-semver" },
]
"#,
        );
        let err = Manifest::from_toml(&toml).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::InvalidDependencyVersion { .. }
        ));
    }

    #[test]
    fn validate_refuses_empty_dependency_name() {
        let toml = manifest_with_dependencies(
            r#"
[dependencies]
required = [""]
"#,
        );
        let err = Manifest::from_toml(&toml).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::EmptyDependencyName { list: "required" }
        ));
    }

    #[test]
    fn dependencies_round_trip_through_serde() {
        let toml = manifest_with_dependencies(
            r#"
[dependencies]
required = [
    "org.evoframework.composition.alsa",
    { plugin_name = "org.evoframework.metadata.local", version = "^0.1" },
]
recommended = ["org.evoframework.notifications"]
conflicts_with = ["com.competing.composition"]
"#,
        );
        let m = Manifest::from_toml(&toml).unwrap();
        let back = toml::to_string(&m).unwrap();
        let m2 = Manifest::from_toml(&back).unwrap();
        assert_eq!(m, m2);
    }

    // ----- Audio data plane capability tests -----

    fn audio_source_manifest(audio_block: &str) -> String {
        format!(
            r#"
[plugin]
name = "com.example.streaming"
version = "0.1.0"
contract = 1

[target]
shelf = "audio.streaming"
shape = 2

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "out-of-process"
exec = "plugin.bin"

[trust]
class = "unprivileged"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = true
filesystem_scopes = []

[resources]
max_memory_mb = 64
max_cpu_percent = 5

[lifecycle]
hot_reload = "restart"
autostart = true
restart_on_crash = true
restart_budget = 5

[capabilities.respondent]
request_types = ["play_now", "play_now_collection", "stop"]
response_budget_ms = 1000

[capabilities.source]
uri_schemes = ["example-streaming"]
{audio_block}
"#
        )
    }

    fn audio_delivery_manifest(delivery_block: &str) -> String {
        format!(
            r#"
[plugin]
name = "org.evoframework.delivery.alsa"
version = "0.1.0"
contract = 1

[target]
shelf = "audio.delivery"
shape = 2

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "out-of-process"
exec = "plugin.bin"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = false
filesystem_scopes = []

[resources]
max_memory_mb = 32
max_cpu_percent = 5

[lifecycle]
hot_reload = "restart"
autostart = true
restart_on_crash = true
restart_budget = 5

[capabilities.respondent]
request_types = ["delivery.set_format"]
response_budget_ms = 1000

{delivery_block}
"#
        )
    }

    fn audio_composition_manifest(composition_block: &str) -> String {
        format!(
            r#"
[plugin]
name = "org.evoframework.composition.alsa"
version = "0.1.0"
contract = 1

[target]
shelf = "audio.composition"
shape = 2

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "out-of-process"
exec = "plugin.bin"

[trust]
class = "platform"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = false
filesystem_scopes = []

[resources]
max_memory_mb = 32
max_cpu_percent = 5

[lifecycle]
hot_reload = "restart"
autostart = true
restart_on_crash = true
restart_budget = 5

[capabilities.respondent]
request_types = ["composition.select_mode"]
response_budget_ms = 1000

{composition_block}
"#
        )
    }

    #[test]
    fn audio_source_with_pcm_formats_round_trips() {
        let toml = audio_source_manifest(
            r#"
output_kind = "audio.pcm"
bit_perfect_capable = true
preferred_topology = "no_intermediate"

[[capabilities.source.audio_formats]]
kind = "pcm"
codec = "pcm_s24_le"
rate_hz = [44100, 48000, 88200, 96000, 176400, 192000]
channels = 2
"#,
        );
        let m = Manifest::from_toml(&toml).expect("audio source parses");
        let source = m
            .capabilities
            .source
            .as_ref()
            .expect("source capabilities present");
        assert_eq!(source.output_kind.as_deref(), Some("audio.pcm"));
        assert!(source.bit_perfect_capable);
        assert_eq!(source.audio_formats.len(), 1);
    }

    #[test]
    fn audio_source_round_trips_through_toml() {
        let toml = audio_source_manifest(
            r#"
output_kind = "audio.pcm"
bit_perfect_capable = true

[[capabilities.source.audio_formats]]
kind = "pcm"
codec = "pcm_s32_le"
rate_hz = [44100, 96000, 192000]
channels = 2
"#,
        );
        let m = Manifest::from_toml(&toml).unwrap();
        let back = toml::to_string(&m).unwrap();
        let m2 = Manifest::from_toml(&back).unwrap();
        assert_eq!(m, m2);
    }

    #[test]
    fn audio_source_refuses_audio_kind_without_formats() {
        let toml = audio_source_manifest(
            r#"
output_kind = "audio.pcm"
"#,
        );
        let err = Manifest::from_toml(&toml).expect_err(
            "audio.pcm output_kind must demand format declarations",
        );
        match err {
            ManifestError::InconsistentCapabilities(msg) => {
                assert!(
                    msg.contains("audio_formats"),
                    "validation message should name audio_formats: {msg:?}"
                );
            }
            _ => panic!("expected InconsistentCapabilities, got {err:?}"),
        }
    }

    #[test]
    fn non_audio_source_omits_audio_fields_cleanly() {
        // Backward-compat: existing source plugins that never
        // declared output_kind / audio_formats parse and
        // validate as before.
        let toml = audio_source_manifest("");
        let m = Manifest::from_toml(&toml).expect("non-audio source parses");
        let source = m.capabilities.source.as_ref().unwrap();
        assert!(source.output_kind.is_none());
        assert!(source.audio_formats.is_empty());
        assert!(!source.bit_perfect_capable);
    }

    #[test]
    fn delivery_capability_with_alsa_device_round_trips() {
        let toml = audio_delivery_manifest(
            r#"
[capabilities.delivery]
input_kind = "audio.pcm"
device = "alsa:hw:0,0"
formats_query = "alsa_hw_params"
exclusive_mode = true
bit_perfect_capable = true

[[capabilities.delivery.audio_formats]]
kind = "pcm"
codec = "pcm_s24_le"
rate_hz = [44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000]
channels = 2
"#,
        );
        let m = Manifest::from_toml(&toml).expect("delivery parses");
        let delivery =
            m.capabilities.delivery.as_ref().expect("delivery present");
        assert_eq!(delivery.device, "alsa:hw:0,0");
        assert!(delivery.exclusive_mode);
        assert!(delivery.bit_perfect_capable);
    }

    #[test]
    fn delivery_refuses_bit_perfect_without_exclusive_mode() {
        let toml = audio_delivery_manifest(
            r#"
[capabilities.delivery]
input_kind = "audio.pcm"
device = "alsa:default"
exclusive_mode = false
bit_perfect_capable = true

[[capabilities.delivery.audio_formats]]
kind = "pcm"
codec = "pcm_s16_le"
rate_hz = [48000]
channels = 2
"#,
        );
        let err = Manifest::from_toml(&toml)
            .expect_err("bit-perfect without exclusive_mode must be refused");
        match err {
            ManifestError::InconsistentCapabilities(msg) => {
                assert!(
                    msg.contains("bit_perfect_capable")
                        && msg.contains("exclusive_mode"),
                    "message should name both flags: {msg:?}"
                );
            }
            _ => panic!("expected InconsistentCapabilities, got {err:?}"),
        }
    }

    #[test]
    fn delivery_refuses_audio_kind_without_formats_or_query() {
        let toml = audio_delivery_manifest(
            r#"
[capabilities.delivery]
input_kind = "audio.pcm"
device = "alsa:hw:0,0"
exclusive_mode = true
"#,
        );
        let err = Manifest::from_toml(&toml).expect_err(
            "audio delivery without formats or query must be refused",
        );
        match err {
            ManifestError::InconsistentCapabilities(msg) => {
                assert!(
                    msg.contains("audio_formats")
                        && msg.contains("formats_query"),
                    "message should name both options: {msg:?}"
                );
            }
            _ => panic!("expected InconsistentCapabilities, got {err:?}"),
        }
    }

    #[test]
    fn composition_with_passthrough_and_eq_modes_round_trips() {
        let toml = audio_composition_manifest(
            r#"
[capabilities.composition]
input_kind = "audio.pcm"
output_kind = "audio.pcm"
default_mode = "passthrough"

[[capabilities.composition.modes]]
name = "passthrough"
preserves_bit_perfect = true

[[capabilities.composition.modes]]
name = "eq_only"
preserves_bit_perfect = false
"#,
        );
        let m = Manifest::from_toml(&toml).expect("composition parses");
        let composition = m
            .capabilities
            .composition
            .as_ref()
            .expect("composition present");
        assert_eq!(composition.modes.len(), 2);
        assert_eq!(composition.default_mode, "passthrough");
    }

    #[test]
    fn composition_refuses_default_mode_pointing_at_unknown_name() {
        let toml = audio_composition_manifest(
            r#"
[capabilities.composition]
input_kind = "audio.pcm"
output_kind = "audio.pcm"
default_mode = "nonexistent"

[[capabilities.composition.modes]]
name = "passthrough"
preserves_bit_perfect = true
"#,
        );
        let err = Manifest::from_toml(&toml).expect_err(
            "default_mode pointing at unknown name must be refused",
        );
        match err {
            ManifestError::InconsistentCapabilities(msg) => {
                assert!(
                    msg.contains("default_mode"),
                    "message should name default_mode: {msg:?}"
                );
            }
            _ => panic!("expected InconsistentCapabilities, got {err:?}"),
        }
    }

    #[test]
    fn composition_refuses_duplicate_mode_names() {
        let toml = audio_composition_manifest(
            r#"
[capabilities.composition]
input_kind = "audio.pcm"
output_kind = "audio.pcm"
default_mode = "passthrough"

[[capabilities.composition.modes]]
name = "passthrough"
preserves_bit_perfect = true

[[capabilities.composition.modes]]
name = "passthrough"
preserves_bit_perfect = false
"#,
        );
        let err = Manifest::from_toml(&toml)
            .expect_err("duplicate mode names must be refused");
        match err {
            ManifestError::InconsistentCapabilities(msg) => {
                assert!(
                    msg.contains("duplicate"),
                    "message should mention duplicate: {msg:?}"
                );
            }
            _ => panic!("expected InconsistentCapabilities, got {err:?}"),
        }
    }

    #[test]
    fn composition_refuses_empty_modes() {
        let toml = audio_composition_manifest(
            r#"
[capabilities.composition]
input_kind = "audio.pcm"
output_kind = "audio.pcm"
default_mode = "passthrough"
modes = []
"#,
        );
        let err = Manifest::from_toml(&toml)
            .expect_err("empty modes list must be refused");
        match err {
            ManifestError::InconsistentCapabilities(msg) => {
                assert!(
                    msg.contains("modes"),
                    "message should name modes: {msg:?}"
                );
            }
            _ => panic!("expected InconsistentCapabilities, got {err:?}"),
        }
    }

    #[test]
    fn manifest_without_audio_capabilities_parses_unchanged() {
        // Backward-compat sweep: the unmodified
        // valid_singleton_respondent fixture (no audio fields)
        // continues to parse and validate.
        let m = Manifest::from_toml(valid_singleton_respondent())
            .expect("base fixture must remain valid");
        assert!(m.capabilities.delivery.is_none());
        assert!(m.capabilities.composition.is_none());
    }

    #[test]
    fn lifecycle_mode_defaults_to_frozen_when_unset() {
        // Plugins that do NOT declare `[lifecycle] mode = ...`
        // inherit the safe default: frozen (no reload; restart
        // is the only mutation vector). Opt-in is explicit for
        // both reactive-only and reload-cleanable modes.
        let m = Manifest::from_toml(valid_singleton_respondent())
            .expect("base fixture must remain valid");
        let life = m.lifecycle.expect("lifecycle section present");
        assert_eq!(life.mode, LifecycleMode::Frozen);
    }

    #[test]
    fn lifecycle_deadlines_default_to_5000_ms() {
        // Both teardown_deadline_ms and admit_deadline_ms
        // default to 5000 ms. Per-plugin manifest override is
        // available for plugins whose work cannot complete in
        // the default window.
        let m = Manifest::from_toml(valid_singleton_respondent())
            .expect("base fixture must remain valid");
        let life = m.lifecycle.expect("lifecycle section present");
        assert_eq!(life.teardown_deadline_ms, 5000);
        assert_eq!(life.admit_deadline_ms, 5000);
    }

    #[test]
    fn lifecycle_mode_reactive_only_parses_from_kebab_case() {
        let toml_text = valid_singleton_respondent().replace(
            "restart_budget = 5",
            "restart_budget = 5\nmode = \"reactive-only\"\nteardown_deadline_ms = 1000\nadmit_deadline_ms = 2000",
        );
        let m = Manifest::from_toml(&toml_text)
            .expect("manifest with reactive-only mode parses");
        let life = m.lifecycle.expect("lifecycle present");
        assert_eq!(life.mode, LifecycleMode::ReactiveOnly);
        assert_eq!(life.teardown_deadline_ms, 1000);
        assert_eq!(life.admit_deadline_ms, 2000);
    }

    #[test]
    fn lifecycle_mode_reload_cleanable_parses_from_kebab_case() {
        let toml_text = valid_singleton_respondent().replace(
            "restart_budget = 5",
            "restart_budget = 5\nmode = \"reload-cleanable\"",
        );
        let m = Manifest::from_toml(&toml_text)
            .expect("manifest with reload-cleanable mode parses");
        let life = m.lifecycle.expect("lifecycle present");
        assert_eq!(life.mode, LifecycleMode::ReloadCleanable);
    }

    #[test]
    fn lifecycle_mode_round_trips_through_toml() {
        let life = Lifecycle {
            hot_reload: HotReloadPolicy::Restart,
            autostart: true,
            restart_on_crash: true,
            restart_budget: 5,
            recommended_essential: false,
            live_blob_max: None,
            mode: LifecycleMode::ReactiveOnly,
            teardown_deadline_ms: 3000,
            admit_deadline_ms: 4000,
            defaults: None,
        };
        let serialised = toml::to_string(&life).unwrap();
        let round_tripped: Lifecycle = toml::from_str(&serialised).unwrap();
        assert_eq!(life.mode, round_tripped.mode);
        assert_eq!(
            life.teardown_deadline_ms,
            round_tripped.teardown_deadline_ms
        );
        assert_eq!(life.admit_deadline_ms, round_tripped.admit_deadline_ms);
    }

    #[test]
    fn lifecycle_defaults_block_absent_by_default() {
        // Most plugins do not need a defaults block; the field
        // is optional and the framework's fall-back chain
        // tolerates absence (transitions straight to degraded
        // when prior-config retry exhausts).
        let m = Manifest::from_toml(valid_singleton_respondent())
            .expect("base fixture must remain valid");
        let life = m.lifecycle.expect("lifecycle section present");
        assert!(life.defaults.is_none());
    }

    #[test]
    fn lifecycle_defaults_block_parses_plugin_specific_fields() {
        // The defaults block carries free-form plugin-author
        // declared fields. The framework treats them opaquely;
        // the plugin's load() path is the only consumer.
        let toml_text = valid_singleton_respondent().replace(
            "restart_budget = 5",
            "restart_budget = 5\nmode = \"reload-cleanable\"\n\n\
             [lifecycle.defaults]\nrole = \"auto\"\n\
             leader_ms = 200\nalsa_pcm = \"evo\"\n",
        );
        let m = Manifest::from_toml(&toml_text)
            .expect("manifest with [lifecycle.defaults] block parses");
        let life = m.lifecycle.expect("lifecycle present");
        let defaults = life
            .defaults
            .expect("defaults block present after explicit declaration");
        assert_eq!(
            defaults.fields.get("role"),
            Some(&toml::Value::String("auto".to_string()))
        );
        assert_eq!(
            defaults.fields.get("leader_ms"),
            Some(&toml::Value::Integer(200))
        );
        assert_eq!(
            defaults.fields.get("alsa_pcm"),
            Some(&toml::Value::String("evo".to_string()))
        );
    }

    #[test]
    fn lifecycle_defaults_round_trip_preserves_fields() {
        let mut fields = BTreeMap::new();
        fields.insert(
            "role".to_string(),
            toml::Value::String("receiver".to_string()),
        );
        fields.insert("leader_ms".to_string(), toml::Value::Integer(150));
        let life = Lifecycle {
            hot_reload: HotReloadPolicy::None,
            autostart: true,
            restart_on_crash: true,
            restart_budget: 5,
            recommended_essential: false,
            live_blob_max: None,
            mode: LifecycleMode::ReloadCleanable,
            teardown_deadline_ms: 5000,
            admit_deadline_ms: 5000,
            defaults: Some(LifecycleDefaults { fields }),
        };
        let serialised = toml::to_string(&life).unwrap();
        let round_tripped: Lifecycle = toml::from_str(&serialised).unwrap();
        let rt_defaults = round_tripped.defaults.expect("defaults preserved");
        assert_eq!(
            rt_defaults.fields.get("role"),
            Some(&toml::Value::String("receiver".to_string()))
        );
        assert_eq!(
            rt_defaults.fields.get("leader_ms"),
            Some(&toml::Value::Integer(150))
        );
    }

    #[test]
    fn manifest_without_ui_section_parses_with_empty_stocks() {
        let m = Manifest::from_toml(valid_singleton_respondent())
            .expect("base fixture must remain valid without [ui] section");
        assert!(m.ui.stocks.is_empty());
    }

    #[test]
    fn manifest_with_ui_stocks_parses_and_validates() {
        let toml_text = r#"
[plugin]
name = "com.example.metadata.local"
version = "0.1.0"
contract = 1

[target]
shelf = "metadata.providers"
shape = 2

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "out-of-process"
exec = "plugin.bin"

[trust]
class = "unprivileged"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = false
filesystem_scopes = []

[resources]
max_memory_mb = 64
max_cpu_percent = 5

[lifecycle]
hot_reload = "restart"
autostart = true
restart_on_crash = true
restart_budget = 5

[capabilities.respondent]
request_types = ["metadata.query"]
response_budget_ms = 5000

[[ui.stocks]]
ui_shelf       = "library.sources"
widget         = "audio.browse.tree.entry"
size           = "third"
schema_version = 1

[[ui.stocks]]
ui_shelf       = "now_playing.context"
widget         = "audio.metering.spectrum"
size           = "full"
mode           = "inline"
schema_version = 1

[ui.stocks.parameters]
fft_size  = 4096
smoothing = 0.6
"#;
        let m = Manifest::from_toml(toml_text)
            .expect("manifest with [[ui.stocks]] must parse and validate");
        assert_eq!(m.ui.stocks.len(), 2);
        assert_eq!(m.ui.stocks[0].ui_shelf, "library.sources");
        assert_eq!(m.ui.stocks[0].widget, "audio.browse.tree.entry");
        assert_eq!(m.ui.stocks[1].ui_shelf, "now_playing.context");
    }

    #[test]
    fn manifest_ui_stocks_empty_shelf_id_refuses() {
        let toml_text = make_ui_fixture(
            r#"
[[ui.stocks]]
ui_shelf       = ""
widget         = "audio.browse.tree.entry"
size           = "third"
schema_version = 1
"#,
        );
        let r = Manifest::from_toml(&toml_text);
        match r {
            Err(crate::error::ManifestError::InvalidUiStocking {
                index,
                field,
                ..
            }) => {
                assert_eq!(index, 0);
                assert_eq!(field, "ui_shelf");
            }
            other => panic!("expected InvalidUiStocking, got {other:?}"),
        }
    }

    #[test]
    fn manifest_ui_stocks_empty_widget_refuses() {
        let toml_text = make_ui_fixture(
            r#"
[[ui.stocks]]
ui_shelf       = "library.sources"
widget         = ""
size           = "third"
schema_version = 1
"#,
        );
        let r = Manifest::from_toml(&toml_text);
        match r {
            Err(crate::error::ManifestError::InvalidUiStocking {
                index,
                field,
                ..
            }) => {
                assert_eq!(index, 0);
                assert_eq!(field, "widget");
            }
            other => panic!("expected InvalidUiStocking, got {other:?}"),
        }
    }

    #[test]
    fn manifest_ui_stocks_zero_schema_version_refuses() {
        let toml_text = make_ui_fixture(
            r#"
[[ui.stocks]]
ui_shelf       = "library.sources"
widget         = "audio.browse.tree.entry"
size           = "third"
schema_version = 0
"#,
        );
        let r = Manifest::from_toml(&toml_text);
        match r {
            Err(crate::error::ManifestError::InvalidUiStocking {
                index,
                field,
                ..
            }) => {
                assert_eq!(index, 0);
                assert_eq!(field, "schema_version");
            }
            other => panic!("expected InvalidUiStocking, got {other:?}"),
        }
    }

    #[test]
    fn manifest_ui_stocks_offending_index_is_correct() {
        // First stocking is well-formed; second has an empty
        // shelf id. The error must report index=1 so the
        // plugin author finds the right entry.
        let toml_text = make_ui_fixture(
            r#"
[[ui.stocks]]
ui_shelf       = "library.sources"
widget         = "audio.browse.tree.entry"
size           = "third"
schema_version = 1

[[ui.stocks]]
ui_shelf       = ""
widget         = "audio.browse.tree.entry"
size           = "third"
schema_version = 1
"#,
        );
        let r = Manifest::from_toml(&toml_text);
        match r {
            Err(crate::error::ManifestError::InvalidUiStocking {
                index,
                ..
            }) => {
                assert_eq!(index, 1);
            }
            other => panic!("expected InvalidUiStocking, got {other:?}"),
        }
    }

    #[test]
    fn manifest_with_ui_stocks_round_trips_through_toml() {
        let toml_text = r#"
[plugin]
name = "com.example.metadata.local"
version = "0.1.0"
contract = 1

[target]
shelf = "metadata.providers"
shape = 2

[kind]
instance = "singleton"
interaction = "respondent"

[transport]
type = "out-of-process"
exec = "plugin.bin"

[trust]
class = "unprivileged"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = false
filesystem_scopes = []

[resources]
max_memory_mb = 64
max_cpu_percent = 5

[lifecycle]
hot_reload = "restart"
autostart = true
restart_on_crash = true
restart_budget = 5

[capabilities.respondent]
request_types = ["metadata.query"]
response_budget_ms = 5000

[[ui.stocks]]
ui_shelf       = "library.sources"
widget         = "audio.browse.tree.entry"
size           = "third"
schema_version = 1
"#;
        let m1 = Manifest::from_toml(toml_text).unwrap();
        let serialised = m1.to_toml().unwrap();
        let m2 = Manifest::from_toml(&serialised).unwrap();
        assert_eq!(m1.ui.stocks.len(), m2.ui.stocks.len());
        assert_eq!(m1.ui.stocks[0].ui_shelf, m2.ui.stocks[0].ui_shelf);
        assert_eq!(m1.ui.stocks[0].widget, m2.ui.stocks[0].widget);
        assert_eq!(m1.ui.stocks[0].size, m2.ui.stocks[0].size);
    }

    /// Compose a full manifest TOML with the supplied
    /// `[[ui.stocks]]` block appended so each ui-test can
    /// concentrate on the offending entry.
    fn make_ui_fixture(ui_block: &str) -> String {
        let mut out = String::from(valid_singleton_respondent());
        out.push_str(ui_block);
        out
    }

    // ----- artefact-kind manifests -----

    #[test]
    fn manifest_default_kind_is_functional() {
        // Backward-compat: a manifest without `plugin.kind`
        // declared parses with `ArtefactKind::Functional`,
        // matching the historical shape every existing
        // plugin in the workspace uses.
        let m = Manifest::from_toml(valid_singleton_respondent())
            .expect("base fixture must remain valid");
        assert_eq!(m.plugin.kind, ArtefactKind::Functional);
    }

    #[test]
    fn manifest_functional_with_artefact_section_refuses() {
        // A functional plugin that accidentally carries a
        // `[theme]` block is misshapen — the operator
        // diagnostic names the offending section.
        let toml_text = format!(
            "{}\n[theme]\ndisplay_name = \"oops\"\n",
            valid_singleton_respondent(),
        );
        match Manifest::from_toml(&toml_text) {
            Err(crate::error::ManifestError::UnexpectedArtefactSection {
                kind,
                section,
            }) => {
                assert_eq!(kind, "functional");
                assert_eq!(section, "theme");
            }
            other => {
                panic!("expected UnexpectedArtefactSection, got {other:?}")
            }
        }
    }

    #[test]
    fn manifest_theme_kind_with_theme_section_admits() {
        let toml_text = artefact_theme_fixture(true);
        let m = Manifest::from_toml(&toml_text)
            .expect("well-formed theme manifest must parse");
        assert_eq!(m.plugin.kind, ArtefactKind::Theme);
        let theme = m.theme.as_ref().expect("theme section must be present");
        assert_eq!(theme.display_name.as_deref(), Some("Test Theme"));
        assert!(theme.tokens.contains_key("brand_primary"));
    }

    #[test]
    fn manifest_theme_kind_without_theme_section_refuses() {
        // plugin.kind = "theme" but the [theme] block is
        // absent — refuse with MissingArtefactSection.
        let toml_text = artefact_theme_fixture(false);
        match Manifest::from_toml(&toml_text) {
            Err(crate::error::ManifestError::MissingArtefactSection {
                kind,
                section,
            }) => {
                assert_eq!(kind, "theme");
                assert_eq!(section, "theme");
            }
            other => panic!("expected MissingArtefactSection, got {other:?}"),
        }
    }

    #[test]
    fn manifest_ui_shell_kind_admits() {
        let toml_text = artefact_ui_shell_fixture();
        let m = Manifest::from_toml(&toml_text)
            .expect("well-formed ui_shell manifest must parse");
        assert_eq!(m.plugin.kind, ArtefactKind::UiShell);
        let ui_shell = m
            .ui_shell
            .as_ref()
            .expect("ui_shell section must be present");
        assert_eq!(ui_shell.shell_type, "web_bundle");
        assert_eq!(ui_shell.entry_point, "index.html");
    }

    #[test]
    fn manifest_widget_kind_pack_admits() {
        let toml_text = artefact_widget_pack_fixture();
        let m = Manifest::from_toml(&toml_text)
            .expect("well-formed widget_kind_pack manifest must parse");
        assert_eq!(m.plugin.kind, ArtefactKind::WidgetKindPack);
        let widgets =
            m.widgets.as_ref().expect("widgets section must be present");
        assert_eq!(widgets.provides.len(), 2);
        assert!(widgets.provides.contains(&"audio.eq.parametric".into()));
    }

    #[test]
    fn manifest_artefact_kind_with_wrong_section_refuses() {
        // plugin.kind = "theme" but the manifest carries
        // [ui_shell] instead — the wrong section refuses.
        let toml_text = r#"
[plugin]
name = "com.example.theme"
version = "0.1.0"
contract = 1
kind = "theme"

[target]
shelf = "system.appearance.themes"
shape = 2

[trust]
class = "unprivileged"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = false
filesystem_scopes = []

[ui_shell]
shell_type = "web_bundle"
entry_point = "index.html"
min_evo_version = "0.1.13"
"#;
        // The manifest carries [ui_shell] but no [theme] —
        // two errors are possible (missing-theme OR
        // unexpected-ui_shell). validate_artefact_sections
        // checks the matching section's presence first per
        // the match-arm order, so MissingArtefactSection
        // surfaces.
        let r = Manifest::from_toml(toml_text);
        match r {
            Err(crate::error::ManifestError::MissingArtefactSection {
                kind,
                section,
            }) => {
                assert_eq!(kind, "theme");
                assert_eq!(section, "theme");
            }
            other => panic!("expected MissingArtefactSection, got {other:?}"),
        }
    }

    #[test]
    fn manifest_functional_without_executable_sections_refuses() {
        // A functional plugin that omits [kind] is misshapen
        // — the operator diagnostic names the missing
        // section.
        let toml_text = r#"
[plugin]
name = "com.example.metadata.local"
version = "0.1.0"
contract = 1

[target]
shelf = "metadata.providers"
shape = 2

[transport]
type = "out-of-process"
exec = "plugin.bin"

[trust]
class = "unprivileged"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = false
filesystem_scopes = []

[resources]
max_memory_mb = 64
max_cpu_percent = 5

[lifecycle]
hot_reload = "restart"
autostart = true
restart_on_crash = true
restart_budget = 5

[capabilities.respondent]
request_types = ["metadata.query"]
response_budget_ms = 5000
"#;
        match Manifest::from_toml(toml_text) {
            Err(crate::error::ManifestError::MissingExecutableSection {
                kind,
                section,
            }) => {
                assert_eq!(kind, "functional");
                assert_eq!(section, "kind");
            }
            other => {
                panic!("expected MissingExecutableSection, got {other:?}")
            }
        }
    }

    #[test]
    fn manifest_artefact_with_executable_section_refuses() {
        // A theme manifest that accidentally carries the
        // executable `[transport]` block is misshapen —
        // artefacts are static asset bundles with no
        // transport machinery.
        let toml_text = r#"
[plugin]
name = "com.example.theme"
version = "0.1.0"
contract = 1
kind = "theme"

[target]
shelf = "system.appearance.themes"
shape = 2

[transport]
type = "out-of-process"
exec = "plugin.bin"

[trust]
class = "unprivileged"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = false
filesystem_scopes = []

[theme]
display_name = "Test"
"#;
        match Manifest::from_toml(toml_text) {
            Err(crate::error::ManifestError::UnexpectedExecutableSection {
                kind,
                section,
            }) => {
                assert_eq!(kind, "theme");
                assert_eq!(section, "transport");
            }
            other => {
                panic!("expected UnexpectedExecutableSection, got {other:?}")
            }
        }
    }

    #[test]
    fn manifest_artefact_round_trip_omits_executable_sections() {
        // A theme manifest that parses cleanly serialises
        // back to TOML without any of the four
        // executable-only section headers, and re-parses
        // identically.
        let toml_text = artefact_theme_fixture(true);
        let m1 = Manifest::from_toml(&toml_text)
            .expect("well-formed theme manifest must parse");
        let serialised = m1.to_toml().expect("serialise must succeed");
        for forbidden_header in
            ["[kind]", "[transport]", "[resources]", "[lifecycle]"]
        {
            assert!(
                !serialised.contains(forbidden_header),
                "serialised theme manifest must not contain \
                 {forbidden_header}; got:\n{serialised}",
            );
        }
        let m2 = Manifest::from_toml(&serialised)
            .expect("round-tripped theme manifest must re-parse");
        assert_eq!(m1.plugin.kind, m2.plugin.kind);
        assert!(m2.kind.is_none());
        assert!(m2.transport.is_none());
        assert!(m2.resources.is_none());
        assert!(m2.lifecycle.is_none());
    }

    /// Compose a manifest with `plugin.kind = "theme"` and
    /// optionally include the `[theme]` section.
    fn artefact_theme_fixture(with_theme_section: bool) -> String {
        let theme_block = if with_theme_section {
            r##"
[theme]
display_name = "Test Theme"
variants = ["light", "dark"]

[theme.tokens]
brand_primary = "#1ABCFF"
spacing_unit = 8
"##
        } else {
            ""
        };
        format!(
            r#"
[plugin]
name = "com.example.theme"
version = "0.1.0"
contract = 1
kind = "theme"

[target]
shelf = "system.appearance.themes"
shape = 2

[trust]
class = "unprivileged"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = false
filesystem_scopes = []
{theme_block}
"#
        )
    }

    fn artefact_ui_shell_fixture() -> String {
        r#"
[plugin]
name = "com.example.shell"
version = "0.1.0"
contract = 1
kind = "ui_shell"

[target]
shelf = "system.ui.shell"
shape = 2

[trust]
class = "unprivileged"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = false
filesystem_scopes = []

[ui_shell]
shell_type = "web_bundle"
entry_point = "index.html"
required_widget_kinds = ["evo.*", "audio.*"]
supports_themes = true
supports_offline = true
min_evo_version = "0.1.13"
"#
        .into()
    }

    fn artefact_widget_pack_fixture() -> String {
        r#"
[plugin]
name = "com.example.widgets"
version = "0.1.0"
contract = 1
kind = "widget_kind_pack"

[target]
shelf = "system.ui.widgets"
shape = 2

[trust]
class = "unprivileged"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = false
filesystem_scopes = []

[widgets]
provides = ["audio.eq.parametric", "audio.spectrum.live"]
size_envelopes_path = "size_envelopes.toml"
accessibility_declarations_path = "a11y.toml"
"#
        .into()
    }

    // ----- VerbCapability + per-verb capability gate tests -----

    #[test]
    fn verb_capability_serde_round_trip_for_each_variant() {
        for variant in [
            VerbCapability::None,
            VerbCapability::Read {
                scope: "playback".into(),
            },
            VerbCapability::Write {
                scope: "network".into(),
            },
            VerbCapability::StepUp {
                scope: "system_admin".into(),
            },
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            let back: VerbCapability = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant, "round-trip for {variant:?}");
        }
    }

    #[test]
    fn verb_capability_none_serialises_to_tagged_object() {
        let json = serde_json::to_string(&VerbCapability::None).unwrap();
        assert_eq!(json, r#"{"kind":"none"}"#);
    }

    #[test]
    fn verb_capability_step_up_serialises_with_scope() {
        let json = serde_json::to_string(&VerbCapability::StepUp {
            scope: "system_admin".into(),
        })
        .unwrap();
        assert_eq!(json, r#"{"kind":"step_up","scope":"system_admin"}"#);
    }

    #[test]
    fn verb_capability_accessors_match_lattice() {
        let none = VerbCapability::None;
        assert_eq!(none.scope(), None);
        assert!(!none.requires_step_up());
        assert!(!none.is_gated());

        let read = VerbCapability::Read {
            scope: "playback".into(),
        };
        assert_eq!(read.scope(), Some("playback"));
        assert!(!read.requires_step_up());
        assert!(read.is_gated());

        let write = VerbCapability::Write {
            scope: "network".into(),
        };
        assert_eq!(write.scope(), Some("network"));
        assert!(!write.requires_step_up());
        assert!(write.is_gated());

        let step_up = VerbCapability::StepUp {
            scope: "system_admin".into(),
        };
        assert_eq!(step_up.scope(), Some("system_admin"));
        assert!(step_up.requires_step_up());
        assert!(step_up.is_gated());
    }

    #[test]
    fn respondent_with_verb_capabilities_round_trips() {
        let toml = valid_singleton_respondent().to_string()
            + r#"
[capabilities.respondent.verb_capabilities]
"metadata.query" = { kind = "read", scope = "metadata" }
"#;
        let m1 = Manifest::from_toml(&toml).expect("manifest must parse");
        let resp = m1
            .capabilities
            .respondent
            .as_ref()
            .expect("respondent present");
        assert_eq!(resp.verb_capabilities.len(), 1);
        assert_eq!(
            resp.verb_capabilities.get("metadata.query"),
            Some(&VerbCapability::Read {
                scope: "metadata".into(),
            })
        );

        let serialised = m1.to_toml().unwrap();
        let m2 = Manifest::from_toml(&serialised).unwrap();
        assert_eq!(m1, m2, "round-trip preserves verb_capabilities");
    }

    #[test]
    fn respondent_legacy_without_verb_capabilities_parses_with_empty_map() {
        let m = Manifest::from_toml(valid_singleton_respondent())
            .expect("legacy manifest must parse");
        let resp = m
            .capabilities
            .respondent
            .as_ref()
            .expect("respondent present");
        assert!(
            resp.verb_capabilities.is_empty(),
            "legacy manifest defaults to empty verb_capabilities"
        );
    }

    #[test]
    fn respondent_verb_capabilities_key_not_in_request_types_refuses() {
        let toml = valid_singleton_respondent().to_string()
            + r#"
[capabilities.respondent.verb_capabilities]
no_such_verb = { kind = "step_up", scope = "system_admin" }
"#;
        let err = Manifest::from_toml(&toml).expect_err(
            "verb_capabilities key absent from request_types must refuse",
        );
        assert!(
            matches!(err, ManifestError::InconsistentCapabilities(ref msg)
                if msg.contains("no_such_verb")
                    && msg.contains("request_types")),
            "expected InconsistentCapabilities naming the offending verb + \
             request_types, got {err:?}"
        );
    }

    #[test]
    fn respondent_verb_capabilities_empty_scope_refuses() {
        let toml = valid_singleton_respondent().to_string()
            + r#"
[capabilities.respondent.verb_capabilities]
"metadata.query" = { kind = "step_up", scope = "" }
"#;
        let err =
            Manifest::from_toml(&toml).expect_err("empty scope must refuse");
        assert!(
            matches!(err, ManifestError::InconsistentCapabilities(ref msg)
                if msg.contains("empty") && msg.contains("scope")),
            "expected InconsistentCapabilities about empty scope, got {err:?}"
        );
    }

    fn warden_with_course_correct_verbs_for_cap_tests() -> String {
        // Inject `course_correct_verbs` inside the [capabilities.warden]
        // table — appending at the end of the fixture would land the
        // key inside [capabilities.factory] instead.
        valid_factory_warden().replace(
            "custody_failure_mode = \"abort\"",
            "custody_failure_mode = \"abort\"\ncourse_correct_verbs = [\"pause_session\", \"resume_session\"]",
        )
    }

    #[test]
    fn warden_with_verb_capabilities_round_trips() {
        let toml = warden_with_course_correct_verbs_for_cap_tests()
            + r#"
[capabilities.warden.verb_capabilities]
pause_session = { kind = "write", scope = "sessions" }
resume_session = { kind = "step_up", scope = "sessions_admin" }
"#;
        let m1 = Manifest::from_toml(&toml).expect("manifest must parse");
        let warden = m1.capabilities.warden.as_ref().expect("warden present");
        assert_eq!(warden.verb_capabilities.len(), 2);
        assert_eq!(
            warden.verb_capabilities.get("resume_session"),
            Some(&VerbCapability::StepUp {
                scope: "sessions_admin".into(),
            })
        );

        let serialised = m1.to_toml().unwrap();
        let m2 = Manifest::from_toml(&serialised).unwrap();
        assert_eq!(m1, m2);
    }

    #[test]
    fn warden_verb_capabilities_key_not_in_course_correct_verbs_refuses() {
        let toml = warden_with_course_correct_verbs_for_cap_tests()
            + r#"
[capabilities.warden.verb_capabilities]
unknown_verb = { kind = "step_up", scope = "sessions_admin" }
"#;
        let err = Manifest::from_toml(&toml).expect_err(
            "verb_capabilities key absent from course_correct_verbs must \
             refuse",
        );
        assert!(
            matches!(err, ManifestError::InconsistentCapabilities(ref msg)
                if msg.contains("unknown_verb")
                    && msg.contains("course_correct_verbs")),
            "expected InconsistentCapabilities naming the offending verb + \
             course_correct_verbs, got {err:?}"
        );
    }

    #[test]
    fn warden_verb_capabilities_populated_without_course_correct_verbs_refuses()
    {
        // Warden without course_correct_verbs but with verb_capabilities
        // populated must refuse: declaring per-verb capabilities without
        // first declaring the verb list is a manifest authoring error.
        let toml = valid_factory_warden().to_string()
            + r#"
[capabilities.warden.verb_capabilities]
some_verb = { kind = "step_up", scope = "sessions_admin" }
"#;
        let err = Manifest::from_toml(&toml).expect_err(
            "warden verb_capabilities without course_correct_verbs must \
             refuse",
        );
        assert!(
            matches!(err, ManifestError::InconsistentCapabilities(ref msg)
                if msg.contains("course_correct_verbs")),
            "expected InconsistentCapabilities naming course_correct_verbs, \
             got {err:?}"
        );
    }

    // ----- Stocking primitive tests -----

    fn multi_stocking_warden_manifest() -> &'static str {
        r#"
[plugin]
name = "com.example.playback.media"
version = "0.1.0"
contract = 1

[[stockings]]
shelf = "audio.playback"
shape = 1
role  = "warden"
request_types = ["play_now", "pause"]

[[stockings]]
shelf = "audio.queue"
shape = 1
role  = "respondent"
request_types = ["queue.get_queue", "queue.enqueue"]

[[stockings]]
shelf = "audio.library"
shape = 1
role  = "respondent"
request_types = ["library.list_sources"]

[kind]
instance    = "singleton"
interaction = "warden"

[transport]
type = "out-of-process"
exec = "plugin.bin"

[trust]
class = "unprivileged"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = false
filesystem_scopes = []

[resources]
max_memory_mb = 64
max_cpu_percent = 5

[lifecycle]
hot_reload = "restart"
autostart = true
restart_on_crash = true
restart_budget = 5

[capabilities.respondent]
request_types = [
    "play_now", "pause",
    "queue.get_queue", "queue.enqueue",
    "library.list_sources",
]
response_budget_ms = 2000

[capabilities.warden]
custody_domain = "audio.playback.transport"
custody_exclusive = true
course_correction_budget_ms = 100
custody_failure_mode = "abort"
"#
    }

    #[test]
    fn legacy_target_synthesizes_one_stocking() {
        let m = Manifest::from_toml(valid_singleton_respondent()).unwrap();
        assert_eq!(m.target.shelf, "metadata.providers");
        assert_eq!(m.target.shape, 2);
        assert_eq!(m.stockings.len(), 1);
        assert_eq!(m.stockings[0].shelf, "metadata.providers");
        assert_eq!(m.stockings[0].shape, 2);
        assert_eq!(m.stockings[0].role, StockingRole::Respondent);
        assert_eq!(m.stockings[0].request_types, vec!["metadata.query"]);
    }

    #[test]
    fn multi_stocking_form_synthesizes_target_from_warden() {
        let m = Manifest::from_toml(multi_stocking_warden_manifest()).unwrap();
        assert_eq!(m.stockings.len(), 3);
        // Warden stocking is the primary; target reflects it.
        assert_eq!(m.target.shelf, "audio.playback");
        assert_eq!(m.target.shape, 1);
        let queue_stocking = m
            .stockings
            .iter()
            .find(|s| s.shelf == "audio.queue")
            .expect("queue stocking present");
        assert_eq!(queue_stocking.role, StockingRole::Respondent);
        assert_eq!(
            queue_stocking.request_types,
            vec!["queue.get_queue", "queue.enqueue"]
        );
    }

    #[test]
    fn mixed_forms_refused() {
        let mixed = valid_singleton_respondent().to_string()
            + "\n\n[[stockings]]\nshelf = \"other.shelf\"\nshape = 1\n\
               role = \"respondent\"\nrequest_types = []\n";
        let err = Manifest::from_toml(&mixed).unwrap_err();
        assert!(
            matches!(err, ManifestError::MixedStockingForms),
            "expected MixedStockingForms, got {err:?}"
        );
    }

    #[test]
    fn no_stocking_refused_on_functional_plugin() {
        // Strip the [target] section out of the canonical fixture.
        let stripped = valid_singleton_respondent().replace(
            "[target]\nshelf = \"metadata.providers\"\nshape = 2\n",
            "",
        );
        let err = Manifest::from_toml(&stripped).unwrap_err();
        assert!(
            matches!(err, ManifestError::NoStocking),
            "expected NoStocking, got {err:?}"
        );
    }

    #[test]
    fn warden_stocking_on_respondent_plugin_refused() {
        let bad = r#"
[plugin]
name = "com.example.bad.warden-on-respondent"
version = "0.1.0"
contract = 1

[[stockings]]
shelf = "media.transport"
shape = 1
role  = "warden"
request_types = ["play"]

[kind]
instance    = "singleton"
interaction = "respondent"

[transport]
type = "out-of-process"
exec = "plugin.bin"

[trust]
class = "unprivileged"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = false
filesystem_scopes = []

[resources]
max_memory_mb = 32
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"
autostart = true
restart_on_crash = true
restart_budget = 5

[capabilities.respondent]
request_types = ["play"]
response_budget_ms = 2000
"#;
        let err = Manifest::from_toml(bad).unwrap_err();
        assert!(
            matches!(err, ManifestError::StockingRoleMismatch { .. }),
            "expected StockingRoleMismatch, got {err:?}"
        );
    }

    #[test]
    fn duplicate_shelf_across_stockings_refused() {
        let bad = r#"
[plugin]
name = "com.example.bad.dup-shelf"
version = "0.1.0"
contract = 1

[[stockings]]
shelf = "media.playback"
shape = 1
role  = "warden"
request_types = ["play"]

[[stockings]]
shelf = "media.playback"
shape = 1
role  = "respondent"
request_types = ["pause"]

[kind]
instance    = "singleton"
interaction = "warden"

[transport]
type = "out-of-process"
exec = "plugin.bin"

[trust]
class = "unprivileged"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = false
filesystem_scopes = []

[resources]
max_memory_mb = 32
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"
autostart = true
restart_on_crash = true
restart_budget = 5

[capabilities.respondent]
request_types = ["play", "pause"]
response_budget_ms = 2000

[capabilities.warden]
custody_domain = "media.transport"
custody_exclusive = true
course_correction_budget_ms = 100
custody_failure_mode = "abort"
"#;
        let err = Manifest::from_toml(bad).unwrap_err();
        assert!(
            matches!(err, ManifestError::StockingShelfDuplicate { .. }),
            "expected StockingShelfDuplicate, got {err:?}"
        );
    }

    #[test]
    fn verb_overlap_across_stockings_refused() {
        let bad = r#"
[plugin]
name = "com.example.bad.verb-overlap"
version = "0.1.0"
contract = 1

[[stockings]]
shelf = "a.shelf"
shape = 1
role  = "respondent"
request_types = ["v1", "v2"]

[[stockings]]
shelf = "b.shelf"
shape = 1
role  = "respondent"
request_types = ["v2", "v3"]

[kind]
instance    = "singleton"
interaction = "respondent"

[transport]
type = "out-of-process"
exec = "plugin.bin"

[trust]
class = "unprivileged"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = false
filesystem_scopes = []

[resources]
max_memory_mb = 32
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"
autostart = true
restart_on_crash = true
restart_budget = 5

[capabilities.respondent]
request_types = ["v1", "v2", "v3"]
response_budget_ms = 2000
"#;
        let err = Manifest::from_toml(bad).unwrap_err();
        assert!(
            matches!(err, ManifestError::StockingVerbOverlap { ref verb } if verb == "v2"),
            "expected StockingVerbOverlap on v2, got {err:?}"
        );
    }

    #[test]
    fn unstocked_verb_refused() {
        let bad = r#"
[plugin]
name = "com.example.bad.unstocked"
version = "0.1.0"
contract = 1

[[stockings]]
shelf = "a.shelf"
shape = 1
role  = "respondent"
request_types = ["v1"]

[kind]
instance    = "singleton"
interaction = "respondent"

[transport]
type = "out-of-process"
exec = "plugin.bin"

[trust]
class = "unprivileged"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = false
filesystem_scopes = []

[resources]
max_memory_mb = 32
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"
autostart = true
restart_on_crash = true
restart_budget = 5

[capabilities.respondent]
request_types = ["v1", "v2"]
response_budget_ms = 2000
"#;
        let err = Manifest::from_toml(bad).unwrap_err();
        assert!(
            matches!(err, ManifestError::StockingVerbsUnstocked { ref verbs } if verbs == &["v2".to_string()]),
            "expected StockingVerbsUnstocked on v2, got {err:?}"
        );
    }

    #[test]
    fn stocking_verb_not_in_plugin_set_refused() {
        let bad = r#"
[plugin]
name = "com.example.bad.undeclared-verb"
version = "0.1.0"
contract = 1

[[stockings]]
shelf = "a.shelf"
shape = 1
role  = "respondent"
request_types = ["v1", "v_undeclared"]

[kind]
instance    = "singleton"
interaction = "respondent"

[transport]
type = "out-of-process"
exec = "plugin.bin"

[trust]
class = "unprivileged"

[prerequisites]
evo_min_version = "0.1.0"
os_family = "linux"
outbound_network = false
filesystem_scopes = []

[resources]
max_memory_mb = 32
max_cpu_percent = 1

[lifecycle]
hot_reload = "restart"
autostart = true
restart_on_crash = true
restart_budget = 5

[capabilities.respondent]
request_types = ["v1"]
response_budget_ms = 2000
"#;
        let err = Manifest::from_toml(bad).unwrap_err();
        assert!(
            matches!(err, ManifestError::StockingVerbNotDeclared { ref verb, .. } if verb == "v_undeclared"),
            "expected StockingVerbNotDeclared on v_undeclared, got {err:?}"
        );
    }
}
