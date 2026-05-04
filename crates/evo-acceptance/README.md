# evo-acceptance

Adaptive acceptance harness for the evo framework. Runs structured T1-T5 tier scenarios against any target (Pi 5 prototype, x86 dev box, vendor board) defined by a target descriptor file, and emits a Markdown readiness report with a tri-state PROMOTE / CONDITIONAL / BLOCK verdict.

## Usage

```bash
evo-acceptance run \
    --target=acceptance/targets/pi5-prototype.toml \
    --plan=acceptance/plans/v0.1.12.toml \
    --release=v0.1.12 \
    --rc=1 \
    --output=reports/V0.1.12-RC.1-PI5-READINESS-REPORT.md
```

## Inputs

- **Target descriptor** (`--target`) — declares the acceptance target: host address, SSH credentials, architecture, hardware capabilities (RTC, IR, jack, etc.). Scenarios that require a missing capability skip-with-reason rather than fail.
- **Scenario plan** (`--plan`) — declares the tier-organised scenario list for a release. Each scenario carries an id, tier, description, executable command, and pass criteria.

## Outputs

- **Readiness report** (`--output`) — Markdown with a fixed ten-section layout. Sections §3-§7 (per-ADR coverage table, cross-ADR integration outcomes, stress / soak metrics, failure-mode outcomes, no-panic verification) are populated automatically from scenario results. Sections §1 / §2 / §8 / §9 / §10 (cut metadata, executive verdict, findings, performance baselines, recommendation) are stubbed for decider authoring after review.
- **Structured sidecar** (`--output-json`, optional) — pass / fail / skip-with-reason per scenario, suitable for CI consumption and historical regression comparison.

## Tier model

The harness runs scenarios in tier order (T1 first, then T2, T3, T4, T5). T1 failure halts subsequent tiers. T2-T5 run all declared scenarios regardless of individual failures (the readiness report records each).

| Tier | Purpose | Typical duration |
| --- | --- | --- |
| T1 | Bring-up smoke (boot, client connect, canary happenings, idle) | <2 min |
| T2 | Per-ADR functional (one scenario per major framework deliverable) | ~30 min |
| T3 | Cross-ADR integration (named multi-surface scenarios) | ~30 min |
| T4 | Stress / soak / regression (burst load, concurrent admission, idle soak, bulk operations) | ~4 hours |
| T5 | Failure-mode (deliberate failures: corruption, crashes, OOM, disk full, dependency loss) | ~30 min |

The plan declares which tiers to run; T4 is typically scheduled separately on a longer cadence.

## Connection model

- **SSH** — declared by `connection_type = "ssh"` in the target descriptor. SSH key path read from the descriptor; falls back to the user's ssh-agent. Commands execute in the target's shell; stdout/stderr are captured and matched against scenario pass criteria.
- **Local** — declared by `connection_type = "local"`. Commands execute on the host machine. Useful for x86 dev boxes where the harness runs alongside the steward.

Future connection types (serial console for embedded boards, etc.) implement the `Connection` trait without changes to the runner.

## Pass criteria

Each scenario declares one or more assertions:

- `exit_code = 0` — command must exit cleanly
- `stdout_contains = "..."` — stdout substring match
- `stdout_matches = "..."` — stdout regex match
- `stderr_absent = "..."` — stderr must NOT contain a substring (e.g. "panic")
- `file_exists_on_target = "..."` — verify a file appears post-command
- `file_absent_on_target = "..."` — verify a file does NOT appear (canary pattern)
- `timeout_secs = N` — overall scenario timeout

Multiple assertions are AND-composed: every assertion must hold for the scenario to PASS.

## Skip-with-reason

A scenario declares `requires_capability = "..."` referencing a capability key in the target descriptor. If the target does not declare the capability, the scenario records `Skip-with-reason: capability "..." not present on target` and the runner moves on. The readiness report's §3 totals Skip rows; if Skip rate exceeds 20% of total scenarios, PROMOTE requires explicit decider sign-off.

The capability vocabulary is documented in [`acceptance/CAPABILITIES.md`](../../acceptance/CAPABILITIES.md) — system, audio, storage, display, input, bridges (i2c / i2s / spi / onewire / gpio), communication (bluetooth / wifi / ir / zigbee / zwave / lirc), real-time / RTC, sensors, plus per-vendor extensions. The list is open: vendors add capability keys as their hardware surfaces require.
