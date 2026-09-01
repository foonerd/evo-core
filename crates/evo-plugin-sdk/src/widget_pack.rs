// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Widget kind pack support files.
//!
//! A `widget_kind_pack` manifest references two side-files on
//! disk via `[widgets].size_envelopes_path` and
//! `[widgets].accessibility_declarations_path`. The structures
//! in this module model those files: TOML maps keyed by widget
//! kind id, carrying the per-kind data the framework needs to
//! admit the kinds into its runtime registries and to
//! enforce the rendering contract.
//!
//! Each map's keys MUST match (one-for-one) the manifest's
//! `[widgets].provides` list; admission compares the set of
//! kind ids across the manifest, the size envelopes file, and
//! the accessibility declarations file, and refuses any plugin
//! whose three sets disagree.

use crate::error::ManifestError;
use crate::ui::WidgetKindEnvelope;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// `size_envelopes.toml` payload — a TOML map of widget kind
/// id to the per-kind envelope.
///
/// The on-disk form uses the kind id as the section key:
///
/// ```toml
/// ["audio.eq.parametric"]
/// id = "audio.eq.parametric"
/// min_size = "third"
/// ideal_size = "half"
/// max_size = "full"
/// mode = "inline"
/// schema_version = 1
/// ```
///
/// The redundant inline `id` field MUST equal the section key;
/// admission refuses on mismatch. Carrying it inline lets the
/// envelope round-trip standalone (e.g. for diagnostics that
/// quote a single kind without its surrounding map context).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
pub struct WidgetSizeEnvelopesFile {
    /// Map of widget kind id → envelope.
    pub envelopes: BTreeMap<String, WidgetKindEnvelope>,
}

impl WidgetSizeEnvelopesFile {
    /// Parse a size-envelopes file from a TOML string.
    ///
    /// Returns the parsed map without any cross-file
    /// validation. Callers (typically the framework's
    /// admission path) cross-validate the kind id set
    /// against the manifest's `provides` list and the
    /// accessibility declarations file.
    pub fn from_toml(input: &str) -> Result<Self, ManifestError> {
        let parsed: BTreeMap<String, WidgetKindEnvelope> =
            toml::from_str(input)?;
        let file = Self { envelopes: parsed };
        file.validate_inline_ids()?;
        Ok(file)
    }

    /// Serialise to a TOML string.
    pub fn to_toml(&self) -> Result<String, ManifestError> {
        Ok(toml::to_string_pretty(&self.envelopes)?)
    }

    /// Confirm every entry's inline `id` matches its map
    /// key. Refuses with [`ManifestError::SchemaViolation`]
    /// on mismatch so authoring drift surfaces at parse
    /// rather than at render.
    fn validate_inline_ids(&self) -> Result<(), ManifestError> {
        for (key, envelope) in &self.envelopes {
            if envelope.id != *key {
                return Err(ManifestError::SchemaViolation(format!(
                    "size_envelopes entry under key {key:?} declares \
                     inline id {:?}; the inline id must match the \
                     map key",
                    envelope.id
                )));
            }
        }
        Ok(())
    }
}

/// `a11y.toml` payload — a TOML map of widget kind id to its
/// accessibility declaration. Same key-must-equal-inline-id
/// invariant as [`WidgetSizeEnvelopesFile`].
///
/// The declaration captures the structural a11y contract the
/// framework's renderer enforces. Per the rendering substrate:
/// every widget kind admits only when its declaration is
/// present and minimally populated. The richness of WCAG AAA
/// enforcement lands progressively against this typed
/// contract; the admission gate today validates structural
/// completeness (non-empty role, declared keyboard surface,
/// declared screen-reader announcements, declared contrast
/// compliance, declared motion behaviour).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct WidgetAccessibilityDeclarationsFile {
    /// Map of widget kind id → accessibility declaration.
    pub declarations: BTreeMap<String, WidgetAccessibilityDeclaration>,
}

impl WidgetAccessibilityDeclarationsFile {
    /// Parse an accessibility-declarations file from a TOML
    /// string. Cross-file validation (set-equality with the
    /// manifest's `provides` list and the size envelopes
    /// file) is the framework admission path's job.
    pub fn from_toml(input: &str) -> Result<Self, ManifestError> {
        let parsed: BTreeMap<String, WidgetAccessibilityDeclaration> =
            toml::from_str(input)?;
        let file = Self {
            declarations: parsed,
        };
        file.validate_inline_ids()?;
        for (key, decl) in &file.declarations {
            decl.validate(key)?;
        }
        Ok(file)
    }

    /// Serialise to a TOML string.
    pub fn to_toml(&self) -> Result<String, ManifestError> {
        Ok(toml::to_string_pretty(&self.declarations)?)
    }

    fn validate_inline_ids(&self) -> Result<(), ManifestError> {
        for (key, decl) in &self.declarations {
            if decl.kind_id != *key {
                return Err(ManifestError::SchemaViolation(format!(
                    "a11y declaration under key {key:?} declares \
                     inline kind_id {:?}; the inline id must match \
                     the map key",
                    decl.kind_id
                )));
            }
        }
        Ok(())
    }
}

/// Per-widget-kind accessibility declaration.
///
/// The framework's renderer uses these fields to attach the
/// correct ARIA role, wire keyboard interactions, attach
/// screen-reader announcements, and decide whether to honour
/// the operator's reduced-motion preference. The admission
/// gate validates structural completeness; the renderer
/// enforces the AAA contract per [`ContrastCompliance`] +
/// [`MotionSensitivity`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WidgetAccessibilityDeclaration {
    /// Inline widget kind id; admission requires this to
    /// match the map key in the enclosing
    /// [`WidgetAccessibilityDeclarationsFile`].
    pub kind_id: String,
    /// ARIA role name (e.g. `"button"`, `"slider"`,
    /// `"dialog"`). The renderer attaches `role="<value>"`
    /// to the widget's root element. Non-empty.
    pub aria_role: String,
    /// How the widget's accessible name is computed.
    pub aria_label_source: AriaLabelSource,
    /// Keyboard interaction surface.
    pub keyboard: KeyboardSemantics,
    /// Screen-reader contract.
    pub screen_reader: ScreenReaderSemantics,
    /// Declared contrast compliance level (target for the
    /// theme tokens that drive this widget's colour pairs).
    pub contrast: ContrastCompliance,
    /// Motion / animation behaviour and prefers-reduced-
    /// motion contract.
    pub motion: MotionSensitivity,
}

impl WidgetAccessibilityDeclaration {
    /// Validate structural completeness. Used by the file
    /// parser and by admission downstream (admission may
    /// re-validate after a registry replace, for example).
    /// Refuses with [`ManifestError::SchemaViolation`] on
    /// structural defects.
    pub fn validate(&self, key: &str) -> Result<(), ManifestError> {
        if self.aria_role.trim().is_empty() {
            return Err(ManifestError::SchemaViolation(format!(
                "a11y declaration for {key:?}: aria_role is empty"
            )));
        }
        if self.motion.animates && !self.motion.honours_prefers_reduced_motion {
            return Err(ManifestError::SchemaViolation(format!(
                "a11y declaration for {key:?}: animates = true but \
                 honours_prefers_reduced_motion = false; an animating \
                 widget MUST honour the operator's reduced-motion \
                 preference"
            )));
        }
        Ok(())
    }
}

/// Strategy for computing the widget's accessible name.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum AriaLabelSource {
    /// The widget receives a `label` prop / parameter that
    /// is used verbatim.
    #[default]
    LabelProp,
    /// The widget's inner text is the accessible name (used
    /// for buttons whose visible label IS the name).
    InnerText,
    /// The accessible name comes from a localised key
    /// declared on the stocking; the renderer resolves the
    /// key against the active locale.
    LocalisedKey,
}

/// Keyboard interaction surface a widget provides.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KeyboardSemantics {
    /// True when the widget is keyboard-focusable. Required
    /// for AAA on every interactive widget.
    pub focusable: bool,
    /// Per-key interaction list. Each entry binds a key
    /// (W3C `KeyboardEvent.key` form, e.g. `"Enter"`,
    /// `"ArrowUp"`, `" "` for space) to a localised
    /// description of the action.
    #[serde(default)]
    pub interactions: Vec<KeyboardInteraction>,
}

/// One keyboard binding.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KeyboardInteraction {
    /// W3C-form key name.
    pub key: String,
    /// Localised key for the action label (resolved at
    /// render time against the active locale).
    pub action_label_key: String,
}

/// Screen-reader contract.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScreenReaderSemantics {
    /// Localised keys for status announcements the renderer
    /// emits as the widget's state changes (e.g.
    /// `"audio.eq.band.gain.changed"` announced when a
    /// band's gain updates). Empty for purely-presentational
    /// widgets that do not announce.
    #[serde(default)]
    pub announces: Vec<String>,
}

/// Contrast compliance the widget's theme tokens are
/// expected to honour. The renderer rejects theme tokens
/// that fail to meet this level for the widget's foreground
/// / background pairs.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum ContrastCompliance {
    /// WCAG 2.1 Level AA (4.5:1 normal text, 3:1 large
    /// text). Acceptable for legacy widgets.
    Aa,
    /// WCAG 2.1 Level AAA (7:1 normal text, 4.5:1 large
    /// text). Required for new widget kinds.
    #[default]
    Aaa,
}

/// Motion / animation contract.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MotionSensitivity {
    /// True when the widget animates (transitions,
    /// transforms, or other motion). False for static
    /// widgets.
    pub animates: bool,
    /// True when the widget's renderer honours the
    /// operator's `prefers-reduced-motion` setting and
    /// reduces or removes animation on request. Required
    /// when [`animates`] is true; admission refuses the
    /// invalid combination (`animates && !honours`).
    ///
    /// [`animates`]: MotionSensitivity::animates
    pub honours_prefers_reduced_motion: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{UiAspect, UiMode, UiSize};

    fn sample_envelope(id: &str) -> WidgetKindEnvelope {
        WidgetKindEnvelope {
            id: id.to_string(),
            min_size: UiSize::Third,
            ideal_size: UiSize::Half,
            max_size: UiSize::Full,
            aspect_ratio: UiAspect::Any,
            responsive: BTreeMap::new(),
            mode: UiMode::Inline,
            schema_version: 1,
        }
    }

    fn sample_a11y(id: &str) -> WidgetAccessibilityDeclaration {
        WidgetAccessibilityDeclaration {
            kind_id: id.to_string(),
            aria_role: "slider".into(),
            aria_label_source: AriaLabelSource::LabelProp,
            keyboard: KeyboardSemantics {
                focusable: true,
                interactions: vec![KeyboardInteraction {
                    key: "ArrowUp".into(),
                    action_label_key: "audio.eq.band.gain.up".into(),
                }],
            },
            screen_reader: ScreenReaderSemantics {
                announces: vec!["audio.eq.band.gain.changed".into()],
            },
            contrast: ContrastCompliance::Aaa,
            motion: MotionSensitivity {
                animates: false,
                honours_prefers_reduced_motion: true,
            },
        }
    }

    #[test]
    fn size_envelopes_round_trip() {
        let mut file = WidgetSizeEnvelopesFile::default();
        file.envelopes.insert(
            "audio.eq.parametric".into(),
            sample_envelope("audio.eq.parametric"),
        );
        file.envelopes.insert(
            "audio.spectrum.live".into(),
            sample_envelope("audio.spectrum.live"),
        );
        let toml_text = file.to_toml().expect("size envelopes must serialise");
        let round_tripped = WidgetSizeEnvelopesFile::from_toml(&toml_text)
            .expect("round-trip must parse");
        assert_eq!(file, round_tripped);
    }

    #[test]
    fn size_envelopes_inline_id_mismatch_refuses() {
        let mut file = WidgetSizeEnvelopesFile::default();
        // Map key is "audio.eq.parametric" but the inline
        // id says something else.
        file.envelopes
            .insert("audio.eq.parametric".into(), sample_envelope("oops"));
        let toml_text = file.to_toml().expect("serialise still works");
        match WidgetSizeEnvelopesFile::from_toml(&toml_text) {
            Err(ManifestError::SchemaViolation(msg)) => {
                assert!(
                    msg.contains("audio.eq.parametric"),
                    "diagnostic should name the offending key, got: {msg}"
                );
                assert!(
                    msg.contains("oops"),
                    "diagnostic should name the inline id, got: {msg}"
                );
            }
            other => {
                panic!("expected SchemaViolation, got {other:?}")
            }
        }
    }

    #[test]
    fn a11y_round_trip() {
        let mut file = WidgetAccessibilityDeclarationsFile::default();
        file.declarations.insert(
            "audio.eq.parametric".into(),
            sample_a11y("audio.eq.parametric"),
        );
        let toml_text = file.to_toml().expect("a11y must serialise");
        let round_tripped =
            WidgetAccessibilityDeclarationsFile::from_toml(&toml_text)
                .expect("round-trip must parse");
        assert_eq!(file, round_tripped);
    }

    #[test]
    fn a11y_empty_aria_role_refuses() {
        let mut decl = sample_a11y("audio.eq.parametric");
        decl.aria_role = String::new();
        let mut file = WidgetAccessibilityDeclarationsFile::default();
        file.declarations.insert("audio.eq.parametric".into(), decl);
        let toml_text = file.to_toml().expect("serialise still works");
        match WidgetAccessibilityDeclarationsFile::from_toml(&toml_text) {
            Err(ManifestError::SchemaViolation(msg)) => {
                assert!(
                    msg.contains("aria_role is empty"),
                    "diagnostic should name the missing field, got: {msg}"
                );
            }
            other => panic!("expected SchemaViolation, got {other:?}"),
        }
    }

    #[test]
    fn a11y_animates_without_reduced_motion_refuses() {
        let mut decl = sample_a11y("audio.eq.parametric");
        decl.motion.animates = true;
        decl.motion.honours_prefers_reduced_motion = false;
        let mut file = WidgetAccessibilityDeclarationsFile::default();
        file.declarations.insert("audio.eq.parametric".into(), decl);
        let toml_text = file.to_toml().expect("serialise still works");
        match WidgetAccessibilityDeclarationsFile::from_toml(&toml_text) {
            Err(ManifestError::SchemaViolation(msg)) => {
                assert!(
                    msg.contains("honours_prefers_reduced_motion"),
                    "diagnostic should name the missing honour, got: {msg}"
                );
            }
            other => panic!("expected SchemaViolation, got {other:?}"),
        }
    }

    #[test]
    fn a11y_inline_id_mismatch_refuses() {
        let mut file = WidgetAccessibilityDeclarationsFile::default();
        file.declarations
            .insert("audio.eq.parametric".into(), sample_a11y("wrong"));
        let toml_text = file.to_toml().expect("serialise still works");
        match WidgetAccessibilityDeclarationsFile::from_toml(&toml_text) {
            Err(ManifestError::SchemaViolation(msg)) => {
                assert!(
                    msg.contains("audio.eq.parametric"),
                    "diagnostic should name the key, got: {msg}"
                );
                assert!(
                    msg.contains("wrong"),
                    "diagnostic should name the inline id, got: {msg}"
                );
            }
            other => panic!("expected SchemaViolation, got {other:?}"),
        }
    }
}
