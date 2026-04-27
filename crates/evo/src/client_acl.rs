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
                Err(_) => Self {
                    uid: None,
                    gid: None,
                },
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
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

/// Top-level structure parsed from `client_acl.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
struct ClientAclFile {
    #[serde(default)]
    capabilities: ClientAclCapabilities,
}

/// `[capabilities]` section of the ACL file.
#[derive(Debug, Clone, Default, Deserialize)]
struct ClientAclCapabilities {
    #[serde(default)]
    resolve_claimants: ResolveClaimantsPolicy,
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
}
