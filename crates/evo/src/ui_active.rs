// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Active UI selection runtime.
//!
//! The framework admits multiple themes through
//! [`crate::ui_registry::ThemeRegistry`] and multiple UI shells
//! through [`crate::ui_registry::UiShellRegistry`]. At any
//! moment exactly one of each MAY be active; the active
//! selection drives the renderer's per-client decisions
//! (which theme tokens compose with which shell entry-point
//! bundle).
//!
//! This module owns the in-memory selection state plus the
//! persistence + validation surface around it. The state
//! survives steward restarts via the `active_ui_selection`
//! persistence table; rehydration runs at boot before any
//! UI client connects, so connecting clients always observe
//! the operator's last-written selection.
//!
//! Validation discipline: activating a plugin refuses unless
//! the named plugin is currently admitted in the
//! corresponding registry. Clearing (passing `None`) always
//! succeeds — the operator can deactivate a slot even when
//! the previously-active artefact is still admitted, and
//! repeated clears are idempotent.
//!
//! Hot-swap happenings (`UiThemeChanged` /
//! `UiShellChanged`) are emitted by sub-primitive D layered
//! atop this substrate; this module is the storage + write
//! pathway, not the reactive notification surface.

use std::sync::Arc;
use std::time::SystemTime;

use thiserror::Error;
use tokio::sync::RwLock;

use crate::happenings::{Happening, HappeningBus};
use crate::persistence::{
    system_time_to_ms_now, PersistedActiveUiSelection, PersistenceError,
    PersistenceStore,
};
use crate::ui_registry::{ThemeRegistry, UiShellRegistry};

/// Slot identifier for an active selection. Stored verbatim
/// in persistence; the wire op layer constrains writes to
/// the supported variants.
pub mod slots {
    /// Slot id for the active theme selection.
    pub const THEME: &str = "theme";
    /// Slot id for the active UI shell selection.
    pub const UI_SHELL: &str = "ui_shell";
}

/// Errors the active-selection runtime surfaces.
#[derive(Debug, Error)]
pub enum ActiveUiSelectionError {
    /// The caller asked to activate a plugin that is not
    /// currently admitted in the corresponding registry.
    /// The diagnostic carries the slot + plugin name so
    /// operators see exactly which artefact + slot the
    /// activation referred to.
    #[error(
        "active ui selection: plugin {plugin_name:?} is not \
         admitted in the {slot} registry"
    )]
    NotAdmitted {
        /// Slot the activation targeted (`"theme"` or
        /// `"ui_shell"`).
        slot: &'static str,
        /// Plugin name the operator asked to activate.
        plugin_name: String,
    },
    /// The persistence layer refused the write or read.
    #[error("active ui selection: persistence error: {0}")]
    Persistence(#[from] PersistenceError),
}

/// In-memory snapshot of the active selection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActiveUiSelectionSnapshot {
    /// Active theme plugin name, or `None` when no theme is
    /// active.
    pub theme: Option<String>,
    /// Active UI shell plugin name, or `None` when no shell
    /// is active.
    pub ui_shell: Option<String>,
}

#[derive(Debug, Default)]
struct State {
    theme: Option<String>,
    ui_shell: Option<String>,
}

/// Active UI selection runtime.
///
/// Holds the in-memory active theme + active UI shell, plus
/// the persistence handle that backs them and the registry
/// handles used to validate set calls. Constructed via
/// builders so test harnesses can drop the persistence
/// handle entirely (tests that exercise selection logic
/// without touching disk) while the production boot path
/// always wires both.
#[derive(Debug, Default)]
pub struct ActiveUiSelection {
    state: RwLock<State>,
    persistence: Option<Arc<dyn PersistenceStore>>,
    themes: Option<Arc<ThemeRegistry>>,
    shells: Option<Arc<UiShellRegistry>>,
    happenings: Option<Arc<HappeningBus>>,
}

impl ActiveUiSelection {
    /// Construct an empty selection runtime.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a shared selection runtime handle.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Attach the persistence handle. Without it, set / clear
    /// calls succeed in memory but do not survive steward
    /// restart. The production boot path always wires this;
    /// tests that focus on validation logic may skip it.
    pub fn with_persistence(
        mut self,
        persistence: Arc<dyn PersistenceStore>,
    ) -> Self {
        self.persistence = Some(persistence);
        self
    }

    /// Attach the theme registry. Required for
    /// [`Self::set_active_theme`] to validate the named
    /// plugin is admitted. Without it, theme activation
    /// refuses every plugin name.
    pub fn with_themes(mut self, themes: Arc<ThemeRegistry>) -> Self {
        self.themes = Some(themes);
        self
    }

    /// Attach the UI shell registry. Required for
    /// [`Self::set_active_ui_shell`].
    pub fn with_shells(mut self, shells: Arc<UiShellRegistry>) -> Self {
        self.shells = Some(shells);
        self
    }

    /// Attach the happenings bus. When present, every
    /// successful change emits a corresponding
    /// `UiActiveThemeChanged` / `UiActiveShellChanged`
    /// happening so connected UI clients re-render
    /// reactively. Without it set / clear calls succeed
    /// silently — the persistence + in-memory state still
    /// update, but no one is told.
    pub fn with_happenings(mut self, happenings: Arc<HappeningBus>) -> Self {
        self.happenings = Some(happenings);
        self
    }

    /// Rehydrate the selection from persistence.
    ///
    /// Called once at boot before admission walks the
    /// catalogue + before any UI client connects. Reads every
    /// `active_ui_selection` row and seeds the in-memory
    /// state from each known slot. Rows for unknown slots
    /// (the operator may have set a slot in a future
    /// framework version we don't recognise) are tolerated
    /// silently — the wire op layer constrains writes, so
    /// unknown slots don't accumulate in normal operation.
    ///
    /// Does NOT re-validate the rehydrated selection against
    /// the registries. The registries are populated by
    /// admission, which races boot in the other direction;
    /// admission picks up the pre-existing selection on the
    /// next set call. The renderer reading the selection
    /// gets a "stale plugin name" if the previously-active
    /// theme is no longer admitted, but the operator's
    /// preference is preserved for reactivation when the
    /// artefact bundle re-appears (e.g., after a partial
    /// catalogue restore).
    pub async fn rehydrate(&self) -> Result<usize, ActiveUiSelectionError> {
        let Some(persistence) = self.persistence.as_ref() else {
            return Ok(0);
        };
        let rows = persistence.list_active_ui_selection().await?;
        let mut state = self.state.write().await;
        let mut count = 0usize;
        for row in rows {
            match row.slot.as_str() {
                slots::THEME => {
                    state.theme = row.plugin_name.clone();
                    count += 1;
                }
                slots::UI_SHELL => {
                    state.ui_shell = row.plugin_name.clone();
                    count += 1;
                }
                _ => {
                    tracing::warn!(
                        slot = %row.slot,
                        "active ui selection: unknown slot in \
                         persistence; ignoring"
                    );
                }
            }
        }
        Ok(count)
    }

    /// Activate a theme by canonical plugin name (or clear
    /// the active theme by passing `None`).
    ///
    /// Validates the named plugin is admitted in the theme
    /// registry; refuses with `NotAdmitted` otherwise.
    /// Clearing (passing `None`) always succeeds. Persists
    /// the new state if a persistence handle is wired.
    pub async fn set_active_theme(
        &self,
        plugin_name: Option<&str>,
        principal: &str,
    ) -> Result<(), ActiveUiSelectionError> {
        if let Some(name) = plugin_name {
            let admitted = match self.themes.as_ref() {
                Some(registry) => registry.contains(name).await,
                None => false,
            };
            if !admitted {
                return Err(ActiveUiSelectionError::NotAdmitted {
                    slot: slots::THEME,
                    plugin_name: name.to_string(),
                });
            }
        }
        self.persist_and_set(
            slots::THEME,
            plugin_name.map(|s| s.to_string()),
            principal,
        )
        .await
    }

    /// Activate a UI shell by canonical plugin name (or
    /// clear by passing `None`). Same validation +
    /// persistence semantics as [`Self::set_active_theme`].
    pub async fn set_active_ui_shell(
        &self,
        plugin_name: Option<&str>,
        principal: &str,
    ) -> Result<(), ActiveUiSelectionError> {
        if let Some(name) = plugin_name {
            let admitted = match self.shells.as_ref() {
                Some(registry) => registry.contains(name).await,
                None => false,
            };
            if !admitted {
                return Err(ActiveUiSelectionError::NotAdmitted {
                    slot: slots::UI_SHELL,
                    plugin_name: name.to_string(),
                });
            }
        }
        self.persist_and_set(
            slots::UI_SHELL,
            plugin_name.map(|s| s.to_string()),
            principal,
        )
        .await
    }

    /// Snapshot the current selection.
    pub async fn snapshot(&self) -> ActiveUiSelectionSnapshot {
        let g = self.state.read().await;
        ActiveUiSelectionSnapshot {
            theme: g.theme.clone(),
            ui_shell: g.ui_shell.clone(),
        }
    }

    /// Borrow just the active theme plugin name.
    pub async fn active_theme(&self) -> Option<String> {
        self.state.read().await.theme.clone()
    }

    /// Borrow just the active UI shell plugin name.
    pub async fn active_ui_shell(&self) -> Option<String> {
        self.state.read().await.ui_shell.clone()
    }

    async fn persist_and_set(
        &self,
        slot: &'static str,
        plugin_name: Option<String>,
        principal: &str,
    ) -> Result<(), ActiveUiSelectionError> {
        let now_ms = system_time_to_ms_now();
        if let Some(persistence) = self.persistence.as_ref() {
            persistence
                .put_active_ui_selection(PersistedActiveUiSelection {
                    slot: slot.to_string(),
                    plugin_name: plugin_name.clone(),
                    set_at_ms: now_ms,
                    set_by_principal: principal.to_string(),
                })
                .await?;
        }
        let previous = {
            let mut state = self.state.write().await;
            match slot {
                slots::THEME => {
                    let previous = state.theme.clone();
                    state.theme = plugin_name.clone();
                    previous
                }
                slots::UI_SHELL => {
                    let previous = state.ui_shell.clone();
                    state.ui_shell = plugin_name.clone();
                    previous
                }
                other => {
                    // Internal invariant: callers go through
                    // the typed setters which always pass
                    // the documented slot constants.
                    panic!(
                        "active ui selection: unknown slot {other} in \
                         persist_and_set"
                    );
                }
            }
        };
        if let Some(bus) = self.happenings.as_ref() {
            let happening = match slot {
                slots::THEME => Happening::UiActiveThemeChanged {
                    previous,
                    current: plugin_name,
                    principal: principal.to_string(),
                    at: SystemTime::now(),
                },
                slots::UI_SHELL => Happening::UiActiveShellChanged {
                    previous,
                    current: plugin_name,
                    principal: principal.to_string(),
                    at: SystemTime::now(),
                },
                _ => unreachable!(
                    "slot validated above; only THEME / UI_SHELL reach here"
                ),
            };
            // emit_durable persists + broadcasts; we
            // tolerate failures by logging-but-not-aborting
            // because the persistent state has already
            // updated and a missed broadcast is recoverable
            // via describe_active_ui_selection.
            if let Err(e) = bus.emit_durable(happening).await {
                tracing::warn!(
                    error = %e,
                    slot,
                    "active ui selection: happening emission failed; \
                     in-memory state is correct but subscribers may \
                     have missed the change"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::MemoryPersistenceStore;
    use crate::ui_registry::{
        AdmittedTheme, AdmittedUiShell, ThemeRegistry, UiShellRegistry,
    };
    use evo_plugin_sdk::manifest::{ThemeSection, UiShellSection};
    use std::path::PathBuf;

    fn sample_theme(plugin_name: &str) -> AdmittedTheme {
        AdmittedTheme {
            plugin_name: plugin_name.to_string(),
            plugin_version: semver::Version::new(0, 1, 0),
            plugin_dir: PathBuf::from(format!("/plugins/{plugin_name}")),
            section: ThemeSection {
                display_name: Some(plugin_name.to_string()),
                ..Default::default()
            },
        }
    }

    fn sample_shell(plugin_name: &str) -> AdmittedUiShell {
        AdmittedUiShell {
            plugin_name: plugin_name.to_string(),
            plugin_version: semver::Version::new(0, 1, 0),
            plugin_dir: PathBuf::from(format!("/plugins/{plugin_name}")),
            section: UiShellSection {
                display_name: Some(plugin_name.to_string()),
                shell_type: "web_bundle".into(),
                entry_point: "index.html".into(),
                manifest_assets: Default::default(),
                service_worker: None,
                required_widget_kinds: Vec::new(),
                supports_themes: true,
                supports_offline: false,
                min_evo_version: semver::Version::new(0, 1, 13),
            },
        }
    }

    fn build_runtime() -> (
        ActiveUiSelection,
        Arc<MemoryPersistenceStore>,
        Arc<ThemeRegistry>,
        Arc<UiShellRegistry>,
    ) {
        let persistence = Arc::new(MemoryPersistenceStore::default());
        let themes = ThemeRegistry::shared();
        let shells = UiShellRegistry::shared();
        let runtime = ActiveUiSelection::new()
            .with_persistence(
                Arc::clone(&persistence) as Arc<dyn PersistenceStore>
            )
            .with_themes(Arc::clone(&themes))
            .with_shells(Arc::clone(&shells));
        (runtime, persistence, themes, shells)
    }

    #[tokio::test]
    async fn empty_runtime_has_no_active_selection() {
        let (runtime, _, _, _) = build_runtime();
        let s = runtime.snapshot().await;
        assert_eq!(s.theme, None);
        assert_eq!(s.ui_shell, None);
    }

    #[tokio::test]
    async fn activate_theme_admitted_succeeds() {
        let (runtime, _, themes, _) = build_runtime();
        themes
            .register(sample_theme("com.example.theme"))
            .await
            .unwrap();
        runtime
            .set_active_theme(Some("com.example.theme"), "peer:1000")
            .await
            .expect("activation must succeed");
        assert_eq!(
            runtime.active_theme().await,
            Some("com.example.theme".into())
        );
    }

    #[tokio::test]
    async fn activate_theme_not_admitted_refuses() {
        let (runtime, _, _, _) = build_runtime();
        match runtime
            .set_active_theme(Some("com.example.unknown"), "peer:1000")
            .await
        {
            Err(ActiveUiSelectionError::NotAdmitted { slot, plugin_name }) => {
                assert_eq!(slot, slots::THEME);
                assert_eq!(plugin_name, "com.example.unknown");
            }
            other => panic!("expected NotAdmitted, got {other:?}"),
        }
        // State unchanged on refusal.
        assert_eq!(runtime.active_theme().await, None);
    }

    #[tokio::test]
    async fn clear_theme_always_succeeds() {
        let (runtime, _, themes, _) = build_runtime();
        themes
            .register(sample_theme("com.example.theme"))
            .await
            .unwrap();
        runtime
            .set_active_theme(Some("com.example.theme"), "peer:1000")
            .await
            .unwrap();
        runtime
            .set_active_theme(None, "peer:1000")
            .await
            .expect("clear must succeed");
        assert_eq!(runtime.active_theme().await, None);
    }

    #[tokio::test]
    async fn activate_ui_shell_admitted_succeeds() {
        let (runtime, _, _, shells) = build_runtime();
        shells
            .register(sample_shell("com.example.shell"))
            .await
            .unwrap();
        runtime
            .set_active_ui_shell(Some("com.example.shell"), "peer:1000")
            .await
            .expect("activation must succeed");
        assert_eq!(
            runtime.active_ui_shell().await,
            Some("com.example.shell".into())
        );
    }

    #[tokio::test]
    async fn activate_persists_through_runtime_restart() {
        let (runtime, persistence, themes, shells) = build_runtime();
        themes
            .register(sample_theme("com.example.theme"))
            .await
            .unwrap();
        shells
            .register(sample_shell("com.example.shell"))
            .await
            .unwrap();
        runtime
            .set_active_theme(Some("com.example.theme"), "peer:1000")
            .await
            .unwrap();
        runtime
            .set_active_ui_shell(Some("com.example.shell"), "peer:1000")
            .await
            .unwrap();

        // Simulate restart: build a fresh runtime against
        // the same persistence + same registries; rehydrate.
        let restored = ActiveUiSelection::new()
            .with_persistence(
                Arc::clone(&persistence) as Arc<dyn PersistenceStore>
            )
            .with_themes(Arc::clone(&themes))
            .with_shells(Arc::clone(&shells));
        let n = restored.rehydrate().await.unwrap();
        assert_eq!(n, 2);
        assert_eq!(
            restored.active_theme().await,
            Some("com.example.theme".into())
        );
        assert_eq!(
            restored.active_ui_shell().await,
            Some("com.example.shell".into())
        );
    }

    #[tokio::test]
    async fn rehydrate_tolerates_unknown_slot_rows() {
        let (_, persistence, themes, shells) = build_runtime();
        // Seed an unknown slot row directly via the
        // persistence trait; the runtime must not panic on
        // unknown slot keys at rehydration.
        persistence
            .put_active_ui_selection(PersistedActiveUiSelection {
                slot: "unknown_slot".into(),
                plugin_name: Some("com.example.ghost".into()),
                set_at_ms: 1,
                set_by_principal: "peer:1".into(),
            })
            .await
            .unwrap();
        let runtime = ActiveUiSelection::new()
            .with_persistence(
                Arc::clone(&persistence) as Arc<dyn PersistenceStore>
            )
            .with_themes(themes)
            .with_shells(shells);
        let n = runtime.rehydrate().await.unwrap();
        // The unknown slot is counted-and-warned but does
        // not seed any state; theme + shell remain None.
        assert_eq!(n, 0);
        assert_eq!(runtime.active_theme().await, None);
        assert_eq!(runtime.active_ui_shell().await, None);
    }

    #[tokio::test]
    async fn rehydrate_with_no_persistence_returns_zero() {
        let runtime = ActiveUiSelection::new();
        let n = runtime.rehydrate().await.unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn activate_without_registry_refuses_every_named_plugin() {
        let runtime = ActiveUiSelection::new();
        // No theme registry wired; every named-plugin
        // activation refuses with NotAdmitted (the
        // registry-absent state is structurally equivalent
        // to "no plugin admitted").
        match runtime
            .set_active_theme(Some("com.example.theme"), "peer:1000")
            .await
        {
            Err(ActiveUiSelectionError::NotAdmitted { slot, .. }) => {
                assert_eq!(slot, slots::THEME);
            }
            other => panic!("expected NotAdmitted, got {other:?}"),
        }
        // Clearing still succeeds — the runtime accepts a
        // null selection in any wiring state.
        runtime
            .set_active_theme(None, "peer:1000")
            .await
            .expect("clear without registry must still succeed");
    }

    #[tokio::test]
    async fn activate_theme_emits_active_theme_changed() {
        let bus = Arc::new(HappeningBus::new());
        let mut rx = bus.subscribe();
        let themes = ThemeRegistry::shared();
        themes
            .register(sample_theme("com.example.theme"))
            .await
            .unwrap();
        let runtime = ActiveUiSelection::new()
            .with_themes(Arc::clone(&themes))
            .with_happenings(Arc::clone(&bus));
        runtime
            .set_active_theme(Some("com.example.theme"), "peer:1000")
            .await
            .unwrap();
        let got = rx.recv().await.expect("recv must produce");
        match got {
            Happening::UiActiveThemeChanged {
                previous,
                current,
                principal,
                ..
            } => {
                assert_eq!(previous, None);
                assert_eq!(current, Some("com.example.theme".into()));
                assert_eq!(principal, "peer:1000");
            }
            other => {
                panic!("expected UiActiveThemeChanged, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn activate_then_clear_emits_two_distinct_changes() {
        let bus = Arc::new(HappeningBus::new());
        let mut rx = bus.subscribe();
        let themes = ThemeRegistry::shared();
        themes
            .register(sample_theme("com.example.theme"))
            .await
            .unwrap();
        let runtime = ActiveUiSelection::new()
            .with_themes(Arc::clone(&themes))
            .with_happenings(Arc::clone(&bus));
        runtime
            .set_active_theme(Some("com.example.theme"), "peer:1000")
            .await
            .unwrap();
        runtime.set_active_theme(None, "peer:1000").await.unwrap();
        // First emission: None → Some.
        match rx.recv().await.unwrap() {
            Happening::UiActiveThemeChanged {
                previous, current, ..
            } => {
                assert_eq!(previous, None);
                assert_eq!(current, Some("com.example.theme".into()));
            }
            other => panic!("expected first change, got {other:?}"),
        }
        // Second emission: Some → None.
        match rx.recv().await.unwrap() {
            Happening::UiActiveThemeChanged {
                previous, current, ..
            } => {
                assert_eq!(previous, Some("com.example.theme".into()));
                assert_eq!(current, None);
            }
            other => panic!("expected second change, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn activate_ui_shell_emits_active_shell_changed() {
        let bus = Arc::new(HappeningBus::new());
        let mut rx = bus.subscribe();
        let shells = UiShellRegistry::shared();
        shells
            .register(sample_shell("com.example.shell"))
            .await
            .unwrap();
        let runtime = ActiveUiSelection::new()
            .with_shells(Arc::clone(&shells))
            .with_happenings(Arc::clone(&bus));
        runtime
            .set_active_ui_shell(Some("com.example.shell"), "peer:1000")
            .await
            .unwrap();
        match rx.recv().await.unwrap() {
            Happening::UiActiveShellChanged {
                previous,
                current,
                principal,
                ..
            } => {
                assert_eq!(previous, None);
                assert_eq!(current, Some("com.example.shell".into()));
                assert_eq!(principal, "peer:1000");
            }
            other => {
                panic!("expected UiActiveShellChanged, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn refused_activation_does_not_emit() {
        let bus = Arc::new(HappeningBus::new());
        let mut rx = bus.subscribe();
        let themes = ThemeRegistry::shared();
        let runtime = ActiveUiSelection::new()
            .with_themes(Arc::clone(&themes))
            .with_happenings(Arc::clone(&bus));
        // Plugin not admitted; activation refuses.
        let _ = runtime
            .set_active_theme(Some("com.example.unknown"), "peer:1000")
            .await
            .unwrap_err();
        // No happening should land on the bus.
        match tokio::time::timeout(
            std::time::Duration::from_millis(50),
            rx.recv(),
        )
        .await
        {
            Err(_) => { /* timeout — no emission, as expected */ }
            Ok(Ok(unexpected)) => panic!(
                "expected no emission on refused activation, got {unexpected:?}"
            ),
            Ok(Err(e)) => {
                panic!("subscriber error before timeout: {e}")
            }
        }
    }
}
