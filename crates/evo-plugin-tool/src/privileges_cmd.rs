//! `evo-plugin-tool privileges {describe, check}` — read-only inspection
//! and host-prerequisites verification of a `privileges.yaml` record.
//!
//! Both verbs are pure file + read-only host probes; neither writes to the
//! host nor talks to the steward. Plugin authors run these locally during
//! development; operators run `check` on a target host before admission.
//!
//! Scope for v0.1.12.1: schema validation (always) plus host prerequisites
//! (binary presence, kernel-module loadability, system-service reachability,
//! verification command execution). The admission-time parity gate
//! (declared vs actual systemd unit / polkit drop-in / group membership)
//! lands with the per-plugin isolated identity implementation in v0.1.13.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use evo_plugin_sdk::privileges::{
    PrivilegesError, PrivilegesV1, SchemaViolation, ValidationIssue,
    ValidationSeverity,
};

/// Output format for `describe`.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum DescribeFormat {
    /// Human-readable report with sectioned summary.
    Text,
    /// Compact JSON for downstream tooling.
    Json,
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
/// what schema validation reports).
struct HostFinding {
    severity: ValidationSeverity,
    code: &'static str,
    message: String,
}

pub fn check(
    path: &Path,
    schema_only: bool,
    skip_verification: bool,
    strict: bool,
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
                });
            }
            None
        }
        Err(other) => {
            return Err(anyhow!("{other}"));
        }
    };

    if let Some(r) = &record {
        for i in r.validate_collect() {
            all_issues.push(ReportEntry {
                severity: i.severity,
                code: i.code,
                message: i.message,
                stage: Stage::Schema,
            });
        }
        if !schema_only {
            for f in run_host_checks(r, skip_verification) {
                all_issues.push(ReportEntry {
                    severity: f.severity,
                    code: f.code,
                    message: f.message,
                    stage: Stage::Host,
                });
            }
        }
    }

    print_check_report(
        record.as_ref(),
        &all_issues,
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
}

fn print_check_report(
    record: Option<&PrivilegesV1>,
    entries: &[ReportEntry],
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

fn run_host_checks(
    record: &PrivilegesV1,
    skip_verification: bool,
) -> Vec<HostFinding> {
    let mut findings = Vec::new();

    for bin in &record.required_binaries {
        match probe_binary(&bin.name) {
            BinaryProbe::Present => {}
            BinaryProbe::Missing => {
                findings.push(HostFinding {
                    severity: ValidationSeverity::Block,
                    code: "binary_missing",
                    message: format!(
                        "required_binary `{}` not found on PATH (declared failure_mode: {})",
                        bin.name, bin.failure_mode
                    ),
                });
            }
        }
        // min_version recorded in the schema; per-binary version probes
        // are not implemented in v0.1.12.1 (each binary has its own
        // --version format). Emit an informational warning so authors
        // know the recorded value is not yet enforced.
        if bin.min_version.is_some() {
            findings.push(HostFinding {
                severity: ValidationSeverity::Warn,
                code: "binary_min_version_unverified",
                message: format!(
                    "required_binary `{}` declares min_version `{}`; per-binary version probes land in v0.1.13",
                    bin.name,
                    bin.min_version.as_deref().unwrap_or("?")
                ),
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
                            "verification command [{idx}] `{cmd}` exited non-zero (note: not run as service identity in v0.1.12.1; admission gate runs in v0.1.13)"
                        ),
                    });
                }
                Err(e) => {
                    findings.push(HostFinding {
                        severity: ValidationSeverity::Block,
                        code: "verification_command_error",
                        message: format!(
                            "verification command [{idx}] `{cmd}` could not be invoked: {e}"
                        ),
                    });
                }
            }
        }
    }

    findings
}

enum BinaryProbe {
    Present,
    Missing,
}

fn probe_binary(name: &str) -> BinaryProbe {
    // Use the `which` crate: native PATH walk, no fork+exec. Mirrors
    // POSIX `which` semantics (returns first executable in $PATH).
    match which::which(name) {
        Ok(_) => BinaryProbe::Present,
        Err(_) => BinaryProbe::Missing,
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
