// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! The GraphQL listener's stop: a stop signalled at any time after
//! `serve_graphql` returns ends the listener, even before its task has
//! first run.

#![cfg(feature = "enabled")]

use evo_graphql::{serve_graphql, DispatchOutcome, WireOpDispatcher};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

struct NoDispatch;

#[async_trait::async_trait]
impl WireOpDispatcher for NoDispatch {
    async fn dispatch(&self, _: &str, _: &str, _: &str) -> DispatchOutcome {
        DispatchOutcome {
            response_json: "{}".to_string(),
            is_error: false,
        }
    }
}

/// A stop signalled the moment the listener is spawned, before its task
/// has run at all, ends it.
#[tokio::test]
async fn a_stop_signalled_before_the_listener_first_runs_ends_it() {
    // On this single-threaded runtime nothing else runs between the
    // spawn and the stop.
    let stop = Arc::new(Notify::new());
    let (_, task) = serve_graphql(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(NoDispatch),
        Arc::clone(&stop),
    )
    .await
    .unwrap();
    stop.notify_waiters();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the listener ended on the stop")
        .unwrap();
}
