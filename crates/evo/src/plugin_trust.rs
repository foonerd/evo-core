//! Load trust keys, revocations, and options from [`StewardConfig`]
//! for the `plugin_trust` argument of
//! [`crate::admission::AdmissionEngine::new`].

use std::sync::Arc;

use evo_trust::{load_trust_root, RevocationSet, TrustKey, TrustOptions};

use crate::config::StewardConfig;
use crate::error::StewardError;

/// Keys, revocation set, and policy for out-of-process bundle admission.
pub struct PluginTrustState {
    /// Public keys and `*.meta.toml` authorisation.
    pub keys: Vec<TrustKey>,
    /// Revoked install digests.
    pub revocations: RevocationSet,
    /// `allow_unsigned` and `degrade_trust` from config.
    pub options: TrustOptions,
}

impl PluginTrustState {
    /// Load from configured trust directories and the revocations file. A
    /// missing directory is treated as empty; a missing revocations file
    /// is an empty set.
    pub fn load(config: &StewardConfig) -> Result<Self, StewardError> {
        let keys = load_trust_root(
            &config.plugins.trust_dir_opt,
            &config.plugins.trust_dir_etc,
        )
        .map_err(|e| StewardError::Admission(format!("trust key load: {e}")))?;
        let revocations = RevocationSet::load(&config.plugins.revocations_path)
            .map_err(|e| StewardError::io("loading revocations file", e))?;
        Ok(Self {
            keys,
            revocations,
            options: TrustOptions {
                allow_unsigned: config.plugins.allow_unsigned,
                degrade_trust: config.plugins.degrade_trust,
            },
        })
    }
}

/// Loads trust with [`PluginTrustState::load`], for use as the
/// `plugin_trust` argument to
/// [`crate::admission::AdmissionEngine::new`].
pub fn load_plugin_trust_arc(
    config: &StewardConfig,
) -> Result<Arc<PluginTrustState>, StewardError> {
    Ok(Arc::new(PluginTrustState::load(config)?))
}
