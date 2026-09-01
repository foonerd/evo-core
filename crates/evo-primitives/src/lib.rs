// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Framework-foundational primitives.
//!
//! This crate is the lowest tier of the evo framework's
//! dependency graph. The `evo` crate (the framework runtime)
//! depends on it. Domain-tier crates (audio, multi-room,
//! networking, etc.) ALSO depend on it. This lets domain crates
//! reach primitive types like canonical identifiers without
//! pulling in the framework runtime, which would create a
//! dependency cycle and prevent a clean framework / domain
//! separation.
//!
//! Invariants on the contents of this crate:
//!
//! - **No domain concepts.** Multi-room, audio, networking,
//!   etc. types do NOT belong here. The line is sharp: a type
//!   in `evo-primitives` must be conceivably useful to ANY
//!   plugin author, regardless of what their plugin does.
//! - **No framework runtime.** No async tasks, no I/O, no
//!   subscription channels owned by the framework. Traits
//!   that abstract over those surfaces are acceptable; the
//!   implementations stay in the runtime crate.
//! - **Small and stable.** This crate's API is the
//!   foundational contract every other crate compiles
//!   against. Breaking changes here ripple across the entire
//!   workspace; the trade-off is worth it only for clearly-
//!   primitive types.
//!
//! Current inhabitants:
//!
//! - `DeviceId` — canonical UUIDv4 device identifier.
//! - `PersistenceError` — domain-agnostic persistence error
//!   surface. The framework's SQLite backend wraps its
//!   driver-specific errors into the `Sqlite` / `MigrationFailed`
//!   variants as stringified context, so this crate stays free
//!   of the `rusqlite` dependency.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Canonical stable device identifier. UUIDv4 token form
/// (`"550e8400-e29b-41d4-a716-446655440000"`). Generated at
/// first boot; never rotates.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceId(pub String);

impl DeviceId {
    /// Generate a fresh device id. Used at first boot only.
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Returns the canonical id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Short form for display labels — first 8 hex chars of
    /// the UUID. Used to construct the default display name
    /// (`evo-12345678`) at first boot.
    pub fn short(&self) -> &str {
        let bytes = self.0.as_bytes();
        let mut i = 0;
        let mut taken = 0;
        while i < bytes.len() && taken < 8 {
            if bytes[i] != b'-' {
                taken += 1;
            }
            i += 1;
        }
        std::str::from_utf8(&bytes[..i.min(bytes.len())]).unwrap_or(&self.0)
    }
}

/// Provenance of the current `display_name` value on a
/// [`DeviceIdentity`]. Distinguishes framework-managed
/// auto-derivations (subject to the collision resolver) from
/// operator-set names (sticky; framework MUST NOT rewrite).
/// Default `Auto` so existing persisted rows lacking the
/// field migrate cleanly — auto-seeded names from prior
/// boots remain eligible for collision resolution.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum NameSource {
    /// Framework-managed: the collision resolver may rewrite
    /// the display_name when an mDNS-SD peer is observed
    /// carrying the same name.
    #[default]
    Auto,
    /// Operator-set via `set_device_display_name`: the
    /// collision resolver MUST NOT rewrite. Sticky until a
    /// subsequent operator gesture changes it.
    Operator,
}

/// Persistent device identity. Foundation substrate for the
/// multi-room native protocol; the framework owns the typed
/// shape + first-boot generation; vendor distributions
/// populate the optional public-key slot through the
/// cryptographic-services hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceIdentity {
    /// Canonical stable id.
    pub device_id: DeviceId,
    /// Operator-editable display name. Seeded from the OS
    /// hostname at first boot when the hostname is sane;
    /// falls back to `evo-<short>` when no usable hostname is
    /// available. Mutable via the `set_device_display_name`
    /// wire op, which also promotes [`Self::name_source`] to
    /// [`NameSource::Operator`].
    pub display_name: String,
    /// Optional vendor-distribution identifier. `None` when
    /// no vendor populated it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor_id: Option<String>,
    /// Optional vendor-supplied public key bytes. The
    /// framework owns the typed slot but does NOT generate
    /// the keypair — the vendor's cryptographic-services hook
    /// populates this. mDNS-SD discovery embeds the
    /// fingerprint when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key_bytes: Option<Vec<u8>>,
    /// Wall-clock millisecond timestamp the identity was
    /// first generated.
    pub created_at_ms: u64,
    /// Provenance of the current `display_name`. Drives the
    /// collision-resolver gate: only `Auto` names are eligible
    /// for rewrite. Defaults to `Auto` on deserialisation of
    /// legacy rows that lack this field.
    #[serde(default)]
    pub name_source: NameSource,
}

/// Maximum permitted byte length of a `display_name` (RFC 1035
/// label limit; mDNS-SD TXT records inherit the convention so
/// the discovery surface caps cleanly).
pub const DISPLAY_NAME_MAX_LEN: usize = 63;

/// One source-host election record, per multi-room group.
/// Records the local node's last-known source-host election
/// against its current local view of `{local_id, live_peers}`.
/// Mirror of the persistence-layer row of the same shape —
/// kept in the foundation crate so consumer code (election
/// runtime impl in a domain crate, operator wire-op handlers
/// in the framework) can share the record type without
/// circular dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceHostElection {
    /// Canonical group id this election applies to.
    pub group_id: String,
    /// Canonical device id of the elected source-host. `None`
    /// when no candidate was live at decision time (the group
    /// has no reachable members).
    pub source_host_device_id: Option<String>,
    /// Number of candidates considered at decision time (group
    /// members intersected with the live device set). Recorded
    /// for diagnostic visibility.
    pub candidate_count: u32,
    /// Wall-clock millisecond timestamp the election was
    /// recorded.
    pub elected_at_ms: u64,
}

/// Read-only handle to source-host election state for
/// multi-room groups. Framework subsystems that need to know
/// "which device is the source-host for group X" hold an
/// `Arc<dyn ElectionState>` rather than a concrete election
/// runtime — that way the framework crate stays independent of
/// the multi-room domain crate where the actual election
/// implementation lives.
///
/// `None` is the substrate-empty default for groups with no
/// elected source-host (empty candidate set, multi-room
/// runtime not yet provided by a plugin, or the local node has
/// no live view of any member yet).
#[async_trait::async_trait]
pub trait ElectionState: Send + Sync + std::fmt::Debug {
    /// Look up the elected source-host's canonical device id
    /// for one multi-room group. Returns `None` when no group
    /// with that id exists or the candidate set is empty.
    /// High-frequency dispatch paths prefer this lightweight
    /// shape over [`Self::election_for`].
    async fn source_host_for(&self, group_id: &str) -> Option<String>;

    /// Snapshot every group's current source-host election as
    /// `(group_id, source_host_device_id)` pairs. Groups with
    /// no elected source-host are present with `None` on the
    /// second slot. Order is unspecified.
    async fn list_source_hosts(&self) -> Vec<(String, Option<String>)>;

    /// Read the full election record for one group, including
    /// diagnostic fields (`candidate_count`, `elected_at_ms`).
    /// `None` when no election is recorded for that id.
    /// Operator-facing wire-op handlers (`get_source_host`)
    /// use this richer return shape.
    async fn election_for(&self, group_id: &str) -> Option<SourceHostElection>;

    /// Snapshot every recorded election as full records. Order
    /// is unspecified. Operator-facing wire-op handlers
    /// (`list_source_hosts`) use this shape.
    async fn list_elections(&self) -> Vec<SourceHostElection>;

    /// Liveness window (milliseconds) the implementation uses to
    /// classify a peer as live for election purposes. Read by
    /// the framework's diagnostic surfaces (e.g. session-status
    /// projection) so operator-visible visibility surfaces report
    /// the same window the underlying election decides on. The
    /// trait default (60 000 ms) matches the multi-room
    /// reference implementation's default and is also what
    /// [`NoElection`] reports.
    fn liveness_window_ms(&self) -> u64 {
        60_000
    }
}

/// Shared, swappable handle to an [`ElectionState`]
/// implementation. Framework substrates that consume election
/// state (`audio_plane`, `group_topology`, `server` wire-op
/// handlers) hold `Arc`-clones of this type so a domain plugin
/// can inject a real `ElectionState` impl (typically the
/// concrete `ElectionRuntime`) after boot. Every substrate
/// sees the swap on its next `current()` call.
///
/// The default-constructed value points at [`NoElection`], so
/// boot-without-multi-room is a valid state: queries return
/// `None`, no panic.
///
/// Implementation note: `arc_swap::ArcSwap<dyn Trait>` requires
/// `Sized`, which trait objects don't satisfy. The handle
/// therefore uses `std::sync::RwLock<Arc<dyn ElectionState>>`
/// internally — election-handle reads are infrequent (one per
/// election query, dominated by the underlying election
/// substrate work) and the read lock's cost is in the noise
/// against the async work that follows.
#[derive(Debug, Clone)]
pub struct SharedElectionState(
    std::sync::Arc<std::sync::RwLock<std::sync::Arc<dyn ElectionState>>>,
);

impl SharedElectionState {
    /// Construct a `SharedElectionState` pointing at the
    /// supplied implementation.
    pub fn new(initial: std::sync::Arc<dyn ElectionState>) -> Self {
        Self(std::sync::Arc::new(std::sync::RwLock::new(initial)))
    }

    /// Construct a `SharedElectionState` pointing at
    /// [`NoElection`]. Boot-path default.
    pub fn no_op() -> Self {
        Self::new(std::sync::Arc::new(NoElection))
    }

    /// Read the current implementation. Cloning the inner
    /// `Arc<dyn ElectionState>` is cheap (single atomic ref-
    /// count bump). Callers await trait methods on the result.
    pub fn current(&self) -> std::sync::Arc<dyn ElectionState> {
        std::sync::Arc::clone(
            &*self.0.read().expect("SharedElectionState rwlock poisoned"),
        )
    }

    /// Replace the current implementation. Subsequent
    /// [`Self::current`] calls return the new handle.
    pub fn set(&self, new: std::sync::Arc<dyn ElectionState>) {
        let mut guard =
            self.0.write().expect("SharedElectionState rwlock poisoned");
        *guard = new;
    }
}

impl Default for SharedElectionState {
    fn default() -> Self {
        Self::no_op()
    }
}

/// Zero-state implementation of [`ElectionState`]. Every query
/// returns the empty / `None` response. Framework substrates
/// (`audio_plane`, `group_topology`, `server`) default to this
/// at boot when no domain crate has provided a real election
/// runtime; the multi-room plugin (or any other domain plugin
/// that constructs an `ElectionState` implementation) injects
/// a real handle later via the substrate's `set_election`
/// surface.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoElection;

#[async_trait::async_trait]
impl ElectionState for NoElection {
    async fn source_host_for(&self, _group_id: &str) -> Option<String> {
        None
    }

    async fn list_source_hosts(&self) -> Vec<(String, Option<String>)> {
        Vec::new()
    }

    async fn election_for(
        &self,
        _group_id: &str,
    ) -> Option<SourceHostElection> {
        None
    }

    async fn list_elections(&self) -> Vec<SourceHostElection> {
        Vec::new()
    }
}

/// Opaque per-plugin identity token used in happenings,
/// projections, and wire-protocol envelopes wherever the
/// framework needs to refer to "the plugin that did X" without
/// leaking the plugin's canonical name (which is operator-
/// configurable). Production tokens are derived from the
/// plugin name + instance id via a stable hash; consumers
/// treat the token as opaque and compare by exact-string
/// equality only.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct ClaimantToken(String);

impl ClaimantToken {
    /// Build a token from a pre-encoded string. Primarily for
    /// reconstructing tokens read from the wire or persistence;
    /// production tokens are minted via the framework's
    /// `derive_token` (in the `evo` crate's `claimant` module).
    pub fn from_string(s: String) -> Self {
        Self(s)
    }

    /// View the token as a string slice (for serialisation, log
    /// output, hashmap keys).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Move the token's inner string out for callers that need to
    /// own it (e.g. building a HashMap key).
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for ClaimantToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ClaimantToken {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Cardinality constraint on one side of a relation predicate
/// per the framework's relation-graph grammar. Cardinality
/// violations emit warnings rather than rejecting assertions
/// — the relation graph is permissive about what plugins
/// declare; the framework's projection layer surfaces the
/// violation to operators.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum Cardinality {
    /// Exactly one subject on this side.
    ExactlyOne,
    /// At most one subject on this side; zero is allowed.
    AtMostOne,
    /// At least one subject on this side; upper bound unconstrained.
    AtLeastOne,
    /// No constraint.
    #[default]
    Many,
}

/// Domain-agnostic persistence-layer error. The framework's
/// concrete SQLite backend (in the `evo` crate) wraps its
/// driver-specific errors into the `Sqlite` / `MigrationFailed`
/// variants as stringified context; downstream consumers
/// observe a stable enum without depending on the underlying
/// SQL driver.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PersistenceError {
    /// Database-backend error. `detail` is the driver's
    /// `Display` form rather than the typed error to keep the
    /// primitives crate dependency-free at the API surface.
    #[error("SQLite error ({context}): {detail}")]
    Sqlite {
        /// What the steward was attempting when the error fired.
        context: String,
        /// Stringified driver error.
        detail: String,
    },

    /// Wrapped I/O error from a filesystem operation around the
    /// database file (open, create-parent, etc.).
    #[error("I/O error ({context}): {detail}")]
    Io {
        /// What the steward was attempting.
        context: String,
        /// Stringified I/O error.
        detail: String,
    },

    /// The database's recorded schema version is greater than this
    /// build of the steward supports. Downgrade refusal: the steward
    /// would otherwise risk silent best-effort operation against a
    /// schema it does not understand.
    #[error(
        "database schema version {database} is newer than supported version \
         {supported}; downgrades are not supported"
    )]
    SchemaVersionAhead {
        /// Maximum version recorded in the database's `schema_version`
        /// table.
        database: u32,
        /// Maximum version this build understands.
        supported: u32,
    },

    /// A migration step failed. Carries the version that failed and
    /// the stringified driver error so an operator can correlate the
    /// failure with the migration file.
    #[error("migration to schema version {version} failed: {detail}")]
    MigrationFailed {
        /// The migration version that failed.
        version: u32,
        /// Stringified driver error.
        detail: String,
    },

    /// The async pool returned an error (connection acquisition or
    /// blocking-task join failure).
    #[error("pool error ({context}): {message}")]
    Pool {
        /// What the steward was attempting.
        context: String,
        /// Stringified pool error.
        message: String,
    },

    /// A `record_*` call was given malformed input (empty addressing
    /// list, etc.) the persistence layer refuses to write.
    #[error("invalid persistence input: {0}")]
    Invalid(String),

    /// A `link_ledger_withdrawal` call against an entry whose
    /// `withdrawn_by_entry_id` is already non-NULL with a DIFFERENT
    /// withdrawal id. Idempotent retries with the SAME withdrawal id
    /// succeed.
    #[error(
        "ledger entry {original_entry_id} already withdrawn by \
         {existing_withdrawal_id}; refused to overwrite with \
         {requested_withdrawal_id}"
    )]
    AlreadyWithdrawn {
        /// The entry whose withdrawal-link was attempted to overwrite.
        original_entry_id: String,
        /// The entry id already linked to the original.
        existing_withdrawal_id: String,
        /// The new withdrawal id the caller supplied.
        requested_withdrawal_id: String,
    },

    /// The substrate row addressed by the operation does not exist.
    #[error("not found: {0}")]
    NotFound(String),
}

// ---------------------------------------------------------------
// Multi-room role substrate — typed shape + trait + shared handle
// ---------------------------------------------------------------

/// Multi-room role a device plays. Per-device operator
/// gesture; the multi-room domain's role substrate is the
/// source of truth.
///
/// Wire form is the lowercase string of the discriminant
/// (`"source"` / `"receiver"` / `"auto"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Operator's preferred source-host. Election still
    /// resolves the actual elected device per its
    /// canonical-min rule; this signals operator intent.
    Source,
    /// Rendering-only. Opens an audio output substrate
    /// and subscribes to audio-plane frames for its group.
    Receiver,
    /// No multi-room engagement. Output free for local-only
    /// playback.
    Auto,
}

impl Role {
    /// Stable lowercase identifier — matches the SQL CHECK
    /// constraint and the wire-form serde rendering.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Source => "source",
            Role::Receiver => "receiver",
            Role::Auto => "auto",
        }
    }
}

impl std::str::FromStr for Role {
    type Err = RoleStoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "source" => Ok(Role::Source),
            "receiver" => Ok(Role::Receiver),
            "auto" => Ok(Role::Auto),
            other => Err(RoleStoreError::InvalidRole(other.to_string())),
        }
    }
}

/// Substrate-mutation event the role-store emits on its
/// subscription channel. Reactive-only multi-room plugins
/// consume the channel and run the role-transition state
/// machine in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleChange {
    /// A device's role was set (first set or changed from a
    /// prior role). Carries the prior role if any (`None` on
    /// first set) so subscribers can dispatch to the
    /// correct transition arm.
    Set {
        /// Canonical device id.
        device_id: String,
        /// Role before the change. `None` on first
        /// operator gesture for this device.
        prior_role: Option<Role>,
        /// Role after the change.
        new_role: Role,
    },
    /// A device's role was cleared (returns to substrate-
    /// empty / `Auto` default). Carries the prior role for
    /// subscribers to dispatch the correct teardown arm.
    Cleared {
        /// Canonical device id.
        device_id: String,
        /// Role before clearing.
        prior_role: Role,
    },
}

/// Errors raised by the role substrate. The shape is
/// framework-agnostic; the underlying persistence error is
/// wrapped as the `Persistence` variant.
#[derive(Debug, thiserror::Error)]
pub enum RoleStoreError {
    /// Underlying persistence error.
    #[error("persistence error: {0}")]
    Persistence(#[from] PersistenceError),
    /// Operator supplied a role string outside the enum
    /// (`source` / `receiver` / `auto`).
    #[error("invalid role: {0}")]
    InvalidRole(String),
    /// Operator supplied an empty device id.
    #[error("device_id must not be empty")]
    InvalidDeviceId,
}

/// Read + write handle to the per-device role substrate.
/// Framework consumers (e.g. the server wire-op handlers for
/// `set_device_role` / `list_device_roles`) hold an
/// `Arc<dyn RoleStoreHandle>` rather than a concrete store —
/// the concrete implementation lives in the multi-room domain
/// crate where role substrate semantics belong.
///
/// `Cleared` semantics: `clear_role` returns the device to
/// the substrate-empty `Auto` default; idempotent on devices
/// without an explicit row.
#[async_trait::async_trait]
pub trait RoleStoreHandle: Send + Sync + std::fmt::Debug {
    /// Set the operator-declared role for a device.
    /// Idempotent on unchanged value (no event, no broadcast).
    /// `set_by` is an optional operator / surface identifier
    /// (CLI session, wire-op caller); it surfaces in the
    /// substrate's emitted happening for audit.
    async fn set_role(
        &self,
        device_id: &str,
        role: Role,
        set_by: Option<String>,
    ) -> Result<(), RoleStoreError>;

    /// Read the operator-declared role for a device. Returns
    /// `Role::Auto` when the device has no row in the
    /// substrate — the non-disruptive substrate-empty default.
    async fn get_role(&self, device_id: &str) -> Result<Role, RoleStoreError>;

    /// List every device with an explicit operator-gestured
    /// role. Devices in the substrate-empty / `Auto` default
    /// are NOT enumerated (they have no row to surface).
    async fn list_explicit_roles(
        &self,
    ) -> Result<Vec<(String, Role)>, RoleStoreError>;

    /// Clear the operator-declared role for a device. The
    /// device returns to the substrate-empty `Auto` default.
    /// Idempotent on devices that already have no explicit
    /// row.
    async fn clear_role(&self, device_id: &str) -> Result<(), RoleStoreError>;
}

/// Shared, swappable handle to a [`RoleStoreHandle`]
/// implementation. Framework consumers hold `Arc`-clones of
/// this type so a domain plugin can inject a real
/// `RoleStoreHandle` impl after boot. Every consumer sees the
/// swap on its next `current()` call.
///
/// The default-constructed value points at [`NoRoleStore`], so
/// a steward booting without a multi-room domain runtime is a
/// valid state: every query returns the substrate-empty
/// default, every mutation returns `Ok(())` after a no-op.
#[derive(Debug, Clone)]
pub struct SharedRoleStore(
    std::sync::Arc<std::sync::RwLock<std::sync::Arc<dyn RoleStoreHandle>>>,
);

impl SharedRoleStore {
    /// Construct a `SharedRoleStore` pointing at the supplied
    /// implementation.
    pub fn new(initial: std::sync::Arc<dyn RoleStoreHandle>) -> Self {
        Self(std::sync::Arc::new(std::sync::RwLock::new(initial)))
    }

    /// Construct a `SharedRoleStore` pointing at
    /// [`NoRoleStore`]. Boot-path default.
    pub fn no_op() -> Self {
        Self::new(std::sync::Arc::new(NoRoleStore))
    }

    /// Read the current implementation. Cloning the inner
    /// `Arc<dyn RoleStoreHandle>` is cheap (single atomic ref-
    /// count bump). Callers await trait methods on the result.
    pub fn current(&self) -> std::sync::Arc<dyn RoleStoreHandle> {
        std::sync::Arc::clone(
            &*self.0.read().expect("SharedRoleStore rwlock poisoned"),
        )
    }

    /// Replace the current implementation. Subsequent
    /// [`Self::current`] calls return the new handle.
    pub fn set(&self, new: std::sync::Arc<dyn RoleStoreHandle>) {
        let mut guard =
            self.0.write().expect("SharedRoleStore rwlock poisoned");
        *guard = new;
    }
}

impl Default for SharedRoleStore {
    fn default() -> Self {
        Self::no_op()
    }
}

/// Zero-state implementation of [`RoleStoreHandle`]. Reads
/// resolve to `Role::Auto` (the substrate-empty default);
/// `list_explicit_roles` returns the empty list; writes succeed
/// silently. Framework boot uses this implementor until a
/// domain plugin (multi-room) installs the concrete role
/// substrate via the shared handle.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoRoleStore;

#[async_trait::async_trait]
impl RoleStoreHandle for NoRoleStore {
    async fn set_role(
        &self,
        _device_id: &str,
        _role: Role,
        _set_by: Option<String>,
    ) -> Result<(), RoleStoreError> {
        Ok(())
    }

    async fn get_role(&self, _device_id: &str) -> Result<Role, RoleStoreError> {
        Ok(Role::Auto)
    }

    async fn list_explicit_roles(
        &self,
    ) -> Result<Vec<(String, Role)>, RoleStoreError> {
        Ok(Vec::new())
    }

    async fn clear_role(&self, _device_id: &str) -> Result<(), RoleStoreError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_returns_uuid_v4_form() {
        let id = DeviceId::generate();
        // 8-4-4-4-12 hex form, total 36 chars with 4 hyphens.
        assert_eq!(id.0.len(), 36);
        assert_eq!(id.0.matches('-').count(), 4);
    }

    #[test]
    fn short_returns_first_eight_hex_chars() {
        let id = DeviceId("550e8400-e29b-41d4-a716-446655440000".to_string());
        assert_eq!(id.short(), "550e8400");
    }

    #[test]
    fn as_str_returns_full_id() {
        let id = DeviceId("foo".to_string());
        assert_eq!(id.as_str(), "foo");
    }
}
