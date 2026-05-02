//! Operator policy for client-API capabilities.
//!
//! The client API exposes a small set of named, per-connection
//! capabilities. Some capabilities (notably `resolve_claimants`,
//! which exchanges opaque claimant tokens for plain plugin names)
//! reveal information that operators may not want every client to
//! see. This module loads the operator-controlled policy that
//! decides, on a per-connection basis, which capabilities a
//! consumer is permitted to negotiate.
//!
//! ## File location and shape
//!
//! Default path is `/etc/evo/client_acl.toml`. The file is optional
//! — when absent, the steward applies the conservative default
//! described below. Shape:
//!
//! ```toml
//! [capabilities.resolve_claimants]
//! allow_local = true        # any local-socket peer
//! allow_uids  = [0, 1000]   # additional explicit UIDs
//! allow_gids  = [27]        # additional explicit GIDs
//! ```
//!
//! All keys are optional; an empty `[capabilities.resolve_claimants]`
//! table behaves the same as a missing one.
//!
//! ## Default policy (file absent or table absent)
//!
//! `resolve_claimants` is granted to a connection only when both
//! sides of the Unix-domain socket have the same effective UID. The
//! consumer is, by construction, talking to the steward over the
//! local socket; matching UIDs is the strongest cheap signal that
//! the consumer is the operator (or a process running as the same
//! identity as the steward) rather than an unprivileged service or
//! external bridge. Distributions with a frontend or bridge running
//! under a different UID widen the policy via the file.
//!
//! ## Why a separate policy file?
//!
//! `evo.toml` describes how the steward runs (paths, log levels,
//! security knobs that gate plugin admission). The capability ACL
//! describes who, on the consumer side, may see what — a different
//! axis of operator concern, with different review and audit
//! cadence. Splitting the file lets a distribution ship a default
//! `evo.toml` and let an integrator drop in a tighter or looser
//! `client_acl.toml` without touching the steward's own runtime
//! configuration.

use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::error::StewardError;

/// Default location of the client-API capability ACL.
pub const DEFAULT_CLIENT_ACL_PATH: &str = "/etc/evo/client_acl.toml";

/// Identifying details about the peer on the other side of a
/// Unix-domain client connection.
///
/// The steward queries these values once at connection accept time
/// (via `tokio::net::UnixStream::peer_cred`) and passes them to the
/// ACL when the connection negotiates capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCredentials {
    /// Effective UID of the peer process. `None` when the platform
    /// did not return one (which should not happen on Linux).
    pub uid: Option<u32>,
    /// Effective GID of the peer process. `None` when the platform
    /// did not return one.
    pub gid: Option<u32>,
}

/// Identity the steward itself is running as, captured at boot.
///
/// The default ACL grants `resolve_claimants` only when the peer's
/// UID matches the steward's UID, so the steward needs to know its
/// own identity at boot rather than resolving it on every
/// connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StewardIdentity {
    /// Effective UID of the steward process at boot.
    pub uid: Option<u32>,
    /// Effective GID of the steward process at boot.
    pub gid: Option<u32>,
}

impl StewardIdentity {
    /// Capture the current process's effective UID and GID.
    ///
    /// On non-Unix targets both fields are `None`; the default ACL
    /// then denies `resolve_claimants` to every connection (no UID
    /// can match an absent UID), which is the conservative behaviour
    /// for those targets.
    ///
    /// On Unix targets where `/proc/self` is unreadable (a sandboxed
    /// runtime that hides `/proc`, or a non-Linux Unix without a
    /// procfs equivalent at the same path), the steward emits a
    /// `tracing::warn!` so the operator sees that the UID/GID
    /// capture fell back to `None` rather than discovering at runtime
    /// that every local-grant `resolve_claimants` request is denied.
    pub fn current() -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // Reading `/proc/self` metadata gives the effective UID
            // and GID of the current process without an unsafe libc
            // call. Falls through to `None`/`None` on platforms or
            // sandboxes where /proc is not available; the ACL then
            // denies the local-grant path, which is the conservative
            // outcome.
            match std::fs::metadata("/proc/self") {
                Ok(meta) => Self {
                    uid: Some(meta.uid()),
                    gid: Some(meta.gid()),
                },
                Err(e) => {
                    tracing::warn!(
                        component = "client_acl",
                        error = %e,
                        "could not read /proc/self for steward UID/GID; \
                         resolve_claimants local-grant path will deny every \
                         peer until the file is readable. Check whether the \
                         steward is running under a sandbox that hides /proc."
                    );
                    Self {
                        uid: None,
                        gid: None,
                    }
                }
            }
        }
        #[cfg(not(unix))]
        {
            Self {
                uid: None,
                gid: None,
            }
        }
    }
}

/// Per-capability policy for `resolve_claimants`.
///
/// Each connection's grant decision answers a single question: does
/// the connecting peer satisfy any one of the configured
/// allowances? The check short-circuits on the first match.
///
/// Unknown fields are rejected at parse time so an operator typo
/// (`allow_uds = [0]` instead of `allow_uids = [0]`) surfaces as a
/// clear boot error rather than a silent default-deny.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveClaimantsPolicy {
    /// When `true`, any local-socket peer whose UID matches the
    /// steward's UID is granted `resolve_claimants`. Mirrors the
    /// "local admin" default.
    #[serde(default)]
    pub allow_local: Option<bool>,
    /// Explicit UIDs that may negotiate the capability regardless
    /// of whether they match the steward's UID. Empty by default.
    #[serde(default)]
    pub allow_uids: Vec<u32>,
    /// Explicit GIDs that may negotiate the capability. Empty by
    /// default. Useful for granting access to a system group
    /// without listing every user that group rotates through.
    #[serde(default)]
    pub allow_gids: Vec<u32>,
}

/// Per-capability policy block. Mirrors
/// [`ResolveClaimantsPolicy`] in shape and semantics; used by every
/// capability that follows the same allow-local / allow-uids /
/// allow-gids gate (e.g., `plugins_admin`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginsAdminPolicy {
    /// When `true`, any local-socket peer whose UID matches the
    /// steward's UID is granted `plugins_admin`. Mirrors the
    /// "local admin" default.
    #[serde(default)]
    pub allow_local: Option<bool>,
    /// Explicit UIDs that may negotiate `plugins_admin` regardless
    /// of whether they match the steward's UID. Empty by default.
    #[serde(default)]
    pub allow_uids: Vec<u32>,
    /// Explicit GIDs that may negotiate `plugins_admin`. Empty by
    /// default.
    #[serde(default)]
    pub allow_gids: Vec<u32>,
}

/// Per-capability policy block for the operator-issued
/// reconciliation-admin verb (`reconcile_pair_now`). Mirrors
/// [`PluginsAdminPolicy`] in shape and semantics.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationAdminPolicy {
    /// When `true`, any local-socket peer whose UID matches the
    /// steward's UID is granted `reconciliation_admin`. Mirrors
    /// the "local admin" default.
    #[serde(default)]
    pub allow_local: Option<bool>,
    /// Explicit UIDs that may negotiate `reconciliation_admin`
    /// regardless of whether they match the steward's UID. Empty
    /// by default.
    #[serde(default)]
    pub allow_uids: Vec<u32>,
    /// Explicit GIDs that may negotiate `reconciliation_admin`.
    /// Empty by default.
    #[serde(default)]
    pub allow_gids: Vec<u32>,
}

/// Per-capability policy block for the Fast Path channel
/// (`fast_path_admin`). Mirrors [`PluginsAdminPolicy`] in shape
/// and semantics; gates which consumers may dispatch Fast Path
/// frames on the slow-path control connection.
///
/// Distributions that want remote Fast Path access (a separate-
/// host bridge, an MQTT gateway running as a different service
/// user) configure the ACL explicitly per `client_acl.toml`.
/// The default — same-UID local-socket peers only — mirrors the
/// other admin policies and refuses unknown peers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FastPathAdminPolicy {
    /// When `true`, any local-socket peer whose UID matches the
    /// steward's UID is granted `fast_path_admin`. Mirrors the
    /// "local admin" default.
    #[serde(default)]
    pub allow_local: Option<bool>,
    /// Explicit UIDs that may negotiate `fast_path_admin`
    /// regardless of whether they match the steward's UID. Empty
    /// by default.
    #[serde(default)]
    pub allow_uids: Vec<u32>,
    /// Explicit GIDs that may negotiate `fast_path_admin`. Empty
    /// by default.
    #[serde(default)]
    pub allow_gids: Vec<u32>,
}

/// Per-capability policy block for the consumer-side user-
/// interaction responder (`user_interaction_responder`).
/// Mirrors [`PluginsAdminPolicy`] in shape and semantics.
///
/// Multi-consumer setups (frontend + voice assistant + MQTT
/// bridge) all want to be the responder; only one can hold the
/// capability at a time per the framework's first-claimer-wins
/// rule. Operator decides precedence via the
/// `[capabilities.user_interaction_responder]` block: the
/// connection that negotiates the capability first AND is
/// permitted by this policy holds it; subsequent connections
/// receive `permission_denied / responder_already_assigned`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserInteractionResponderPolicy {
    /// When `true`, any local-socket peer whose UID matches the
    /// steward's UID is permitted to negotiate the capability.
    /// Default-derived consistently with the other admin
    /// policies: `Some(true)` when no UID/GID lists are
    /// configured, `Some(false)` once explicit lists are set.
    #[serde(default)]
    pub allow_local: Option<bool>,
    /// Explicit UIDs permitted to negotiate the capability.
    /// Empty by default.
    #[serde(default)]
    pub allow_uids: Vec<u32>,
    /// Explicit GIDs permitted to negotiate the capability.
    /// Empty by default.
    #[serde(default)]
    pub allow_gids: Vec<u32>,
}

/// Per-capability policy block for the operator-issued
/// appointment-management surface (`appointments_admin`).
/// Mirrors [`PluginsAdminPolicy`] in shape and semantics.
///
/// The capability gates the four operator wire ops
/// (`create_appointment` / `cancel_appointment` /
/// `list_appointments` / `project_appointment`). Plugins reach
/// the runtime in-process via the
/// [`evo_plugin_sdk::contract::AppointmentScheduler`] trait and
/// do not consult this policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppointmentsAdminPolicy {
    /// When `true`, any local-socket peer whose UID matches the
    /// steward's UID is granted `appointments_admin`. Default-
    /// derived consistently with the other admin policies:
    /// `Some(true)` when no UID/GID lists are configured,
    /// `Some(false)` once explicit lists are set.
    #[serde(default)]
    pub allow_local: Option<bool>,
    /// Explicit UIDs that may negotiate `appointments_admin`.
    #[serde(default)]
    pub allow_uids: Vec<u32>,
    /// Explicit GIDs that may negotiate `appointments_admin`.
    #[serde(default)]
    pub allow_gids: Vec<u32>,
}

/// Per-capability policy block for the operator-issued
/// watch-management surface (`watches_admin`). Mirrors
/// [`PluginsAdminPolicy`] in shape and semantics.
///
/// The capability gates the operator wire ops
/// (`create_watch` / `cancel_watch` / `list_watches` /
/// `project_watch`). Plugins reach the runtime in-process via
/// the [`evo_plugin_sdk::contract::WatchScheduler`] trait and
/// do not consult this policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchesAdminPolicy {
    /// When `true`, any local-socket peer whose UID matches the
    /// steward's UID is granted `watches_admin`. Default-
    /// derived consistently with the other admin policies.
    #[serde(default)]
    pub allow_local: Option<bool>,
    /// Explicit UIDs that may negotiate `watches_admin`.
    #[serde(default)]
    pub allow_uids: Vec<u32>,
    /// Explicit GIDs that may negotiate `watches_admin`.
    #[serde(default)]
    pub allow_gids: Vec<u32>,
}

/// Per-capability policy block for the operator-issued
/// subject-grammar migration surface (`grammar_admin`).
/// Mirrors [`PluginsAdminPolicy`] in shape and semantics.
///
/// Gates the operator wire ops `list_grammar_orphans` /
/// `accept_grammar_orphans` / `migrate_grammar_orphans`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrammarAdminPolicy {
    /// When `true`, any local-socket peer whose UID matches the
    /// steward's UID is granted `grammar_admin`.
    #[serde(default)]
    pub allow_local: Option<bool>,
    /// Explicit UIDs that may negotiate `grammar_admin`.
    #[serde(default)]
    pub allow_uids: Vec<u32>,
    /// Explicit GIDs that may negotiate `grammar_admin`.
    #[serde(default)]
    pub allow_gids: Vec<u32>,
}

/// Top-level structure parsed from `client_acl.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientAclFile {
    #[serde(default)]
    capabilities: ClientAclCapabilities,
}

/// `[capabilities]` section of the ACL file.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientAclCapabilities {
    #[serde(default)]
    resolve_claimants: ResolveClaimantsPolicy,
    #[serde(default)]
    plugins_admin: PluginsAdminPolicy,
    #[serde(default)]
    reconciliation_admin: ReconciliationAdminPolicy,
    #[serde(default)]
    fast_path_admin: FastPathAdminPolicy,
    #[serde(default)]
    user_interaction_responder: UserInteractionResponderPolicy,
    #[serde(default)]
    appointments_admin: AppointmentsAdminPolicy,
    #[serde(default)]
    watches_admin: WatchesAdminPolicy,
    #[serde(default)]
    grammar_admin: GrammarAdminPolicy,
}

/// Loaded, immutable client-API ACL.
///
/// Held as `Arc<ClientAcl>` and consulted by the server for every
/// `negotiate` request. Construct via [`ClientAcl::load`] (reads
/// the file at the default path or returns the default policy when
/// absent), [`ClientAcl::load_from`] (any path), or
/// [`ClientAcl::default`] (no file, default-deny posture).
#[derive(Debug, Clone, Default)]
pub struct ClientAcl {
    resolve_claimants: ResolveClaimantsPolicy,
    plugins_admin: PluginsAdminPolicy,
    reconciliation_admin: ReconciliationAdminPolicy,
    fast_path_admin: FastPathAdminPolicy,
    user_interaction_responder: UserInteractionResponderPolicy,
    appointments_admin: AppointmentsAdminPolicy,
    watches_admin: WatchesAdminPolicy,
    grammar_admin: GrammarAdminPolicy,
    /// Source path the ACL was loaded from, for diagnostics. `None`
    /// when constructed via [`ClientAcl::default`] or when the
    /// default-path file was absent and the defaults applied.
    source: Option<PathBuf>,
}

impl ClientAcl {
    /// Load the ACL from the default path.
    ///
    /// Missing file is not an error: the default-deny policy is
    /// returned. Malformed file is an error so the operator notices
    /// the typo at boot rather than discovering at runtime that
    /// every consumer is unexpectedly denied.
    pub fn load() -> Result<Self, StewardError> {
        Self::load_from(Path::new(DEFAULT_CLIENT_ACL_PATH))
    }

    /// Load the ACL from a specific path.
    ///
    /// Same semantics as [`ClientAcl::load`]: absent file yields the
    /// default policy; a present-but-malformed file yields a TOML
    /// parse error.
    pub fn load_from(path: &Path) -> Result<Self, StewardError> {
        match std::fs::read_to_string(path) {
            Ok(content) => Self::parse(&content, Some(path.to_path_buf())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(Self::default())
            }
            Err(e) => Err(StewardError::io(
                format!("reading client ACL {}", path.display()),
                e,
            )),
        }
    }

    /// Parse a TOML body. Intended for tests and for the loader
    /// helpers above.
    pub fn parse(
        input: &str,
        source: Option<PathBuf>,
    ) -> Result<Self, StewardError> {
        let file: ClientAclFile = toml::from_str(input).map_err(|e| {
            let ctx = source
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "inline client ACL".to_string());
            StewardError::toml(ctx, e)
        })?;
        Ok(Self {
            resolve_claimants: file.capabilities.resolve_claimants,
            plugins_admin: file.capabilities.plugins_admin,
            reconciliation_admin: file.capabilities.reconciliation_admin,
            fast_path_admin: file.capabilities.fast_path_admin,
            user_interaction_responder: file
                .capabilities
                .user_interaction_responder,
            appointments_admin: file.capabilities.appointments_admin,
            watches_admin: file.capabilities.watches_admin,
            grammar_admin: file.capabilities.grammar_admin,
            source,
        })
    }

    /// Source path the ACL was loaded from. `None` when the ACL was
    /// constructed by default or the default-path file was absent.
    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    /// Return the parsed `resolve_claimants` policy. Used in tests
    /// and diagnostics; the runtime gating goes through
    /// [`ClientAcl::allows_resolve_claimants`].
    pub fn resolve_claimants_policy(&self) -> &ResolveClaimantsPolicy {
        &self.resolve_claimants
    }

    /// Decide whether a connection from `peer` may negotiate the
    /// `resolve_claimants` capability.
    ///
    /// The check returns `true` on the first satisfied allowance:
    ///
    /// 1. If `allow_local` is `true` (or absent — see below) and the
    ///    peer's UID is known and equals the steward's own UID, the
    ///    connection is granted.
    /// 2. If the peer's UID is in `allow_uids`, granted.
    /// 3. If the peer's GID is in `allow_gids`, granted.
    /// 4. Otherwise, denied.
    ///
    /// `allow_local` defaults to `true` when neither it nor any of
    /// the explicit lists are set: that is the documented default
    /// posture (local admin gets the capability). Once the operator
    /// configures any allowance, `allow_local` defaults to `false`
    /// — explicit configuration is a deliberate signal that the
    /// operator wants to control which peers are granted.
    pub fn allows_resolve_claimants(
        &self,
        peer: PeerCredentials,
        steward: StewardIdentity,
    ) -> bool {
        let policy = &self.resolve_claimants;

        // Determine effective allow_local:
        //   - explicit setting wins,
        //   - else default to true iff neither uid nor gid lists are configured.
        let allow_local = policy.allow_local.unwrap_or(
            policy.allow_uids.is_empty() && policy.allow_gids.is_empty(),
        );

        if allow_local {
            if let (Some(peer_uid), Some(steward_uid)) = (peer.uid, steward.uid)
            {
                if peer_uid == steward_uid {
                    return true;
                }
            }
        }

        if let Some(peer_uid) = peer.uid {
            if policy.allow_uids.contains(&peer_uid) {
                return true;
            }
        }

        if let Some(peer_gid) = peer.gid {
            if policy.allow_gids.contains(&peer_gid) {
                return true;
            }
        }

        false
    }

    /// Return the parsed `plugins_admin` policy. Used in tests
    /// and diagnostics; the runtime gating goes through
    /// [`Self::allows_plugins_admin`].
    pub fn plugins_admin_policy(&self) -> &PluginsAdminPolicy {
        &self.plugins_admin
    }

    /// Return the parsed `reconciliation_admin` policy.
    pub fn reconciliation_admin_policy(&self) -> &ReconciliationAdminPolicy {
        &self.reconciliation_admin
    }

    /// Decide whether a connection from `peer` may negotiate the
    /// `reconciliation_admin` capability. Mirrors
    /// [`Self::allows_plugins_admin`] in shape and semantics.
    pub fn allows_reconciliation_admin(
        &self,
        peer: PeerCredentials,
        steward: StewardIdentity,
    ) -> bool {
        let policy = &self.reconciliation_admin;

        let allow_local = policy.allow_local.unwrap_or(
            policy.allow_uids.is_empty() && policy.allow_gids.is_empty(),
        );

        if allow_local {
            if let (Some(peer_uid), Some(steward_uid)) = (peer.uid, steward.uid)
            {
                if peer_uid == steward_uid {
                    return true;
                }
            }
        }

        if let Some(peer_uid) = peer.uid {
            if policy.allow_uids.contains(&peer_uid) {
                return true;
            }
        }

        if let Some(peer_gid) = peer.gid {
            if policy.allow_gids.contains(&peer_gid) {
                return true;
            }
        }

        false
    }

    /// Return the parsed `fast_path_admin` policy.
    pub fn fast_path_admin_policy(&self) -> &FastPathAdminPolicy {
        &self.fast_path_admin
    }

    /// Return the parsed `user_interaction_responder` policy.
    pub fn user_interaction_responder_policy(
        &self,
    ) -> &UserInteractionResponderPolicy {
        &self.user_interaction_responder
    }

    /// Decide whether a connection from `peer` is permitted to
    /// negotiate the `user_interaction_responder` capability.
    /// Mirrors [`Self::allows_plugins_admin`] in shape and
    /// semantics. Note that ACL permission is the *first* gate;
    /// even a permitted peer is refused when another connection
    /// already holds the capability (the steward maintains a
    /// single-responder lock at runtime; first-claimer-wins).
    pub fn allows_user_interaction_responder(
        &self,
        peer: PeerCredentials,
        steward: StewardIdentity,
    ) -> bool {
        let policy = &self.user_interaction_responder;

        let allow_local = policy.allow_local.unwrap_or(
            policy.allow_uids.is_empty() && policy.allow_gids.is_empty(),
        );

        if allow_local {
            if let (Some(peer_uid), Some(steward_uid)) = (peer.uid, steward.uid)
            {
                if peer_uid == steward_uid {
                    return true;
                }
            }
        }

        if let Some(peer_uid) = peer.uid {
            if policy.allow_uids.contains(&peer_uid) {
                return true;
            }
        }

        if let Some(peer_gid) = peer.gid {
            if policy.allow_gids.contains(&peer_gid) {
                return true;
            }
        }

        false
    }

    /// Decide whether a connection from `peer` may negotiate the
    /// `fast_path_admin` capability. Mirrors
    /// [`Self::allows_plugins_admin`] in shape and semantics.
    pub fn allows_fast_path_admin(
        &self,
        peer: PeerCredentials,
        steward: StewardIdentity,
    ) -> bool {
        let policy = &self.fast_path_admin;

        let allow_local = policy.allow_local.unwrap_or(
            policy.allow_uids.is_empty() && policy.allow_gids.is_empty(),
        );

        if allow_local {
            if let (Some(peer_uid), Some(steward_uid)) = (peer.uid, steward.uid)
            {
                if peer_uid == steward_uid {
                    return true;
                }
            }
        }

        if let Some(peer_uid) = peer.uid {
            if policy.allow_uids.contains(&peer_uid) {
                return true;
            }
        }

        if let Some(peer_gid) = peer.gid {
            if policy.allow_gids.contains(&peer_gid) {
                return true;
            }
        }

        false
    }

    /// Return the parsed `appointments_admin` policy.
    pub fn appointments_admin_policy(&self) -> &AppointmentsAdminPolicy {
        &self.appointments_admin
    }

    /// Decide whether a connection from `peer` may negotiate the
    /// `appointments_admin` capability. Mirrors
    /// [`Self::allows_plugins_admin`] in shape and semantics.
    pub fn allows_appointments_admin(
        &self,
        peer: PeerCredentials,
        steward: StewardIdentity,
    ) -> bool {
        let policy = &self.appointments_admin;

        let allow_local = policy.allow_local.unwrap_or(
            policy.allow_uids.is_empty() && policy.allow_gids.is_empty(),
        );

        if allow_local {
            if let (Some(peer_uid), Some(steward_uid)) = (peer.uid, steward.uid)
            {
                if peer_uid == steward_uid {
                    return true;
                }
            }
        }

        if let Some(peer_uid) = peer.uid {
            if policy.allow_uids.contains(&peer_uid) {
                return true;
            }
        }

        if let Some(peer_gid) = peer.gid {
            if policy.allow_gids.contains(&peer_gid) {
                return true;
            }
        }

        false
    }

    /// Return the parsed `watches_admin` policy.
    pub fn watches_admin_policy(&self) -> &WatchesAdminPolicy {
        &self.watches_admin
    }

    /// Decide whether a connection from `peer` may negotiate
    /// the `watches_admin` capability. Mirrors
    /// [`Self::allows_appointments_admin`] in shape and
    /// semantics.
    pub fn allows_watches_admin(
        &self,
        peer: PeerCredentials,
        steward: StewardIdentity,
    ) -> bool {
        let policy = &self.watches_admin;

        let allow_local = policy.allow_local.unwrap_or(
            policy.allow_uids.is_empty() && policy.allow_gids.is_empty(),
        );

        if allow_local {
            if let (Some(peer_uid), Some(steward_uid)) = (peer.uid, steward.uid)
            {
                if peer_uid == steward_uid {
                    return true;
                }
            }
        }

        if let Some(peer_uid) = peer.uid {
            if policy.allow_uids.contains(&peer_uid) {
                return true;
            }
        }

        if let Some(peer_gid) = peer.gid {
            if policy.allow_gids.contains(&peer_gid) {
                return true;
            }
        }

        false
    }

    /// Return the parsed `grammar_admin` policy.
    pub fn grammar_admin_policy(&self) -> &GrammarAdminPolicy {
        &self.grammar_admin
    }

    /// Decide whether a connection from `peer` may negotiate the
    /// `grammar_admin` capability. Mirrors
    /// [`Self::allows_appointments_admin`].
    pub fn allows_grammar_admin(
        &self,
        peer: PeerCredentials,
        steward: StewardIdentity,
    ) -> bool {
        let policy = &self.grammar_admin;

        let allow_local = policy.allow_local.unwrap_or(
            policy.allow_uids.is_empty() && policy.allow_gids.is_empty(),
        );

        if allow_local {
            if let (Some(peer_uid), Some(steward_uid)) = (peer.uid, steward.uid)
            {
                if peer_uid == steward_uid {
                    return true;
                }
            }
        }

        if let Some(peer_uid) = peer.uid {
            if policy.allow_uids.contains(&peer_uid) {
                return true;
            }
        }

        if let Some(peer_gid) = peer.gid {
            if policy.allow_gids.contains(&peer_gid) {
                return true;
            }
        }

        false
    }

    /// Decide whether a connection from `peer` may negotiate the
    /// `plugins_admin` capability. Mirrors
    /// [`Self::allows_resolve_claimants`] in shape: allow-local
    /// (default-true when neither UID nor GID list is configured),
    /// allow-UID list, allow-GID list, otherwise deny.
    pub fn allows_plugins_admin(
        &self,
        peer: PeerCredentials,
        steward: StewardIdentity,
    ) -> bool {
        let policy = &self.plugins_admin;

        let allow_local = policy.allow_local.unwrap_or(
            policy.allow_uids.is_empty() && policy.allow_gids.is_empty(),
        );

        if allow_local {
            if let (Some(peer_uid), Some(steward_uid)) = (peer.uid, steward.uid)
            {
                if peer_uid == steward_uid {
                    return true;
                }
            }
        }

        if let Some(peer_uid) = peer.uid {
            if policy.allow_uids.contains(&peer_uid) {
                return true;
            }
        }

        if let Some(peer_gid) = peer.gid {
            if policy.allow_gids.contains(&peer_gid) {
                return true;
            }
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steward_at(uid: u32) -> StewardIdentity {
        StewardIdentity {
            uid: Some(uid),
            gid: Some(uid),
        }
    }

    fn peer(uid: u32, gid: u32) -> PeerCredentials {
        PeerCredentials {
            uid: Some(uid),
            gid: Some(gid),
        }
    }

    #[test]
    fn default_acl_grants_local_steward_uid_only() {
        let acl = ClientAcl::default();
        let steward = steward_at(1000);
        assert!(
            acl.allows_resolve_claimants(peer(1000, 1000), steward),
            "matching steward UID is the default local-admin grant"
        );
        assert!(
            !acl.allows_resolve_claimants(peer(2000, 1000), steward),
            "non-matching UID is denied by the conservative default"
        );
    }

    #[test]
    fn unset_allow_local_defaults_to_true_when_no_lists() {
        let acl = ClientAcl::parse("[capabilities.resolve_claimants]\n", None)
            .expect("parse empty table");
        let steward = steward_at(0);
        assert!(acl.allows_resolve_claimants(peer(0, 0), steward));
        assert!(!acl.allows_resolve_claimants(peer(1, 0), steward));
    }

    #[test]
    fn explicit_uid_list_grants_listed_uids() {
        let acl = ClientAcl::parse(
            r#"
            [capabilities.resolve_claimants]
            allow_uids = [1234]
            "#,
            None,
        )
        .expect("parse uid list");
        let steward = steward_at(0);
        // Steward UID is no longer the default-local grant since the
        // operator configured an explicit allow_uids list.
        assert!(!acl.allows_resolve_claimants(peer(0, 0), steward));
        // Listed UID is granted regardless.
        assert!(acl.allows_resolve_claimants(peer(1234, 0), steward));
        // Other UIDs remain denied.
        assert!(!acl.allows_resolve_claimants(peer(5678, 0), steward));
    }

    #[test]
    fn explicit_gid_list_grants_listed_gids() {
        let acl = ClientAcl::parse(
            r#"
            [capabilities.resolve_claimants]
            allow_gids = [27]
            "#,
            None,
        )
        .expect("parse gid list");
        let steward = steward_at(0);
        // Steward UID's default-local grant is suppressed by the
        // explicit list; only the listed GID matches.
        assert!(!acl.allows_resolve_claimants(peer(0, 0), steward));
        assert!(acl.allows_resolve_claimants(peer(123, 27), steward));
    }

    #[test]
    fn allow_local_true_combines_with_uid_list() {
        let acl = ClientAcl::parse(
            r#"
            [capabilities.resolve_claimants]
            allow_local = true
            allow_uids = [4242]
            "#,
            None,
        )
        .expect("parse mixed");
        let steward = steward_at(1000);
        // Both forms granted independently.
        assert!(acl.allows_resolve_claimants(peer(1000, 0), steward));
        assert!(acl.allows_resolve_claimants(peer(4242, 0), steward));
        // Neither matches: denied.
        assert!(!acl.allows_resolve_claimants(peer(9999, 0), steward));
    }

    #[test]
    fn allow_local_false_revokes_default_grant() {
        let acl = ClientAcl::parse(
            r#"
            [capabilities.resolve_claimants]
            allow_local = false
            "#,
            None,
        )
        .expect("parse local off");
        let steward = steward_at(1000);
        // Even matching UID is denied because the operator turned
        // off the local grant.
        assert!(!acl.allows_resolve_claimants(peer(1000, 0), steward));
    }

    #[test]
    fn missing_peer_uid_denies_local_grant() {
        let acl = ClientAcl::default();
        let steward = steward_at(1000);
        // Without a peer UID, the local grant cannot match the
        // steward UID; the conservative outcome is deny.
        assert!(!acl.allows_resolve_claimants(
            PeerCredentials {
                uid: None,
                gid: None
            },
            steward
        ));
    }

    #[test]
    fn missing_steward_uid_denies_local_grant() {
        let acl = ClientAcl::default();
        let steward = StewardIdentity {
            uid: None,
            gid: None,
        };
        // Same logic in the other direction: without a steward
        // identity to match, no peer can satisfy the local grant.
        assert!(!acl.allows_resolve_claimants(peer(0, 0), steward));
    }

    #[test]
    fn malformed_toml_is_error() {
        let err = ClientAcl::parse("not = [valid", None);
        assert!(
            err.is_err(),
            "malformed input must surface as a parse error"
        );
    }

    #[test]
    fn typo_in_capability_field_is_rejected_at_parse_time() {
        // A common operator typo: `allow_uds` instead of
        // `allow_uids`. With `deny_unknown_fields` on the policy
        // struct, the loader refuses the file and names the unknown
        // field. Without the deny, the typo would parse cleanly,
        // the policy would default-deny, and the operator would
        // discover the silent denial only at runtime.
        let err = ClientAcl::parse(
            r#"
            [capabilities.resolve_claimants]
            allow_uds = [0]
            "#,
            None,
        );
        let msg = format!("{:?}", err.expect_err("typo must surface"));
        assert!(
            msg.contains("allow_uds"),
            "error must name the unknown field; got {msg}"
        );
    }

    #[test]
    fn unknown_capability_section_is_rejected_at_parse_time() {
        // A typo at the [capabilities] level (e.g.
        // `[capabilities.resolve_claims]` instead of
        // `[capabilities.resolve_claimants]`) must also fail loudly.
        let err = ClientAcl::parse(
            r#"
            [capabilities.resolve_claims]
            allow_local = true
            "#,
            None,
        );
        let msg = format!("{:?}", err.expect_err("typo must surface"));
        assert!(
            msg.contains("resolve_claims"),
            "error must name the unknown section; got {msg}"
        );
    }

    #[test]
    fn missing_default_file_yields_default_acl() {
        // A path that does not exist; load_from must return the
        // default policy without erroring.
        let nonexistent =
            Path::new("/tmp/this/path/should/not/exist/client_acl.toml");
        let acl = ClientAcl::load_from(nonexistent).expect("absent file is ok");
        assert!(acl.source().is_none());
        // Default behaviour preserved.
        let steward = steward_at(0);
        assert!(acl.allows_resolve_claimants(peer(0, 0), steward));
    }

    #[test]
    fn load_from_existing_file_records_source_path() {
        let tmp = tempfile::tempdir().expect("tmp dir");
        let path = tmp.path().join("client_acl.toml");
        std::fs::write(
            &path,
            "[capabilities.resolve_claimants]\nallow_uids = [42]\n",
        )
        .expect("write fixture");
        let acl = ClientAcl::load_from(&path).expect("load fixture");
        assert_eq!(acl.source(), Some(path.as_path()));
        let steward = steward_at(0);
        assert!(acl.allows_resolve_claimants(peer(42, 0), steward));
    }

    #[test]
    fn default_acl_grants_plugins_admin_to_local_steward_uid() {
        let acl = ClientAcl::default();
        let steward = steward_at(1000);
        assert!(
            acl.allows_plugins_admin(peer(1000, 1000), steward),
            "matching steward UID is the default local-admin grant"
        );
        assert!(
            !acl.allows_plugins_admin(peer(2000, 1000), steward),
            "non-matching UID is denied by the conservative default"
        );
    }

    #[test]
    fn plugins_admin_explicit_uid_list_overrides_local_default() {
        let acl = ClientAcl::parse(
            r#"
            [capabilities.plugins_admin]
            allow_uids = [4242]
            "#,
            None,
        )
        .expect("parse plugins_admin uid list");
        let steward = steward_at(1000);
        // The explicit list grants only the listed UID; local
        // defaults off once a list is configured.
        assert!(acl.allows_plugins_admin(peer(4242, 0), steward));
        assert!(!acl.allows_plugins_admin(peer(1000, 0), steward));
    }

    #[test]
    fn plugins_admin_and_resolve_claimants_are_independent() {
        // Granting one capability does not implicitly grant the
        // other; each capability gates on its own block.
        let acl = ClientAcl::parse(
            r#"
            [capabilities.plugins_admin]
            allow_uids = [4242]
            [capabilities.resolve_claimants]
            allow_uids = [9999]
            "#,
            None,
        )
        .expect("parse both blocks");
        let steward = steward_at(0);
        assert!(acl.allows_plugins_admin(peer(4242, 0), steward));
        assert!(!acl.allows_plugins_admin(peer(9999, 0), steward));
        assert!(acl.allows_resolve_claimants(peer(9999, 0), steward));
        assert!(!acl.allows_resolve_claimants(peer(4242, 0), steward));
    }

    #[test]
    fn default_acl_grants_reconciliation_admin_to_local_steward_uid() {
        let acl = ClientAcl::default();
        let steward = steward_at(1000);
        assert!(
            acl.allows_reconciliation_admin(peer(1000, 1000), steward),
            "matching steward UID is the default local-admin grant"
        );
        assert!(
            !acl.allows_reconciliation_admin(peer(2000, 1000), steward),
            "non-matching UID is denied by the conservative default"
        );
    }

    #[test]
    fn reconciliation_admin_explicit_uid_list_overrides_local_default() {
        let acl = ClientAcl::parse(
            r#"
            [capabilities.reconciliation_admin]
            allow_uids = [4242]
            "#,
            None,
        )
        .expect("parse reconciliation_admin uid list");
        let steward = steward_at(1000);
        // The explicit list grants only the listed UID; local
        // defaults off once a list is configured.
        assert!(acl.allows_reconciliation_admin(peer(4242, 0), steward));
        assert!(!acl.allows_reconciliation_admin(peer(1000, 0), steward));
    }

    #[test]
    fn reconciliation_admin_and_plugins_admin_are_independent() {
        // The whole point of splitting reconciliation_admin out as
        // a separate capability is that operators can grant manual-
        // trigger authority on a different axis from plugin-
        // lifecycle authority — e.g. a CI/CD bridge that may force
        // a re-apply but should not be able to disable plugins.
        // Pin the independence: granting one MUST NOT implicitly
        // grant the other.
        let acl = ClientAcl::parse(
            r#"
            [capabilities.plugins_admin]
            allow_uids = [4242]
            [capabilities.reconciliation_admin]
            allow_uids = [9999]
            "#,
            None,
        )
        .expect("parse both admin blocks");
        let steward = steward_at(0);
        assert!(acl.allows_plugins_admin(peer(4242, 0), steward));
        assert!(!acl.allows_plugins_admin(peer(9999, 0), steward));
        assert!(acl.allows_reconciliation_admin(peer(9999, 0), steward));
        assert!(!acl.allows_reconciliation_admin(peer(4242, 0), steward));
    }

    #[test]
    fn reconciliation_admin_unknown_peer_credentials_are_denied() {
        // When peer_credentials() returns uid=None / gid=None
        // (e.g. a non-Linux Unix that doesn't expose SO_PEERCRED at
        // the same option, or a sandbox that strips the credential),
        // the ACL MUST refuse a capability that gates on identity.
        // A grant would let an unidentified peer escalate.
        let acl = ClientAcl::default();
        let steward = steward_at(1000);
        let unidentified = PeerCredentials {
            uid: None,
            gid: None,
        };
        assert!(!acl.allows_reconciliation_admin(unidentified, steward));
    }

    #[test]
    fn default_acl_grants_fast_path_admin_to_local_steward_uid() {
        let acl = ClientAcl::default();
        let steward = steward_at(1000);
        assert!(
            acl.allows_fast_path_admin(peer(1000, 1000), steward),
            "matching steward UID is the default local-admin grant"
        );
        assert!(
            !acl.allows_fast_path_admin(peer(2000, 1000), steward),
            "non-matching UID is denied by the conservative default"
        );
    }

    #[test]
    fn fast_path_admin_explicit_uid_list_overrides_local_default() {
        let acl = ClientAcl::parse(
            r#"
            [capabilities.fast_path_admin]
            allow_uids = [4242]
            "#,
            None,
        )
        .expect("parse fast_path_admin uid list");
        let steward = steward_at(1000);
        // Explicit list grants only the listed UID; local
        // defaults off once a list is configured.
        assert!(acl.allows_fast_path_admin(peer(4242, 0), steward));
        assert!(!acl.allows_fast_path_admin(peer(1000, 0), steward));
    }

    #[test]
    fn fast_path_admin_and_reconciliation_admin_are_independent() {
        // Fast Path authority and reconciliation-trigger authority
        // are different axes: a CI/CD bridge that may force-
        // reapply a reconciliation pair should not automatically
        // also be allowed to dispatch tactile-latency Fast Path
        // frames into operator-physical-button verbs. Pin the
        // independence.
        let acl = ClientAcl::parse(
            r#"
            [capabilities.reconciliation_admin]
            allow_uids = [4242]
            [capabilities.fast_path_admin]
            allow_uids = [9999]
            "#,
            None,
        )
        .expect("parse both admin blocks");
        let steward = steward_at(0);
        assert!(acl.allows_reconciliation_admin(peer(4242, 0), steward));
        assert!(!acl.allows_reconciliation_admin(peer(9999, 0), steward));
        assert!(acl.allows_fast_path_admin(peer(9999, 0), steward));
        assert!(!acl.allows_fast_path_admin(peer(4242, 0), steward));
    }

    #[test]
    fn fast_path_admin_unknown_peer_credentials_are_denied() {
        let acl = ClientAcl::default();
        let steward = steward_at(1000);
        let unidentified = PeerCredentials {
            uid: None,
            gid: None,
        };
        assert!(!acl.allows_fast_path_admin(unidentified, steward));
    }

    #[test]
    fn default_acl_grants_user_interaction_responder_to_local_steward_uid() {
        let acl = ClientAcl::default();
        let steward = steward_at(1000);
        assert!(
            acl.allows_user_interaction_responder(peer(1000, 1000), steward),
            "matching steward UID is the default local-admin grant"
        );
        assert!(
            !acl.allows_user_interaction_responder(peer(2000, 1000), steward),
            "non-matching UID is denied by the conservative default"
        );
    }

    #[test]
    fn user_interaction_responder_explicit_uid_list_overrides_local_default() {
        let acl = ClientAcl::parse(
            r#"
            [capabilities.user_interaction_responder]
            allow_uids = [4242]
            "#,
            None,
        )
        .expect("parse user_interaction_responder uid list");
        let steward = steward_at(1000);
        assert!(acl.allows_user_interaction_responder(peer(4242, 0), steward));
        assert!(!acl.allows_user_interaction_responder(peer(1000, 0), steward));
    }

    #[test]
    fn user_interaction_responder_unknown_peer_credentials_are_denied() {
        let acl = ClientAcl::default();
        let steward = steward_at(1000);
        let unidentified = PeerCredentials {
            uid: None,
            gid: None,
        };
        assert!(!acl.allows_user_interaction_responder(unidentified, steward));
    }

    #[test]
    fn default_acl_grants_appointments_admin_to_local_steward_uid() {
        let acl = ClientAcl::default();
        let steward = steward_at(1000);
        assert!(
            acl.allows_appointments_admin(peer(1000, 1000), steward),
            "matching steward UID is the default local-admin grant"
        );
        assert!(
            !acl.allows_appointments_admin(peer(2000, 1000), steward),
            "non-matching UID is denied under defaults"
        );
    }

    #[test]
    fn appointments_admin_uid_list_overrides_local_default() {
        let acl = ClientAcl::parse(
            r#"
            [capabilities.appointments_admin]
            allow_uids = [4242]
            "#,
            None,
        )
        .expect("parse appointments_admin block");
        let steward = steward_at(1000);
        assert!(acl.allows_appointments_admin(peer(4242, 0), steward));
        assert!(
            !acl.allows_appointments_admin(peer(1000, 0), steward),
            "explicit list flips allow_local default to false"
        );
    }

    #[test]
    fn appointments_admin_independent_from_other_admin_caps() {
        // Granting one capability does not implicitly grant the
        // appointments-admin axis. Operators configure each axis
        // independently.
        let acl = ClientAcl::parse(
            r#"
            [capabilities.plugins_admin]
            allow_uids = [4242]
            [capabilities.appointments_admin]
            allow_uids = [9999]
            "#,
            None,
        )
        .expect("parse both blocks");
        let steward = steward_at(0);
        assert!(acl.allows_plugins_admin(peer(4242, 0), steward));
        assert!(!acl.allows_appointments_admin(peer(4242, 0), steward));
        assert!(acl.allows_appointments_admin(peer(9999, 0), steward));
        assert!(!acl.allows_plugins_admin(peer(9999, 0), steward));
    }

    #[test]
    fn default_acl_grants_watches_admin_to_local_steward_uid() {
        let acl = ClientAcl::default();
        let steward = steward_at(1000);
        assert!(
            acl.allows_watches_admin(peer(1000, 1000), steward),
            "matching steward UID is the default local-admin grant"
        );
        assert!(
            !acl.allows_watches_admin(peer(2000, 1000), steward),
            "non-matching UID is denied under defaults"
        );
    }

    #[test]
    fn watches_admin_uid_list_overrides_local_default() {
        let acl = ClientAcl::parse(
            r#"
            [capabilities.watches_admin]
            allow_uids = [4242]
            "#,
            None,
        )
        .expect("parse watches_admin block");
        let steward = steward_at(1000);
        assert!(acl.allows_watches_admin(peer(4242, 0), steward));
        assert!(
            !acl.allows_watches_admin(peer(1000, 0), steward),
            "explicit list flips allow_local default to false"
        );
    }

    #[test]
    fn watches_admin_independent_from_other_admin_caps() {
        let acl = ClientAcl::parse(
            r#"
            [capabilities.appointments_admin]
            allow_uids = [4242]
            [capabilities.watches_admin]
            allow_uids = [9999]
            "#,
            None,
        )
        .expect("parse both blocks");
        let steward = steward_at(0);
        assert!(acl.allows_appointments_admin(peer(4242, 0), steward));
        assert!(!acl.allows_watches_admin(peer(4242, 0), steward));
        assert!(acl.allows_watches_admin(peer(9999, 0), steward));
        assert!(!acl.allows_appointments_admin(peer(9999, 0), steward));
    }

    #[test]
    fn default_acl_grants_grammar_admin_to_local_steward_uid() {
        let acl = ClientAcl::default();
        let steward = steward_at(1000);
        assert!(
            acl.allows_grammar_admin(peer(1000, 1000), steward),
            "matching steward UID is the default local-admin grant"
        );
        assert!(!acl.allows_grammar_admin(peer(2000, 1000), steward));
    }

    #[test]
    fn grammar_admin_uid_list_overrides_local_default() {
        let acl = ClientAcl::parse(
            r#"
            [capabilities.grammar_admin]
            allow_uids = [4242]
            "#,
            None,
        )
        .expect("parse grammar_admin block");
        let steward = steward_at(1000);
        assert!(acl.allows_grammar_admin(peer(4242, 0), steward));
        assert!(!acl.allows_grammar_admin(peer(1000, 0), steward));
    }

    #[test]
    fn grammar_admin_independent_from_other_admin_caps() {
        let acl = ClientAcl::parse(
            r#"
            [capabilities.appointments_admin]
            allow_uids = [4242]
            [capabilities.grammar_admin]
            allow_uids = [9999]
            "#,
            None,
        )
        .expect("parse both blocks");
        let steward = steward_at(0);
        assert!(acl.allows_appointments_admin(peer(4242, 0), steward));
        assert!(!acl.allows_grammar_admin(peer(4242, 0), steward));
        assert!(acl.allows_grammar_admin(peer(9999, 0), steward));
        assert!(!acl.allows_appointments_admin(peer(9999, 0), steward));
    }

    #[test]
    fn user_interaction_responder_independent_from_other_admin_caps() {
        // Granting one of the admin capabilities MUST NOT
        // implicitly grant the responder capability. Operators
        // configure each axis independently.
        let acl = ClientAcl::parse(
            r#"
            [capabilities.plugins_admin]
            allow_uids = [4242]
            [capabilities.user_interaction_responder]
            allow_uids = [9999]
            "#,
            None,
        )
        .expect("parse both blocks");
        let steward = steward_at(0);
        assert!(acl.allows_plugins_admin(peer(4242, 0), steward));
        assert!(!acl.allows_plugins_admin(peer(9999, 0), steward));
        assert!(acl.allows_user_interaction_responder(peer(9999, 0), steward));
        assert!(!acl.allows_user_interaction_responder(peer(4242, 0), steward));
    }
}
