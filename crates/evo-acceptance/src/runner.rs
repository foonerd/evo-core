//! Orchestrator: walk the plan in tier order, execute each scenario
//! against the connection, collect per-scenario outcomes, hand them
//! to the report module.

use std::time::Instant;

use regex::Regex;

use crate::connection::{self, CommandResult, Connection};
use crate::error::{HarnessError, Result};
use crate::plan::{ScenarioPlan, Tier};
use crate::scenario::{Assertion, Scenario, ScenarioOutcome, ScenarioStatus};
use crate::target::TargetDescriptor;

const STDOUT_EXCERPT_BYTES: usize = 4096;
const STDERR_EXCERPT_BYTES: usize = 4096;

pub struct RunOptions {
    pub release: String,
    pub rc: u32,
}

pub struct Runner {
    target: TargetDescriptor,
    plan: ScenarioPlan,
}

impl Runner {
    pub fn new(target: TargetDescriptor, plan: ScenarioPlan) -> Self {
        Self { target, plan }
    }

    pub fn target(&self) -> &TargetDescriptor {
        &self.target
    }
    pub fn plan(&self) -> &ScenarioPlan {
        &self.plan
    }

    pub async fn run(
        &self,
        _options: &RunOptions,
    ) -> Result<Vec<ScenarioOutcome>> {
        // Per LOGGING.md §2: harness-side verb invocation. The
        // acceptance runner is operator-facing tooling, so info is
        // the right level for "scenario execution starting" — it
        // narrates what the run is about to do.
        tracing::info!(
            target = %self.target.name,
            scenario_count = self.plan.scenarios.len(),
            "acceptance runner: run starting"
        );
        let connection = connection::build(&self.target)?;
        let mut outcomes = Vec::with_capacity(self.plan.scenarios.len());
        let mut t1_failed = false;

        for scenario in self.plan.scenarios_in_tier_order() {
            // T1 fail blocks subsequent tiers.
            if t1_failed && scenario.tier > Tier::T1 {
                outcomes.push(ScenarioOutcome {
                    id: scenario.id.clone(),
                    tier: scenario.tier,
                    status: ScenarioStatus::Skip,
                    duration_ms: 0,
                    stdout_excerpt: String::new(),
                    stderr_excerpt: String::new(),
                    detail: "Skip: T1 tier failed; subsequent tiers blocked."
                        .into(),
                });
                continue;
            }

            // Capability gate.
            if let Some(cap) = &scenario.requires_capability {
                if !self.target.has_capability(cap) {
                    outcomes.push(ScenarioOutcome {
                        id: scenario.id.clone(),
                        tier: scenario.tier,
                        status: ScenarioStatus::Skip,
                        duration_ms: 0,
                        stdout_excerpt: String::new(),
                        stderr_excerpt: String::new(),
                        detail: format!(
                            "Skip: target does not declare capability {cap:?}."
                        ),
                    });
                    continue;
                }
            }

            let outcome = self.run_scenario(scenario, &connection).await?;
            if scenario.tier == Tier::T1
                && outcome.status == ScenarioStatus::Fail
            {
                t1_failed = true;
            }
            outcomes.push(outcome);
        }

        Ok(outcomes)
    }

    async fn run_scenario(
        &self,
        scenario: &Scenario,
        connection: &Connection,
    ) -> Result<ScenarioOutcome> {
        // Per LOGGING.md §2: each scenario execution is a verb-shaped
        // operation; debug-level so the per-scenario flow surfaces
        // when an operator turns on debug logging on the harness.
        tracing::debug!(
            scenario = %scenario.id,
            tier = ?scenario.tier,
            "acceptance runner: scenario starting"
        );
        let start = Instant::now();
        let result = connection
            .run(&scenario.command, scenario.timeout())
            .await?;
        let duration_ms = start.elapsed().as_millis() as u64;

        let (status, detail) = if result.timed_out {
            (
                ScenarioStatus::Fail,
                format!(
                    "Fail: command timed out after {}s.",
                    scenario.timeout_secs
                ),
            )
        } else {
            evaluate_assertions(scenario, &result)?
        };

        Ok(ScenarioOutcome {
            id: scenario.id.clone(),
            tier: scenario.tier,
            status,
            duration_ms,
            stdout_excerpt: truncate(&result.stdout, STDOUT_EXCERPT_BYTES),
            stderr_excerpt: truncate(&result.stderr, STDERR_EXCERPT_BYTES),
            detail,
        })
    }
}

fn evaluate_assertions(
    scenario: &Scenario,
    result: &CommandResult,
) -> Result<(ScenarioStatus, String)> {
    if scenario.assertions.is_empty() {
        // No assertions = exit-code-zero is the implicit pass criterion.
        if result.exit_code == 0 {
            return Ok((ScenarioStatus::Pass, String::new()));
        }
        return Ok((ScenarioStatus::Fail, format!("Fail: exit code {} (no explicit assertions; default expects 0).", result.exit_code)));
    }

    for assertion in &scenario.assertions {
        match assertion {
            Assertion::ExitCode { expected } => {
                if result.exit_code != *expected {
                    return Ok((
                        ScenarioStatus::Fail,
                        format!(
                            "Fail: exit code {} (expected {}).",
                            result.exit_code, expected
                        ),
                    ));
                }
            }
            Assertion::StdoutContains { value } => {
                if !result.stdout.contains(value) {
                    return Ok((
                        ScenarioStatus::Fail,
                        format!("Fail: stdout did not contain {value:?}."),
                    ));
                }
            }
            Assertion::StdoutMatches { pattern } => {
                let re = Regex::new(pattern).map_err(|source| {
                    HarnessError::InvalidRegex {
                        pattern: pattern.clone(),
                        source,
                    }
                })?;
                if !re.is_match(&result.stdout) {
                    return Ok((
                        ScenarioStatus::Fail,
                        format!(
                            "Fail: stdout did not match pattern {pattern:?}."
                        ),
                    ));
                }
            }
            Assertion::StderrAbsent { value } => {
                if result.stderr.contains(value) {
                    return Ok((ScenarioStatus::Fail, format!("Fail: stderr contained forbidden substring {value:?}.")));
                }
            }
            Assertion::FileExistsOnTarget { path: _ }
            | Assertion::FileAbsentOnTarget { path: _ } => {
                // V1 does not run file-existence assertions automatically.
                // Scenario authors include the `test -f <path>` (or `! test -f`) in the scenario command itself,
                // and the result is captured by exit code. The two assertion variants are reserved for future
                // expansion where the runner shells the check separately.
            }
        }
    }

    Ok((ScenarioStatus::Pass, String::new()))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut t = s[..max].to_string();
        t.push_str("\n…[truncated]");
        t
    }
}
