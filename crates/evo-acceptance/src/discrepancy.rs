//! Discrepancy rules — compares the target descriptor's declared
//! capabilities against the inspector findings and surfaces
//! mismatches.
//!
//! v1 carries a small fixed set of rules. Add rules here as the
//! inspection coverage grows; each rule is a pure function over
//! (target, findings) returning zero or more discrepancies.

use serde::{Deserialize, Serialize};

use crate::inspector::InspectorFinding;
use crate::target::TargetDescriptor;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Discrepancy {
    pub severity: Severity,
    /// What the discrepancy is about — typically a capability
    /// name, a subsystem name, or a config key.
    pub subject: String,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Severity {
    /// Active mismatch that affects scenario validity (e.g.
    /// declared capability has no kernel module loaded).
    Block,
    /// Surface-level mismatch worth recording (e.g. config.txt
    /// auto-loads HDMI but only DSI is connected).
    Warn,
    /// Informational — the inspection found something the
    /// descriptor does not declare; might or might not be
    /// intentional.
    Info,
}

pub fn evaluate(
    target: &TargetDescriptor,
    findings: &[InspectorFinding],
) -> Vec<Discrepancy> {
    let mut out = Vec::new();
    for rule in RULES {
        rule(target, findings, &mut out);
    }
    out
}

type Rule = fn(&TargetDescriptor, &[InspectorFinding], &mut Vec<Discrepancy>);

const RULES: &[Rule] = &[
    rule_module_for_i2c,
    rule_module_for_bluetooth,
    rule_drm_connector_state,
    rule_input_subsystem_for_input_caps,
];

/// `bridge_i2c` capability declared but `i2c_dev` module absent.
fn rule_module_for_i2c(
    target: &TargetDescriptor,
    findings: &[InspectorFinding],
    out: &mut Vec<Discrepancy>,
) {
    if !target.has_capability("bridge_i2c") {
        return;
    }
    let Some(lsmod) = findings.iter().find(|f| f.inspector == "loaded_modules")
    else {
        return;
    };
    if !lsmod
        .stdout
        .lines()
        .any(|l| l.starts_with("i2c_dev ") || l.starts_with("i2c_bcm"))
    {
        out.push(Discrepancy {
            severity: Severity::Block,
            subject: "bridge_i2c".to_string(),
            detail: "Capability declared but no i2c kernel module (i2c_dev / i2c_bcm*) appears in lsmod.".to_string(),
        });
    }
}

/// `comm_bluetooth` declared but `bluetooth` / `btusb` module absent.
fn rule_module_for_bluetooth(
    target: &TargetDescriptor,
    findings: &[InspectorFinding],
    out: &mut Vec<Discrepancy>,
) {
    if !target.has_capability("comm_bluetooth") {
        return;
    }
    let Some(lsmod) = findings.iter().find(|f| f.inspector == "loaded_modules")
    else {
        return;
    };
    let has_bt = lsmod.stdout.lines().any(|l| {
        l.starts_with("bluetooth ")
            || l.starts_with("btusb ")
            || l.starts_with("hci_uart ")
    });
    if !has_bt {
        out.push(Discrepancy {
            severity: Severity::Block,
            subject: "comm_bluetooth".to_string(),
            detail: "Capability declared but no bluetooth kernel module (bluetooth / btusb / hci_uart) appears in lsmod.".to_string(),
        });
    }
}

/// Display autodetect mismatch: scan DRM connectors. For each
/// declared `display_<kind>` capability, verify a connected
/// connector of the matching DRM type. Conversely, surface a
/// connector that is `connected` but not declared.
fn rule_drm_connector_state(
    target: &TargetDescriptor,
    findings: &[InspectorFinding],
    out: &mut Vec<Discrepancy>,
) {
    let Some(drm) = findings.iter().find(|f| f.inspector == "drm_connectors")
    else {
        return;
    };

    let mut connected_kinds: Vec<&str> = Vec::new();
    for line in drm.stdout.lines() {
        if !line.contains("status=connected") {
            continue;
        }
        if line.contains("HDMI") {
            connected_kinds.push("hdmi");
        } else if line.contains("DSI") {
            connected_kinds.push("dsi");
        } else if line.contains("DP") {
            connected_kinds.push("dp");
        } else if line.contains("DPI") {
            connected_kinds.push("dpi");
        }
    }

    for kind in ["hdmi", "dsi", "dp", "dpi"] {
        let cap = format!("display_{kind}");
        let declared = target.has_capability(&cap);
        let connected = connected_kinds.contains(&kind);
        match (declared, connected) {
            (true, false) => out.push(Discrepancy {
                severity: Severity::Warn,
                subject: cap.clone(),
                detail: format!("Capability declared but no DRM connector of {kind:?} kind reports status=connected."),
            }),
            (false, true) => out.push(Discrepancy {
                severity: Severity::Info,
                subject: cap.clone(),
                detail: format!("DRM connector of {kind:?} kind reports status=connected but capability is not declared in the target descriptor."),
            }),
            _ => {}
        }
    }
}

/// `input_*` capabilities declared but no input subsystem present
/// (no /proc/bus/input/devices or empty).
fn rule_input_subsystem_for_input_caps(
    target: &TargetDescriptor,
    findings: &[InspectorFinding],
    out: &mut Vec<Discrepancy>,
) {
    let any_input_cap = target
        .capabilities
        .iter()
        .any(|c| c.name.starts_with("input_"));
    if !any_input_cap {
        return;
    }
    let Some(input) = findings.iter().find(|f| f.inspector == "input_devices")
    else {
        return;
    };
    if input.stdout.contains("input subsystem absent")
        || input.stdout.trim().is_empty()
    {
        out.push(Discrepancy {
            severity: Severity::Block,
            subject: "input_*".to_string(),
            detail: "Capability declared (one or more `input_*`) but the input subsystem is absent or empty.".to_string(),
        });
    }
}
