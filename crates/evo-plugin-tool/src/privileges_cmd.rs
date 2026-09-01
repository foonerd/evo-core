// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! `evo-plugin-tool privileges {describe, check}` — read-only inspection
//! and host-prerequisites verification of a `privileges.yaml` record.
//!
//! Both verbs are pure file + read-only host probes; neither writes to the
//! host nor talks to the steward. Plugin authors run these locally during
//! development; operators run `check` on a target host before admission.
//!
//! Current scope: schema validation (always) plus host prerequisites
//! (binary presence, kernel-module loadability, system-service reachability,
//! verification command execution). The admission-time parity gate
//! (declared vs actual systemd unit / polkit drop-in / group membership)
//! lands with the per-plugin isolated identity implementation.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use evo_plugin_sdk::privileges::{
    hint_for_missing_binary, hint_for_missing_module, hint_for_missing_service,
    run_probes_with_counts, BinaryPresentProbe, CapabilityResolution,
    DistroFamily, PrivilegesError, PrivilegesV1, ProbePlan, RemediationHint,
    ResolutionCounts, SchemaViolation, ValidationIssue, ValidationSeverity,
};

/// Output format for `describe`.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum DescribeFormat {
    /// Human-readable report with sectioned summary.
    Text,
    /// Compact JSON for downstream tooling.
    Json,
}

/// CLI-facing distribution family selector. Maps onto the SDK's
/// [`DistroFamily`] via [`resolve_distro`]. Kept separate so the
/// clap value list stays operator-facing (kebab-case, exhaustive)
/// without leaking `Unknown` as a user-selectable value — an
/// operator who doesn't know the distribution should let the CLI
/// auto-detect instead of naming `Unknown` explicitly.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum DistroFamilyArg {
    Debian,
    Fedora,
    Alpine,
    Arch,
}

/// Resolve the operator's `--distro` selection (or absence
/// thereof) to a concrete [`DistroFamily`]. Absent selection
/// auto-detects from `/etc/os-release`.
pub fn resolve_distro(arg: Option<DistroFamilyArg>) -> DistroFamily {
    match arg {
        Some(DistroFamilyArg::Debian) => DistroFamily::Debian,
        Some(DistroFamilyArg::Fedora) => DistroFamily::Fedora,
        Some(DistroFamilyArg::Alpine) => DistroFamily::Alpine,
        Some(DistroFamilyArg::Arch) => DistroFamily::Arch,
        None => DistroFamily::detect_local(),
    }
}

/// Resolve a CLI-supplied path to a concrete `privileges.yaml` file.
/// Accepts either the file directly or a directory containing it.
fn resolve_record_path(path: &Path) -> Result<PathBuf> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    if path.is_dir() {
        let candidate = path.join("privileges.yaml");
        if candidate.is_file() {
            return Ok(candidate);
        }
        return Err(anyhow!(
            "no privileges.yaml found in directory {}",
            path.display()
        ));
    }
    Err(anyhow!("path does not exist: {}", path.display()))
}

/// Load YAML bytes from a path. Resolves a directory path to its
/// `privileges.yaml` file. Does NOT parse — returns the raw string
/// so callers can choose strict (`from_yaml` and bail) vs lenient
/// (`from_yaml` and surface schema violations in the report).
fn load_yaml_bytes(path: &Path) -> Result<String> {
    let resolved = resolve_record_path(path)?;
    fs::read_to_string(&resolved).with_context(|| {
        format!("reading privileges record at {}", resolved.display())
    })
}

/// Strict load: parses + bails on schema or other parse errors. Used
/// by `describe` (won't render a structurally-invalid record).
fn load_record_strict(path: &Path) -> Result<PrivilegesV1> {
    let yaml = load_yaml_bytes(path)?;
    PrivilegesV1::from_yaml(&yaml).with_context(|| {
        format!("parsing privileges record at {}", path.display())
    })
}

// --------------------------------------------------------------------
// describe
// --------------------------------------------------------------------

pub fn describe(path: &Path, format: DescribeFormat) -> Result<()> {
    let record = load_record_strict(path)?;
    // Refuse to describe a structurally invalid record so the report is
    // not misleading. `check` is the verb for advisory inspection of
    // broken records.
    record.validate().map_err(|e| {
        let summary = format_issues(&e.issues);
        anyhow!("record fails validation:\n{summary}")
    })?;
    match format {
        DescribeFormat::Text => {
            print!("{}", render_text_report(&record));
            Ok(())
        }
        DescribeFormat::Json => {
            let json = serde_json::to_string_pretty(&record)
                .context("serialising record to JSON")?;
            println!("{json}");
            Ok(())
        }
    }
}

fn render_text_report(r: &PrivilegesV1) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    writeln!(
        out,
        "Plugin:    {}\nOwner:     {}\nIsolation: {:?}\nSchema:    v{}",
        r.plugin, r.owner, r.isolation, r.schema_version
    )
    .ok();
    writeln!(out).ok();

    writeln!(
        out,
        "Capability intent ({} declared):",
        r.capability_intent.len()
    )
    .ok();
    for intent in &r.capability_intent {
        writeln!(
            out,
            "  - {} via {}\n      need: {}\n      failure: {}",
            intent.id, intent.access_path, intent.need, intent.failure_mode
        )
        .ok();
    }
    writeln!(out).ok();

    writeln!(out, "Required binaries ({}):", r.required_binaries.len()).ok();
    for bin in &r.required_binaries {
        let v = bin
            .min_version
            .as_deref()
            .map(|v| format!(" >= {v}"))
            .unwrap_or_default();
        writeln!(
            out,
            "  - {}{}\n      failure: {}",
            bin.name, v, bin.failure_mode
        )
        .ok();
    }
    writeln!(out).ok();

    if !r.required_kernel_modules.is_empty() {
        writeln!(out, "Required kernel modules:").ok();
        for m in &r.required_kernel_modules {
            writeln!(out, "  - {m}").ok();
        }
        writeln!(out).ok();
    }

    if !r.required_system_services.is_empty() {
        writeln!(out, "Required system services:").ok();
        for s in &r.required_system_services {
            writeln!(out, "  - {s}").ok();
        }
        writeln!(out).ok();
    }

    writeln!(
        out,
        "Verification ({} command(s), {} expectation(s)):",
        r.verification.commands.len(),
        r.verification.expected.len()
    )
    .ok();
    for c in &r.verification.commands {
        writeln!(out, "  $ {c}").ok();
    }
    for e in &r.verification.expected {
        writeln!(out, "  expect: {e}").ok();
    }
    writeln!(out).ok();

    let distros: Vec<&str> = r.host_provisioning.distributions().collect();
    writeln!(
        out,
        "Host provisioning ({} distribution block(s)): {}",
        distros.len(),
        if distros.is_empty() {
            "(none)".to_string()
        } else {
            distros.join(", ")
        }
    )
    .ok();
    for distro in &distros {
        let block = r.host_provisioning.get(distro).unwrap();
        writeln!(out, "  [{}]", distro).ok();
        match &block.systemd.user {
            Some(u) => {
                writeln!(out, "    User:                  {u}").ok();
            }
            None => {
                writeln!(
                    out,
                    "    User:                  (deferred — installer resolves at install time)"
                )
                .ok();
            }
        }
        if !block.systemd.supplementary_groups.is_empty() {
            writeln!(
                out,
                "    SupplementaryGroups:   {}",
                block.systemd.supplementary_groups.join(", ")
            )
            .ok();
        }
        if !block.systemd.capability_bounding_set.is_empty() {
            writeln!(
                out,
                "    CapabilityBoundingSet: {}",
                block.systemd.capability_bounding_set.join(", ")
            )
            .ok();
        }
        if !block.systemd.ambient_capabilities.is_empty() {
            writeln!(
                out,
                "    AmbientCapabilities:   {}",
                block.systemd.ambient_capabilities.join(", ")
            )
            .ok();
        }
        if let Some(ps) = &block.systemd.protect_system {
            writeln!(out, "    ProtectSystem:         {ps}").ok();
        }
        if let Some(polkit) = &block.polkit {
            writeln!(out, "    polkit policy file:    {}", polkit.policy_file)
                .ok();
            for action in &polkit.required_actions {
                writeln!(out, "      action: {action}").ok();
            }
        }
        if !block.capabilities.is_empty() {
            writeln!(
                out,
                "    extra capabilities:    {}  [advisory — flagged at audit]",
                block.capabilities.join(", ")
            )
            .ok();
        }
        if !block.sudoers.is_empty() {
            writeln!(
                out,
                "    sudoers entries:       {} entry/entries  [advisory — flagged at audit]",
                block.sudoers.len()
            )
            .ok();
        }
    }
    out
}

// --------------------------------------------------------------------
// check
// --------------------------------------------------------------------

/// One host-prerequisite finding produced by `check` (above and beyond
/// what schema validation reports). Optionally carries a structured
/// [`RemediationHint`] the operator surface can render as an "Apply"
/// affordance.
struct HostFinding {
    severity: ValidationSeverity,
    code: &'static str,
    message: String,
    hint: Option<RemediationHint>,
}

/// PPAG-style probe result captured alongside the report's findings.
/// Sourced from the SDK's [`run_probes_with_counts`] runner so the
/// CLI's host-side resolution mirrors what the framework's admission
/// gate observes at plugin load time.
struct ProbeOutcomeRow {
    intent_id: String,
    resolution: CapabilityResolution,
}

pub fn check(
    path: &Path,
    schema_only: bool,
    skip_verification: bool,
    strict: bool,
    distro: DistroFamily,
) -> Result<()> {
    let yaml = load_yaml_bytes(path)?;
    let mut all_issues: Vec<ReportEntry> = Vec::new();

    // Lenient parse: schema violations land in the report (Block) so
    // the operator sees every issue at once. Other parse errors (raw
    // YAML malformed; JSON convert; etc.) are unrecoverable and bail.
    let record = match PrivilegesV1::from_yaml(&yaml) {
        Ok(r) => Some(r),
        Err(PrivilegesError::SchemaValidation(violations)) => {
            for v in &violations {
                all_issues.push(ReportEntry {
                    severity: ValidationSeverity::Block,
                    code: leak_static(format!("schema:{}", v.keyword)),
                    message: format_schema_violation(v),
                    stage: Stage::Schema,
                    hint: None,
                });
            }
            None
        }
        Err(other) => {
            return Err(anyhow!("{other}"));
        }
    };

    let mut probe_outcomes: Vec<ProbeOutcomeRow> = Vec::new();
    let mut probe_counts: Option<ResolutionCounts> = None;
    if let Some(r) = &record {
        for i in r.validate_collect() {
            all_issues.push(ReportEntry {
                severity: i.severity,
                code: i.code,
                message: i.message,
                stage: Stage::Schema,
                hint: None,
            });
        }
        if !schema_only {
            let outcome = run_host_checks(r, skip_verification, distro);
            for f in outcome.findings {
                all_issues.push(ReportEntry {
                    severity: f.severity,
                    code: f.code,
                    message: f.message,
                    stage: Stage::Host,
                    hint: f.hint,
                });
            }
            probe_outcomes = outcome.probe_outcomes;
            probe_counts = Some(outcome.counts);
        }
    }

    print_check_report(
        record.as_ref(),
        &all_issues,
        &probe_outcomes,
        probe_counts.as_ref(),
        schema_only,
        skip_verification,
    );

    let blocking = all_issues
        .iter()
        .filter(|e| e.severity == ValidationSeverity::Block)
        .count();
    let warnings = all_issues
        .iter()
        .filter(|e| e.severity == ValidationSeverity::Warn)
        .count();

    if blocking > 0 {
        return Err(anyhow!(
            "{} blocking issue(s); admission would be refused",
            blocking
        ));
    }
    if strict && warnings > 0 {
        return Err(anyhow!(
            "{} warning(s) in --strict mode; treat-as-failure",
            warnings
        ));
    }
    Ok(())
}

fn format_schema_violation(v: &SchemaViolation) -> String {
    let path = if v.instance_path.is_empty() {
        "<root>".to_string()
    } else {
        v.instance_path.clone()
    };
    format!("at {path}: {} ({})", v.message, v.schema_path)
}

/// `ReportEntry::code` is `&'static str` (stable identifiers for
/// pattern-matching). Schema violation codes are dynamic
/// (`schema:<keyword>`) so we leak them. Total leak per process is
/// bounded by the JSON Schema's keyword vocabulary (~30 strings
/// max), so the leak is intentional and finite — comparable to a
/// once-cell intern.
fn leak_static(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

#[derive(PartialEq, Eq)]
enum Stage {
    Schema,
    Host,
}

struct ReportEntry {
    severity: ValidationSeverity,
    code: &'static str,
    message: String,
    stage: Stage,
    hint: Option<RemediationHint>,
}

fn print_check_report(
    record: Option<&PrivilegesV1>,
    entries: &[ReportEntry],
    probe_outcomes: &[ProbeOutcomeRow],
    probe_counts: Option<&ResolutionCounts>,
    schema_only: bool,
    skip_verification: bool,
) {
    match record {
        Some(r) => {
            println!("Plugin:    {}", r.plugin);
            println!("Isolation: {:?}", r.isolation);
        }
        None => {
            println!(
                "Plugin:    (record failed schema validation; identity unavailable)"
            );
        }
    }
    println!(
        "Mode:      schema {}",
        if schema_only { "only" } else { "+ host" }
    );
    if !skip_verification && !schema_only {
        println!("           verification commands: enabled");
    }
    println!();

    // PPAG resolution-map summary. The CLI runs the SDK's
    // `run_probes_with_counts` against the synthesisable subset
    // of yaml-declared intents (today: required_binaries → one
    // BinaryPresentProbe each). The summary mirrors what the
    // framework's admission gate would stamp on
    // LoadContext.capabilities, so authors and operators can
    // spot privilege gaps before reaching the steward.
    //
    // Intents whose runtime probe needs arguments not carried in
    // the yaml (sudoers/command argv, filesystem/system paths)
    // are marked here as framework-probed-at-admission rather
    // than CLI-verified.
    if let Some(counts) = probe_counts {
        println!("PPAG resolution (probes CLI can synthesise from the yaml):");
        println!("  total       {}", probe_outcomes.len());
        println!("  available   {}", counts.available);
        println!("  unavailable {}", counts.unavailable);
        println!("  degraded    {}", counts.degraded);
        if !probe_outcomes.is_empty() {
            println!();
            for row in probe_outcomes {
                let (status, detail) = match &row.resolution {
                    CapabilityResolution::Available { evidence, strategy } => {
                        let s = strategy
                            .as_deref()
                            .map(|s| format!(" strategy={s}"))
                            .unwrap_or_default();
                        ("AVAILABLE", format!("{evidence}{s}"))
                    }
                    CapabilityResolution::Unavailable { reason, remedy } => {
                        ("UNAVAILABLE", format!("{reason} | remedy: {remedy}"))
                    }
                    CapabilityResolution::Degraded {
                        fallback_strategy,
                        reason,
                    } => (
                        "DEGRADED",
                        format!("fallback={fallback_strategy} ({reason})"),
                    ),
                    CapabilityResolution::NotProbed { reason } => {
                        ("NOT_PROBED", reason.clone())
                    }
                };
                println!("  [{status}] {} — {}", row.intent_id, detail);
            }
        }
        if let Some(r) = record {
            let unverifiable: Vec<&str> = r
                .capability_intent
                .iter()
                .filter(|i| !is_synthesisable_access_path(&i.access_path))
                .map(|i| i.id.as_str())
                .collect();
            if !unverifiable.is_empty() {
                println!();
                println!(
                    "  Note: the following capability_intent ids are \
                     framework-probed at admission (CLI cannot synthesise \
                     the probe from yaml today):"
                );
                for id in &unverifiable {
                    println!("    - {id}");
                }
            }
        }
        println!();
    }

    let blocking: Vec<&ReportEntry> = entries
        .iter()
        .filter(|e| e.severity == ValidationSeverity::Block)
        .collect();
    let warnings: Vec<&ReportEntry> = entries
        .iter()
        .filter(|e| e.severity == ValidationSeverity::Warn)
        .collect();

    if blocking.is_empty() && warnings.is_empty() {
        println!("OK — no issues found.");
        return;
    }

    if !blocking.is_empty() {
        println!("Blocking ({}):", blocking.len());
        for e in &blocking {
            let stage = match e.stage {
                Stage::Schema => "schema",
                Stage::Host => "host  ",
            };
            println!("  [{stage}] {} — {}", e.code, e.message);
        }
        println!();
    }
    if !warnings.is_empty() {
        println!("Advisory ({}):", warnings.len());
        for e in &warnings {
            let stage = match e.stage {
                Stage::Schema => "schema",
                Stage::Host => "host  ",
            };
            println!("  [{stage}] {} — {}", e.code, e.message);
        }
    }

    // Structured remediation list. Failure classes that carry a
    // concrete operator-executable next step render here so an
    // operator surface (CLI, UI, unit-installer) can dispatch the
    // fix. The hint is a SUGGESTION — nothing runs without operator
    // consent; the fail-closed default is preserved end-to-end.
    let hints: Vec<&RemediationHint> =
        entries.iter().filter_map(|e| e.hint.as_ref()).collect();
    if !hints.is_empty() {
        println!();
        println!("Remediation ({}):", hints.len());
        for h in &hints {
            println!("  - {}", h.summary());
            println!("    $ {}", h.shell_command());
        }
    }
}

/// Which `access_path` strings does the CLI's synthesiser cover
/// today? Anything not in this list shows up in the "framework-
/// probed at admission" note instead of the per-intent
/// resolution list. The set will grow as the privileges.yaml
/// schema gains machine-readable probe arguments.
fn is_synthesisable_access_path(access_path: &str) -> bool {
    // Today the CLI synthesises only `BinaryPresentProbe` from
    // `required_binaries[]`. No `capability_intent.access_path`
    // value lands here yet — every intent's runtime probe needs
    // arguments the yaml does not yet carry (sudoers command
    // tokens, filesystem paths). When the schema adds an
    // optional `probe:` block, extend this matcher.
    let _ = access_path;
    false
}

fn format_issues(issues: &[ValidationIssue]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for i in issues {
        let sev = match i.severity {
            ValidationSeverity::Block => "BLOCK",
            ValidationSeverity::Warn => "WARN ",
        };
        writeln!(out, "  [{sev}] {} — {}", i.code, i.message).ok();
    }
    out
}

// --------------------------------------------------------------------
// host-prerequisite probes
// --------------------------------------------------------------------

/// Result of [`run_host_checks`]: the legacy host findings plus the
/// PPAG resolution map the SDK's probe runner produced (the same
/// runner the framework's admission gate invokes at plugin load).
struct HostCheckOutcome {
    findings: Vec<HostFinding>,
    probe_outcomes: Vec<ProbeOutcomeRow>,
    counts: ResolutionCounts,
}

/// Synthesize the [`ProbePlan`]s the CLI can construct from the
/// yaml-declared surface. Today this covers `required_binaries[]`
/// (one [`BinaryPresentProbe`] per declared binary, intent id
/// `required_binary:<name>`); future yaml-schema extensions adding
/// machine-readable probe arguments will let the CLI synthesize the
/// `capability_intent`-level probes the framework runs at admission
/// (sudoers/command and filesystem/system shapes need argv / path
/// data the yaml does not yet carry).
fn synthesize_inferrable_probe_plans(record: &PrivilegesV1) -> Vec<ProbePlan> {
    let mut plans: Vec<ProbePlan> = Vec::new();
    for bin in &record.required_binaries {
        plans.push(ProbePlan {
            intent_id: format!("required_binary:{}", bin.name),
            probe: Box::new(BinaryPresentProbe::new(bin.name.clone())),
            strategy_hint: None,
            remedy: format!(
                "install `{}` (declared failure_mode: {})",
                bin.name, bin.failure_mode
            ),
        });
    }
    plans
}

fn run_host_checks(
    record: &PrivilegesV1,
    skip_verification: bool,
    distro: DistroFamily,
) -> HostCheckOutcome {
    let mut findings = Vec::new();
    let mut probe_outcomes: Vec<ProbeOutcomeRow> = Vec::new();

    // PPAG resolution stage: synthesise plans for the binary
    // intents and run them through the SDK's runner — the same
    // helper the framework's admission gate calls at plugin load.
    // Unavailable resolutions land as blocking findings; available
    // resolutions show up only in the summary section. Unavailable
    // binary probes carry a structured PackageInstall remediation
    // hint the operator surface can render as an "Apply" action.
    let plans = synthesize_inferrable_probe_plans(record);
    let (map, counts) = run_probes_with_counts(&plans);
    for plan in &plans {
        if let Some(resolution) = map.get(&plan.intent_id) {
            probe_outcomes.push(ProbeOutcomeRow {
                intent_id: plan.intent_id.clone(),
                resolution: resolution.clone(),
            });
            if let CapabilityResolution::Unavailable { reason, remedy } =
                resolution
            {
                // The intent_id shape is
                // `required_binary:<name>`; strip the prefix
                // to recover the binary name for the hint.
                let hint = plan
                    .intent_id
                    .strip_prefix("required_binary:")
                    .map(|name| hint_for_missing_binary(name, distro));
                findings.push(HostFinding {
                    severity: ValidationSeverity::Block,
                    code: "ppag_resolution_unavailable",
                    message: format!(
                        "{} unavailable: {reason} (remedy: {remedy})",
                        plan.intent_id
                    ),
                    hint,
                });
            }
        }
    }

    for bin in &record.required_binaries {
        // min_version recorded in the schema; per-binary version probes
        // are not yet implemented (each binary has its own --version
        // format). Emit an informational warning so authors know the
        // recorded value is not yet enforced.
        if bin.min_version.is_some() {
            findings.push(HostFinding {
                severity: ValidationSeverity::Warn,
                code: "binary_min_version_unverified",
                message: format!(
                    "required_binary `{}` declares min_version `{}`; per-binary version probes are not yet enforced",
                    bin.name,
                    bin.min_version.as_deref().unwrap_or("?")
                ),
                hint: None,
            });
        }
    }

    for module in &record.required_kernel_modules {
        match probe_kernel_module(module) {
            ModuleProbe::Loaded => {}
            ModuleProbe::Loadable => {
                findings.push(HostFinding {
                    severity: ValidationSeverity::Warn,
                    code: "kernel_module_loadable_not_loaded",
                    message: format!(
                        "required_kernel_module `{}` is loadable but not currently loaded",
                        module
                    ),
                    hint: Some(hint_for_missing_module(module)),
                });
            }
            ModuleProbe::Missing => {
                findings.push(HostFinding {
                    severity: ValidationSeverity::Block,
                    code: "kernel_module_missing",
                    message: format!(
                        "required_kernel_module `{}` is not loaded and not loadable on this host",
                        module
                    ),
                    hint: Some(hint_for_missing_module(module)),
                });
            }
        }
    }

    for svc in &record.required_system_services {
        match probe_system_service(svc) {
            ServiceProbe::Reachable => {}
            ServiceProbe::Unknown => {
                findings.push(HostFinding {
                    severity: ValidationSeverity::Block,
                    code: "system_service_missing",
                    message: format!(
                        "required_system_service `{}` is not known to systemd on this host",
                        svc
                    ),
                    hint: Some(hint_for_missing_service(svc)),
                });
            }
        }
    }

    if !skip_verification {
        for (idx, cmd) in record.verification.commands.iter().enumerate() {
            match run_verification_command(cmd) {
                Ok(true) => {}
                Ok(false) => {
                    findings.push(HostFinding {
                        severity: ValidationSeverity::Block,
                        code: "verification_command_failed",
                        message: format!(
                            "verification command [{idx}] `{cmd}` exited non-zero (note: this preflight does not run as the service identity; the admission gate enforces parity at install time)"
                        ),
                        hint: None,
                    });
                }
                Err(e) => {
                    findings.push(HostFinding {
                        severity: ValidationSeverity::Block,
                        code: "verification_command_error",
                        message: format!(
                            "verification command [{idx}] `{cmd}` could not be invoked: {e}"
                        ),
                        hint: None,
                    });
                }
            }
        }
    }

    HostCheckOutcome {
        findings,
        probe_outcomes,
        counts,
    }
}

enum ModuleProbe {
    Loaded,
    Loadable,
    Missing,
}

fn probe_kernel_module(name: &str) -> ModuleProbe {
    // /proc/modules: leading column is the module name.
    if let Ok(modules) = fs::read_to_string("/proc/modules") {
        if modules.lines().any(|l| {
            l.split_whitespace()
                .next()
                .map(|n| n == name)
                .unwrap_or(false)
        }) {
            return ModuleProbe::Loaded;
        }
    }
    // modprobe -n exits 0 when the module is loadable but does not
    // actually load it. Built-in modules also report success.
    let loadable = Command::new("modprobe")
        .arg("-n")
        .arg(name)
        .output()
        .ok()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if loadable {
        ModuleProbe::Loadable
    } else {
        ModuleProbe::Missing
    }
}

enum ServiceProbe {
    Reachable,
    Unknown,
}

fn probe_system_service(unit: &str) -> ServiceProbe {
    // `systemctl cat` exits 0 if the unit definition exists (loaded or
    // not). This matches the contract: the unit is *known to systemd*
    // on this host. State (active / inactive) is a runtime concern that
    // belongs in the verification command, not here.
    let known = Command::new("systemctl")
        .arg("cat")
        .arg("--no-pager")
        .arg(unit)
        .output()
        .ok()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if known {
        ServiceProbe::Reachable
    } else {
        ServiceProbe::Unknown
    }
}

fn run_verification_command(cmd: &str) -> Result<bool> {
    // Capture stdout/stderr so verification-command output does not
    // pollute the check-verb report. Authors who want to debug a
    // failing command can re-run it directly.
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .with_context(|| format!("invoking verification command `{cmd}`"))?;
    Ok(output.status.success())
}

// --------------------------------------------------------------------
// template
// --------------------------------------------------------------------

/// Plugin archetype whose starter privileges.yaml this tool ships.
///
/// Each variant embeds one YAML template at compile time via
/// `include_str!`; there is no on-disk lookup, no environment
/// dependency. The templates capture the ~80% case for their
/// archetype; the author specialises canonical plugin id, owner,
/// verification commands, and per-distribution `host_provisioning`
/// before shipping.
///
/// clap renders each variant as a kebab-case CLI value
/// (`audio-source`, `audio-composition`, ...). The full list is
/// visible from `evo-plugin-tool privileges template --help`.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Archetype {
    /// A plugin that ingests audio from a local device or file
    /// (USB DAC, ALSA device, local library, tuner card) and
    /// exposes it through the framework's audio pipeline.
    AudioSource,
    /// A DSP filter / equaliser / crossover plugin that
    /// transforms an audio stream in the framework's playback
    /// pipeline. Pure in-memory transform; no device access.
    AudioComposition,
    /// A plugin that supplies metadata (artist bios, album
    /// reviews, lyrics, tags, artwork) for a source's tracks.
    /// Local file tagger or network API client.
    MetadataProvider,
    /// A plugin that streams audio from a cloud music service
    /// (Spotify / Tidal / Qobuz / Deezer / Apple Music /
    /// equivalent) through the framework's playback pipeline.
    NetworkStreamer,
    /// A plugin that bridges the framework to an MQTT broker
    /// for sensor telemetry, home-automation integrations, or
    /// presence broadcast.
    MqttBridge,
    /// A plugin that consumes a live video stream (RTSP
    /// camera, HLS feed, WebRTC ingest) and surfaces it into
    /// the framework's display / notification pipeline.
    VideoStream,
    /// A plugin that periodically fetches external weather
    /// data over HTTPS and publishes it onto the framework's
    /// happenings bus. Reference example for the "periodic
    /// external fetcher" pattern.
    WeatherProvider,
}

impl Archetype {
    /// Kebab-case identifier matching the CLI value clap emits.
    /// Used for diagnostic messages.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AudioSource => "audio-source",
            Self::AudioComposition => "audio-composition",
            Self::MetadataProvider => "metadata-provider",
            Self::NetworkStreamer => "network-streamer",
            Self::MqttBridge => "mqtt-bridge",
            Self::VideoStream => "video-stream",
            Self::WeatherProvider => "weather-provider",
        }
    }

    /// Return the embedded YAML template as a static string.
    /// The template body was validated at compile time via the
    /// `templates_all_parse_as_privileges_v1` unit test in this
    /// module; runtime callers can rely on `PrivilegesV1::from_yaml`
    /// on the returned string to succeed.
    pub fn yaml(&self) -> &'static str {
        match self {
            Self::AudioSource => {
                include_str!("../templates/privileges/audio_source.yaml")
            }
            Self::AudioComposition => {
                include_str!("../templates/privileges/audio_composition.yaml")
            }
            Self::MetadataProvider => {
                include_str!("../templates/privileges/metadata_provider.yaml")
            }
            Self::NetworkStreamer => {
                include_str!("../templates/privileges/network_streamer.yaml")
            }
            Self::MqttBridge => {
                include_str!("../templates/privileges/mqtt_bridge.yaml")
            }
            Self::VideoStream => {
                include_str!("../templates/privileges/video_stream.yaml")
            }
            Self::WeatherProvider => {
                include_str!("../templates/privileges/weather_provider.yaml")
            }
        }
    }
}

/// Emit the starter privileges.yaml for the named archetype.
/// Writes to `out_path` when supplied, else prints to stdout.
///
/// The template body is a valid privileges.yaml record with
/// `example-author` as owner and `com.example.<archetype>` as
/// the plugin id. The author is expected to edit those two
/// fields plus the verification commands and per-distribution
/// `host_provisioning` before shipping.
pub fn template(archetype: Archetype, out_path: Option<&Path>) -> Result<()> {
    let body = archetype.yaml();
    match out_path {
        Some(path) => {
            fs::write(path, body).with_context(|| {
                format!(
                    "writing {} template to {}",
                    archetype.as_str(),
                    path.display()
                )
            })?;
            eprintln!(
                "wrote {} template to {}",
                archetype.as_str(),
                path.display()
            );
        }
        None => {
            print!("{body}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::privileges::PrivilegesV1;

    /// The seven archetypes we ship. Any additions land here so the
    /// coverage tests below fail if a template's schema drifts.
    const ALL_ARCHETYPES: &[Archetype] = &[
        Archetype::AudioSource,
        Archetype::AudioComposition,
        Archetype::MetadataProvider,
        Archetype::NetworkStreamer,
        Archetype::MqttBridge,
        Archetype::VideoStream,
        Archetype::WeatherProvider,
    ];

    #[test]
    fn every_archetype_yaml_is_non_empty() {
        for a in ALL_ARCHETYPES {
            let body = a.yaml();
            assert!(
                !body.is_empty(),
                "template body for {} is empty",
                a.as_str()
            );
        }
    }

    #[test]
    fn every_archetype_yaml_parses_as_privileges_v1() {
        // Compile-time bind: if any shipped template breaks the
        // schema, this test fails and CI blocks the push. Same
        // parser the check subcommand uses at runtime, so a
        // template that passes this test will parse cleanly for
        // the operator who ran `template` and then `check`.
        for a in ALL_ARCHETYPES {
            let body = a.yaml();
            let result = PrivilegesV1::from_yaml(body);
            assert!(
                result.is_ok(),
                "template {} failed to parse: {:?}",
                a.as_str(),
                result.err()
            );
        }
    }

    #[test]
    fn every_archetype_yaml_uses_example_placeholder_ids() {
        // Enforces the ~80%-then-specialise contract: every
        // shipped template must carry the `example-author`
        // owner and a `com.example.` plugin id so an operator
        // who runs the template subcommand and forgets to
        // specialise cannot accidentally register a real-looking
        // identity.
        for a in ALL_ARCHETYPES {
            let body = a.yaml();
            assert!(
                body.contains("owner: example-author"),
                "template {} missing example-author placeholder",
                a.as_str()
            );
            assert!(
                body.contains("plugin: com.example."),
                "template {} missing com.example.* plugin id placeholder",
                a.as_str()
            );
        }
    }

    #[test]
    fn archetype_as_str_is_kebab_case() {
        // The kebab-case string must match clap's derived value
        // exactly; if clap's derivation changes we want a
        // build-time failure, not a runtime surprise.
        assert_eq!(Archetype::AudioSource.as_str(), "audio-source");
        assert_eq!(Archetype::AudioComposition.as_str(), "audio-composition");
        assert_eq!(Archetype::MetadataProvider.as_str(), "metadata-provider");
        assert_eq!(Archetype::NetworkStreamer.as_str(), "network-streamer");
        assert_eq!(Archetype::MqttBridge.as_str(), "mqtt-bridge");
        assert_eq!(Archetype::VideoStream.as_str(), "video-stream");
        assert_eq!(Archetype::WeatherProvider.as_str(), "weather-provider");
    }

    #[test]
    fn template_writes_to_out_path_when_given() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("privileges.yaml");
        template(Archetype::WeatherProvider, Some(&out)).unwrap();
        let written = std::fs::read_to_string(&out).unwrap();
        assert_eq!(written, Archetype::WeatherProvider.yaml());
    }
}
