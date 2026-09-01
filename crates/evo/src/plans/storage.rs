// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Storage layer for plan definitions.
//!
//! Plans live on disk as TOML files at a framework-determined
//! root: one file per plan, named `<plan_id>.toml`. The on-disk
//! shape matches the canonical [`Plan`] serde shape so an
//! operator can edit a file by hand and have the engine pick the
//! changes up at the next list call. Vendor distributions ship
//! plans as TOML files in their distribution bundle; the
//! framework loads them from a vendor-controlled subdirectory.
//!
//! ## Trait
//!
//! [`PlanStorage`] abstracts the four operations the engine
//! needs: list, load, save, delete. Saves are upsert (overwrite
//! by id); deletes are no-op on absent ids; loads return `None`
//! on absent ids; lists skip files that fail to parse and emit
//! one tracing warning per skip so a malformed file does not
//! block the whole boot. Backends are async via
//! [`Pin<Box<dyn Future + Send + 'a>>`] futures so the trait
//! object stays object-safe.
//!
//! ## Backends
//!
//! - [`FilesystemPlanStorage`]: persists each plan as a TOML
//!   file under a configured root directory. The on-boot list
//!   call walks the directory; saves write atomically via a
//!   tempfile + rename so a crashed write never leaves a
//!   half-written plan visible.
//! - [`InMemoryPlanStorage`]: holds plans in a `Mutex`-guarded
//!   `HashMap`. Used by tests and by callers that want a
//!   storage-shaped object without a filesystem dependency.
//!
//! ## Validation
//!
//! [`PlanStorage::save`] calls [`Plan::validate`] before writing
//! so a plan that fails the schema-level checks never reaches
//! disk. Loads do not re-validate (the file may pre-date a
//! validation rule the engine has since tightened); the engine
//! validates again at registration time.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;

use evo_plugin_sdk::contract::{Plan, PlanError, PlanId};

/// Errors raised by a [`PlanStorage`] backend. Variants are
/// structured so callers can match on the failure mode rather
/// than parse a string. Wrapped underlying errors carry their
/// original cause for diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum PlanStorageError {
    /// Filesystem I/O error (open, read, write, rename, list).
    #[error("plan storage I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// On-disk TOML failed to parse into a [`Plan`].
    #[error("plan storage TOML decode error: {0}")]
    TomlDecode(#[from] toml::de::Error),
    /// Could not serialise a [`Plan`] to TOML.
    #[error("plan storage TOML encode error: {0}")]
    TomlEncode(#[from] toml::ser::Error),
    /// Plan failed schema-level validation before write.
    #[error("plan storage rejected invalid plan: {0}")]
    InvalidPlan(#[from] PlanError),
    /// On-disk filename does not parse as a valid [`PlanId`].
    /// The file is skipped during list operations and the
    /// failure surfaces here only when a caller asks for a
    /// specific id by string.
    #[error("plan storage filename is not a valid plan id: {0}")]
    InvalidFilename(String),
    /// Plan id on disk does not match the filename. Indicates
    /// external tampering or a hand-edit error.
    #[error("plan id mismatch: filename {filename}, plan id {plan_id}")]
    IdMismatch {
        /// The filename stem (without `.toml`).
        filename: String,
        /// The id field embedded in the file.
        plan_id: String,
    },
}

/// Boxed-future shape for object-safe async trait methods.
type PlanFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, PlanStorageError>> + Send + 'a>>;

/// Storage abstraction the plan engine talks to. Lives behind a
/// trait so vendor distributions can substitute alternative
/// backends (encrypted-at-rest, network-mounted, vendored
/// read-only registry) without touching the engine.
pub trait PlanStorage: Send + Sync {
    /// List every plan currently in storage. Files that fail to
    /// parse are skipped (with a tracing warning); the list
    /// reflects only the plans the engine can actually
    /// register. Order is unspecified; callers that need a
    /// stable order sort by `Plan::id`.
    fn list(&self) -> PlanFuture<'_, Vec<Plan>>;

    /// Load one plan by id. Returns `None` if the id is not
    /// present. A file that exists but fails to parse surfaces
    /// the parse error rather than `None` so callers can
    /// distinguish "not present" from "present but corrupt".
    fn load<'a>(&'a self, id: &'a PlanId) -> PlanFuture<'a, Option<Plan>>;

    /// Save a plan. Upsert by id: an existing plan with the same
    /// id is overwritten. Calls [`Plan::validate`] before write
    /// and refuses to persist invalid plans.
    fn save<'a>(&'a self, plan: &'a Plan) -> PlanFuture<'a, ()>;

    /// Delete a plan by id. No-op on absent ids (consistent
    /// with the persistence trait's forget-shape convention).
    fn delete<'a>(&'a self, id: &'a PlanId) -> PlanFuture<'a, ()>;
}

/// Filename suffix used on disk. Single source of truth so a
/// rename of the convention here propagates everywhere.
pub const PLAN_FILE_EXTENSION: &str = "toml";

/// Filesystem-backed [`PlanStorage`]. Persists each plan as a
/// TOML file under a configured root directory. Saves are atomic
/// (write to `<id>.toml.tmp`, rename to `<id>.toml`), so a
/// crashed write never leaves a half-written plan visible to a
/// concurrent reader.
///
/// Concurrency: the backend serialises mutating calls behind an
/// internal mutex so two saves to the same id do not interleave
/// their tempfile / rename pair. List / load calls are
/// concurrent (read-only filesystem traversal).
pub struct FilesystemPlanStorage {
    root: PathBuf,
    write_lock: tokio::sync::Mutex<()>,
}

impl FilesystemPlanStorage {
    /// Construct a backend rooted at `root`. The directory is
    /// created at construction time if it does not exist; the
    /// constructor returns an error if the path exists but is
    /// not a directory or if it cannot be created.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, PlanStorageError> {
        let root = root.into();
        if root.exists() {
            if !root.is_dir() {
                return Err(PlanStorageError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotADirectory,
                    format!(
                        "plan storage root is not a directory: {}",
                        root.display()
                    ),
                )));
            }
        } else {
            std::fs::create_dir_all(&root)?;
        }
        Ok(Self {
            root,
            write_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Borrow the storage root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn plan_path(&self, id: &PlanId) -> PathBuf {
        self.root
            .join(format!("{}.{}", id.as_str(), PLAN_FILE_EXTENSION))
    }

    fn tmp_path(&self, id: &PlanId) -> PathBuf {
        self.root
            .join(format!("{}.{}.tmp", id.as_str(), PLAN_FILE_EXTENSION))
    }

    async fn list_inner(&self) -> Result<Vec<Plan>, PlanStorageError> {
        let mut entries = match tokio::fs::read_dir(&self.root).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(PlanStorageError::Io(e)),
        };
        let mut plans = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(OsStr::to_str)
                != Some(PLAN_FILE_EXTENSION)
            {
                continue;
            }
            let stem = match path.file_stem().and_then(OsStr::to_str) {
                Some(s) => s,
                None => continue,
            };
            let id = match PlanId::new(stem) {
                Ok(id) => id,
                Err(_) => {
                    tracing::warn!(
                        plan_path = %path.display(),
                        "plan storage skipping file with invalid id stem"
                    );
                    continue;
                }
            };
            let raw = match tokio::fs::read_to_string(&path).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        plan_path = %path.display(),
                        error = %e,
                        "plan storage skipping unreadable file"
                    );
                    continue;
                }
            };
            let plan: Plan = match toml::from_str(&raw) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        plan_path = %path.display(),
                        error = %e,
                        "plan storage skipping malformed file"
                    );
                    continue;
                }
            };
            if plan.id != id {
                tracing::warn!(
                    plan_path = %path.display(),
                    filename = %id,
                    plan_id = %plan.id,
                    "plan storage skipping file with id mismatch"
                );
                continue;
            }
            plans.push(plan);
        }
        Ok(plans)
    }

    async fn load_inner(
        &self,
        id: &PlanId,
    ) -> Result<Option<Plan>, PlanStorageError> {
        let path = self.plan_path(id);
        let raw = match tokio::fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(e) => return Err(PlanStorageError::Io(e)),
        };
        let plan: Plan = toml::from_str(&raw)?;
        if &plan.id != id {
            return Err(PlanStorageError::IdMismatch {
                filename: id.as_str().to_string(),
                plan_id: plan.id.as_str().to_string(),
            });
        }
        Ok(Some(plan))
    }

    async fn save_inner(&self, plan: &Plan) -> Result<(), PlanStorageError> {
        plan.validate()?;
        let toml_text = toml::to_string_pretty(plan)?;
        let tmp = self.tmp_path(&plan.id);
        let final_path = self.plan_path(&plan.id);
        let _guard = self.write_lock.lock().await;
        tokio::fs::write(&tmp, toml_text.as_bytes()).await?;
        tokio::fs::rename(&tmp, &final_path).await?;
        Ok(())
    }

    async fn delete_inner(&self, id: &PlanId) -> Result<(), PlanStorageError> {
        let path = self.plan_path(id);
        let _guard = self.write_lock.lock().await;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(PlanStorageError::Io(e)),
        }
    }
}

impl PlanStorage for FilesystemPlanStorage {
    fn list(&self) -> PlanFuture<'_, Vec<Plan>> {
        Box::pin(self.list_inner())
    }

    fn load<'a>(&'a self, id: &'a PlanId) -> PlanFuture<'a, Option<Plan>> {
        Box::pin(self.load_inner(id))
    }

    fn save<'a>(&'a self, plan: &'a Plan) -> PlanFuture<'a, ()> {
        Box::pin(self.save_inner(plan))
    }

    fn delete<'a>(&'a self, id: &'a PlanId) -> PlanFuture<'a, ()> {
        Box::pin(self.delete_inner(id))
    }
}

/// In-memory [`PlanStorage`] backend. Holds plans in a
/// mutex-guarded `HashMap`. Used by tests and by callers that
/// want the storage shape without a filesystem dependency
/// (a transient process-internal registry, an embedded test
/// harness).
#[derive(Default)]
pub struct InMemoryPlanStorage {
    plans: Mutex<HashMap<PlanId, Plan>>,
}

impl InMemoryPlanStorage {
    /// Construct an empty in-memory backend.
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot every plan currently in storage. Internal helper
    /// shared by the trait impl; held outside the trait so test
    /// callers without a runtime can read state synchronously.
    pub fn snapshot(&self) -> Vec<Plan> {
        let guard = self.plans.lock().expect("plans mutex poisoned");
        guard.values().cloned().collect()
    }

    /// Number of plans currently in storage.
    pub fn len(&self) -> usize {
        let guard = self.plans.lock().expect("plans mutex poisoned");
        guard.len()
    }

    /// True if the storage holds zero plans.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl PlanStorage for InMemoryPlanStorage {
    fn list(&self) -> PlanFuture<'_, Vec<Plan>> {
        Box::pin(async move { Ok(self.snapshot()) })
    }

    fn load<'a>(&'a self, id: &'a PlanId) -> PlanFuture<'a, Option<Plan>> {
        Box::pin(async move {
            let guard = self.plans.lock().expect("plans mutex poisoned");
            Ok(guard.get(id).cloned())
        })
    }

    fn save<'a>(&'a self, plan: &'a Plan) -> PlanFuture<'a, ()> {
        Box::pin(async move {
            plan.validate()?;
            let mut guard = self.plans.lock().expect("plans mutex poisoned");
            guard.insert(plan.id.clone(), plan.clone());
            Ok(())
        })
    }

    fn delete<'a>(&'a self, id: &'a PlanId) -> PlanFuture<'a, ()> {
        Box::pin(async move {
            let mut guard = self.plans.lock().expect("plans mutex poisoned");
            guard.remove(id);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::contract::context::AppointmentTimeZone;
    use evo_plugin_sdk::contract::metadata::ItemUri;
    use evo_plugin_sdk::contract::{
        Authorship, ClockTime, DayMask, OnComplete, PlanSegment, PlanTrigger,
        SegmentContent, SegmentDuration, TransitionType,
    };
    use tempfile::TempDir;

    fn plan(id: &str) -> Plan {
        Plan {
            id: PlanId::new(id).unwrap(),
            name: format!("Plan {id}"),
            description: None,
            trigger: PlanTrigger::TimeOfDay {
                time: ClockTime::new("07:00").unwrap(),
                days_of_week: DayMask::Daily,
                timezone: AppointmentTimeZone::Local,
            },
            preempt: false,
            segments: vec![PlanSegment {
                content: SegmentContent::Item {
                    uri: ItemUri::new("uri:test").unwrap(),
                },
                duration: SegmentDuration::UntilCompletion,
                transition: TransitionType::Hard,
                fade_in: None,
                fade_out: None,
            }],
            on_complete: OnComplete::Stop,
            authored_by: Authorship::User,
            last_modified_ms: 1_700_000_000_000,
        }
    }

    async fn round_trip_save_load_delete(store: &dyn PlanStorage) {
        let p = plan("morning");
        assert!(store.list().await.unwrap().is_empty());
        store.save(&p).await.unwrap();
        let loaded = store.load(&p.id).await.unwrap().unwrap();
        assert_eq!(p, loaded);
        let listed = store.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, p.id);
        store.delete(&p.id).await.unwrap();
        assert!(store.load(&p.id).await.unwrap().is_none());
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn in_memory_round_trip() {
        let store = InMemoryPlanStorage::new();
        round_trip_save_load_delete(&store).await;
    }

    #[tokio::test]
    async fn filesystem_round_trip() {
        let tmp = TempDir::new().unwrap();
        let store = FilesystemPlanStorage::new(tmp.path()).unwrap();
        round_trip_save_load_delete(&store).await;
    }

    #[tokio::test]
    async fn save_overwrites_existing() {
        let store = InMemoryPlanStorage::new();
        let mut p = plan("rolling");
        store.save(&p).await.unwrap();
        p.name = "Renamed".into();
        store.save(&p).await.unwrap();
        let loaded = store.load(&p.id).await.unwrap().unwrap();
        assert_eq!(loaded.name, "Renamed");
        assert_eq!(store.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn delete_is_noop_on_absent() {
        let store = InMemoryPlanStorage::new();
        let id = PlanId::new("ghost").unwrap();
        store.delete(&id).await.unwrap();

        let tmp = TempDir::new().unwrap();
        let fs_store = FilesystemPlanStorage::new(tmp.path()).unwrap();
        fs_store.delete(&id).await.unwrap();
    }

    #[tokio::test]
    async fn save_refuses_invalid_plan() {
        let store = InMemoryPlanStorage::new();
        let mut p = plan("broken");
        p.segments.clear();
        let err = store.save(&p).await.unwrap_err();
        assert!(matches!(err, PlanStorageError::InvalidPlan(_)));
    }

    #[tokio::test]
    async fn filesystem_create_root_when_absent() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("nested/plans");
        let store = FilesystemPlanStorage::new(&nested).unwrap();
        assert!(nested.is_dir());
        store.save(&plan("seed")).await.unwrap();
        assert_eq!(store.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn filesystem_root_must_be_directory() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("not-a-dir");
        std::fs::write(&file_path, "blocking file").unwrap();
        let result = FilesystemPlanStorage::new(&file_path);
        assert!(matches!(result, Err(PlanStorageError::Io(_))));
    }

    #[tokio::test]
    async fn filesystem_skips_unrelated_files() {
        let tmp = TempDir::new().unwrap();
        let store = FilesystemPlanStorage::new(tmp.path()).unwrap();
        store.save(&plan("kept")).await.unwrap();
        std::fs::write(tmp.path().join("README.md"), "ignore me").unwrap();
        std::fs::write(tmp.path().join("garbage.toml"), "not a plan").unwrap();
        std::fs::write(tmp.path().join(".hidden.toml"), "not a plan").unwrap();
        let listed = store.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id.as_str(), "kept");
    }

    #[tokio::test]
    async fn filesystem_load_detects_id_mismatch() {
        let tmp = TempDir::new().unwrap();
        let store = FilesystemPlanStorage::new(tmp.path()).unwrap();
        let p = plan("right");
        let toml_text = toml::to_string_pretty(&p).unwrap();
        std::fs::write(tmp.path().join("wrong.toml"), toml_text).unwrap();
        let id = PlanId::new("wrong").unwrap();
        let err = store.load(&id).await.unwrap_err();
        assert!(matches!(err, PlanStorageError::IdMismatch { .. }));
    }

    #[tokio::test]
    async fn filesystem_list_skips_id_mismatched_file() {
        let tmp = TempDir::new().unwrap();
        let store = FilesystemPlanStorage::new(tmp.path()).unwrap();
        let p = plan("real");
        let toml_text = toml::to_string_pretty(&p).unwrap();
        std::fs::write(tmp.path().join("misnamed.toml"), toml_text).unwrap();
        store.save(&plan("good")).await.unwrap();
        let listed = store.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id.as_str(), "good");
    }

    #[tokio::test]
    async fn filesystem_atomic_write_via_tempfile() {
        let tmp = TempDir::new().unwrap();
        let store = FilesystemPlanStorage::new(tmp.path()).unwrap();
        store.save(&plan("atomic")).await.unwrap();
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(entries.contains(&"atomic.toml".to_string()));
        assert!(!entries.iter().any(|n| n.ends_with(".tmp")));
    }

    #[tokio::test]
    async fn filesystem_list_on_missing_root_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("inner");
        let store = FilesystemPlanStorage::new(&nested).unwrap();
        std::fs::remove_dir(&nested).unwrap();
        let listed = store.list().await.unwrap();
        assert!(listed.is_empty());
    }

    #[tokio::test]
    async fn list_returns_multiple_plans() {
        let store = InMemoryPlanStorage::new();
        store.save(&plan("a")).await.unwrap();
        store.save(&plan("b")).await.unwrap();
        store.save(&plan("c")).await.unwrap();
        let listed = store.list().await.unwrap();
        assert_eq!(listed.len(), 3);
        let mut ids: Vec<_> =
            listed.iter().map(|p| p.id.as_str().to_string()).collect();
        ids.sort();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }
}
