//! Inspectors — pre-flight target introspection.
//!
//! Each inspector runs one introspection command on the target,
//! captures the raw output, and returns it as a structured
//! finding. Findings feed into the discrepancy engine which
//! compares the target's declared capabilities against what is
//! actually present, loaded, or configured.
//!
//! Inspectors are deliberately read-only and side-effect-free.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::connection::Connection;
use crate::error::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectorFinding {
    pub inspector: String,
    pub command: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

pub enum Inspector {
    /// `lsmod` — kernel modules currently loaded.
    LoadedModules,
    /// HID device tree via `/sys/class/hidraw/` plus `lsusb -t`.
    HidDevices,
    /// `i2cdetect -y <bus>` for each bus the kernel exposes.
    /// Discovers buses via `ls /sys/class/i2c-dev/`.
    I2cBuses,
    /// `cat /boot/firmware/config.txt` (Pi 5) with fallback to
    /// `/boot/config.txt` (older Pi). Returns empty stdout on
    /// non-Pi targets.
    BootConfig,
    /// `cat /proc/cmdline` — running kernel's command line.
    KernelCmdline,
    /// `ls /sys/firmware/devicetree/base/` plus
    /// `ls /sys/firmware/devicetree/overlays/` — applied DT
    /// overlays. Returns empty on non-DT systems.
    DtOverlays,
    /// KMS / DRM chain: connectors, their status, modes,
    /// and the device-class linkage from /sys/class/drm/.
    DrmConnectors,
    /// Input devices via /proc/bus/input/devices — covers
    /// keyboards, mice, touchscreens, GPIO buttons, evdev
    /// sources of all kinds.
    InputDevices,
    /// IIO bus sensors: rotation / gyroscope, accelerometer,
    /// magnetometer, light, proximity, gravity.
    /// /sys/bus/iio/devices/iio:device*/name + ./type.
    IioSensors,
}

impl Inspector {
    pub fn all() -> [Self; 9] {
        [
            Self::LoadedModules,
            Self::HidDevices,
            Self::I2cBuses,
            Self::BootConfig,
            Self::KernelCmdline,
            Self::DtOverlays,
            Self::DrmConnectors,
            Self::InputDevices,
            Self::IioSensors,
        ]
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::LoadedModules => "loaded_modules",
            Self::HidDevices => "hid_devices",
            Self::I2cBuses => "i2c_buses",
            Self::BootConfig => "boot_config",
            Self::KernelCmdline => "kernel_cmdline",
            Self::DtOverlays => "dt_overlays",
            Self::DrmConnectors => "drm_connectors",
            Self::InputDevices => "input_devices",
            Self::IioSensors => "iio_sensors",
        }
    }

    pub fn command(&self) -> &'static str {
        match self {
            Self::LoadedModules => "lsmod",
            Self::HidDevices => "echo '--- /sys/class/hidraw/ ---'; ls -la /sys/class/hidraw/ 2>/dev/null || true; echo; echo '--- lsusb -t ---'; lsusb -t 2>/dev/null || true",
            Self::I2cBuses => "for b in /sys/class/i2c-dev/i2c-*; do n=${b##*-}; echo \"--- bus $n ---\"; sudo i2cdetect -y \"$n\" 2>&1 || true; done",
            Self::BootConfig => "if [ -r /boot/firmware/config.txt ]; then cat /boot/firmware/config.txt; elif [ -r /boot/config.txt ]; then cat /boot/config.txt; else echo '(no config.txt found; non-Pi target)'; fi",
            Self::KernelCmdline => "cat /proc/cmdline",
            Self::DtOverlays => "echo '--- devicetree base ---'; ls /sys/firmware/devicetree/base/ 2>/dev/null || echo '(no devicetree)'; echo; echo '--- applied overlays ---'; ls /sys/firmware/devicetree/overlays/ 2>/dev/null || echo '(no overlays)'",
            Self::DrmConnectors => "for c in /sys/class/drm/card*-*/; do name=${c%/}; name=${name##*/}; status=$(cat \"${c}status\" 2>/dev/null || echo unknown); enabled=$(cat \"${c}enabled\" 2>/dev/null || echo unknown); modes=$(cat \"${c}modes\" 2>/dev/null | head -1 || echo none); link=$(readlink -f \"${c}device\" 2>/dev/null || echo unknown); echo \"$name status=$status enabled=$enabled mode0=$modes device=$link\"; done",
            Self::InputDevices => "cat /proc/bus/input/devices 2>/dev/null || echo '(input subsystem absent)'",
            Self::IioSensors => "for d in /sys/bus/iio/devices/iio:device*/; do n=$(cat \"${d}name\" 2>/dev/null || echo unknown); link=$(readlink -f \"$d\" 2>/dev/null || echo unknown); echo \"${d##*/iio:} name=$n device=$link\"; done; [ -d /sys/bus/iio/devices ] || echo '(no IIO devices on this target)'",
        }
    }

    pub async fn probe(
        &self,
        conn: &Connection,
        timeout: Duration,
    ) -> Result<InspectorFinding> {
        let result = conn.run(self.command(), timeout).await?;
        Ok(InspectorFinding {
            inspector: self.name().to_string(),
            command: self.command().to_string(),
            exit_code: result.exit_code,
            stdout: result.stdout,
            stderr: result.stderr,
        })
    }
}
