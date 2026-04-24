//! Default FHS-style paths; align with `StewardConfig` and `PLUGIN_TOOL.md`.

use std::path::PathBuf;

/// `/opt/evo/trust` — first trust root
pub fn default_trust_dir_opt() -> PathBuf {
    PathBuf::from("/opt/evo/trust")
}

/// `/etc/evo/trust.d` — second trust root
pub fn default_trust_dir_etc() -> PathBuf {
    PathBuf::from("/etc/evo/trust.d")
}

/// Install-digest revocations
pub fn default_revocations_path() -> PathBuf {
    PathBuf::from("/etc/evo/revocations.toml")
}

/// Default `install --to` (Strategy B final tree)
pub const DEFAULT_SEARCH_ROOT: &str = "/var/lib/evo/plugins";

/// Default cap for `install` URL bodies (256 MiB)
pub const DEFAULT_MAX_URL_BYTES: u64 = 256 * 1024 * 1024;
