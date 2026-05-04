//! Walk configured plugin search roots, skip unsupported manifests, and
//! admit out-of-process singletons. See `PLUGIN_PACKAGING.md` for
//! directory layout.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use evo_plugin_sdk::manifest::TransportKind;
use evo_plugin_sdk::Manifest;

use crate::admission::AdmissionEngine;
use crate::catalogue::Catalogue;
use crate::config::StewardConfig;
use crate::error::StewardError;
use crate::happenings::Happening;

/// Scan `config.plugins.search_roots`, then admit each discovered
/// out-of-process singleton bundle. Duplicate `plugin.name` values use the
/// path from the last matching search root. Factory plugins and
/// non-out-of-process transports are skipped with a warning.
pub async fn discover_and_admit(
    engine: &mut AdmissionEngine,
    config: &StewardConfig,
) -> Result<(), StewardError> {
    let catalogue = engine.catalogue();
    if config.plugins.search_roots.is_empty() {
        tracing::warn!(
            "plugins.search_roots is empty; skipping plugin discovery"
        );
        log_admission_outcome(engine, &catalogue);
        return Ok(());
    }

    std::fs::create_dir_all(&config.plugins.runtime_dir).map_err(|e| {
        StewardError::io(
            format!(
                "creating plugin runtime_dir {}",
                config.plugins.runtime_dir.display()
            ),
            e,
        )
    })?;
    std::fs::create_dir_all(&config.plugins.plugin_data_root).map_err(|e| {
        StewardError::io(
            format!(
                "creating plugin_data_root {}",
                config.plugins.plugin_data_root.display()
            ),
            e,
        )
    })?;

    tracing::info!(
        roots = ?config.plugins.search_roots,
        "plugin discovery"
    );

    let mut by_name: HashMap<String, (PathBuf, Manifest)> = HashMap::new();
    for root in &config.plugins.search_roots {
        let bundles = discover_plugin_bundles(root)?;
        tracing::debug!(
            root = %root.display(),
            bundle_count = bundles.len(),
            staged = is_staged_root(root),
            "discovery: walked search root"
        );
        for dir in bundles {
            let mpath = dir.join("manifest.toml");
            tracing::debug!(
                bundle = %dir.display(),
                manifest = %mpath.display(),
                "discovery: parsing bundle manifest"
            );
            let text = std::fs::read_to_string(&mpath).map_err(|e| {
                StewardError::io(format!("reading {}", mpath.display()), e)
            })?;
            let manifest = Manifest::from_toml(&text).map_err(|e| {
                StewardError::Admission(format!(
                    "invalid manifest in {}: {e}",
                    mpath.display()
                ))
            })?;
            tracing::debug!(
                plugin = %manifest.plugin.name,
                version = %manifest.plugin.version,
                shelf = %manifest.target.shelf,
                instance = ?manifest.kind.instance,
                interaction = ?manifest.kind.interaction,
                transport = ?manifest.transport.kind,
                trust_class = ?manifest.trust.class,
                "discovery: manifest parsed"
            );
            if let Some((existing_dir, _)) = by_name.get(&manifest.plugin.name)
            {
                tracing::debug!(
                    plugin = %manifest.plugin.name,
                    superseded = %existing_dir.display(),
                    superseding = %dir.display(),
                    "discovery: duplicate plugin name; later root wins"
                );
            }
            by_name.insert(manifest.plugin.name.clone(), (dir, manifest));
        }
    }
    tracing::debug!(
        total_unique_plugins = by_name.len(),
        "discovery: bundle-walk complete"
    );

    // Load the operator-disabled set from persistence. Plugins
    // absent from the table are treated as enabled by default;
    // only rows with `enabled = false` populate the skip-set.
    // Persistence read failures are downgraded to a warning and
    // the skip-set is treated as empty: a discovery pass that
    // proceeds without the persistent state is preferable to a
    // boot abort, since the operator-disabled bit is purely
    // additive (its absence admits more plugins, never fewer).
    let disabled: HashSet<String> =
        match engine.persistence().load_all_installed_plugins().await {
            Ok(rows) => rows
                .into_iter()
                .filter(|row| !row.enabled)
                .map(|row| row.plugin_name)
                .collect(),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "failed to load installed_plugins; treating skip-set \
                     as empty (every discovered plugin will be admitted)"
                );
                HashSet::new()
            }
        };

    let mut names: Vec<String> = by_name.keys().cloned().collect();
    names.sort();

    tracing::debug!(
        plugin_count = names.len(),
        disabled_count = disabled.len(),
        "discovery: starting per-plugin admission walk"
    );
    for name in names {
        let (dir, manifest) =
            by_name.get(&name).expect("name came from by_name keys");
        tracing::debug!(
            plugin = %name,
            path = %dir.display(),
            transport = ?manifest.transport.kind,
            "discovery: evaluating plugin for admission"
        );

        // Operator-disabled plugins are skipped at discovery
        // time and the structured happening is emitted so
        // frontends can render the disabled set.
        if disabled.contains(&name) {
            tracing::info!(
                plugin = %name,
                path = %dir.display(),
                "skipping plugin: operator-disabled"
            );
            let _ = engine
                .happening_bus()
                .emit_durable(Happening::PluginAdmissionSkipped {
                    plugin: name.clone(),
                    reason: "operator_disabled".to_string(),
                    at: std::time::SystemTime::now(),
                })
                .await;
            continue;
        }

        // Factory admission gating lives in the admission engine.
        // Discovery surfaces every parseable manifest; admission
        // produces the structured refusal so operators see a
        // consistent error shape regardless of the entry point.
        if manifest.transport.kind != TransportKind::OutOfProcess {
            tracing::warn!(
                plugin = %name,
                path = %dir.display(),
                ?manifest.transport.kind,
                "skipping plugin: transport is not out-of-process"
            );
            continue;
        }

        ensure_plugin_state_and_credentials(engine.plugin_data_root(), &name)?;
        // Per-plugin admission failure is non-fatal at startup. The
        // steward must stay up so the operator can restore catalogue
        // state, remove the misbehaving plugin, or otherwise recover
        // — particularly under the catalogue-resilience degraded-boot
        // path (corrupted catalogue + LKG → built-in skeleton, which
        // necessarily declares fewer shelves than the live plugin
        // set). Skip + structured happening + continue, mirroring
        // the operator-disabled and transport-mismatch skip paths
        // above.
        if let Err(e) = engine
            .admit_out_of_process_from_directory(
                dir.as_path(),
                &config.plugins.runtime_dir,
            )
            .await
        {
            tracing::warn!(
                plugin = %name,
                path = %dir.display(),
                error = %e,
                "skipping plugin: admission failed"
            );
            let _ = engine
                .happening_bus()
                .emit_durable(Happening::PluginAdmissionSkipped {
                    plugin: name.clone(),
                    reason: format!("admission_failed: {e}"),
                    at: std::time::SystemTime::now(),
                })
                .await;
            continue;
        }
    }

    log_admission_outcome(engine, &catalogue);
    Ok(())
}

fn log_admission_outcome(engine: &AdmissionEngine, catalogue: &Catalogue) {
    let n = engine.len();
    if n == 0 {
        if catalogue.racks.is_empty() {
            tracing::info!(
                "no plugins admitted; catalogue is empty; valid startup state"
            );
        } else {
            let shelves: usize =
                catalogue.racks.iter().map(|r| r.shelves.len()).sum();
            // `warn` because a catalogue declaring shelves with no
            // admissions after discovery is a plausible operational
            // anomaly: misconfigured `plugins.search_roots`, stale
            // bundles, manifests skipped as factory or in-process,
            // or a custom binary that admits via other paths. This
            // is not a hard failure in the framework (see
            // `STEWARD.md` section 12.9: empty-admission is a valid
            // state) but it does warrant an operator glance.
            tracing::warn!(
                shelves,
                "catalogue declares shelves but no plugins were admitted; \
                 check plugins.search_roots, manifest validity, and \
                 transport.type (discovery admits out-of-process singletons only)"
            );
        }
    } else {
        tracing::info!(plugins = n, "admission from discovery");
    }
}

/// Ensure `<data_root>/<plugin_name>/state` and `credentials/` exist, mode
/// `0o700` on Unix.
pub(crate) fn ensure_plugin_state_and_credentials(
    data_root: &Path,
    plugin_name: &str,
) -> Result<(), StewardError> {
    for sub in ["state", "credentials"] {
        let p = data_root.join(plugin_name).join(sub);
        std::fs::create_dir_all(&p).map_err(|e| {
            StewardError::io(format!("creating {}", p.display()), e)
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut m = std::fs::metadata(&p)
                .map_err(|e| {
                    StewardError::io(format!("metadata for {}", p.display()), e)
                })?
                .permissions();
            m.set_mode(0o700);
            std::fs::set_permissions(&p, m).map_err(|e| {
                StewardError::io(format!("chmod {}", p.display()), e)
            })?;
        }
    }
    Ok(())
}

fn is_staged_root(root: &Path) -> bool {
    root.join("evo").is_dir()
        || root.join("distribution").is_dir()
        || root.join("vendor").is_dir()
}

fn collect_direct_bundles(parent: &Path) -> Result<Vec<PathBuf>, StewardError> {
    if !parent.is_dir() {
        return Ok(vec![]);
    }
    let r = std::fs::read_dir(parent).map_err(|e| {
        StewardError::io(format!("reading directory {}", parent.display()), e)
    })?;
    let mut out = Vec::new();
    for e in r {
        let e = e.map_err(|e| {
            StewardError::io(format!("iterating {}", parent.display()), e)
        })?;
        let p = e.path();
        if p.is_dir() && p.join("manifest.toml").is_file() {
            out.push(p);
        }
    }
    out.sort();
    Ok(out)
}

/// Discover per-bundle directories under a search root (staged
/// `evo`/`distribution`/`vendor` per `PLUGIN_PACKAGING.md`, or a flat
/// list of child directories with `manifest.toml`).
fn discover_plugin_bundles(root: &Path) -> Result<Vec<PathBuf>, StewardError> {
    if !root.is_dir() {
        tracing::debug!(path = %root.display(), "plugin search root missing; skipping");
        return Ok(vec![]);
    }

    if is_staged_root(root) {
        let mut out = Vec::new();
        for cat in ["evo", "distribution", "vendor"] {
            let p = root.join(cat);
            if p.is_dir() {
                out.extend(collect_direct_bundles(&p)?);
            }
        }
        out.sort();
        return Ok(out);
    }

    collect_direct_bundles(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn staged_finds_bundles_under_categories_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        for (cat, name) in [("evo", "a"), ("distribution", "b")] {
            let p = root.join(cat).join(name);
            std::fs::create_dir_all(&p).expect("bundle dir");
            let mut f = std::fs::File::create(p.join("manifest.toml"))
                .expect("manifest");
            f.write_all(b"invalid").ok();
        }
        let b = discover_plugin_bundles(root).expect("discover");
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn flat_root_lists_direct_bundles() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        for name in ["x", "y"] {
            let p = root.join(name);
            std::fs::create_dir_all(&p).expect("bundle");
            let mut f =
                std::fs::File::create(p.join("manifest.toml")).expect("m");
            f.write_all(b"x").ok();
        }
        let b = discover_plugin_bundles(root).expect("discover");
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn is_staged_detects_vocabulary_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        assert!(!is_staged_root(root));
        std::fs::create_dir_all(root.join("evo").join("p")).expect("d");
        assert!(is_staged_root(root));
    }
}
