//! Scenario plan — a per-release file that lists the T1-T5 tier
//! scenarios. Authored by deciders alongside the release; the
//! harness consumes it as data.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{HarnessError, Result};
use crate::scenario::Scenario;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioPlan {
    /// Release this plan covers, e.g. "v0.1.12".
    pub release: String,
    /// One-line description of the plan's coverage scope.
    #[serde(default)]
    pub coverage_note: Option<String>,
    /// Performance baselines the report's §9 carries forward for
    /// regression comparison. Optional; harness records but does
    /// not auto-evaluate (deciders inspect).
    #[serde(default)]
    pub performance_baselines: Vec<PerformanceBaseline>,
    /// The full scenario list. Tier ordering is enforced by the
    /// runner: T1 first, then T2, T3, T4, T5.
    #[serde(default)]
    pub scenarios: Vec<Scenario>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceBaseline {
    pub metric: String,
    pub target: String,
    /// Scenario id whose run produces the value to compare.
    pub measured_at: String,
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord,
)]
pub enum Tier {
    T1,
    T2,
    T3,
    T4,
    T5,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Self::T1 => "T1 — Bring-up smoke",
            Self::T2 => "T2 — Per-ADR functional",
            Self::T3 => "T3 — Cross-ADR integration",
            Self::T4 => "T4 — Stress / soak",
            Self::T5 => "T5 — Failure-mode",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Self::T1 => "T1",
            Self::T2 => "T2",
            Self::T3 => "T3",
            Self::T4 => "T4",
            Self::T5 => "T5",
        }
    }
}

impl ScenarioPlan {
    pub fn from_file(path: &Path) -> Result<Self> {
        let bytes = std::fs::read_to_string(path).map_err(|source| {
            HarnessError::PlanRead {
                path: path.to_path_buf(),
                source,
            }
        })?;
        toml::from_str(&bytes).map_err(|source| HarnessError::PlanParse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Scenarios in tier order (T1 first). Within a tier the order
    /// matches the file's declaration order.
    pub fn scenarios_in_tier_order(&self) -> Vec<&Scenario> {
        let mut out: Vec<&Scenario> = self.scenarios.iter().collect();
        out.sort_by_key(|s| s.tier);
        out
    }
}
