//! Scenario types: a single named test the harness executes against
//! the target. Scenarios carry an id, the tier they belong to, a
//! description, the command to execute on the target, and one or
//! more pass-criterion assertions that the runner verifies against
//! the command's output.

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::plan::Tier;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    pub id: String,
    pub tier: Tier,
    pub description: String,
    /// Command executed on the target via the connection. Multi-line
    /// strings are joined with newlines and run as a shell pipeline.
    pub command: String,
    /// Optional capability the scenario requires. If the target
    /// descriptor does not declare this capability, the scenario
    /// records Skip-with-reason and the runner moves on.
    #[serde(default)]
    pub requires_capability: Option<String>,
    /// Per-scenario timeout in seconds. The runner kills the command
    /// if it exceeds this; the scenario records Fail with a timeout
    /// reason. Defaults to 300s for T1-T3 and T5; T4 plans should
    /// declare longer per-scenario timeouts explicitly.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Pass-criterion assertions. AND-composed: every assertion
    /// must hold for the scenario to PASS.
    #[serde(default)]
    pub assertions: Vec<Assertion>,
}

fn default_timeout_secs() -> u64 {
    300
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Assertion {
    /// Command must exit with the given code. Defaults imply 0.
    ExitCode { expected: i32 },
    /// stdout must contain the given substring.
    StdoutContains { value: String },
    /// stdout must match the given regular expression.
    StdoutMatches { pattern: String },
    /// stderr must NOT contain the given substring (e.g. "panic").
    StderrAbsent { value: String },
    /// A given file must exist on the target after the command runs.
    /// Verified by a follow-up `test -f <path>` on the connection.
    FileExistsOnTarget { path: String },
    /// A given file must NOT exist on the target after the command
    /// runs. Useful for canary patterns: the test asserts that a
    /// rejected operation did not produce a side effect.
    FileAbsentOnTarget { path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioOutcome {
    pub id: String,
    pub tier: Tier,
    pub status: ScenarioStatus,
    pub duration_ms: u64,
    /// Captured stdout (truncated for the report; full output kept
    /// in the JSON sidecar if requested).
    pub stdout_excerpt: String,
    /// Captured stderr (same truncation rule).
    pub stderr_excerpt: String,
    /// Why the scenario reached its current status. For Pass: empty
    /// or a short note. For Fail: the failing assertion. For Skip:
    /// the missing capability or other reason.
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScenarioStatus {
    Pass,
    Fail,
    Skip,
}

impl Scenario {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }
}
