// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Audit-grade ledger primitive.
//!
//! Wraps the [`PersistenceStore`]'s `ledger_entries` substrate with
//! typed concrete-ledger surfaces, signing via a configured
//! [`CryptographicServices`] (NoOp by default), and a two-step
//! withdrawal flow that preserves the original entry while
//! recording the withdrawal as its own appended entry.
//!
//! ## Concrete ledgers
//!
//! Four ledger ids ship currently:
//!
//! - [`LEDGER_CONSENT`] (`evo.consent`) — TOS / privacy / telemetry
//!   consent decisions. Payload: [`ConsentEntry`].
//! - [`LEDGER_TRUST`] (`evo.trust`) — publisher trust grants and
//!   revocations. Payload: [`TrustEntry`].
//! - [`LEDGER_ACTION`] (`evo.action`) — operator-approved
//!   mutations (install, update, factory reset, plugin lifecycle,
//!   policy change, ...). Payload: [`ActionEntry`].
//! - [`LEDGER_LIFECYCLE`] (`evo.lifecycle`) — framework-recorded
//!   per-call lifecycle events from the in-process plugin-facing
//!   primitives (streams open/close, notifications send/cancel,
//!   metadata query dispatch / provider failure / provider
//!   timeout). Payload: [`LifecycleEntry`]. Distinct from
//!   `evo.action` in audience and vocabulary: action entries are
//!   operator-driven administrative mutations (low-rate, witness-
//!   grade), lifecycle entries are plugin-driven primitive events
//!   (medium-rate, forensic-record). Sharing the substrate (signed,
//!   tamper-evident, withdrawal-flow-supported) gives both audiences
//!   the same accountability guarantees with vocabulary appropriate
//!   to each.
//!
//! Ad-hoc plugin-defined ledgers are not permitted: every concrete
//! ledger has a framework-defined schema and id, validated by
//! external auditors. The primitive refuses appends to ledger ids
//! not in the recognised set.
//!
//! ## Cryptographic signing
//!
//! Signing is OPTIONAL at the framework layer. The framework
//! default [`NoOpCryptographicServices`] writes `signature_bytes =
//! NULL` and `signature_algorithm = "none"`; vendor distributions
//! plug in their own [`CryptographicServices`] impl. The substrate
//! stores raw signature bytes faithfully so a NoOp install can
//! later be upgraded to vendor signing without rewriting
//! historical entries.
//!
//! The signing message is the canonical concatenation of the
//! immutable entry fields: `entry_id`, `ledger_id`,
//! `schema_version`, `payload_json`, `created_at_ms`,
//! `subject_plugin` (or empty when `None`). The mutable
//! `withdrawn_by_entry_id` field is deliberately excluded so the
//! original entry's signature remains valid forever; the
//! withdrawal entry is its own separately-signed row.
//!
//! ## Withdrawal
//!
//! Withdrawal is *not* a mutation of the original entry. It is:
//!
//! 1. Append a fresh entry to the same ledger whose payload is a
//!    [`WithdrawalEntry`] carrying the original entry id, the
//!    operator-supplied reason, and the withdrawal timestamp.
//! 2. Link the withdrawal entry to the original via the
//!    substrate's `link_ledger_withdrawal`.
//!
//! Step 1 is a normal append; the withdrawal entry is signed and
//! verifiable. Step 2 sets the original's `withdrawn_by_entry_id`
//! pointer (one-shot; idempotent for the same withdrawal id;
//! refused for a different id). The original entry is never
//! modified beyond this single pointer set.

use crate::persistence::{
    system_time_to_ms_now, LedgerEntryFilter, LedgerEntryRecord,
    PersistedLedgerEntry, PersistenceError, PersistenceStore,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

// =========================================================================
// Cryptographic services trait
// =========================================================================

/// Cryptographic services consumed by the ledger primitive (signing
/// and verification) and by future at-rest-encryption consumers
/// (the credential vault, PII fields). The framework default
/// [`NoOpCryptographicServices`] provides no signing and no
/// encryption; vendor distributions plug in richer implementations
/// at the wiring layer.
pub trait CryptographicServices: Send + Sync + std::fmt::Debug {
    /// Sign a message. Framework default returns `None`. Vendor
    /// implementations return the raw signature bytes; the
    /// algorithm marker is reported separately via
    /// [`Self::signature_algorithm`].
    fn sign(&self, message: &[u8]) -> Option<Vec<u8>>;

    /// Verify a signature against a message. The signature is
    /// `Some` for vendor-signed entries and `None` for entries
    /// written under the framework default. Framework default
    /// returns `true` only when the signature is `None` (matches
    /// its no-signing model; never makes a positive cryptographic
    /// assertion).
    fn verify(&self, message: &[u8], signature: Option<&[u8]>) -> bool;

    /// Identifier for the signing algorithm. Framework default
    /// returns `"none"`. Vendor implementations report a stable
    /// string consumed by external auditors.
    fn signature_algorithm(&self) -> &'static str;

    /// Encrypt a plaintext for at-rest storage with the given
    /// consumer-domain context. Framework default returns the
    /// plaintext unchanged. Reserved for the credential vault and
    /// future PII-encryption consumers; the ledger primitive does
    /// not use this method directly today.
    fn encrypt_at_rest(
        &self,
        plaintext: &[u8],
        consumer_domain: &str,
    ) -> Vec<u8>;

    /// Decrypt at-rest ciphertext under the same consumer-domain
    /// context. Framework default returns the ciphertext unchanged.
    fn decrypt_at_rest(
        &self,
        ciphertext: &[u8],
        consumer_domain: &str,
    ) -> Vec<u8>;
}

/// Framework default cryptographic services: no signing, no
/// encryption. Suitable for community / hobbyist distributions
/// that do not need cryptographic audit-grade properties from the
/// framework itself.
///
/// `sign` returns `None`; `verify` returns `true` only when the
/// supplied signature is `None`; `signature_algorithm` returns
/// `"none"`; `encrypt_at_rest` and `decrypt_at_rest` return their
/// inputs unchanged.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoOpCryptographicServices;

impl CryptographicServices for NoOpCryptographicServices {
    fn sign(&self, _message: &[u8]) -> Option<Vec<u8>> {
        None
    }

    fn verify(&self, _message: &[u8], signature: Option<&[u8]>) -> bool {
        signature.is_none()
    }

    fn signature_algorithm(&self) -> &'static str {
        "none"
    }

    fn encrypt_at_rest(
        &self,
        plaintext: &[u8],
        _consumer_domain: &str,
    ) -> Vec<u8> {
        plaintext.to_vec()
    }

    fn decrypt_at_rest(
        &self,
        ciphertext: &[u8],
        _consumer_domain: &str,
    ) -> Vec<u8> {
        ciphertext.to_vec()
    }
}

// =========================================================================
// Concrete-ledger payload schemas
// =========================================================================

/// `evo.consent` ledger entry payload (schema v1). Records a TOS,
/// privacy, telemetry, or accessibility-related consent decision.
///
/// The `consent_id` is versioned: a bump (e.g. `tos.v1` →
/// `tos.v2`) means a fresh entry with a new `consent_id`; existing
/// entries are not migrated. `document_hash` is the SHA-256 hex
/// digest of the consent document the user saw at decision time —
/// lets a future audit verify the user accepted the exact version
/// shown on screen.
///
/// Withdrawal is recorded as a separate entry via
/// [`LedgerPrimitive::withdraw`], not by mutating an existing
/// entry's `decision`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsentEntry {
    /// Versioned consent-document identifier (e.g. `tos.v1`,
    /// `privacy.v2`, `telemetry.v1`).
    pub consent_id: String,
    /// SHA-256 hex digest of the consent document the user saw
    /// when making the decision.
    pub document_hash: String,
    /// User decision.
    pub decision: ConsentDecision,
    /// Wall-clock millisecond timestamp of the user's decision.
    /// May coincide with the substrate-level `created_at_ms` (for
    /// synchronous append) or precede it (for batched / queued
    /// writes); both are recorded so audits distinguish event time
    /// from storage time.
    pub decided_at_ms: u64,
    /// Operator-supplied user identifier when known. `None` for
    /// device-level decisions not bound to a specific user.
    pub user_id: Option<String>,
}

/// User decision recorded in [`ConsentEntry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsentDecision {
    /// User accepted the consent document.
    Accepted,
    /// User declined the consent document.
    Declined,
    /// Recorded only on a withdrawal entry's payload (the original
    /// entry's decision is unchanged; withdrawal is a new entry
    /// pointing at the original).
    Withdrawn,
}

/// `evo.trust` ledger entry payload (schema v1). Records a
/// publisher trust grant or revocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustEntry {
    /// Publisher identifier (e.g. `com.example.publisher`).
    pub publisher_id: String,
    /// Operator-visible display name of the publisher.
    pub display_name: String,
    /// Trust level granted or revoked.
    pub trust_level: TrustLevel,
    /// Wall-clock millisecond timestamp of the grant / revocation.
    pub granted_at_ms: u64,
    /// How the trust grant was sourced.
    pub granted_via: GrantedVia,
    /// Scope of the grant (all plugins from this publisher, or one
    /// specific plugin).
    pub scope: TrustScope,
}

/// Trust level recorded in [`TrustEntry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    /// Pre-trusted publisher (first-party / vendor-bundled).
    Pretrusted,
    /// Operator explicitly granted trust (operator-prompt path).
    OperatorTrusted,
    /// Trust not yet granted (pending operator decision).
    NotYetTrusted,
    /// Trust previously granted, now revoked.
    Revoked,
}

/// How a trust grant was sourced (recorded in [`TrustEntry`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantedVia {
    /// Bundled with the vendor distribution.
    VendorBundle,
    /// Subscribed to a registry that ships the publisher's keys.
    RegistrySubscription,
    /// Operator manually approved a prompt at install time.
    OperatorPrompt,
    /// Operator explicitly installed a single bundle whose
    /// signature was verified out-of-band.
    DirectInstall,
}

/// Scope of a [`TrustEntry`] grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrustScope {
    /// All plugins published under this publisher.
    AllPlugins,
    /// One specific plugin published under this publisher.
    PerPlugin {
        /// Canonical plugin id the grant applies to.
        plugin_id: String,
    },
}

/// `evo.action` ledger entry payload (schema v1). Records an
/// operator-approved system mutation (install, update, factory
/// reset, plugin lifecycle, policy change, ...).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionEntry {
    /// Type of action.
    pub action_type: ActionType,
    /// Target of the action (a plugin or a system scope).
    pub target: ActionTarget,
    /// Pre-action state, free-form per action type. Captures
    /// rollback context for forensics.
    pub before_state: serde_json::Value,
    /// Post-action state, free-form per action type. Empty object
    /// when the action did not change state (e.g., `BulkOperation`
    /// dry-run).
    pub after_state: serde_json::Value,
    /// Wall-clock millisecond timestamp of action application.
    pub applied_at_ms: u64,
    /// Identifier of the operator client that approved the action
    /// (UI session id, system-actor id for plan-engine-dispatched
    /// actions, etc.). Free-form; the wiring layer provides the
    /// value at append time.
    pub approved_by: String,
    /// Outcome of the action.
    pub outcome: ActionOutcome,
}

/// Type of action recorded in [`ActionEntry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionType {
    /// Plugin install.
    Install,
    /// Plugin update.
    Update,
    /// Plugin uninstall.
    Uninstall,
    /// Plugin enable.
    Enable,
    /// Plugin disable.
    Disable,
    /// Device-level factory reset.
    FactoryReset,
    /// Plugin purge (full removal of plugin state).
    PluginPurge,
    /// Bulk operation across multiple targets.
    BulkOperation,
    /// Operator switched profile.
    ProfileSwitch,
    /// Operator changed a policy setting.
    PolicyChange,
}

/// Target of an [`ActionEntry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActionTarget {
    /// A specific plugin.
    Plugin {
        /// Canonical plugin id.
        plugin_id: String,
    },
    /// A system scope (e.g., `device`, `network`, `audio`).
    Scope {
        /// Free-form scope name.
        name: String,
    },
}

/// Outcome of an [`ActionEntry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActionOutcome {
    /// The action applied successfully.
    Success,
    /// The action failed; carries the failure reason.
    Failed {
        /// Human-readable failure reason.
        reason: String,
    },
    /// The action was rolled back after partial application.
    RolledBack,
}

/// Audit-grade record of one plugin-initiated primitive lifecycle
/// event (a stream open / close, a notification send / cancel, a
/// metadata query dispatch / provider failure / provider timeout).
/// Distinct from [`ActionEntry`] in audience and vocabulary: action
/// entries are operator-driven administrative mutations (low-rate,
/// witness-grade), lifecycle entries are plugin-driven primitive
/// events (medium-rate, forensic-record). Both share the
/// audit-grade substrate (signed, tamper-evident, withdrawal-flow-
/// supported); the separation keeps the operator-action audit log
/// small and forensically focused while giving per-call lifecycle
/// its own queryable track with vocabulary appropriate to its
/// audience.
///
/// ## Forensic reconstruction
///
/// Each entry carries enough provenance to reconstruct the event
/// without consulting plugin-side logs:
///
/// - `source_plugin` is the plugin's canonical name; the framework
///   wrapper enforces this matches the plugin's identity (a plugin
///   cannot spoof another plugin's identity in the audit record).
/// - `target` identifies what the event was about (a stream id, a
///   notification handle, a metadata provider id, ...).
/// - `payload` carries event-type-specific fields the forensic
///   analyst needs: stream schema and codecs at open, resolved
///   notification mode at send, per-provider statuses at metadata
///   dispatch, deadline at provider timeout.
/// - `recorded_at_ms` and `outcome` complete the record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LifecycleEntry {
    /// Type of lifecycle event.
    pub event_type: LifecycleEventType,
    /// Plugin that initiated the event. The framework wrapper
    /// overwrites this with the plugin's canonical name before
    /// the entry reaches the ledger; a plugin cannot spoof
    /// another plugin's identity.
    pub source_plugin: String,
    /// What the event targeted.
    pub target: LifecycleTarget,
    /// Wall-clock millisecond timestamp of the event.
    pub recorded_at_ms: u64,
    /// Outcome of the event. Lifecycle events that surface
    /// `Failed` carry the framework's reason for forensic
    /// reconstruction.
    pub outcome: LifecycleOutcome,
    /// Per-event-type forensic payload. Stable shape per
    /// [`LifecycleEventType`]; the wrapper documents the payload
    /// contract on each event_type.
    pub payload: serde_json::Value,
}

/// Closed-set vocabulary of framework-recorded lifecycle events.
/// Adding a new event_type is a deliberate framework change with
/// admission-side compatibility guarantees; plugin-defined
/// vocabulary lives on the happenings bus's `PluginEvent` variant
/// instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleEventType {
    /// A plugin opened a new stream (or a coalesce-into-existing
    /// open returned the existing stream's id). Payload carries
    /// the stream schema, codecs, and max_rate_hz.
    StreamOpened,
    /// A plugin closed a previously-opened stream. Payload carries
    /// the stream id; the framework infers the duration from the
    /// pair of recorded_at_ms timestamps.
    StreamClosed,
    /// A plugin sent a notification. Payload carries the
    /// notification's level, priority, resolved active mode, and
    /// any group_with id.
    NotificationSent,
    /// A plugin cancelled a previously-sent notification. Payload
    /// carries the cancelled handle.
    NotificationCancelled,
    /// A plugin dispatched a metadata query through the chain.
    /// Payload carries the per-provider status summary (which
    /// providers contributed, which timed out, which failed,
    /// cache freshness).
    MetadataQueryDispatched,
    /// A metadata provider exceeded its deadline during query
    /// dispatch. Payload carries the provider id and the
    /// deadline_ms applied. Emitted alongside a corresponding
    /// `MetadataQueryDispatched` entry, not in place of it.
    MetadataProviderTimedOut,
    /// A metadata provider returned an error during query
    /// dispatch. Payload carries the provider id and the error
    /// reason. Emitted alongside the dispatch entry.
    MetadataProviderFailed,
    /// A framework-internal entity (the listening-plans engine,
    /// operator-issued UI, alarms, multi-room control, prompts)
    /// dispatched a source verb against a source plugin through
    /// the framework's verb-dispatch primitive. Distinct from
    /// the existing variants by audience: those record
    /// plugin-driven events, this records framework-driven
    /// dispatches against plugins. The
    /// [`LifecycleEntry::source_plugin`] field carries the
    /// approver string (e.g. `plan:morning-routine`,
    /// `user:1000`, `system:alarm-fire`) so per-approver
    /// audit queries hit the substrate's index without
    /// payload-side filtering. The target is a
    /// [`LifecycleTarget::SourcePlugin`] naming the resolved
    /// handler shelf; the payload is a [`VerbDispatchedPayload`]
    /// carrying the verb stable string, the primary URI (when
    /// applicable), the URI count (1 for single-item verbs, N
    /// for collection verbs), and whether the dispatch
    /// acquired active-source custody.
    VerbDispatched,
    /// An operator granted trust to a publisher whose signing
    /// key is not in the vendor distribution's trust roots.
    /// Payload carries the publisher fingerprint, display
    /// name, granted_via, and scope kind. Recorded so the
    /// audit trail surfaces every operator-issued trust
    /// elevation against a non-vendor publisher.
    PublisherTrustGranted,
    /// An operator revoked a previously-granted publisher
    /// trust. Payload carries the publisher fingerprint and
    /// display name.
    PublisherTrustRevoked,
    /// An operator attempted privileged step-up authentication.
    /// Recorded for both successes and failures so the audit
    /// trail surfaces every credential-verification call. Payload
    /// is a [`StepUpAuthAttemptedPayload`] carrying the
    /// presented username, the verifying peer's UID/GID, the
    /// classifying outcome, and the implementation name reported
    /// by the [`crate::auth::AuthService`] that handled the
    /// attempt. The wrapping [`LifecycleEntry::outcome`] mirrors
    /// the success / failure split so per-outcome audit queries
    /// hit the substrate's index without payload-side filtering.
    StepUpAuthAttempted,
    /// An operator-initiated privileged operation executed
    /// against the steward — plugin lifecycle, channel mutation,
    /// log-level change, diagnostics export, update apply, SSH
    /// state, or any other entry in the operations control plane.
    /// Recorded after every successful execution; the matching
    /// [`OperationDenied`] variant covers refused calls. Payload
    /// is an [`OperationExecutedPayload`] with the operation's
    /// canonical name, the capability key the gate consulted, the
    /// step-up principal (when one was presented), and the
    /// verifying peer's UID/GID.
    OperationExecuted,
    /// An operator-initiated privileged operation was refused
    /// before execution: capability missing, step-up token
    /// absent, step-up token invalid for the presenting peer, or
    /// step-up token expired. Recorded so denied attempts appear
    /// in the same audit stream as executed ones; payload is an
    /// [`OperationDeniedPayload`] with the operation's canonical
    /// name, the classifying reason, and the peer's UID/GID.
    OperationDenied,
}

/// Target of a [`LifecycleEntry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LifecycleTarget {
    /// Targets a stream by its operator-visible id.
    Stream {
        /// Operator-visible stream id.
        stream_id: String,
    },
    /// Targets a notification by its handle.
    Notification {
        /// String form of the notification handle.
        handle: String,
    },
    /// Targets a metadata query by a digest of its canonical
    /// form (the same key used by the chain's result-cache).
    MetadataQuery {
        /// Cache-key digest of the canonical query form.
        query_digest: String,
    },
    /// Targets a metadata provider directly.
    MetadataProvider {
        /// Provider id (canonical plugin identity domain).
        provider_id: String,
    },
    /// Targets a source plugin through a verb dispatch. The
    /// `plugin_id` is the resolved handler shelf the dispatcher
    /// routed the verb to. Pre-resolution failures (URI scheme
    /// not registered) do not produce a lifecycle entry;
    /// post-resolution failures (router refused, plugin
    /// returned an error) produce an entry with
    /// [`LifecycleOutcome::Failed`].
    SourcePlugin {
        /// Resolved handler shelf the dispatcher routed against.
        plugin_id: String,
    },
    /// Targets a publisher trust record by the publisher's
    /// signing-key fingerprint.
    Publisher {
        /// Publisher fingerprint (lowercase hex).
        publisher_id: String,
    },
    /// Targets a privileged step-up auth session by the
    /// presented username. The framework does not record
    /// principal UIDs in `target` form (UIDs live on the
    /// payload) so anonymised audit consumers can group by
    /// username without dereferencing the OS-level identity.
    AuthSession {
        /// Username (or equivalent identifier) the verification
        /// call presented. Recorded verbatim from the caller; the
        /// framework does not normalise or validate.
        username: String,
    },
    /// Targets a privileged operation by its canonical name.
    /// The canonical name is the snake_case identifier the
    /// operations control plane uses (e.g. `plugin_enable`,
    /// `set_log_level`, `set_update_channel`); per-operation
    /// detail (target plugin, channel selection, etc.) lives on
    /// the payload, not this target.
    Operation {
        /// Canonical operation name.
        operation: String,
    },
}

/// Outcome of a [`LifecycleEntry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LifecycleOutcome {
    /// The event applied successfully.
    Success,
    /// The event failed; carries the failure reason from the
    /// framework's perspective.
    Failed {
        /// Human-readable failure reason.
        reason: String,
    },
}

/// Payload of a [`LifecycleEventType::VerbDispatched`] entry.
/// Recorded by the source-verb dispatcher every time it routes a
/// verb to a source plugin (after URI-scheme resolution
/// succeeds; pre-resolution failures do not produce an entry).
/// The shape is stable on disk; new fields land as additional
/// optional members with serde defaults to keep older entries
/// readable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerbDispatchedPayload {
    /// Stable wire string for the dispatched verb (`play_now`,
    /// `play_now_collection`, ...). Matches
    /// [`evo_plugin_sdk::contract::SourceVerb::as_str`] for the
    /// dispatched variant.
    pub verb: String,
    /// Primary URI the verb targeted, when applicable. For
    /// `PlayNow` this is the item URI; for `PlayNowCollection`
    /// this is the first URI of the collection (every URI in a
    /// collection shares the same scheme so a single handler
    /// plugin owns the playback). Verbs without a URI target
    /// (Pause, Resume, Stop, ...) leave this `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_uri: Option<String>,
    /// Number of URIs the verb dispatched against. 1 for
    /// `PlayNow` and similar single-item verbs; N for
    /// `PlayNowCollection` and similar collection verbs; 0 for
    /// verbs without URI targets.
    pub uri_count: u32,
    /// Whether the dispatch implicitly acquired
    /// `audio.active_source` custody (the play_now-class set:
    /// PlayNow, PlayNowCollection). The dispatcher records the
    /// claim BEFORE dispatching so the custody record reflects
    /// the in-flight dispatch even if the plugin's verb handling
    /// races; this field captures that arbitration outcome.
    pub acquired_custody: bool,
}

/// Payload of a [`LifecycleEventType::StepUpAuthAttempted`]
/// entry. Recorded after every privileged-step-up verification call
/// — the wrapping outcome carries success or failure; this payload
/// carries the call's identifying metadata (presented username,
/// peer UID/GID, classifying outcome, implementation name).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepUpAuthAttemptedPayload {
    /// Username (or equivalent identifier) presented in the
    /// verification call. Recorded verbatim so audit consumers can
    /// trace every attempted credential against a username, even
    /// when the credential was wrong.
    pub username: String,
    /// OS UID of the verifying peer (the connection that called
    /// `step_up_auth_verify`). Recorded so audit consumers can
    /// distinguish multiple operators on the same device.
    pub peer_uid: u32,
    /// OS GID of the verifying peer.
    pub peer_gid: u32,
    /// Classifying outcome. For success, the variant is
    /// `Authenticated`; for failure, it carries the framework's
    /// error taxonomy (`InvalidCredentials`, `UserNotPermitted`,
    /// `BackendUnavailable`).
    pub outcome: StepUpAuthOutcome,
    /// `AuthService::implementation_name` of the implementation
    /// that handled the call. Recorded so external auditors can
    /// correlate verification outcomes with the backend that
    /// produced them.
    pub implementation: String,
}

/// Classifying outcome of a step-up verification call. Distinct
/// from [`crate::auth::AuthVerificationError`] — the in-process
/// error type — so audit consumers see a stable on-disk shape that
/// will not drift with implementation changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepUpAuthOutcome {
    /// Verification succeeded; a privileged session token was
    /// issued. Recorded with the wrapping
    /// [`LifecycleOutcome::Success`].
    Authenticated,
    /// Verification failed: username/secret combination did not
    /// authenticate.
    InvalidCredentials,
    /// Verification failed: principal authenticated but is not
    /// permitted to perform privileged operations against this
    /// steward.
    UserNotPermitted,
    /// Verification failed: backend was fundamentally unavailable
    /// (no AuthService configured; PAM stack down; IdP transport
    /// failure). Reason is recorded for audit; not exposed to the
    /// caller.
    BackendUnavailable {
        /// Implementation-supplied reason. Recorded in audit only.
        reason: String,
    },
    /// Verification failed: backend was in place but could not
    /// read the stored credential (shadow file unreadable;
    /// runtime user missing from shadow; equivalent).
    /// Distribution-setup defect, distinct from
    /// [`Self::BackendUnavailable`].
    BackendReadFailed {
        /// Implementation-supplied reason. Recorded in audit only.
        reason: String,
    },
    /// Verification failed: backend read the stored credential
    /// but the verification primitive rejected it (unsupported
    /// hash format; malformed hash; libcrypt internal error).
    /// Distinct from [`Self::InvalidCredentials`] (which means
    /// "hash understood; wrong password") and from
    /// [`Self::BackendReadFailed`].
    BackendVerifyError {
        /// Implementation-supplied reason. Recorded in audit only.
        reason: String,
    },
}

/// Payload of a [`LifecycleEventType::OperationExecuted`] entry.
/// Recorded after a privileged operation runs to completion; the
/// wrapping outcome reflects the operation's return — `Success`
/// when the operation reported success, `Failed { reason }` when
/// it returned an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationExecutedPayload {
    /// Canonical name of the operation (`plugin_enable`,
    /// `set_log_level`, `set_update_channel`, ...). Stable wire
    /// string consumers compare against without parsing further
    /// detail.
    pub operation: String,
    /// Capability key the gate consulted before dispatching
    /// (`plugins_admin`, `system_admin`, ...). Recorded so audit
    /// consumers see the policy axis the framework enforced.
    pub capability_key: String,
    /// Step-up principal's username, when a step-up token was
    /// presented. `None` for operations that do not require
    /// step-up; present for everything else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_up_username: Option<String>,
    /// OS UID of the peer that issued the operation.
    pub peer_uid: u32,
    /// OS GID of the peer that issued the operation.
    pub peer_gid: u32,
}

/// Payload of a [`LifecycleEventType::OperationDenied`] entry.
/// Recorded when a privileged operation is refused before
/// execution. The wrapping outcome is always
/// [`LifecycleOutcome::Failed`]; the payload's `reason_class` is
/// the classifying axis (capability missing, step-up required,
/// step-up invalid, step-up expired) and `detail` is a short
/// implementation-side string for forensics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationDeniedPayload {
    /// Canonical name of the operation that was denied.
    pub operation: String,
    /// Classifying axis of the denial. Stable wire string so
    /// consumers can group denials by class without parsing
    /// further detail.
    pub reason_class: OperationDenialReason,
    /// OS UID of the peer that issued the operation.
    pub peer_uid: u32,
    /// OS GID of the peer that issued the operation.
    pub peer_gid: u32,
}

/// Classifying axis of an [`OperationDeniedPayload`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationDenialReason {
    /// The connection did not carry the required capability key
    /// (the operator config did not grant it on this connection).
    CapabilityMissing,
    /// The operation requires step-up but no step-up token was
    /// presented on the request.
    StepUpRequired,
    /// A step-up token was presented but did not validate against
    /// the active session store (unknown token, or bound to a
    /// different peer UID).
    StepUpInvalid,
    /// A step-up token was presented and matched, but had expired.
    StepUpExpired,
}

/// Payload appended when an entry is withdrawn via
/// [`LedgerPrimitive::withdraw`]. The withdrawal payload's
/// `original_entry_id` lets a verifier walk back to the original
/// entry; the original's `withdrawn_by_entry_id` field walks
/// forward to the withdrawal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WithdrawalEntry {
    /// Identifier of the entry being withdrawn.
    pub original_entry_id: String,
    /// Operator-supplied reason for withdrawal.
    pub reason: String,
    /// Wall-clock millisecond timestamp of the withdrawal.
    pub withdrawn_at_ms: u64,
}

// =========================================================================
// LedgerPrimitive
// =========================================================================

/// Concrete-ledger discriminator: TOS / privacy / telemetry consent
/// decisions.
pub const LEDGER_CONSENT: &str = "evo.consent";

/// Concrete-ledger discriminator: publisher trust grants and
/// revocations.
pub const LEDGER_TRUST: &str = "evo.trust";

/// Concrete-ledger discriminator: operator-approved system
/// mutations.
pub const LEDGER_ACTION: &str = "evo.action";

/// Concrete-ledger discriminator: framework-recorded per-call
/// lifecycle events from the in-process plugin-facing primitives
/// (streams open/close, notifications send/cancel, metadata query
/// dispatch / provider failure / provider timeout).
pub const LEDGER_LIFECYCLE: &str = "evo.lifecycle";

/// Per-payload schema version constants. Bumping the per-payload
/// shape means new appends carry a new version; existing rows are
/// not migrated.
pub const CONSENT_SCHEMA_V1: u32 = 1;
/// See [`CONSENT_SCHEMA_V1`].
pub const TRUST_SCHEMA_V1: u32 = 1;
/// See [`CONSENT_SCHEMA_V1`].
pub const ACTION_SCHEMA_V1: u32 = 1;
/// See [`CONSENT_SCHEMA_V1`].
pub const LIFECYCLE_SCHEMA_V1: u32 = 1;
/// Schema version for the [`WithdrawalEntry`] payload appended
/// by [`LedgerPrimitive::withdraw`].
pub const WITHDRAWAL_SCHEMA_V1: u32 = 1;

/// Errors raised by [`LedgerPrimitive`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LedgerError {
    /// Caller supplied a ledger id not in the framework-defined
    /// set. The recognised set is fixed at compile time;
    /// plugin-defined ledger ids are not permitted.
    #[error(
        "unknown ledger id {0:?}; recognised: evo.consent, evo.trust, \
         evo.action, evo.lifecycle"
    )]
    UnknownLedgerId(String),
    /// Payload serialisation failed. In practice this fires only
    /// for payloads carrying non-serialisable values (e.g.,
    /// `serde_json::Value` carrying a NaN); the typed payload
    /// structs in this module never produce serialisation errors.
    #[error("payload serialisation failed: {0}")]
    Serialise(#[source] serde_json::Error),
    /// The persistence layer returned an error. Carries the
    /// underlying [`PersistenceError`] so callers can match on
    /// substrate-level failure modes (e.g., `AlreadyWithdrawn`).
    #[error("persistence: {0}")]
    Persistence(#[from] PersistenceError),
}

/// Audit-grade ledger primitive. Wraps the
/// [`PersistenceStore`]'s `ledger_entries` substrate with typed
/// concrete-ledger surfaces, signing via the configured
/// [`CryptographicServices`] (NoOp by default), and the two-step
/// withdrawal flow.
///
/// ## Causal-ordering guarantee
///
/// Every successful append against a given ledger id mints a
/// `created_at_ms` strictly greater than every previous append's
/// against the same ledger id. The substrate's per-ms timestamps
/// have millisecond resolution, so two appends within the same
/// millisecond would tie under raw-clock semantics — a forensic
/// analyst reconstructing event order across burst events would
/// see ambiguity, and a witness-grade signature covers the
/// timestamp so naive sub-ms ordering is not a substrate-side
/// patch.
///
/// The primitive maintains a per-ledger-id monotonic floor in
/// memory, lazily seeded from the substrate's max-per-ledger on
/// first append after construction (so reboots inherit the
/// already-persisted causal frontier). On each append, the
/// effective `created_at_ms` is `max(system_time_to_ms_now(),
/// floor[ledger_id] + 1)`. The floor is updated to the chosen
/// value before signing, so signature + persistence agree on the
/// timestamp covered.
pub struct LedgerPrimitive {
    persistence: Arc<dyn PersistenceStore>,
    crypto: Arc<dyn CryptographicServices>,
    /// Per-ledger-id monotonic floor for `created_at_ms`. Lazily
    /// initialised from the substrate's max-per-ledger on first
    /// append; bumped to the chosen value on every successful
    /// timestamp pick.
    monotonic_floor: Arc<AsyncMutex<HashMap<String, u64>>>,
}

impl std::fmt::Debug for LedgerPrimitive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LedgerPrimitive")
            .field("signature_algorithm", &self.crypto.signature_algorithm())
            .finish()
    }
}

impl LedgerPrimitive {
    /// Construct a ledger primitive backed by the given persistence
    /// store and cryptographic services.
    pub fn new(
        persistence: Arc<dyn PersistenceStore>,
        crypto: Arc<dyn CryptographicServices>,
    ) -> Self {
        Self {
            persistence,
            crypto,
            monotonic_floor: Arc::new(AsyncMutex::new(HashMap::new())),
        }
    }

    /// Construct a ledger primitive with the framework default
    /// [`NoOpCryptographicServices`].
    pub fn with_no_op_crypto(persistence: Arc<dyn PersistenceStore>) -> Self {
        Self::new(persistence, Arc::new(NoOpCryptographicServices))
    }

    /// Mint the next `created_at_ms` for a ledger id under the
    /// per-ledger monotonic-floor invariant. The returned value
    /// is `max(system_time_to_ms_now(), floor + 1)` and the
    /// floor is updated to it before this call returns. Lazy-
    /// initialises the floor from the substrate's max-per-ledger
    /// on the first call per ledger id so reboots inherit the
    /// already-persisted causal frontier.
    async fn next_monotonic_ms(
        &self,
        ledger_id: &str,
    ) -> Result<u64, LedgerError> {
        let mut floor = self.monotonic_floor.lock().await;
        let last = match floor.get(ledger_id) {
            Some(&v) => v,
            None => {
                // First append against this ledger id since
                // construction. Seed from the substrate so reboot
                // continuity holds. Drop the lock across the
                // substrate query is not necessary — query is
                // fast and the lock is per-primitive.
                let from_db = self
                    .persistence
                    .query_max_created_at_ms_for_ledger(ledger_id)
                    .await?;
                floor.insert(ledger_id.to_string(), from_db);
                from_db
            }
        };
        let now_raw = system_time_to_ms_now();
        let chosen = now_raw.max(last + 1);
        floor.insert(ledger_id.to_string(), chosen);
        Ok(chosen)
    }

    /// Append a consent decision to `evo.consent`. Returns the
    /// minted entry id.
    pub async fn append_consent(
        &self,
        entry: &ConsentEntry,
        subject_plugin: Option<&str>,
    ) -> Result<String, LedgerError> {
        self.append(LEDGER_CONSENT, CONSENT_SCHEMA_V1, entry, subject_plugin)
            .await
    }

    /// Append a trust grant or revocation to `evo.trust`. Returns
    /// the minted entry id.
    pub async fn append_trust(
        &self,
        entry: &TrustEntry,
        subject_plugin: Option<&str>,
    ) -> Result<String, LedgerError> {
        self.append(LEDGER_TRUST, TRUST_SCHEMA_V1, entry, subject_plugin)
            .await
    }

    /// Append an operator action to `evo.action`. Returns the
    /// minted entry id.
    pub async fn append_action(
        &self,
        entry: &ActionEntry,
        subject_plugin: Option<&str>,
    ) -> Result<String, LedgerError> {
        self.append(LEDGER_ACTION, ACTION_SCHEMA_V1, entry, subject_plugin)
            .await
    }

    /// Append a primitive lifecycle event to `evo.lifecycle`.
    /// Returns the minted entry id. The entry's
    /// [`LifecycleEntry::source_plugin`] doubles as the
    /// `subject_plugin` substrate column so per-plugin queries
    /// (forensic reconstruction, anomaly detection) hit the
    /// substrate's index without payload-side filtering.
    pub async fn append_lifecycle(
        &self,
        entry: &LifecycleEntry,
    ) -> Result<String, LedgerError> {
        let plugin = entry.source_plugin.clone();
        self.append(
            LEDGER_LIFECYCLE,
            LIFECYCLE_SCHEMA_V1,
            entry,
            Some(plugin.as_str()),
        )
        .await
    }

    /// Append a verb-dispatch lifecycle event. Convenience over
    /// [`Self::append_lifecycle`] that constructs the entry with
    /// the right shape: event_type =
    /// [`LifecycleEventType::VerbDispatched`], target =
    /// [`LifecycleTarget::SourcePlugin`], payload =
    /// [`VerbDispatchedPayload`]. The `approver` string
    /// (e.g. `plan:morning-routine`, `user:1000`,
    /// `system:alarm-fire`) lands on the entry's
    /// `source_plugin` so per-approver audit queries hit the
    /// substrate's index. Returns the minted entry id.
    pub async fn append_verb_dispatch(
        &self,
        approver: &str,
        handler_shelf: &str,
        payload: &VerbDispatchedPayload,
        outcome: LifecycleOutcome,
    ) -> Result<String, LedgerError> {
        let entry = LifecycleEntry {
            event_type: LifecycleEventType::VerbDispatched,
            source_plugin: approver.to_string(),
            target: LifecycleTarget::SourcePlugin {
                plugin_id: handler_shelf.to_string(),
            },
            recorded_at_ms: system_time_to_ms_now(),
            outcome,
            payload: serde_json::to_value(payload)
                .map_err(LedgerError::Serialise)?,
        };
        self.append_lifecycle(&entry).await
    }

    /// Append a publisher-trust grant entry. The `approver`
    /// is the operator identity (peer UID) issuing the grant;
    /// the `publisher_id` is the publisher's signing-key
    /// fingerprint; the `payload` carries display name,
    /// granted_via, and scope kind.
    pub async fn append_publisher_trust_grant(
        &self,
        approver: &str,
        publisher_id: &str,
        payload: serde_json::Value,
    ) -> Result<String, LedgerError> {
        let entry = LifecycleEntry {
            event_type: LifecycleEventType::PublisherTrustGranted,
            source_plugin: approver.to_string(),
            target: LifecycleTarget::Publisher {
                publisher_id: publisher_id.to_string(),
            },
            recorded_at_ms: system_time_to_ms_now(),
            outcome: LifecycleOutcome::Success,
            payload,
        };
        self.append_lifecycle(&entry).await
    }

    /// Append a publisher-trust revocation entry.
    pub async fn append_publisher_trust_revoke(
        &self,
        approver: &str,
        publisher_id: &str,
        payload: serde_json::Value,
    ) -> Result<String, LedgerError> {
        let entry = LifecycleEntry {
            event_type: LifecycleEventType::PublisherTrustRevoked,
            source_plugin: approver.to_string(),
            target: LifecycleTarget::Publisher {
                publisher_id: publisher_id.to_string(),
            },
            recorded_at_ms: system_time_to_ms_now(),
            outcome: LifecycleOutcome::Success,
            payload,
        };
        self.append_lifecycle(&entry).await
    }

    /// Append a step-up authentication attempt entry. Recorded
    /// for both successes and failures so the audit trail
    /// surfaces every credential-verification call. The
    /// `approver` is `peer:<uid>` of the verifying connection
    /// (matches the convention used elsewhere in this primitive
    /// for operator-issued events). Returns the minted entry id.
    pub async fn append_step_up_attempt(
        &self,
        approver: &str,
        payload: &StepUpAuthAttemptedPayload,
        outcome: LifecycleOutcome,
    ) -> Result<String, LedgerError> {
        let entry = LifecycleEntry {
            event_type: LifecycleEventType::StepUpAuthAttempted,
            source_plugin: approver.to_string(),
            target: LifecycleTarget::AuthSession {
                username: payload.username.clone(),
            },
            recorded_at_ms: system_time_to_ms_now(),
            outcome,
            payload: serde_json::to_value(payload)
                .map_err(LedgerError::Serialise)?,
        };
        self.append_lifecycle(&entry).await
    }

    /// Append a privileged-operation execution entry. Recorded
    /// after the operation runs to completion; the `outcome`
    /// reflects whether the operation reported success or
    /// failure to the framework.
    pub async fn append_operation_executed(
        &self,
        approver: &str,
        payload: &OperationExecutedPayload,
        outcome: LifecycleOutcome,
    ) -> Result<String, LedgerError> {
        let entry = LifecycleEntry {
            event_type: LifecycleEventType::OperationExecuted,
            source_plugin: approver.to_string(),
            target: LifecycleTarget::Operation {
                operation: payload.operation.clone(),
            },
            recorded_at_ms: system_time_to_ms_now(),
            outcome,
            payload: serde_json::to_value(payload)
                .map_err(LedgerError::Serialise)?,
        };
        self.append_lifecycle(&entry).await
    }

    /// Append a privileged-operation denial entry. Recorded
    /// when an operation is refused before execution. The
    /// wrapping outcome is always `Failed` with a short reason
    /// derived from the `reason_class`.
    pub async fn append_operation_denied(
        &self,
        approver: &str,
        payload: &OperationDeniedPayload,
    ) -> Result<String, LedgerError> {
        let reason = match payload.reason_class {
            OperationDenialReason::CapabilityMissing => "capability_missing",
            OperationDenialReason::StepUpRequired => "step_up_required",
            OperationDenialReason::StepUpInvalid => "step_up_invalid",
            OperationDenialReason::StepUpExpired => "step_up_expired",
        };
        let entry = LifecycleEntry {
            event_type: LifecycleEventType::OperationDenied,
            source_plugin: approver.to_string(),
            target: LifecycleTarget::Operation {
                operation: payload.operation.clone(),
            },
            recorded_at_ms: system_time_to_ms_now(),
            outcome: LifecycleOutcome::Failed {
                reason: reason.to_string(),
            },
            payload: serde_json::to_value(payload)
                .map_err(LedgerError::Serialise)?,
        };
        self.append_lifecycle(&entry).await
    }

    /// Withdraw an entry. Appends a [`WithdrawalEntry`] to the same
    /// ledger as the original, then links it via the substrate's
    /// one-shot `link_ledger_withdrawal`.
    ///
    /// On a substrate
    /// [`PersistenceError::AlreadyWithdrawn`](crate::persistence::PersistenceError::AlreadyWithdrawn),
    /// the withdrawal entry has been appended but is not linked —
    /// the operator surface should surface this as "withdrawal
    /// recorded but not linked" so audit history is preserved.
    /// Idempotent retries (same withdrawal entry id) are not
    /// expected here because the primitive mints a fresh UUID per
    /// call; deduplication is a higher-layer concern.
    pub async fn withdraw(
        &self,
        ledger_id: &str,
        original_entry_id: &str,
        reason: &str,
        subject_plugin: Option<&str>,
    ) -> Result<String, LedgerError> {
        if !is_known_ledger_id(ledger_id) {
            return Err(LedgerError::UnknownLedgerId(ledger_id.to_string()));
        }
        let payload = WithdrawalEntry {
            original_entry_id: original_entry_id.to_string(),
            reason: reason.to_string(),
            withdrawn_at_ms: system_time_to_ms_now(),
        };
        let withdrawal_id = self
            .append(ledger_id, WITHDRAWAL_SCHEMA_V1, &payload, subject_plugin)
            .await?;
        self.persistence
            .link_ledger_withdrawal(original_entry_id, &withdrawal_id)
            .await?;
        Ok(withdrawal_id)
    }

    /// Verify the signature on a previously-appended entry using
    /// the configured cryptographic services. Returns `true` when
    /// the signature is valid for the entry's canonical signing
    /// message; the NoOp default returns `true` only when the
    /// entry's `signature_bytes` is `None`.
    pub fn verify_entry(&self, entry: &PersistedLedgerEntry) -> bool {
        let signing_message = canonical_signing_message(
            &entry.entry_id,
            &entry.ledger_id,
            entry.schema_version,
            &entry.payload_json,
            entry.created_at_ms,
            entry.subject_plugin.as_deref(),
        );
        self.crypto
            .verify(&signing_message, entry.signature_bytes.as_deref())
    }

    /// Query entries from the substrate matching the filter. The
    /// primitive does not interpret the payloads here; callers
    /// deserialise the relevant variant by `ledger_id`.
    pub async fn query(
        &self,
        filter: LedgerEntryFilter<'_>,
    ) -> Result<Vec<PersistedLedgerEntry>, LedgerError> {
        Ok(self.persistence.query_ledger_entries(filter).await?)
    }

    async fn append<T: Serialize>(
        &self,
        ledger_id: &str,
        schema_version: u32,
        payload: &T,
        subject_plugin: Option<&str>,
    ) -> Result<String, LedgerError> {
        if !is_known_ledger_id(ledger_id) {
            return Err(LedgerError::UnknownLedgerId(ledger_id.to_string()));
        }
        let entry_id = Uuid::new_v4().to_string();
        let payload_json =
            serde_json::to_string(payload).map_err(LedgerError::Serialise)?;
        // Causal-ordering invariant: per-ledger-id monotonic floor.
        // The chosen `now_ms` becomes the timestamp the signature
        // covers AND the timestamp persisted on the row; both
        // agree by construction.
        let now_ms = self.next_monotonic_ms(ledger_id).await?;

        let signing_message = canonical_signing_message(
            &entry_id,
            ledger_id,
            schema_version,
            &payload_json,
            now_ms,
            subject_plugin,
        );
        let signature_bytes = self.crypto.sign(&signing_message);
        let signature_algorithm = self.crypto.signature_algorithm();

        self.persistence
            .append_ledger_entry(LedgerEntryRecord {
                entry_id: &entry_id,
                ledger_id,
                schema_version,
                payload_json: &payload_json,
                signature_bytes: signature_bytes.as_deref(),
                signature_algorithm,
                created_at_ms: now_ms,
                subject_plugin,
            })
            .await?;
        Ok(entry_id)
    }
}

fn is_known_ledger_id(ledger_id: &str) -> bool {
    matches!(
        ledger_id,
        LEDGER_CONSENT | LEDGER_TRUST | LEDGER_ACTION | LEDGER_LIFECYCLE
    )
}

/// Canonical signing message: tagged concatenation of the
/// immutable entry fields. Excludes `withdrawn_by_entry_id` so the
/// original entry's signature stays valid forever; the withdrawal
/// is a separate signed entry. Field separators are `0x00` bytes
/// (which never occur inside UTF-8 text) so the concatenation is
/// unambiguously parseable by a verifier.
fn canonical_signing_message(
    entry_id: &str,
    ledger_id: &str,
    schema_version: u32,
    payload_json: &str,
    created_at_ms: u64,
    subject_plugin: Option<&str>,
) -> Vec<u8> {
    let plugin_bytes = subject_plugin.unwrap_or("").as_bytes();
    let mut buf = Vec::with_capacity(
        entry_id.len()
            + ledger_id.len()
            + payload_json.len()
            + plugin_bytes.len()
            + 16,
    );
    buf.extend_from_slice(entry_id.as_bytes());
    buf.push(0);
    buf.extend_from_slice(ledger_id.as_bytes());
    buf.push(0);
    buf.extend_from_slice(&schema_version.to_be_bytes());
    buf.push(0);
    buf.extend_from_slice(payload_json.as_bytes());
    buf.push(0);
    buf.extend_from_slice(&created_at_ms.to_be_bytes());
    buf.push(0);
    buf.extend_from_slice(plugin_bytes);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::MemoryPersistenceStore;

    /// Test-only mock signing service. `sign` returns a
    /// deterministic blob over the message; `verify` recomputes
    /// the blob and checks equality. Encryption methods passthrough
    /// (these tests don't exercise them).
    #[derive(Debug)]
    struct MockSigningCryptographicServices;

    impl CryptographicServices for MockSigningCryptographicServices {
        fn sign(&self, message: &[u8]) -> Option<Vec<u8>> {
            // Mock signature: "MOCK\0" prefix followed by the
            // message length (BE u64) and the first 16 bytes of
            // the message (or the whole message if shorter, padded
            // with zeros). Deterministic; lets verify recompute and
            // compare.
            Some(mock_signature_for(message))
        }

        fn verify(&self, message: &[u8], signature: Option<&[u8]>) -> bool {
            let Some(sig) = signature else {
                return false;
            };
            let expected = mock_signature_for(message);
            sig == expected.as_slice()
        }

        fn signature_algorithm(&self) -> &'static str {
            "mock"
        }

        fn encrypt_at_rest(&self, plaintext: &[u8], _: &str) -> Vec<u8> {
            plaintext.to_vec()
        }

        fn decrypt_at_rest(&self, ciphertext: &[u8], _: &str) -> Vec<u8> {
            ciphertext.to_vec()
        }
    }

    fn mock_signature_for(message: &[u8]) -> Vec<u8> {
        let mut sig = Vec::with_capacity(5 + 8 + 16);
        sig.extend_from_slice(b"MOCK\0");
        sig.extend_from_slice(&(message.len() as u64).to_be_bytes());
        let mut tail = [0u8; 16];
        let take = message.len().min(16);
        tail[..take].copy_from_slice(&message[..take]);
        sig.extend_from_slice(&tail);
        sig
    }

    fn store() -> Arc<dyn PersistenceStore> {
        Arc::new(MemoryPersistenceStore::new())
    }

    fn no_op_ledger() -> LedgerPrimitive {
        LedgerPrimitive::with_no_op_crypto(store())
    }

    fn mock_ledger() -> LedgerPrimitive {
        LedgerPrimitive::new(
            store(),
            Arc::new(MockSigningCryptographicServices),
        )
    }

    fn sample_consent() -> ConsentEntry {
        ConsentEntry {
            consent_id: "tos.v1".into(),
            document_hash: "abc123".into(),
            decision: ConsentDecision::Accepted,
            decided_at_ms: 1_000,
            user_id: Some("operator-1".into()),
        }
    }

    fn sample_trust() -> TrustEntry {
        TrustEntry {
            publisher_id: "com.example".into(),
            display_name: "Example Publisher".into(),
            trust_level: TrustLevel::OperatorTrusted,
            granted_at_ms: 2_000,
            granted_via: GrantedVia::OperatorPrompt,
            scope: TrustScope::AllPlugins,
        }
    }

    fn sample_action() -> ActionEntry {
        ActionEntry {
            action_type: ActionType::Install,
            target: ActionTarget::Plugin {
                plugin_id: "org.example.target".into(),
            },
            before_state: serde_json::json!(null),
            after_state: serde_json::json!({"installed_version": "1.0.0"}),
            applied_at_ms: 3_000,
            approved_by: "ui-session-42".into(),
            outcome: ActionOutcome::Success,
        }
    }

    #[tokio::test]
    async fn append_consent_round_trips_under_no_op() {
        let ledger = no_op_ledger();
        let entry = sample_consent();
        let id = ledger
            .append_consent(&entry, Some("org.example.target"))
            .await
            .unwrap();
        let rows = ledger
            .query(LedgerEntryFilter {
                ledger_id: LEDGER_CONSENT,
                time_range: None,
                subject_plugin: None,
                include_withdrawn: true,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entry_id, id);
        assert_eq!(rows[0].ledger_id, LEDGER_CONSENT);
        assert_eq!(rows[0].schema_version, CONSENT_SCHEMA_V1);
        assert_eq!(rows[0].signature_algorithm, "none");
        assert!(rows[0].signature_bytes.is_none());
        assert_eq!(
            rows[0].subject_plugin.as_deref(),
            Some("org.example.target")
        );
        // Payload deserialises back to ConsentEntry intact.
        let decoded: ConsentEntry =
            serde_json::from_str(&rows[0].payload_json).unwrap();
        assert_eq!(decoded, entry);
        // NoOp verify accepts a missing signature.
        assert!(ledger.verify_entry(&rows[0]));
    }

    #[tokio::test]
    async fn append_trust_round_trips_under_no_op() {
        let ledger = no_op_ledger();
        let entry = sample_trust();
        ledger.append_trust(&entry, None).await.unwrap();
        let rows = ledger
            .query(LedgerEntryFilter {
                ledger_id: LEDGER_TRUST,
                time_range: None,
                subject_plugin: None,
                include_withdrawn: true,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        let decoded: TrustEntry =
            serde_json::from_str(&rows[0].payload_json).unwrap();
        assert_eq!(decoded, entry);
    }

    #[tokio::test]
    async fn append_action_round_trips_under_no_op() {
        let ledger = no_op_ledger();
        let entry = sample_action();
        ledger
            .append_action(&entry, Some("org.example.target"))
            .await
            .unwrap();
        let rows = ledger
            .query(LedgerEntryFilter {
                ledger_id: LEDGER_ACTION,
                time_range: None,
                subject_plugin: Some("org.example.target"),
                include_withdrawn: true,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        let decoded: ActionEntry =
            serde_json::from_str(&rows[0].payload_json).unwrap();
        assert_eq!(decoded, entry);
    }

    #[tokio::test]
    async fn mock_signing_round_trips_signature_and_verifies() {
        let ledger = mock_ledger();
        ledger
            .append_consent(&sample_consent(), None)
            .await
            .unwrap();
        let rows = ledger
            .query(LedgerEntryFilter {
                ledger_id: LEDGER_CONSENT,
                time_range: None,
                subject_plugin: None,
                include_withdrawn: true,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].signature_algorithm, "mock");
        assert!(rows[0].signature_bytes.is_some());
        // Verify recomputes and matches.
        assert!(ledger.verify_entry(&rows[0]));
        // Tampering with the payload string breaks verification.
        let mut tampered = rows[0].clone();
        tampered.payload_json.push_str(" tampered");
        assert!(!ledger.verify_entry(&tampered));
    }

    #[tokio::test]
    async fn no_op_verify_rejects_unexpected_signature() {
        // A NoOp ledger asked to verify an entry that carries a
        // signature (e.g., a row written by a vendor-signing
        // primitive then read back by a downgraded-config NoOp
        // verifier) MUST refuse: NoOp never makes a positive
        // cryptographic assertion.
        let ledger = no_op_ledger();
        let row = PersistedLedgerEntry {
            entry_id: "e-1".into(),
            ledger_id: LEDGER_CONSENT.into(),
            schema_version: CONSENT_SCHEMA_V1,
            payload_json: r#"{"x":1}"#.into(),
            signature_bytes: Some(b"some-sig".to_vec()),
            signature_algorithm: "ed25519".into(),
            created_at_ms: 100,
            subject_plugin: None,
            withdrawn_by_entry_id: None,
        };
        assert!(!ledger.verify_entry(&row));
    }

    #[tokio::test]
    async fn append_to_unknown_ledger_id_is_refused() {
        let ledger = no_op_ledger();
        // The typed append surfaces are constrained to the three
        // recognised ledgers; the only path that exposes the
        // ledger_id as a parameter is `withdraw`, which validates.
        let err = ledger
            .withdraw("evo.bogus", "some-id", "reason", None)
            .await
            .unwrap_err();
        match err {
            LedgerError::UnknownLedgerId(id) => assert_eq!(id, "evo.bogus"),
            other => panic!("expected UnknownLedgerId, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn withdrawal_links_original_and_appends_withdrawal_entry() {
        let ledger = no_op_ledger();
        let original_id = ledger
            .append_consent(&sample_consent(), Some("org.example.target"))
            .await
            .unwrap();
        let withdrawal_id = ledger
            .withdraw(
                LEDGER_CONSENT,
                &original_id,
                "user revoked",
                Some("org.example.target"),
            )
            .await
            .unwrap();
        assert_ne!(original_id, withdrawal_id);

        // Original is now linked.
        let rows = ledger
            .query(LedgerEntryFilter {
                ledger_id: LEDGER_CONSENT,
                time_range: None,
                subject_plugin: None,
                include_withdrawn: true,
            })
            .await
            .unwrap();
        let original = rows.iter().find(|r| r.entry_id == original_id).unwrap();
        assert_eq!(
            original.withdrawn_by_entry_id.as_deref(),
            Some(withdrawal_id.as_str())
        );

        // Withdrawal entry has the WithdrawalEntry payload shape.
        let withdrawal =
            rows.iter().find(|r| r.entry_id == withdrawal_id).unwrap();
        let payload: WithdrawalEntry =
            serde_json::from_str(&withdrawal.payload_json).unwrap();
        assert_eq!(payload.original_entry_id, original_id);
        assert_eq!(payload.reason, "user revoked");

        // Querying with include_withdrawn = false hides the original
        // but keeps the withdrawal entry itself visible.
        let visible = ledger
            .query(LedgerEntryFilter {
                ledger_id: LEDGER_CONSENT,
                time_range: None,
                subject_plugin: None,
                include_withdrawn: false,
            })
            .await
            .unwrap();
        assert!(!visible.iter().any(|r| r.entry_id == original_id));
        assert!(visible.iter().any(|r| r.entry_id == withdrawal_id));
    }

    #[tokio::test]
    async fn double_withdraw_is_refused_with_already_withdrawn() {
        let ledger = no_op_ledger();
        let original_id = ledger
            .append_consent(&sample_consent(), None)
            .await
            .unwrap();
        ledger
            .withdraw(LEDGER_CONSENT, &original_id, "first", None)
            .await
            .unwrap();
        // Second withdrawal mints a NEW withdrawal id and tries to
        // link; substrate refuses with AlreadyWithdrawn.
        let err = ledger
            .withdraw(LEDGER_CONSENT, &original_id, "second", None)
            .await
            .unwrap_err();
        match err {
            LedgerError::Persistence(PersistenceError::AlreadyWithdrawn {
                original_entry_id,
                ..
            }) => assert_eq!(original_entry_id, original_id),
            other => panic!("expected AlreadyWithdrawn, got {other:?}"),
        }
    }

    /// Cross-restart integration test against the real
    /// SQLite-backed persistence store. Opens a store at a
    /// tempdir path; appends one entry to each concrete ledger;
    /// withdraws one of them; drops the store; reopens at the
    /// same path; verifies every entry is still present with the
    /// correct payload + signature shape and the withdrawal link
    /// survives. Asserts the persistence layer carries the audit
    /// trail across what would be a steward restart in
    /// production.
    #[tokio::test]
    async fn sqlite_ledger_primitive_survives_restart() {
        use crate::persistence::SqlitePersistenceStore;
        use std::sync::Arc;
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("evo.db");

        // First steward "boot": open store, instantiate primitive,
        // append entries, withdraw one.
        let (consent_id, trust_id, action_id, withdrawal_id) = {
            let store: Arc<dyn PersistenceStore> = Arc::new(
                SqlitePersistenceStore::open(db_path.clone())
                    .expect("first open"),
            );
            let ledger = LedgerPrimitive::with_no_op_crypto(Arc::clone(&store));

            let c = ledger
                .append_consent(&sample_consent(), Some("org.example.target"))
                .await
                .unwrap();
            let t = ledger.append_trust(&sample_trust(), None).await.unwrap();
            let a = ledger
                .append_action(&sample_action(), Some("org.example.target"))
                .await
                .unwrap();

            let w = ledger
                .withdraw(
                    LEDGER_CONSENT,
                    &c,
                    "user revoked at first session",
                    None,
                )
                .await
                .unwrap();

            (c, t, a, w)
        };

        // Second steward "boot": reopen at the same path. Migrations
        // must be idempotent (no-op on a v15 database); every entry
        // must be present with the correct payload.
        let store: Arc<dyn PersistenceStore> = Arc::new(
            SqlitePersistenceStore::open(db_path.clone()).expect("reopen"),
        );
        let ledger = LedgerPrimitive::with_no_op_crypto(Arc::clone(&store));

        // Consent entry round-trips through SQLite.
        let consent_rows = ledger
            .query(LedgerEntryFilter {
                ledger_id: LEDGER_CONSENT,
                time_range: None,
                subject_plugin: None,
                include_withdrawn: true,
            })
            .await
            .unwrap();
        let original_consent = consent_rows
            .iter()
            .find(|r| r.entry_id == consent_id)
            .expect("original consent row present after restart");
        let decoded: ConsentEntry =
            serde_json::from_str(&original_consent.payload_json).unwrap();
        assert_eq!(decoded, sample_consent());
        assert_eq!(original_consent.signature_algorithm, "none");
        assert!(original_consent.signature_bytes.is_none());
        assert_eq!(
            original_consent.subject_plugin.as_deref(),
            Some("org.example.target")
        );
        assert_eq!(
            original_consent.withdrawn_by_entry_id.as_deref(),
            Some(withdrawal_id.as_str()),
            "withdrawal link must survive the restart"
        );

        // Withdrawal entry survives separately and decodes as
        // WithdrawalEntry.
        let withdrawal_row = consent_rows
            .iter()
            .find(|r| r.entry_id == withdrawal_id)
            .expect("withdrawal row present after restart");
        let wd: WithdrawalEntry =
            serde_json::from_str(&withdrawal_row.payload_json).unwrap();
        assert_eq!(wd.original_entry_id, consent_id);
        assert_eq!(wd.reason, "user revoked at first session");
        assert_eq!(withdrawal_row.schema_version, WITHDRAWAL_SCHEMA_V1);

        // Trust entry round-trips through SQLite.
        let trust_rows = ledger
            .query(LedgerEntryFilter {
                ledger_id: LEDGER_TRUST,
                time_range: None,
                subject_plugin: None,
                include_withdrawn: true,
            })
            .await
            .unwrap();
        assert_eq!(trust_rows.len(), 1);
        assert_eq!(trust_rows[0].entry_id, trust_id);
        let decoded: TrustEntry =
            serde_json::from_str(&trust_rows[0].payload_json).unwrap();
        assert_eq!(decoded, sample_trust());

        // Action entry round-trips through SQLite.
        let action_rows = ledger
            .query(LedgerEntryFilter {
                ledger_id: LEDGER_ACTION,
                time_range: None,
                subject_plugin: Some("org.example.target"),
                include_withdrawn: true,
            })
            .await
            .unwrap();
        assert_eq!(action_rows.len(), 1);
        assert_eq!(action_rows[0].entry_id, action_id);
        let decoded: ActionEntry =
            serde_json::from_str(&action_rows[0].payload_json).unwrap();
        assert_eq!(decoded, sample_action());

        // include_withdrawn=false hides the withdrawn original but
        // keeps the withdrawal entry visible across the restart.
        let visible_consent = ledger
            .query(LedgerEntryFilter {
                ledger_id: LEDGER_CONSENT,
                time_range: None,
                subject_plugin: None,
                include_withdrawn: false,
            })
            .await
            .unwrap();
        assert!(!visible_consent.iter().any(|r| r.entry_id == consent_id));
        assert!(visible_consent.iter().any(|r| r.entry_id == withdrawal_id));

        // The substrate also refuses a fresh withdrawal of an entry
        // already withdrawn in the prior session.
        let err = ledger
            .withdraw(LEDGER_CONSENT, &consent_id, "second-session", None)
            .await
            .unwrap_err();
        match err {
            LedgerError::Persistence(PersistenceError::AlreadyWithdrawn {
                original_entry_id,
                existing_withdrawal_id,
                ..
            }) => {
                assert_eq!(original_entry_id, consent_id);
                assert_eq!(existing_withdrawal_id, withdrawal_id);
            }
            other => panic!("expected AlreadyWithdrawn, got {other:?}"),
        }
    }

    fn sample_lifecycle(plugin: &str) -> LifecycleEntry {
        LifecycleEntry {
            event_type: LifecycleEventType::StreamOpened,
            source_plugin: plugin.into(),
            target: LifecycleTarget::Stream {
                stream_id: "test.stream".into(),
            },
            recorded_at_ms: 0,
            outcome: LifecycleOutcome::Success,
            payload: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn append_per_ledger_created_at_ms_is_strictly_monotonic() {
        // Causal-ordering invariant: every append against the same
        // ledger id mints a created_at_ms strictly greater than
        // every previous append's, even when the system clock
        // hands back the same millisecond for every call.
        // Forensic reconstruction (joining a dispatched entry to
        // its provider-failed entry, or comparing burst events)
        // depends on this ordering.
        let store = store();
        let ledger = LedgerPrimitive::with_no_op_crypto(Arc::clone(&store));

        // Append a burst tight enough to land in the same ms.
        let mut ids = Vec::new();
        for _ in 0..50 {
            let id = ledger
                .append_lifecycle(&sample_lifecycle("org.test.burst"))
                .await
                .unwrap();
            ids.push(id);
        }

        // Read the rows back in stored order. Each row's
        // created_at_ms must be strictly greater than the row
        // before it. The row order in the result reflects the
        // (created_at_ms ASC, entry_id ASC) sort applied at the
        // substrate.
        let rows = store
            .query_ledger_entries(LedgerEntryFilter {
                ledger_id: LEDGER_LIFECYCLE,
                time_range: None,
                subject_plugin: None,
                include_withdrawn: true,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 50);
        for w in rows.windows(2) {
            assert!(
                w[1].created_at_ms > w[0].created_at_ms,
                "expected strictly-increasing created_at_ms; saw \
                 {} -> {}",
                w[0].created_at_ms,
                w[1].created_at_ms
            );
        }

        // The minted ids in the burst order match the rows once
        // sorted by created_at_ms — i.e. the substrate's ordering
        // matches the appender's call order.
        let row_ids: Vec<&String> = rows.iter().map(|r| &r.entry_id).collect();
        let id_refs: Vec<&String> = ids.iter().collect();
        assert_eq!(
            row_ids, id_refs,
            "substrate ordering must match call order under the \
             monotonic-floor invariant"
        );
    }

    #[tokio::test]
    async fn append_monotonic_floor_inherits_persisted_max_on_construction() {
        // Reboot continuity: a fresh LedgerPrimitive constructed
        // over a substrate that already holds entries inherits
        // the per-ledger floor on first append.
        let store = store();

        // Generation 1: write one entry, capture its
        // created_at_ms, drop the primitive.
        let id_g1 = {
            let ledger = LedgerPrimitive::with_no_op_crypto(Arc::clone(&store));
            ledger
                .append_lifecycle(&sample_lifecycle("org.test"))
                .await
                .unwrap()
        };

        // Generation 2: fresh primitive over the same substrate,
        // append again. The new entry's created_at_ms must be
        // strictly greater than g1's, even if the wall clock
        // hasn't ticked.
        let ledger_g2 = LedgerPrimitive::with_no_op_crypto(Arc::clone(&store));
        let id_g2 = ledger_g2
            .append_lifecycle(&sample_lifecycle("org.test"))
            .await
            .unwrap();

        let rows = store
            .query_ledger_entries(LedgerEntryFilter {
                ledger_id: LEDGER_LIFECYCLE,
                time_range: None,
                subject_plugin: None,
                include_withdrawn: true,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        // Find the two entries by id.
        let g1_row = rows.iter().find(|r| r.entry_id == id_g1).unwrap();
        let g2_row = rows.iter().find(|r| r.entry_id == id_g2).unwrap();
        assert!(
            g2_row.created_at_ms > g1_row.created_at_ms,
            "post-reboot append must have strictly-greater \
             created_at_ms than the pre-reboot frontier; saw {} \
             -> {}",
            g1_row.created_at_ms,
            g2_row.created_at_ms
        );
    }

    #[tokio::test]
    async fn append_monotonic_floor_is_per_ledger_id() {
        // Two appends against different ledger ids do not share a
        // floor — one ledger's high watermark does not inflate
        // another ledger's first append.
        let store = store();
        let ledger = LedgerPrimitive::with_no_op_crypto(Arc::clone(&store));

        // Burst 100 lifecycle entries to push evo.lifecycle's
        // floor well above the wall clock.
        for _ in 0..100 {
            ledger
                .append_lifecycle(&sample_lifecycle("org.test"))
                .await
                .unwrap();
        }
        let lifecycle_max = store
            .query_max_created_at_ms_for_ledger(LEDGER_LIFECYCLE)
            .await
            .unwrap();

        // First append against evo.consent uses raw wall clock,
        // not the lifecycle floor.
        ledger
            .append_consent(&sample_consent(), None)
            .await
            .unwrap();
        let consent_max = store
            .query_max_created_at_ms_for_ledger(LEDGER_CONSENT)
            .await
            .unwrap();

        assert!(
            consent_max < lifecycle_max,
            "evo.consent's first append must not inherit evo.lifecycle's \
             burst-inflated floor; saw consent_max={} lifecycle_max={}",
            consent_max,
            lifecycle_max
        );
    }
}
