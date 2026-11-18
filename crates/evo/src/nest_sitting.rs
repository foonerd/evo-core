// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The household sitting a nested dispatch inherits from the
//! consumer request it serves.
//!
//! A plugin answering a consumer's request may dispatch another
//! plugin's verb to do part of the work: `library.remove_source`
//! hands a USB volume to `storage.usb.safe_remove`. The nested call
//! runs under the framework's plugin principal and carries no
//! step-up token of its own. At a household level that protects the
//! nested verb's scope, the household gate would refuse it, even
//! when the consumer proved a sitting on the outer call.
//!
//! This module carries the consumer's sitting across the nest:
//!
//! - While a consumer's `request` dispatch is open on a plugin, the
//!   steward records the consumer's connection against that plugin,
//!   and the step-up token the request presented, if any. The record
//!   is a scoped guard. It drops when the dispatch returns, on every
//!   path.
//! - A nested dispatch names the plugin it comes from. In process,
//!   that is the plugin whose handler is running on the current
//!   task. Out of process, it is the plugin the wire connection
//!   belongs to.
//! - The household gate on the nested verb accepts the recorded
//!   token as if the consumer had presented it. The token is
//!   validated at that moment against the consumer's identity, not
//!   the nest's.
//!
//! The rules that bound it:
//!
//! - **One consumer.** While dispatches from two connections are
//!   open on the plugin, the nest cannot be attributed, so nothing
//!   is inherited and a protected nest is refused.
//! - **No sitting, nothing inherited.** A consumer request that
//!   presented no token gives its nests nothing. An ungated outer
//!   verb is therefore not a way round the gate on an inner one.
//! - **Per dispatch.** The record lives exactly as long as the
//!   dispatch. It is never stored on a connection and never reaches
//!   a plugin payload. It grants no capability: the nested verb's
//!   manifest gates as before.
//! - **Down the chain.** A nest is recorded under the consumer it
//!   serves, so a nest it makes in turn finds the same consumer
//!   under the same rules.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::prompts::ResponderConnectionId;

/// A step-up token as one consumer request presented it, with the
/// identity that token must validate against.
#[derive(Clone)]
pub(crate) struct ConsumerSitting {
    token: String,
    identity: String,
}

impl ConsumerSitting {
    /// Record `token` as presented by the caller `identity`, in the
    /// form `crate::auth::caller_identity` gives.
    pub(crate) fn new(
        token: impl Into<String>,
        identity: impl Into<String>,
    ) -> Self {
        Self {
            token: token.into(),
            identity: identity.into(),
        }
    }

    /// The token, for validation at the household gate.
    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    /// The identity the token must validate against.
    pub(crate) fn identity(&self) -> &str {
        &self.identity
    }
}

impl std::fmt::Debug for ConsumerSitting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsumerSitting")
            .field("token", &"<redacted>")
            .field("identity", &self.identity)
            .finish()
    }
}

/// The consumer a dispatch serves: its connection, and its sitting
/// when the consumer's request presented one.
#[derive(Clone, Debug)]
pub(crate) struct NestOrigin {
    connection: ResponderConnectionId,
    sitting: Option<ConsumerSitting>,
}

impl NestOrigin {
    /// A consumer on `connection`, with or without a sitting.
    pub(crate) fn new(
        connection: ResponderConnectionId,
        sitting: Option<ConsumerSitting>,
    ) -> Self {
        Self {
            connection,
            sitting,
        }
    }

    /// The consumer's sitting, if its request presented one.
    pub(crate) fn sitting(&self) -> Option<&ConsumerSitting> {
        self.sitting.as_ref()
    }
}

/// One open dispatch on a plugin.
#[derive(Debug)]
struct OpenDispatch {
    id: u64,
    origin: NestOrigin,
}

/// The consumer dispatches open on each plugin.
///
/// Rows enter through `enter` and leave when the returned guard
/// drops. `origin_of` answers which consumer a nest from a plugin
/// serves.
#[derive(Debug, Default)]
pub(crate) struct NestSittings {
    open: Mutex<HashMap<String, Vec<OpenDispatch>>>,
    next_id: AtomicU64,
}

impl NestSittings {
    /// Record that `origin` has a dispatch open on `plugin` for as
    /// long as the returned guard lives.
    #[must_use = "the record lasts only as long as the guard"]
    pub(crate) fn enter(
        self: &Arc<Self>,
        plugin: &str,
        origin: NestOrigin,
    ) -> OpenDispatchGuard {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.open
            .lock()
            .expect("nest sitting registry mutex poisoned")
            .entry(plugin.to_string())
            .or_default()
            .push(OpenDispatch { id, origin });
        OpenDispatchGuard {
            registry: Arc::clone(self),
            plugin: plugin.to_string(),
            id,
        }
    }

    /// The consumer a nest from `plugin` serves, when that is
    /// unambiguous.
    ///
    /// `Some` only while every dispatch open on the plugin belongs
    /// to one connection. The sitting is the most recent one that
    /// connection presented among those dispatches, or `None` when
    /// none of them presented one. Dispatches from two connections,
    /// or none at all, answer `None`.
    pub(crate) fn origin_of(&self, plugin: &str) -> Option<NestOrigin> {
        let open = self
            .open
            .lock()
            .expect("nest sitting registry mutex poisoned");
        let rows = open.get(plugin)?;
        let connection = rows.first()?.origin.connection;
        if rows.iter().any(|r| r.origin.connection != connection) {
            return None;
        }
        let sitting = rows.iter().rev().find_map(|r| r.origin.sitting.clone());
        Some(NestOrigin {
            connection,
            sitting,
        })
    }
}

/// Removes one open dispatch from the registry when dropped.
pub(crate) struct OpenDispatchGuard {
    registry: Arc<NestSittings>,
    plugin: String,
    id: u64,
}

impl Drop for OpenDispatchGuard {
    fn drop(&mut self) {
        let mut open = self
            .registry
            .open
            .lock()
            .expect("nest sitting registry mutex poisoned");
        if let Some(rows) = open.get_mut(&self.plugin) {
            rows.retain(|r| r.id != self.id);
            if rows.is_empty() {
                open.remove(&self.plugin);
            }
        }
    }
}

tokio::task_local! {
    /// The plugin whose handler the current task is running.
    static RUNNING_PLUGIN: String;
    /// The plugin a nested dispatch on the current task comes from.
    static NESTED_FROM: String;
}

/// Run `fut` as `plugin`'s handler, so that a nest the handler makes
/// on this task names `plugin` as its origin.
pub(crate) async fn run_as_plugin<F: Future>(
    plugin: Option<String>,
    fut: F,
) -> F::Output {
    match plugin {
        Some(plugin) => RUNNING_PLUGIN.scope(plugin, fut).await,
        None => fut.await,
    }
}

/// Run `fut`, a nested dispatch, as coming from `origin`.
pub(crate) async fn nest_from<F: Future>(
    origin: Option<String>,
    fut: F,
) -> F::Output {
    match origin {
        Some(origin) => NESTED_FROM.scope(origin, fut).await,
        None => fut.await,
    }
}

/// The plugin the nested dispatch running on this task comes from.
/// `None` outside a nested dispatch.
pub(crate) fn nested_from() -> Option<String> {
    NESTED_FROM.try_with(Clone::clone).ok()
}

/// The plugin a nested dispatch should name as its origin.
///
/// `claimed` is the plugin the dispatcher was told the call comes
/// from (the wire connection's plugin, for an out-of-process
/// caller), or `None` when it was not told. The handler running on
/// this task, if any, takes precedence. A claim that contradicts it
/// names no origin, so one plugin's handler cannot borrow the
/// sitting of a consumer another plugin serves.
pub(crate) fn origin_plugin(claimed: Option<&str>) -> Option<String> {
    let running = RUNNING_PLUGIN.try_with(Clone::clone).ok();
    resolve_origin(claimed, running)
}

fn resolve_origin(
    claimed: Option<&str>,
    running: Option<String>,
) -> Option<String> {
    match (claimed, running) {
        (None, running) => running,
        (Some(claimed), None) => Some(claimed.to_string()),
        (Some(claimed), Some(running)) => {
            (claimed == running).then_some(running)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(n: u64) -> ResponderConnectionId {
        ResponderConnectionId(n)
    }

    fn sat(token: &str) -> Option<ConsumerSitting> {
        Some(ConsumerSitting::new(token, "bearer:operator"))
    }

    #[test]
    fn one_consumer_with_a_sitting_is_the_origin() {
        let reg = Arc::new(NestSittings::default());
        let _g = reg.enter("org.relay", NestOrigin::new(conn(1), sat("t1")));
        let origin = reg.origin_of("org.relay").expect("one consumer");
        assert_eq!(origin.connection, conn(1));
        assert_eq!(origin.sitting().map(ConsumerSitting::token), Some("t1"));
    }

    #[test]
    fn a_consumer_without_a_sitting_gives_nothing() {
        let reg = Arc::new(NestSittings::default());
        let _g = reg.enter("org.relay", NestOrigin::new(conn(1), None));
        let origin = reg.origin_of("org.relay").expect("one consumer");
        assert!(origin.sitting().is_none());
    }

    #[test]
    fn two_consumers_on_one_plugin_are_no_origin() {
        let reg = Arc::new(NestSittings::default());
        let _a = reg.enter("org.relay", NestOrigin::new(conn(1), sat("t1")));
        let _b = reg.enter("org.relay", NestOrigin::new(conn(2), sat("t2")));
        assert!(reg.origin_of("org.relay").is_none());
    }

    #[test]
    fn one_consumer_twice_keeps_its_latest_sitting() {
        let reg = Arc::new(NestSittings::default());
        let _a = reg.enter("org.relay", NestOrigin::new(conn(1), sat("t1")));
        let _b = reg.enter("org.relay", NestOrigin::new(conn(1), None));
        let _c = reg.enter("org.relay", NestOrigin::new(conn(1), sat("t3")));
        let origin = reg.origin_of("org.relay").expect("one consumer");
        assert_eq!(origin.sitting().map(ConsumerSitting::token), Some("t3"));
    }

    #[test]
    fn the_record_ends_with_its_guard() {
        let reg = Arc::new(NestSittings::default());
        let a = reg.enter("org.relay", NestOrigin::new(conn(1), sat("t1")));
        let b = reg.enter("org.relay", NestOrigin::new(conn(2), None));
        assert!(reg.origin_of("org.relay").is_none());
        drop(b);
        assert_eq!(
            reg.origin_of("org.relay")
                .and_then(|o| o.sitting().map(|s| s.token().to_string())),
            Some("t1".to_string())
        );
        drop(a);
        assert!(reg.origin_of("org.relay").is_none());
        assert!(reg.open.lock().unwrap().is_empty());
    }

    #[test]
    fn origins_are_per_plugin() {
        let reg = Arc::new(NestSittings::default());
        let _a = reg.enter("org.relay", NestOrigin::new(conn(1), sat("t1")));
        let _b = reg.enter("org.other", NestOrigin::new(conn(2), None));
        assert!(reg.origin_of("org.relay").is_some());
        assert!(reg.origin_of("org.unknown").is_none());
    }

    #[test]
    fn the_token_is_redacted_in_debug() {
        let shown = format!("{:?}", ConsumerSitting::new("secret", "bearer:x"));
        assert!(!shown.contains("secret"), "{shown}");
    }

    #[test]
    fn a_claim_against_the_running_handler_names_no_origin() {
        assert_eq!(resolve_origin(None, None), None);
        assert_eq!(
            resolve_origin(None, Some("org.a".into())),
            Some("org.a".into())
        );
        assert_eq!(resolve_origin(Some("org.a"), None), Some("org.a".into()));
        assert_eq!(
            resolve_origin(Some("org.a"), Some("org.a".into())),
            Some("org.a".into())
        );
        assert_eq!(resolve_origin(Some("org.b"), Some("org.a".into())), None);
    }

    #[tokio::test]
    async fn the_task_carries_the_running_plugin_and_the_nest() {
        assert_eq!(origin_plugin(None), None);
        assert_eq!(nested_from(), None);
        run_as_plugin(Some("org.relay".into()), async {
            assert_eq!(origin_plugin(None), Some("org.relay".into()));
            assert_eq!(origin_plugin(Some("org.other")), None);
            nest_from(origin_plugin(None), async {
                assert_eq!(nested_from(), Some("org.relay".into()));
            })
            .await;
            assert_eq!(nested_from(), None);
        })
        .await;
        assert_eq!(origin_plugin(None), None);
    }
}
