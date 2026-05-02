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

    /// Dispatches to `Respondent::handle_request`.
    fn handle_request<'a>(
        &'a mut self,
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
        &'a mut self,
        req: &'a Request,
    ) -> Pin<Box<dyn Future<Output = Result<Response, PluginError>> + Send + 'a>>
    {
        Box::pin(Respondent::handle_request(&mut self.inner, req))
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
