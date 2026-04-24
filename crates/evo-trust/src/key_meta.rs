//! `*.meta.toml` sidecar for a trust `.pem` public key.

use evo_plugin_sdk::manifest::TrustClass;
use serde::Deserialize;

/// TOML shape in `<stem>.meta.toml` beside `<stem>.pem`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct KeyMeta {
    /// Optional table; may be empty.
    #[serde(default)]
    pub key: KeySection,
    /// Authorises name prefixes and maximum trust.
    pub authorisation: Authorisation,
}

/// Optional `[key]` table. All fields are currently advisory and retained
/// for future tooling (e.g. `evo-plugin-tool verify`); none are consulted
/// during admission in this version.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub struct KeySection {
    #[allow(dead_code)]
    #[serde(default)]
    fingerprint: Option<String>,
    /// Must be "plugin-signing" when present; ignored in v0.
    #[allow(dead_code)]
    #[serde(default)]
    purpose: Option<String>,
    /// Human-readable provenance; ignored in v0.
    #[allow(dead_code)]
    #[serde(default)]
    issued_by: Option<String>,
}

/// Per `PLUGIN_PACKAGING.md` section 5.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Authorisation {
    /// e.g. `org.evo.*`, `com.vendor.*` — `*` is a single-segment or suffix wildcard
    /// implemented as: pattern after splitting on `.` and `*`.
    pub name_prefixes: Vec<String>,
    /// Highest (strongest) trust class this key is allowed to sign for. Lower
    /// (weaker) classes in the `TrustClass` `Ord` chain are also allowed.
    pub max_trust_class: TrustClass,
}
