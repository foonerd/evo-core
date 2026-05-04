//! Target descriptor — declares an acceptance target's identity,
//! connection model, and hardware capability surface.
//!
//! A target descriptor is a TOML file. The committed file carries
//! the public-shape capabilities; machine-specific connection
//! details (host, user, ssh key) live in a sibling
//! `<name>.local.toml` overlay that is gitignored and never
//! committed.
//!
//! Committed shape (`pi5-prototype.toml`):
//!
//! ```toml
//! name = "pi5-prototype"
//! arch = "aarch64-unknown-linux-gnu"
//! connection_type = "ssh"
//!
//! [ssh]
//! host = "REPLACE_ME"
//! user = "REPLACE_ME"
//! key_path = "REPLACE_ME"
//! port = 22
//!
//! [[capabilities]]
//! name = "audio_jack"
//! ```
//!
//! Local overlay (`pi5-prototype.local.toml`, gitignored):
//!
//! ```toml
//! [ssh]
//! host = "10.0.0.42"
//! user = "lab-user"
//! key_path = "~/.ssh/id_ed25519"
//! ```
//!
//! Loader behaviour: parse the base file, then if a sibling
//! `<stem>.local.toml` exists, parse it as a partial overlay and
//! apply field-by-field on top of the base. After merge the loader
//! refuses any descriptor that still carries the literal sentinel
//! `REPLACE_ME` in connection-bearing fields, so a missing or
//! incomplete overlay surfaces as a clear startup error rather than
//! silently passing the sentinel down to the SSH connector.
//!
//! Scenarios that declare `requires_capability = "<name>"` skip when
//! the named capability is absent from the target. Capability names
//! are open k/v; the harness does not enforce a closed enum.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{HarnessError, Result};

/// Sentinel that committed target descriptors carry in machine-specific
/// fields. The loader rejects any descriptor that still carries this
/// sentinel after the local overlay is merged.
const PLACEHOLDER_SENTINEL: &str = "REPLACE_ME";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetDescriptor {
    pub name: String,
    pub arch: String,
    pub connection_type: ConnectionType,
    #[serde(default)]
    pub ssh: Option<SshConfig>,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionType {
    Ssh,
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshConfig {
    pub host: String,
    pub user: String,
    #[serde(default)]
    pub key_path: Option<String>,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
}

fn default_ssh_port() -> u16 {
    22
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    pub name: String,
    #[serde(default)]
    pub note: Option<String>,
}

/// Partial overlay applied on top of a parsed `TargetDescriptor`.
/// Every field is optional; only the fields present in the overlay
/// file replace the base values. Capabilities are intentionally not
/// overlay-able — the capability surface is the public shape of the
/// target and lives in the committed file.
#[derive(Debug, Clone, Default, Deserialize)]
struct TargetOverlay {
    #[serde(default)]
    ssh: Option<SshOverlay>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SshOverlay {
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    key_path: Option<String>,
    #[serde(default)]
    port: Option<u16>,
}

impl TargetDescriptor {
    /// Parse a target descriptor from `path`. If a sibling
    /// `<stem>.local.toml` exists, parse it as a partial overlay
    /// and merge its fields on top of the base. The merged
    /// descriptor is then validated for the placeholder sentinel
    /// in connection-bearing fields.
    pub fn from_file(path: &Path) -> Result<Self> {
        let bytes = std::fs::read_to_string(path).map_err(|source| {
            HarnessError::DescriptorRead {
                path: path.to_path_buf(),
                source,
            }
        })?;
        let mut descriptor: TargetDescriptor =
            toml::from_str(&bytes).map_err(|source| {
                HarnessError::DescriptorParse {
                    path: path.to_path_buf(),
                    source,
                }
            })?;

        if let Some(overlay_path) = local_overlay_path(path) {
            if overlay_path.exists() {
                let overlay_bytes = std::fs::read_to_string(&overlay_path)
                    .map_err(|source| HarnessError::DescriptorRead {
                        path: overlay_path.clone(),
                        source,
                    })?;
                let overlay: TargetOverlay = toml::from_str(&overlay_bytes)
                    .map_err(|source| HarnessError::DescriptorParse {
                        path: overlay_path.clone(),
                        source,
                    })?;
                apply_overlay(&mut descriptor, overlay);
            }
        }

        validate_no_placeholders(&descriptor, path)?;
        Ok(descriptor)
    }

    pub fn has_capability(&self, name: &str) -> bool {
        self.capabilities.iter().any(|c| c.name == name)
    }

    pub fn ssh(&self) -> Result<&SshConfig> {
        match (&self.connection_type, &self.ssh) {
            (ConnectionType::Ssh, Some(ssh)) => Ok(ssh),
            (ConnectionType::Ssh, None) => Err(HarnessError::ConnectionFailed {
                detail: format!("target {:?} declares connection_type=ssh but has no [ssh] block", self.name),
            }),
            (ConnectionType::Local, _) => Err(HarnessError::ConnectionFailed {
                detail: format!("target {:?} is connection_type=local; ssh access requested", self.name),
            }),
        }
    }
}

fn local_overlay_path(base: &Path) -> Option<PathBuf> {
    let stem = base.file_stem()?.to_str()?;
    let parent = base.parent()?;
    Some(parent.join(format!("{stem}.local.toml")))
}

fn apply_overlay(descriptor: &mut TargetDescriptor, overlay: TargetOverlay) {
    if let Some(ssh_overlay) = overlay.ssh {
        let target_ssh = descriptor.ssh.get_or_insert(SshConfig {
            host: PLACEHOLDER_SENTINEL.to_string(),
            user: PLACEHOLDER_SENTINEL.to_string(),
            key_path: None,
            port: default_ssh_port(),
        });
        if let Some(host) = ssh_overlay.host {
            target_ssh.host = host;
        }
        if let Some(user) = ssh_overlay.user {
            target_ssh.user = user;
        }
        if let Some(key_path) = ssh_overlay.key_path {
            target_ssh.key_path = Some(key_path);
        }
        if let Some(port) = ssh_overlay.port {
            target_ssh.port = port;
        }
    }
}

fn validate_no_placeholders(
    descriptor: &TargetDescriptor,
    base: &Path,
) -> Result<()> {
    if let Some(ssh) = &descriptor.ssh {
        let mut offenders: Vec<&'static str> = Vec::new();
        if ssh.host == PLACEHOLDER_SENTINEL {
            offenders.push("ssh.host");
        }
        if ssh.user == PLACEHOLDER_SENTINEL {
            offenders.push("ssh.user");
        }
        if let Some(key_path) = &ssh.key_path {
            if key_path == PLACEHOLDER_SENTINEL {
                offenders.push("ssh.key_path");
            }
        }
        if !offenders.is_empty() {
            let overlay_path = local_overlay_path(base).unwrap_or_else(|| {
                let mut p = base.to_path_buf();
                p.set_extension("local.toml");
                p
            });
            return Err(HarnessError::DescriptorPlaceholder {
                path: base.to_path_buf(),
                fields: offenders.iter().map(|s| (*s).to_string()).collect(),
                overlay_path,
            });
        }
    }
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn _unused_path_helper(_p: PathBuf) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_descriptor(dir: &Path, stem: &str, body: &str) -> PathBuf {
        let path = dir.join(format!("{stem}.toml"));
        std::fs::write(&path, body).unwrap();
        path
    }

    fn write_overlay(dir: &Path, stem: &str, body: &str) {
        let path = dir.join(format!("{stem}.local.toml"));
        std::fs::write(&path, body).unwrap();
    }

    const BASE_WITH_PLACEHOLDERS: &str = r#"
name = "test-target"
arch = "aarch64-unknown-linux-gnu"
connection_type = "ssh"

[ssh]
host = "REPLACE_ME"
user = "REPLACE_ME"
key_path = "REPLACE_ME"
port = 22
"#;

    #[test]
    fn missing_overlay_is_rejected_when_base_has_placeholders() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_descriptor(tmp.path(), "x", BASE_WITH_PLACEHOLDERS);
        let err = TargetDescriptor::from_file(&path).unwrap_err();
        match err {
            HarnessError::DescriptorPlaceholder { fields, .. } => {
                assert!(fields.contains(&"ssh.host".to_string()));
                assert!(fields.contains(&"ssh.user".to_string()));
                assert!(fields.contains(&"ssh.key_path".to_string()));
            }
            other => panic!("expected DescriptorPlaceholder, got {other:?}"),
        }
    }

    #[test]
    fn full_overlay_replaces_every_placeholder() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_descriptor(tmp.path(), "y", BASE_WITH_PLACEHOLDERS);
        write_overlay(
            tmp.path(),
            "y",
            r#"
[ssh]
host = "10.0.0.42"
user = "lab-user"
key_path = "~/.ssh/id_ed25519"
"#,
        );
        let descriptor = TargetDescriptor::from_file(&path).unwrap();
        let ssh = descriptor.ssh.as_ref().unwrap();
        assert_eq!(ssh.host, "10.0.0.42");
        assert_eq!(ssh.user, "lab-user");
        assert_eq!(ssh.key_path.as_deref(), Some("~/.ssh/id_ed25519"));
        assert_eq!(ssh.port, 22);
    }

    #[test]
    fn partial_overlay_keeps_remaining_placeholders_visible() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_descriptor(tmp.path(), "z", BASE_WITH_PLACEHOLDERS);
        write_overlay(
            tmp.path(),
            "z",
            r#"
[ssh]
host = "10.0.0.42"
"#,
        );
        let err = TargetDescriptor::from_file(&path).unwrap_err();
        match err {
            HarnessError::DescriptorPlaceholder { fields, .. } => {
                assert!(!fields.contains(&"ssh.host".to_string()));
                assert!(fields.contains(&"ssh.user".to_string()));
                assert!(fields.contains(&"ssh.key_path".to_string()));
            }
            other => panic!("expected DescriptorPlaceholder, got {other:?}"),
        }
    }

    #[test]
    fn overlay_port_overrides_base_port() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_descriptor(tmp.path(), "p", BASE_WITH_PLACEHOLDERS);
        write_overlay(
            tmp.path(),
            "p",
            r#"
[ssh]
host = "10.0.0.42"
user = "lab-user"
key_path = "~/.ssh/id_ed25519"
port = 2222
"#,
        );
        let descriptor = TargetDescriptor::from_file(&path).unwrap();
        assert_eq!(descriptor.ssh.unwrap().port, 2222);
    }

    #[test]
    fn local_connection_type_skips_placeholder_check() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_descriptor(
            tmp.path(),
            "loc",
            r#"
name = "local-target"
arch = "x86_64-unknown-linux-gnu"
connection_type = "local"
"#,
        );
        let descriptor = TargetDescriptor::from_file(&path).unwrap();
        assert!(descriptor.ssh.is_none());
    }
}
