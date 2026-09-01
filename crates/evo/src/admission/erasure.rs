// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Type erasure for admitted plugins.
//!
//! The SDK's public traits ([`Plugin`], [`Respondent`], [`Warden`]) use
//! native async-in-trait with `impl Future + Send` returns. Native
//! async-in-trait is not object-safe (a `dyn Respondent` cannot exist),
//! so the admission engine cannot store concrete plugin types in a
//! single heterogeneous collection without indirection.
//!
//! The indirection lives here: a pair of internal object-safe traits
//! ([`ErasedRespondent`], [`ErasedWarden`]) that mirror the public
//! traits using `Pin<Box<dyn Future + Send>>` returns, plus generic
//! adapters ([`RespondentAdapter`], [`WardenAdapter`]) that wrap any
//! concrete `T: Respondent` / `T: Warden` and implement the matching
//! erased trait by delegating each method to the inner plugin.
//!
//! [`AdmittedHandle`](super::handle::AdmittedHandle) carries one of the
//! two erased traits per admission, decided at admission time from the
//! manifest's `kind.interaction` field. This keeps the public SDK
//! traits zero-allocation (no `dyn` in user-visible code) while letting
//! the engine store mixed-kind plugins behind a single enum.

use evo_plugin_sdk::contract::{
    Assignment, CourseCorrection, CustodyHandle, HealthReport, LoadContext,
    Plugin, PluginDescription, PluginError, Request, Respondent, Response,
    StateBlob, Warden,
};
use std::future::Future;
use std::pin::Pin;

/// Object-safe internal trait for admitted respondent plugins.
///
/// Public SDK traits use native async-in-trait; this internal trait uses
/// `Pin<Box<dyn Future>>` to be object-safe so the engine can store
/// heterogeneous plugins as `Box<dyn ErasedRespondent>`.
pub trait ErasedRespondent: Send + Sync {
    /// Dispatches to `Plugin::describe`.
    fn describe(
        &self,
    ) -> Pin<Box<dyn Future<Output = PluginDescription> + Send + '_>>;

    /// Dispatches to `Plugin::probe_plans`. Synchronous read
    /// of the plugin's declared PPAG probes; the engine runs
    /// the returned plans before `load` so the resulting
    /// `CapabilityResolutionMap` is on the `LoadContext` the
    /// plugin's `load` body sees.
    fn probe_plans(&self) -> Vec<evo_plugin_sdk::privileges::ProbePlan>;

    /// Dispatches to `Plugin::load`.
    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>;

    /// Dispatches to `Plugin::unload`.
    fn unload(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + '_>>;

    /// Dispatches to `Plugin::health_check`.
    fn health_check(
        &self,
    ) -> Pin<Box<dyn Future<Output = HealthReport> + Send + '_>>;

    /// Dispatches to `Respondent::handle_request`. Takes `&self`
    /// so the router can dispatch concurrent requests to the
    /// same plugin without holding a per-entry lock across the
    /// await.
    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> Pin<Box<dyn Future<Output = Result<Response, PluginError>> + Send + 'a>>;

    /// Dispatches to `Plugin::prepare_for_live_reload`.
    fn prepare_for_live_reload(
        &self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<StateBlob>, PluginError>>
                + Send
                + '_,
        >,
    >;

    /// Dispatches to `Plugin::load_with_state`.
    fn load_with_state<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
        blob: Option<StateBlob>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>;
}

/// Generic adapter: wraps any `T: Respondent + 'static` as an
/// [`ErasedRespondent`].
pub struct RespondentAdapter<T: Respondent + 'static> {
    inner: T,
}

impl<T: Respondent + 'static> RespondentAdapter<T> {
    /// Wrap a concrete respondent.
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    /// Unwrap the concrete respondent. Useful for tests.
    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: Respondent + 'static> ErasedRespondent for RespondentAdapter<T> {
    fn describe(
        &self,
    ) -> Pin<Box<dyn Future<Output = PluginDescription> + Send + '_>> {
        Box::pin(Plugin::describe(&self.inner))
    }

    fn probe_plans(&self) -> Vec<evo_plugin_sdk::privileges::ProbePlan> {
        Plugin::probe_plans(&self.inner)
    }

    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        Box::pin(Plugin::load(&mut self.inner, ctx))
    }

    fn unload(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + '_>>
    {
        Box::pin(Plugin::unload(&mut self.inner))
    }

    fn health_check(
        &self,
    ) -> Pin<Box<dyn Future<Output = HealthReport> + Send + '_>> {
        Box::pin(Plugin::health_check(&self.inner))
    }

    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> Pin<Box<dyn Future<Output = Result<Response, PluginError>> + Send + 'a>>
    {
        Box::pin(Respondent::handle_request(&self.inner, req))
    }

    fn prepare_for_live_reload(
        &self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<StateBlob>, PluginError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(Plugin::prepare_for_live_reload(&self.inner))
    }

    fn load_with_state<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
        blob: Option<StateBlob>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        Box::pin(Plugin::load_with_state(&mut self.inner, ctx, blob))
    }
}

/// Object-safe internal trait for admitted warden plugins.
///
/// Parallels [`ErasedRespondent`]: same four core verbs from `Plugin`,
/// plus the three custody verbs from `Warden`. The engine stores
/// wardens as `Box<dyn ErasedWarden>` inside an
/// [`AdmittedHandle`](super::handle::AdmittedHandle).
pub trait ErasedWarden: Send + Sync {
    /// Dispatches to `Plugin::describe`.
    fn describe(
        &self,
    ) -> Pin<Box<dyn Future<Output = PluginDescription> + Send + '_>>;

    /// Dispatches to `Plugin::probe_plans`. Synchronous read
    /// of the plugin's declared PPAG probes; the engine runs
    /// the returned plans before `load` so the resulting
    /// `CapabilityResolutionMap` is on the `LoadContext` the
    /// plugin's `load` body sees.
    fn probe_plans(&self) -> Vec<evo_plugin_sdk::privileges::ProbePlan>;

    /// Dispatches to `Plugin::load`.
    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>;

    /// Dispatches to `Plugin::unload`.
    fn unload(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + '_>>;

    /// Dispatches to `Plugin::health_check`.
    fn health_check(
        &self,
    ) -> Pin<Box<dyn Future<Output = HealthReport> + Send + '_>>;

    /// Dispatches to `Warden::take_custody`.
    fn take_custody<'a>(
        &'a mut self,
        assignment: Assignment,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<CustodyHandle, PluginError>> + Send + 'a,
        >,
    >;

    /// Dispatches to `Warden::course_correct`.
    fn course_correct<'a>(
        &'a mut self,
        handle: &'a CustodyHandle,
        correction: CourseCorrection,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>;

    /// Dispatches to `Warden::release_custody`.
    fn release_custody<'a>(
        &'a mut self,
        handle: CustodyHandle,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>;

    /// Dispatches to `Plugin::prepare_for_live_reload`.
    fn prepare_for_live_reload(
        &self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<StateBlob>, PluginError>>
                + Send
                + '_,
        >,
    >;

    /// Dispatches to `Plugin::load_with_state`.
    fn load_with_state<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
        blob: Option<StateBlob>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>;
}

/// Generic adapter: wraps any `T: Warden + 'static` as an
/// [`ErasedWarden`]. Parallels [`RespondentAdapter`].
pub struct WardenAdapter<T: Warden + 'static> {
    inner: T,
}

impl<T: Warden + 'static> WardenAdapter<T> {
    /// Wrap a concrete warden.
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    /// Unwrap the concrete warden. Useful for tests.
    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: Warden + 'static> ErasedWarden for WardenAdapter<T> {
    fn describe(
        &self,
    ) -> Pin<Box<dyn Future<Output = PluginDescription> + Send + '_>> {
        Box::pin(Plugin::describe(&self.inner))
    }

    fn probe_plans(&self) -> Vec<evo_plugin_sdk::privileges::ProbePlan> {
        Plugin::probe_plans(&self.inner)
    }

    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        Box::pin(Plugin::load(&mut self.inner, ctx))
    }

    fn unload(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + '_>>
    {
        Box::pin(Plugin::unload(&mut self.inner))
    }

    fn health_check(
        &self,
    ) -> Pin<Box<dyn Future<Output = HealthReport> + Send + '_>> {
        Box::pin(Plugin::health_check(&self.inner))
    }

    fn take_custody<'a>(
        &'a mut self,
        assignment: Assignment,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<CustodyHandle, PluginError>> + Send + 'a,
        >,
    > {
        Box::pin(Warden::take_custody(&mut self.inner, assignment))
    }

    fn course_correct<'a>(
        &'a mut self,
        handle: &'a CustodyHandle,
        correction: CourseCorrection,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        Box::pin(Warden::course_correct(&mut self.inner, handle, correction))
    }

    fn release_custody<'a>(
        &'a mut self,
        handle: CustodyHandle,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        Box::pin(Warden::release_custody(&mut self.inner, handle))
    }

    fn prepare_for_live_reload(
        &self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<StateBlob>, PluginError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(Plugin::prepare_for_live_reload(&self.inner))
    }

    fn load_with_state<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
        blob: Option<StateBlob>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        Box::pin(Plugin::load_with_state(&mut self.inner, ctx, blob))
    }
}

/// Object-safe internal trait for plugins that admit as a
/// warden AND additionally expose a respondent surface.
///
/// The audio playback warden (`org.evoframework.playback.mpd`)
/// is the canonical case: it holds playback custody (warden
/// surface for `course_correct`) AND owns one or more music
/// URI schemes (respondent surface for `play_now` /
/// `play_now_collection` source-verb dispatch). Both surfaces
/// must dispatch to the same underlying plugin instance so
/// internal state (custodies, audio_routing handle, MPD
/// connection) is consistent across the two paths.
///
/// Trait composition with two object-safe supertraits
/// (`ErasedWarden` + `ErasedRespondent`) requires explicit
/// accessor methods until dyn-trait upcasting stabilises in
/// the workspace's MSRV. Once MSRV reaches 1.86, the
/// accessors can be removed and the supertraits accessed
/// directly via upcasting.
pub trait ErasedWardenAndRespondent:
    ErasedWarden + ErasedRespondent + Send + Sync
{
    /// Reborrow the handle as an [`ErasedRespondent`] for
    /// request-type dispatch.
    fn as_respondent_mut(&mut self) -> &mut dyn ErasedRespondent;
    /// Reborrow the handle as an [`ErasedWarden`] for
    /// custody / course_correct dispatch.
    fn as_warden_mut(&mut self) -> &mut dyn ErasedWarden;
    /// Reborrow the handle as an [`ErasedRespondent`] for
    /// non-mutating dispatch (describe / health_check /
    /// prepare_for_live_reload).
    fn as_respondent(&self) -> &dyn ErasedRespondent;
    /// Reborrow the handle as an [`ErasedWarden`] for
    /// non-mutating dispatch.
    fn as_warden(&self) -> &dyn ErasedWarden;
}

/// Generic adapter: wraps any `T: Warden + Respondent +
/// 'static` as both [`ErasedWarden`] and [`ErasedRespondent`]
/// over the same `inner: T`. Both surfaces dispatch to the
/// same underlying plugin instance, preserving internal
/// state consistency across the warden and respondent
/// paths.
pub struct WardenAndRespondentAdapter<T: Warden + Respondent + 'static> {
    inner: T,
}

impl<T: Warden + Respondent + 'static> WardenAndRespondentAdapter<T> {
    /// Wrap a concrete plugin that impls both Warden and
    /// Respondent.
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    /// Unwrap the concrete plugin. Useful for tests.
    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: Warden + Respondent + 'static> ErasedWarden
    for WardenAndRespondentAdapter<T>
{
    fn describe(
        &self,
    ) -> Pin<Box<dyn Future<Output = PluginDescription> + Send + '_>> {
        Box::pin(Plugin::describe(&self.inner))
    }

    fn probe_plans(&self) -> Vec<evo_plugin_sdk::privileges::ProbePlan> {
        Plugin::probe_plans(&self.inner)
    }

    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        Box::pin(Plugin::load(&mut self.inner, ctx))
    }

    fn unload(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + '_>>
    {
        Box::pin(Plugin::unload(&mut self.inner))
    }

    fn health_check(
        &self,
    ) -> Pin<Box<dyn Future<Output = HealthReport> + Send + '_>> {
        Box::pin(Plugin::health_check(&self.inner))
    }

    fn take_custody<'a>(
        &'a mut self,
        assignment: Assignment,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<CustodyHandle, PluginError>> + Send + 'a,
        >,
    > {
        Box::pin(Warden::take_custody(&mut self.inner, assignment))
    }

    fn course_correct<'a>(
        &'a mut self,
        handle: &'a CustodyHandle,
        correction: CourseCorrection,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        Box::pin(Warden::course_correct(&mut self.inner, handle, correction))
    }

    fn release_custody<'a>(
        &'a mut self,
        handle: CustodyHandle,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        Box::pin(Warden::release_custody(&mut self.inner, handle))
    }

    fn prepare_for_live_reload(
        &self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<StateBlob>, PluginError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(Plugin::prepare_for_live_reload(&self.inner))
    }

    fn load_with_state<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
        blob: Option<StateBlob>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        Box::pin(Plugin::load_with_state(&mut self.inner, ctx, blob))
    }
}

impl<T: Warden + Respondent + 'static> ErasedRespondent
    for WardenAndRespondentAdapter<T>
{
    fn describe(
        &self,
    ) -> Pin<Box<dyn Future<Output = PluginDescription> + Send + '_>> {
        Box::pin(Plugin::describe(&self.inner))
    }

    fn probe_plans(&self) -> Vec<evo_plugin_sdk::privileges::ProbePlan> {
        Plugin::probe_plans(&self.inner)
    }

    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        Box::pin(Plugin::load(&mut self.inner, ctx))
    }

    fn unload(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + '_>>
    {
        Box::pin(Plugin::unload(&mut self.inner))
    }

    fn health_check(
        &self,
    ) -> Pin<Box<dyn Future<Output = HealthReport> + Send + '_>> {
        Box::pin(Plugin::health_check(&self.inner))
    }

    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> Pin<Box<dyn Future<Output = Result<Response, PluginError>> + Send + 'a>>
    {
        Box::pin(Respondent::handle_request(&self.inner, req))
    }

    fn prepare_for_live_reload(
        &self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<StateBlob>, PluginError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(Plugin::prepare_for_live_reload(&self.inner))
    }

    fn load_with_state<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
        blob: Option<StateBlob>,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>>
    {
        Box::pin(Plugin::load_with_state(&mut self.inner, ctx, blob))
    }
}

impl<T: Warden + Respondent + 'static> ErasedWardenAndRespondent
    for WardenAndRespondentAdapter<T>
{
    fn as_respondent_mut(&mut self) -> &mut dyn ErasedRespondent {
        self
    }

    fn as_warden_mut(&mut self) -> &mut dyn ErasedWarden {
        self
    }

    fn as_respondent(&self) -> &dyn ErasedRespondent {
        self
    }

    fn as_warden(&self) -> &dyn ErasedWarden {
        self
    }
}
