// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! [`NetworkRelayRuntime`] — chain-aware relay
//! orchestration for multi-VLAN and site-to-site VPN
//! visibility.
//!
//! A device with route into more than one network (multi-
//! NIC, trunk port, or L3 reachability to peers on other
//! networks) declares itself a relay via a chain entry
//! (`DomainStateOp::DeclareNetworkRelay`). The declaration
//! is durable + audit-recorded in the chain; receivers see
//! the declared relay in their `relays` projection.
//!
//! Once declared, the relay runtime forwards inbound
//! domain-witness messages from one network to peers on
//! its other declared networks. Forwarding is best-effort
//! and de-duplicated by chain-entry hash (the chain
//! runtime's idempotent `try_append` covers re-delivery
//! of an already-known witness).
//!
//! Multiple relays per network-pair are first-class. Two
//! devices both bridging A↔B is the desired topology —
//! eliminates single-relay failure. Receivers tolerate
//! duplicate delivery via hash de-dup.
//!
//! Site-to-site VPN tunnels are covered by the same
//! mechanism: the VPN is a routed-L3 hop between networks,
//! a device with VPN reach into both sites' networks
//! declares itself as a relay between them.

use std::sync::Arc;

use evo_witness::{
    DomainStateOp, NetworkDeclaration, NetworkEndpoint, RelayCapability,
};
use thiserror::Error;

use crate::domain_witness::runtime::{
    DomainWitnessRuntime, DomainWitnessRuntimeError,
};

/// Errors raised by [`NetworkRelayRuntime`].
#[derive(Debug, Error)]
pub enum RelayRuntimeError {
    /// The runtime layer rejected the declare-relay
    /// gesture (signature, prev_hash, or chain
    /// persistence).
    #[error("witness runtime: {0}")]
    WitnessRuntime(#[from] DomainWitnessRuntimeError),
    /// The chain accepted the gesture for parsing but the
    /// deterministic linearisation race resolved against it
    /// (e.g. a concurrent declare-relay from another seat
    /// landed first). The operator's intent is NOT on the
    /// canonical chain; caller MAY retry on the new head.
    #[error("chain append outcome: {0}")]
    ChainAppend(String),
}

/// Configuration for the relay runtime.
#[derive(Debug, Clone, Default)]
pub struct NetworkRelayConfig {
    /// Capabilities the relay offers when it declares
    /// itself. Defaults to `chain_forward` only — the
    /// minimum viable role. Operator-grade installs can
    /// extend with `presence_correlate` and
    /// `endpoint_resolution` once those are wired.
    pub capabilities: Vec<RelayCapability>,
}

/// Runtime that owns the operator-facing declare-relay
/// gesture + (eventually) the cross-network forward task.
///
/// First cut: declare-relay only. The forward task that
/// re-broadcasts chain entries between networks layers on
/// top of the chain runtime's inbound subscriber surface;
/// it lands in subsequent commits alongside the per-
/// network broadcast socket plumbing.
pub struct NetworkRelayRuntime {
    config: NetworkRelayConfig,
    witness_runtime: Arc<DomainWitnessRuntime>,
}

impl NetworkRelayRuntime {
    /// Construct a relay runtime over the supplied chain
    /// runtime + config.
    pub fn new(
        config: NetworkRelayConfig,
        witness_runtime: Arc<DomainWitnessRuntime>,
    ) -> Self {
        Self {
            config,
            witness_runtime,
        }
    }

    /// Declare this device as a chain-aware relay between
    /// the supplied networks. Operator-gestured via the
    /// `declare_network_relay` wire op, or auto-emitted
    /// at boot when the device detects multi-network
    /// reachability.
    ///
    /// Appends a signed `DeclareNetworkRelay` chain entry;
    /// receivers see the relay in their projection on the
    /// next chain head advance.
    pub async fn declare_relay(
        &self,
        networks: Vec<NetworkDeclaration>,
        local_endpoints: Vec<NetworkEndpoint>,
    ) -> Result<(), RelayRuntimeError> {
        let capabilities = if self.config.capabilities.is_empty() {
            vec![RelayCapability::ChainForward]
        } else {
            self.config.capabilities.clone()
        };
        let op = DomainStateOp::DeclareNetworkRelay {
            networks,
            capabilities,
        };
        let (_witness, outcome) = self
            .witness_runtime
            .append_local_gesture(op, local_endpoints)
            .await?;
        if !outcome.is_canonical() {
            return Err(RelayRuntimeError::ChainAppend(format!(
                "declare_relay: gesture outvoted by concurrent \
                 gesture; outcome={outcome:?}"
            )));
        }
        Ok(())
    }

    /// Snapshot the relays currently declared in the chain
    /// projection. Stable ordering by device id.
    pub fn list_relays(&self) -> Vec<RelayDescriptor> {
        let projection = self.witness_runtime.current_projection();
        let mut rows: Vec<RelayDescriptor> = projection
            .relays
            .values()
            .map(|r| RelayDescriptor {
                device_id: r.device_id.clone(),
                networks: r.networks.clone(),
                capabilities: r.capabilities.clone(),
                declared_at_ns: r.declared_at_ns,
            })
            .collect();
        rows.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        rows
    }
}

/// Operator-facing descriptor for a declared relay,
/// projected from the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayDescriptor {
    /// Relay device id.
    pub device_id: String,
    /// Networks the relay bridges.
    pub networks: Vec<NetworkDeclaration>,
    /// Capabilities the relay offers.
    pub capabilities: Vec<RelayCapability>,
    /// Wall-clock nanoseconds at declaration time.
    pub declared_at_ns: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain_witness::chain::{DomainChain, DomainChainPersistence};
    use ed25519_dalek::SigningKey;
    use evo_witness::{NetworkEndpoint, NetworkReach};
    use rand_core::OsRng;

    fn fresh_key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    fn endpoint(network_id: &str, address: &str, port: u16) -> NetworkEndpoint {
        NetworkEndpoint {
            network_id: network_id.into(),
            address: address.into(),
            port,
        }
    }

    fn fresh_runtime() -> Arc<DomainWitnessRuntime> {
        let chain = Arc::new(DomainChain::new(DomainChainPersistence::Memory));
        Arc::new(DomainWitnessRuntime::new(
            chain,
            fresh_key(),
            "relay-device".into(),
        ))
    }

    #[tokio::test]
    async fn declare_relay_appends_chain_entry() {
        let witness_runtime = fresh_runtime();
        witness_runtime
            .bootstrap_genesis(
                "Relay Device".into(),
                vec![endpoint("audio-vlan-10", "10.10.0.7", 7331)],
            )
            .await
            .unwrap();

        let relay = NetworkRelayRuntime::new(
            NetworkRelayConfig::default(),
            Arc::clone(&witness_runtime),
        );

        let networks = vec![
            NetworkDeclaration {
                network_id: "audio-vlan-10".into(),
                endpoint: endpoint("audio-vlan-10", "10.10.0.7", 7331),
                reach: NetworkReach::LocalL2,
            },
            NetworkDeclaration {
                network_id: "control-vlan-20".into(),
                endpoint: endpoint("control-vlan-20", "10.20.0.7", 7331),
                reach: NetworkReach::LocalL2,
            },
        ];
        relay
            .declare_relay(
                networks,
                vec![endpoint("audio-vlan-10", "10.10.0.7", 7331)],
            )
            .await
            .unwrap();

        let descriptors = relay.list_relays();
        assert_eq!(descriptors.len(), 1);
        assert_eq!(descriptors[0].device_id, "relay-device");
        assert_eq!(descriptors[0].networks.len(), 2);
        assert!(descriptors[0]
            .capabilities
            .contains(&RelayCapability::ChainForward));
    }

    #[tokio::test]
    async fn list_relays_is_empty_pre_declaration() {
        let witness_runtime = fresh_runtime();
        witness_runtime
            .bootstrap_genesis(
                "Device".into(),
                vec![endpoint("a", "10.10.0.7", 7331)],
            )
            .await
            .unwrap();
        let relay = NetworkRelayRuntime::new(
            NetworkRelayConfig::default(),
            witness_runtime,
        );
        assert!(relay.list_relays().is_empty());
    }
}
