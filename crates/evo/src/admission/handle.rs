// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

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

use super::erasure::{
    ErasedRespondent, ErasedWarden, ErasedWardenAndRespondent,
};
use evo_plugin_sdk::contract::{
    HealthReport, LoadContext, PluginDescription, PluginError, StateBlob,
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
    /// A warden plugin that ALSO exposes a respondent
    /// surface. Routes course_correct / take_custody /
    /// release_custody through the warden path; routes
    /// handle_request through the respondent path. Both
    /// surfaces dispatch to the same underlying plugin
    /// instance — internal state stays consistent across
    /// the two dispatch planes. Canonical case: an audio
    /// playback warden that owns one or more music URI
    /// schemes and answers source-verb dispatches
    /// targeting them.
    WardenWithRespondent(Box<dyn ErasedWardenAndRespondent>),
}

impl AdmittedHandle {
    /// Dispatch to the inner plugin's `describe`.
    pub async fn describe(&self) -> PluginDescription {
        match self {
            Self::Respondent(r) => r.describe().await,
            Self::Warden(w) => w.describe().await,
            Self::WardenWithRespondent(wr) => wr.as_warden().describe().await,
        }
    }

    /// Dispatch to the inner plugin's `probe_plans`. Synchronous
    /// — the engine reads the plugin's declared PPAG probes,
    /// runs them, and stamps the resulting
    /// [`CapabilityResolutionMap`](evo_plugin_sdk::privileges::CapabilityResolutionMap)
    /// onto [`LoadContext::capabilities`] before invoking
    /// [`load`](Self::load).
    pub fn probe_plans(&self) -> Vec<evo_plugin_sdk::privileges::ProbePlan> {
        match self {
            Self::Respondent(r) => r.probe_plans(),
            Self::Warden(w) => w.probe_plans(),
            Self::WardenWithRespondent(wr) => wr.as_warden().probe_plans(),
        }
    }

    /// Dispatch to the inner plugin's `load`.
    pub async fn load(&mut self, ctx: &LoadContext) -> Result<(), PluginError> {
        match self {
            Self::Respondent(r) => r.load(ctx).await,
            Self::Warden(w) => w.load(ctx).await,
            Self::WardenWithRespondent(wr) => {
                wr.as_warden_mut().load(ctx).await
            }
        }
    }

    /// Dispatch to the inner plugin's `unload`.
    pub async fn unload(&mut self) -> Result<(), PluginError> {
        match self {
            Self::Respondent(r) => r.unload().await,
            Self::Warden(w) => w.unload().await,
            Self::WardenWithRespondent(wr) => wr.as_warden_mut().unload().await,
        }
    }

    /// Dispatch to the inner plugin's `health_check`.
    pub async fn health_check(&self) -> HealthReport {
        match self {
            Self::Respondent(r) => r.health_check().await,
            Self::Warden(w) => w.health_check().await,
            Self::WardenWithRespondent(wr) => {
                wr.as_warden().health_check().await
            }
        }
    }

    /// Dispatch to the inner plugin's `prepare_for_live_reload`. Used
    /// by the admission engine's Live-mode reload path to obtain a
    /// state blob before unloading.
    pub async fn prepare_for_live_reload(
        &self,
    ) -> Result<Option<StateBlob>, PluginError> {
        match self {
            Self::Respondent(r) => r.prepare_for_live_reload().await,
            Self::Warden(w) => w.prepare_for_live_reload().await,
            Self::WardenWithRespondent(wr) => {
                wr.as_warden().prepare_for_live_reload().await
            }
        }
    }

    /// Dispatch to the inner plugin's `load_with_state`. Used by the
    /// admission engine's Live-mode reload path after the previous
    /// instance was unloaded.
    pub async fn load_with_state(
        &mut self,
        ctx: &LoadContext,
        blob: Option<StateBlob>,
    ) -> Result<(), PluginError> {
        match self {
            Self::Respondent(r) => r.load_with_state(ctx, blob).await,
            Self::Warden(w) => w.load_with_state(ctx, blob).await,
            Self::WardenWithRespondent(wr) => {
                wr.as_warden_mut().load_with_state(ctx, blob).await
            }
        }
    }

    /// Human-readable name of the interaction shape for diagnostics.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Respondent(_) => "respondent",
            Self::Warden(_) => "warden",
            Self::WardenWithRespondent(_) => "warden+respondent",
        }
    }
}
