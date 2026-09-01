// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! GraphQL runtime mount.
//!
//! Projects the canonical wire-protocol schema into an
//! async-graphql Schema whose `dispatch` mutation routes wire
//! ops through a host-supplied dispatcher trait. The runtime
//! mounts this Schema behind axum's `GraphQL` extractor on a
//! plain HTTP listener (production deployments front it with
//! the existing TLS substrate).
//!
//! The Schema deliberately stays generic: one
//! `dispatch(opId: String!, payloadJson: String) -> DispatchResult`
//! mutation, one `describeCapabilities -> DispatchResult` query.
//! The strongly-typed bindings for each wire op live in the
//! per-language client SDKs the schema projection emits, not
//! in the GraphQL Schema itself — keeping the GraphQL surface
//! constant across schema additions and avoiding the
//! N-mutation-per-op explosion industry standard treats as an
//! unfortunate fact of life.

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Errors raised by [`serve_graphql`].
#[derive(Debug, thiserror::Error)]
pub enum GraphQlError {
    /// The configured socket could not be bound.
    #[error("bind {addr}: {source}")]
    Bind {
        /// Address the listener attempted.
        addr: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The crate was compiled without the `enabled` feature.
    #[error("evo-graphql was compiled without the `enabled` feature")]
    Disabled,
}

/// The dispatcher seam the GraphQL service consults. Identical
/// shape to the gRPC dispatcher seam — host crates that already
/// implement one need only an `impl` adapter to satisfy the
/// other.
#[async_trait::async_trait]
pub trait WireOpDispatcher: Send + Sync + 'static {
    /// Dispatch a wire op via its canonical snake-case
    /// identifier and JSON-encoded payload. Returns the JSON
    /// response bytes plus a flag identifying error envelopes.
    async fn dispatch(
        &self,
        op_id: &str,
        payload_json: &str,
        bearer_token: &str,
    ) -> DispatchOutcome;
}

/// Result of a wire-op dispatch via the GraphQL mount.
#[derive(Debug, Clone)]
pub struct DispatchOutcome {
    /// JSON-encoded response string.
    pub response_json: String,
    /// Whether the response is a structured error envelope.
    pub is_error: bool,
}

#[cfg(feature = "enabled")]
mod imp {
    use super::*;
    use async_graphql::{
        Context, EmptySubscription, Object, Schema, SimpleObject,
    };
    use async_graphql_axum::GraphQL;
    use axum::routing::{any_service, MethodRouter};
    use axum::Router;
    use tokio::net::TcpListener;

    /// Holds the host-supplied dispatcher reachable from
    /// GraphQL field resolvers via the async-graphql `Context`.
    pub struct GraphQlState<D: WireOpDispatcher> {
        dispatcher: Arc<D>,
    }

    /// GraphQL result envelope. Mirrors the gRPC `WireOpResponse`
    /// shape so consumers reading both projections can map
    /// 1:1.
    #[derive(SimpleObject)]
    pub struct DispatchResult {
        /// JSON-encoded response body.
        pub response_json: String,
        /// Whether the response is a structured error envelope.
        pub is_error: bool,
    }

    /// Query root.
    pub struct Query;

    #[Object]
    impl Query {
        /// Capability probe. Returns the same JSON the HTTPS
        /// `/api/v1/describe_capabilities` endpoint serves.
        async fn describe_capabilities<'a>(
            &self,
            ctx: &Context<'a>,
        ) -> async_graphql::Result<DispatchResult> {
            let state = ctx
                .data::<Arc<GraphQlState<DynDispatcher>>>()
                .map_err(|e| async_graphql::Error::new(e.message))?;
            let outcome = state
                .dispatcher
                .dispatch("describe_capabilities", "", "")
                .await;
            Ok(DispatchResult {
                response_json: outcome.response_json,
                is_error: outcome.is_error,
            })
        }
    }

    /// Mutation root.
    pub struct Mutation;

    #[Object]
    impl Mutation {
        /// Generic wire-op dispatch.
        async fn dispatch<'a>(
            &self,
            ctx: &Context<'a>,
            op_id: String,
            #[graphql(default)] payload_json: String,
            #[graphql(default)] bearer_token: String,
        ) -> async_graphql::Result<DispatchResult> {
            let state = ctx
                .data::<Arc<GraphQlState<DynDispatcher>>>()
                .map_err(|e| async_graphql::Error::new(e.message))?;
            let outcome = state
                .dispatcher
                .dispatch(&op_id, &payload_json, &bearer_token)
                .await;
            Ok(DispatchResult {
                response_json: outcome.response_json,
                is_error: outcome.is_error,
            })
        }
    }

    /// Dynamic-dispatch wrapper around any `WireOpDispatcher`
    /// implementation. The GraphQL Context stores a single
    /// concrete type, so the runtime erases the host's specific
    /// dispatcher behind this façade at mount time.
    pub struct DynDispatcher {
        inner: Arc<dyn WireOpDispatcher>,
    }

    #[async_trait::async_trait]
    impl WireOpDispatcher for DynDispatcher {
        async fn dispatch(
            &self,
            op_id: &str,
            payload_json: &str,
            bearer_token: &str,
        ) -> DispatchOutcome {
            self.inner.dispatch(op_id, payload_json, bearer_token).await
        }
    }

    /// The Schema type the runtime mounts.
    pub type EvoGraphqlSchema = Schema<Query, Mutation, EmptySubscription>;

    /// Build the GraphQL schema with the host dispatcher
    /// attached via the context data.
    pub fn build_schema(
        dispatcher: Arc<dyn WireOpDispatcher>,
    ) -> EvoGraphqlSchema {
        let state = Arc::new(GraphQlState {
            dispatcher: Arc::new(DynDispatcher { inner: dispatcher }),
        });
        Schema::build(Query, Mutation, EmptySubscription)
            .data(state)
            .finish()
    }

    /// Bind the GraphQL listener and serve until shutdown.
    pub async fn serve_graphql(
        listen: SocketAddr,
        dispatcher: Arc<dyn WireOpDispatcher>,
        shutdown: Arc<Notify>,
    ) -> Result<(SocketAddr, JoinHandle<()>), GraphQlError> {
        let listener = TcpListener::bind(listen).await.map_err(|source| {
            GraphQlError::Bind {
                addr: listen.to_string(),
                source,
            }
        })?;
        let local_addr =
            listener.local_addr().map_err(|source| GraphQlError::Bind {
                addr: listen.to_string(),
                source,
            })?;
        let schema = build_schema(dispatcher);
        let graphql_service: MethodRouter = any_service(GraphQL::new(schema));
        let app = Router::new().route("/graphql", graphql_service);
        let shutdown_signal = async move {
            shutdown.notified().await;
        };
        let task = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal)
                .await
            {
                tracing::warn!(error = %e, "evo-graphql: server exited with error");
            }
        });
        Ok((local_addr, task))
    }
}

#[cfg(feature = "enabled")]
pub use imp::{build_schema, serve_graphql, DispatchResult, EvoGraphqlSchema};

#[cfg(not(feature = "enabled"))]
/// Stub. Always returns `Err(GraphQlError::Disabled)`.
pub async fn serve_graphql(
    _listen: SocketAddr,
    _dispatcher: Arc<dyn WireOpDispatcher>,
    _shutdown: Arc<Notify>,
) -> Result<(SocketAddr, JoinHandle<()>), GraphQlError> {
    Err(GraphQlError::Disabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoDispatcher;
    #[async_trait::async_trait]
    impl WireOpDispatcher for EchoDispatcher {
        async fn dispatch(
            &self,
            op_id: &str,
            payload_json: &str,
            _bearer_token: &str,
        ) -> DispatchOutcome {
            DispatchOutcome {
                response_json: format!("{op_id}:{payload_json}"),
                is_error: false,
            }
        }
    }

    #[tokio::test]
    async fn dispatch_outcome_round_trips_through_trait() {
        let d = EchoDispatcher;
        let outcome = d.dispatch("describe_capabilities", "", "").await;
        assert_eq!(outcome.response_json, "describe_capabilities:");
        assert!(!outcome.is_error);
    }

    #[cfg(feature = "enabled")]
    #[tokio::test]
    async fn schema_dispatch_mutation_routes_to_host_trait() {
        let schema = build_schema(Arc::new(EchoDispatcher));
        let query = r#"mutation {
            dispatch(opId: "list_plugins", payloadJson: "{\"a\":1}") {
                responseJson
                isError
            }
        }"#;
        let response = schema.execute(query).await;
        assert!(response.errors.is_empty(), "errors: {:?}", response.errors);
        let data = response.data.into_json().unwrap();
        let r = &data["dispatch"];
        assert_eq!(r["responseJson"], "list_plugins:{\"a\":1}");
        assert_eq!(r["isError"], false);
    }
}
