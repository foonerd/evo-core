//! The [`AdmittedHandle`] enum: a single heterogeneous handle for any
//! admitted plugin.
//!
//! Each admitted plugin lands in the engine as exactly one variant of
//! this enum, decided at admission time by the manifest's
//! `kind.interaction` field. The four core verbs (`describe`, `load`,
//! `unload`, `health_check`) are common to both variants and exposed
//! via inherent methods that dispatch through the enum. Kind-specific
//! verbs (request handling for respondents; custody for wardens) are
//! routed by the engine's public methods, which match on the variant
//! and surface a structured error if the shelf's plugin kind does not
//! match the caller's request.

use super::erasure::{ErasedRespondent, ErasedWarden};
use evo_plugin_sdk::contract::{
    HealthReport, LoadContext, PluginDescription, PluginError,
};

/// A handle to an admitted plugin. Each admitted plugin is exactly one
/// of these variants, disjoint and decided at admission time by the
/// manifest's `kind.interaction` field.
///
/// All four core verbs (`describe`, `load`, `unload`, `health_check`)
/// are common to both variants and exposed via inherent methods that
/// dispatch through the enum. Kind-specific verbs are routed by the
/// engine's public methods, which match on the variant and return a
/// [`StewardError::Dispatch`](crate::error::StewardError::Dispatch) if
/// the shelf's plugin kind does not match the caller's request (e.g. a
/// `handle_request` on a warden shelf, or a `take_custody` on a
/// respondent shelf).
pub enum AdmittedHandle {
    /// A respondent plugin: handles discrete request-response
    /// exchanges via [`ErasedRespondent::handle_request`].
    Respondent(Box<dyn ErasedRespondent>),
    /// A warden plugin: takes sustained custody via
    /// [`ErasedWarden::take_custody`], [`ErasedWarden::course_correct`],
    /// [`ErasedWarden::release_custody`].
    Warden(Box<dyn ErasedWarden>),
}

impl AdmittedHandle {
    /// Dispatch to the inner plugin's `describe`.
    pub async fn describe(&self) -> PluginDescription {
        match self {
            Self::Respondent(r) => r.describe().await,
            Self::Warden(w) => w.describe().await,
        }
    }

    /// Dispatch to the inner plugin's `load`.
    pub async fn load(&mut self, ctx: &LoadContext) -> Result<(), PluginError> {
        match self {
            Self::Respondent(r) => r.load(ctx).await,
            Self::Warden(w) => w.load(ctx).await,
        }
    }

    /// Dispatch to the inner plugin's `unload`.
    pub async fn unload(&mut self) -> Result<(), PluginError> {
        match self {
            Self::Respondent(r) => r.unload().await,
            Self::Warden(w) => w.unload().await,
        }
    }

    /// Dispatch to the inner plugin's `health_check`.
    pub async fn health_check(&self) -> HealthReport {
        match self {
            Self::Respondent(r) => r.health_check().await,
            Self::Warden(w) => w.health_check().await,
        }
    }

    /// Human-readable name of the interaction shape for diagnostics.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Respondent(_) => "respondent",
            Self::Warden(_) => "warden",
        }
    }
}
