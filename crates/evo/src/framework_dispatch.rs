// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Framework-internal fire dispatch.
//!
//! The framework's appointment and watch primitives dispatch
//! fires through the router, which routes by shelf to admitted
//! plugins. That model is correct for plugin-owned scheduling
//! (an OAuth-refresh task fires back into the plugin that
//! scheduled it), but framework-internal subsystems — the plan
//! engine, future power-management work, audit-ledger compaction
//! sweeps, reconciliation runs — are not plugins and have no
//! shelf. They need a different dispatch path.
//!
//! This module owns that path. [`FrameworkFireHandler`] is the
//! trait every framework-internal scheduled-work consumer
//! implements. [`AppointmentRuntime`] and [`WatchRuntime`] each
//! carry an optional handler slot; when a fire arrives whose
//! `creator` matches the framework-reserved prefix
//! [`FRAMEWORK_CREATOR_PREFIX`] and a handler is registered, the
//! runtime invokes the handler instead of routing through the
//! router. Plugin-owned fires (creator without the reserved
//! prefix) flow through the router as before.
//!
//! ## Architecture
//!
//! Two dispatch surfaces by design:
//!
//! - **Plugin dispatch (existing)**: appointment/watch action →
//!   router.handle_request → plugin admitted on shelf. Stable
//!   for years; covers every plugin-author use case.
//! - **Framework-internal dispatch (new)**: appointment/watch
//!   fire → optional `FrameworkFireHandler` → consumer
//!   subsystem. Reserved for the framework's own subsystems;
//!   plugins never reach this path because the reserved-prefix
//!   creator strings are off-limits at the admission boundary.
//!
//! The boundary is the `creator` field on the appointment /
//! watch entry. Plugins author appointments through
//! `LoadContext::appointments` which derives `creator` from the
//! plugin's canonical name; the framework refuses canonical
//! names that begin with `evo.` so a plugin cannot squat on a
//! framework-reserved creator. Framework subsystems schedule
//! through the runtime directly with `creator = "evo.<subsystem>"`.
//!
//! ## Multi-subscriber composition
//!
//! Each runtime carries one handler slot. With one consumer
//! (the plan engine, today) the slot holds the engine directly.
//! With multiple consumers (when power management or
//! reconciliation lands), a thin
//! [`MultiFrameworkFireHandler`] composes them by creator
//! sub-prefix and is set as the single slot value. The
//! composition is a wrapper, not a substrate change.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use evo_plugin_sdk::contract::{AppointmentAction, WatchAction};

/// Reserved creator prefix for framework-internal scheduled
/// work. Appointment / watch entries whose `creator` starts with
/// this prefix are routed to the registered
/// [`FrameworkFireHandler`] (if any) instead of through the
/// router.
///
/// The admission boundary refuses plugin canonical names that
/// match this prefix so plugins cannot squat on a framework
/// creator. Framework subsystems schedule with creators like
/// `evo.plans`, `evo.power`, `evo.reconciliation`.
pub const FRAMEWORK_CREATOR_PREFIX: &str = "evo.";

/// Boxed-future shape used by the trait methods. Object-safe
/// async traits in stable Rust use this form: an owned future
/// pinned in a Box, with the lifetime threaded through so the
/// callee can borrow from `&self` and the call arguments.
pub type FrameworkFireFuture<'a> =
    Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// Receiver for framework-internal scheduled-work fires.
///
/// Implemented by framework subsystems that schedule appointments
/// or watches with a [`FRAMEWORK_CREATOR_PREFIX`]-prefixed
/// creator. The framework runtime invokes the handler at fire
/// time instead of routing through the plugin router.
///
/// ## Cancellation
///
/// Implementations must keep their internal state consistent if
/// a returned future is dropped mid-execution: no partial
/// writes, no held locks, no leaked resources. The same
/// cooperative-cancellation discipline plugin trait
/// implementations follow.
pub trait FrameworkFireHandler: Send + Sync {
    /// Handle an appointment fire. Called once per fire by the
    /// runtime; the runtime has already passed time-trust and
    /// miss-policy gates before reaching this method. Returning
    /// from the future signals the fire is processed; the
    /// runtime continues with its post-fire state-advance
    /// machinery (next-fire computation, persistence
    /// write-through, etc.) regardless of the handler's outcome.
    fn on_appointment_fire<'a>(
        &'a self,
        creator: &'a str,
        appointment_id: &'a str,
        action: &'a AppointmentAction,
    ) -> FrameworkFireFuture<'a>;

    /// Handle a watch fire. Same contract as appointment fires:
    /// runtime gates have already passed; handler returning is
    /// the fire-processed signal.
    fn on_watch_fire<'a>(
        &'a self,
        creator: &'a str,
        watch_id: &'a str,
        action: &'a WatchAction,
    ) -> FrameworkFireFuture<'a>;
}

/// Routes framework fires to multiple consumers by creator
/// sub-prefix. Today the framework has one consumer (the plan
/// engine); when a second arrives — power management,
/// reconciliation, audit-ledger compaction — the steward boot
/// composes them through this struct rather than touching the
/// runtime substrate.
///
/// Routing is by exact creator-prefix match, longest match
/// wins. A consumer registered under `evo.plans` receives every
/// fire whose creator starts with that string; one under
/// `evo.power` receives only its own. Unmatched creators in the
/// reserved namespace are dropped with a tracing warning so a
/// misconfigured deployment surfaces immediately.
#[derive(Default)]
pub struct MultiFrameworkFireHandler {
    routes: Vec<(String, Arc<dyn FrameworkFireHandler>)>,
}

impl MultiFrameworkFireHandler {
    /// Construct an empty router. Compose with [`Self::route`]
    /// before installing on a runtime.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a handler under a creator-prefix. Builder-style;
    /// chains.
    pub fn route(
        mut self,
        creator_prefix: impl Into<String>,
        handler: Arc<dyn FrameworkFireHandler>,
    ) -> Self {
        self.routes.push((creator_prefix.into(), handler));
        // sort longest-prefix-first so the lookup is a linear
        // scan with the right priority semantics
        self.routes
            .sort_by_key(|(prefix, _)| std::cmp::Reverse(prefix.len()));
        self
    }

    fn lookup(&self, creator: &str) -> Option<&Arc<dyn FrameworkFireHandler>> {
        self.routes
            .iter()
            .find(|(prefix, _)| creator.starts_with(prefix))
            .map(|(_, h)| h)
    }
}

impl FrameworkFireHandler for MultiFrameworkFireHandler {
    fn on_appointment_fire<'a>(
        &'a self,
        creator: &'a str,
        appointment_id: &'a str,
        action: &'a AppointmentAction,
    ) -> FrameworkFireFuture<'a> {
        Box::pin(async move {
            match self.lookup(creator) {
                Some(handler) => {
                    handler
                        .on_appointment_fire(creator, appointment_id, action)
                        .await
                }
                None => {
                    tracing::warn!(
                        creator,
                        appointment_id,
                        "framework fire handler: no route for reserved-prefix creator; fire dropped"
                    );
                }
            }
        })
    }

    fn on_watch_fire<'a>(
        &'a self,
        creator: &'a str,
        watch_id: &'a str,
        action: &'a WatchAction,
    ) -> FrameworkFireFuture<'a> {
        Box::pin(async move {
            match self.lookup(creator) {
                Some(handler) => {
                    handler.on_watch_fire(creator, watch_id, action).await
                }
                None => {
                    tracing::warn!(
                        creator,
                        watch_id,
                        "framework fire handler: no route for reserved-prefix creator; fire dropped"
                    );
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingHandler {
        appointment_fires: Mutex<Vec<(String, String)>>,
        watch_fires: Mutex<Vec<(String, String)>>,
    }

    impl FrameworkFireHandler for RecordingHandler {
        fn on_appointment_fire<'a>(
            &'a self,
            creator: &'a str,
            appointment_id: &'a str,
            _action: &'a AppointmentAction,
        ) -> FrameworkFireFuture<'a> {
            Box::pin(async move {
                self.appointment_fires
                    .lock()
                    .unwrap()
                    .push((creator.to_string(), appointment_id.to_string()));
            })
        }

        fn on_watch_fire<'a>(
            &'a self,
            creator: &'a str,
            watch_id: &'a str,
            _action: &'a WatchAction,
        ) -> FrameworkFireFuture<'a> {
            Box::pin(async move {
                self.watch_fires
                    .lock()
                    .unwrap()
                    .push((creator.to_string(), watch_id.to_string()));
            })
        }
    }

    fn dummy_appointment_action() -> AppointmentAction {
        AppointmentAction {
            target_shelf: "evo.plans".into(),
            request_type: "fire_plan".into(),
            payload: json!({}),
        }
    }

    fn dummy_watch_action() -> WatchAction {
        WatchAction {
            target_shelf: "evo.plans".into(),
            request_type: "fire_plan".into(),
            payload: json!({}),
        }
    }

    #[tokio::test]
    async fn multi_handler_routes_by_prefix() {
        let plans = Arc::new(RecordingHandler::default());
        let power = Arc::new(RecordingHandler::default());
        let multi = MultiFrameworkFireHandler::new()
            .route("evo.plans", plans.clone() as Arc<dyn FrameworkFireHandler>)
            .route("evo.power", power.clone() as Arc<dyn FrameworkFireHandler>);
        multi
            .on_appointment_fire(
                "evo.plans",
                "morning",
                &dummy_appointment_action(),
            )
            .await;
        multi
            .on_appointment_fire(
                "evo.power",
                "wake-rtc",
                &dummy_appointment_action(),
            )
            .await;
        assert_eq!(plans.appointment_fires.lock().unwrap().len(), 1);
        assert_eq!(power.appointment_fires.lock().unwrap().len(), 1);
        assert_eq!(plans.appointment_fires.lock().unwrap()[0].1, "morning");
        assert_eq!(power.appointment_fires.lock().unwrap()[0].1, "wake-rtc");
    }

    #[tokio::test]
    async fn multi_handler_drops_unrouted_creator() {
        let plans = Arc::new(RecordingHandler::default());
        let multi = MultiFrameworkFireHandler::new()
            .route("evo.plans", plans.clone() as Arc<dyn FrameworkFireHandler>);
        multi
            .on_appointment_fire(
                "evo.unknown",
                "stray",
                &dummy_appointment_action(),
            )
            .await;
        assert!(plans.appointment_fires.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn multi_handler_longest_prefix_wins() {
        let general = Arc::new(RecordingHandler::default());
        let specific = Arc::new(RecordingHandler::default());
        let multi = MultiFrameworkFireHandler::new()
            .route("evo.", general.clone() as Arc<dyn FrameworkFireHandler>)
            .route(
                "evo.plans",
                specific.clone() as Arc<dyn FrameworkFireHandler>,
            );
        multi
            .on_appointment_fire(
                "evo.plans",
                "morning",
                &dummy_appointment_action(),
            )
            .await;
        multi
            .on_appointment_fire(
                "evo.audit",
                "compact",
                &dummy_appointment_action(),
            )
            .await;
        assert_eq!(specific.appointment_fires.lock().unwrap().len(), 1);
        assert_eq!(general.appointment_fires.lock().unwrap().len(), 1);
        assert_eq!(specific.appointment_fires.lock().unwrap()[0].1, "morning");
        assert_eq!(general.appointment_fires.lock().unwrap()[0].1, "compact");
    }

    #[tokio::test]
    async fn multi_handler_routes_watch_fires_too() {
        let plans = Arc::new(RecordingHandler::default());
        let multi = MultiFrameworkFireHandler::new()
            .route("evo.plans", plans.clone() as Arc<dyn FrameworkFireHandler>);
        multi
            .on_watch_fire("evo.plans", "trigger-1", &dummy_watch_action())
            .await;
        assert_eq!(plans.watch_fires.lock().unwrap().len(), 1);
        assert_eq!(plans.watch_fires.lock().unwrap()[0].1, "trigger-1");
    }
}
