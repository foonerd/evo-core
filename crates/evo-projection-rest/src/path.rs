// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! URL path derivation for the REST projection.
//!
//! Paths are derived directly from the wire op id under the
//! versioned `/api/v1/*` prefix. The 1:1 mapping is the simplest
//! and most operator-debuggable shape: the REST endpoint name
//! equals the wire op name on every observability surface.

use evo_projection_core::WireOpId;

/// The version prefix the REST projection mounts under.
///
/// All endpoints live below `/api/v1/`. Future version
/// extensions land as parallel mounts (`/api/v2/`) rather than
/// in-place mutations of the v1 paths.
pub const REST_VERSION_PREFIX: &str = "/api/v1";

/// Derive the REST URL path for one wire op.
///
/// The returned path is the wire op id appended to the version
/// prefix, e.g. `set_update_channel` → `/api/v1/set_update_channel`.
/// The path string is safe to use as-is in router patterns: the
/// wire op id is snake_case ASCII per [`WireOpId`]'s validator,
/// so no percent-encoding is required.
pub fn rest_path_for(id: &WireOpId) -> String {
    format!("{}/{}", REST_VERSION_PREFIX, id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_starts_with_version_prefix() {
        let id = WireOpId::new("describe_capabilities").unwrap();
        let p = rest_path_for(&id);
        assert!(p.starts_with(REST_VERSION_PREFIX));
    }

    #[test]
    fn path_appends_op_id_under_prefix() {
        let id = WireOpId::new("describe_capabilities").unwrap();
        assert_eq!(rest_path_for(&id), "/api/v1/describe_capabilities");
    }

    #[test]
    fn path_for_short_op_id() {
        let id = WireOpId::new("request").unwrap();
        assert_eq!(rest_path_for(&id), "/api/v1/request");
    }

    #[test]
    fn path_for_step_up_op_id() {
        let id = WireOpId::new("set_update_channel").unwrap();
        assert_eq!(rest_path_for(&id), "/api/v1/set_update_channel");
    }

    #[test]
    fn path_for_audit_op_id() {
        let id = WireOpId::new("step_up_auth_verify").unwrap();
        assert_eq!(rest_path_for(&id), "/api/v1/step_up_auth_verify");
    }

    #[test]
    fn version_prefix_is_api_v1() {
        assert_eq!(REST_VERSION_PREFIX, "/api/v1");
    }
}
