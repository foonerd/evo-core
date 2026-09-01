// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Bridge between [`crate::domain_witness::
//! DomainWitnessRuntime`] and [`crate::happenings::
//! HappeningBus`].
//!
//! The chain runtime emits observation events via the
//! [`WitnessEventEmitter`] trait without knowing where the
//! events land. This module provides the production
//! binding: a thin wrapper around `Arc<HappeningBus>` that
//! translates each trait call into the appropriate
//! [`Happening`] variant and dispatches on the bus.
//!
//! Trait methods are synchronous — the chain's append
//! path is hot. The `HappeningBus::emit` method is also
//! sync (non-blocking channel sends) so no task spawn is
//! needed.

use std::sync::Arc;
use std::time::SystemTime;

use evo_witness::DomainWitness;

use crate::domain_witness::runtime::WitnessEventEmitter;
use crate::happenings::{Happening, HappeningBus};

/// Production [`WitnessEventEmitter`] impl backed by the
/// happening bus.
///
/// Optionally fans inbound (remote-originated) chain
/// witnesses out to the `GroupStore` subscription channel
/// when a handle is bound. Local mutations broadcast through
/// the `GroupStore` mutation methods directly; this fan-out
/// covers the missing case where another seat's gesture
/// applies through the inbound pump and would otherwise
/// reach the chain projection without ever firing
/// `GroupChange`.
pub struct HappeningBusWitnessEmitter {
    bus: Arc<HappeningBus>,
    group_store: arc_swap::ArcSwapOption<crate::groups::GroupStore>,
}

impl HappeningBusWitnessEmitter {
    /// Construct a bridge around the supplied happening
    /// bus. Use [`Self::with_group_store`] to bind the
    /// group-store fan-out target.
    pub fn new(bus: Arc<HappeningBus>) -> Self {
        Self {
            bus,
            group_store: arc_swap::ArcSwapOption::const_empty(),
        }
    }

    /// Bind a [`crate::groups::GroupStore`] so inbound
    /// chain witnesses fan out to its subscription channel.
    /// Safe to call from any thread; the underlying
    /// `ArcSwapOption` performs the store lock-free.
    pub fn with_group_store(
        self,
        store: Arc<crate::groups::GroupStore>,
    ) -> Self {
        self.group_store.store(Some(store));
        self
    }
}

impl WitnessEventEmitter for HappeningBusWitnessEmitter {
    fn chain_head_changed(&self, new_head_b64: &str, chain_length: usize) {
        self.bus.emit(Happening::ChainHeadChanged {
            new_head_b64: new_head_b64.to_string(),
            chain_length,
            at: SystemTime::now(),
        });
    }

    fn gesture_applied(&self, witness: &DomainWitness, was_local: bool) {
        self.bus.emit(Happening::GestureApplied {
            witness_id: witness.id.as_str().to_string(),
            op_kind: witness.op.kind().to_string(),
            originator_device_id: witness.originator_device_id.clone(),
            was_local,
            at: SystemTime::now(),
        });
        if let Some(store) = self.group_store.load_full() {
            store.notify_from_chain_witness(witness, was_local);
        }
    }

    fn gesture_duplicate(&self, _witness_id: &str) {
        // The duplicate event is an observability hook
        // only; it does not surface as a Happening to UI
        // (avoids noise on multi-carrier re-delivery). A
        // future debug-surface emission could be added if
        // needed; for now it intentionally drops.
    }
}
