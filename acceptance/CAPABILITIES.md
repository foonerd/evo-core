# Acceptance target capability vocabulary

Capability names declared in target descriptors (`acceptance/targets/<target>.toml`) and referenced by scenarios via `requires_capability = "<name>"`. The vocabulary is **open** — vendors and operators add capability keys as their hardware surfaces require. This document records the canonical names so multiple targets stay consistent.

Scenarios skip-with-reason when a target does not declare a required capability. Adding a capability to a target descriptor advertises the hardware as present and ready for tests that exercise it.

## Naming conventions

- **lowercase_snake_case.** Names are tokens, not prose.
- **Category prefix when disambiguating.** Examples: `storage_microsd`, `display_hdmi`, `input_keyboard`. Bare names (e.g. `bluetooth`) are accepted when category is unambiguous in context, but disambiguating is preferred.
- **Specific over generic.** `usb_dac_topping_d10` is fine for vendor-board-specific capability declarations; generic `usb_dac` declares "some USB DAC class-compliant device is present" without naming the model.
- **One capability = one independently-testable hardware fact.** "I have a Bluetooth radio" is one capability; "I have BT codec X" is a different capability if scenarios distinguish.

## System capabilities

Software / OS-level capabilities exposed by the target's environment.

| Name | What it asserts |
| --- | --- |
| `sysrq_trigger` | `/proc/sysrq-trigger` is writable; failure-mode scenarios can inject events (OOM, panic) via sysrq |
| `nopasswd_sudo` | `sudo` is configured NOPASSWD for the test account; non-interactive privileged commands are runnable |
| `systemd` | systemd is the init system; `systemctl is-active <unit>` and `journalctl` work |
| `journald` | journald collects unit logs; scenarios that grep journalctl assume this |
| `cap_sys_admin` | The test account has `CAP_SYS_ADMIN` (e.g. for mount / namespace operations) |

## Audio output

| Name | What it asserts |
| --- | --- |
| `audio_jack` | 3.5mm analog audio output is wired and playable |
| `audio_hdmi` | Audio output via HDMI is wired (display attached + audio capable) |
| `audio_spdif` | S/PDIF (optical or coax) digital audio output is present |
| `audio_i2s_hat` | An I2S DAC HAT is attached (Pi-class boards); generic — name a specific model in vendor descriptors when it matters |
| `audio_usb_dac` | A USB-class-compliant DAC is plugged in; generic — name a specific model when scenarios need it |
| `audio_bluetooth_a2dp` | A paired and connected A2DP sink is reachable |
| `audio_pulseaudio` | PulseAudio is the active audio server |
| `audio_pipewire` | PipeWire is the active audio server |
| `audio_alsa_only` | ALSA-only environment; no PulseAudio / PipeWire layer |

## Storage

| Name | What it asserts |
| --- | --- |
| `storage_microsd` | A microSD card slot is present and accessible |
| `storage_usb` | At least one USB mass-storage device is plugged in and mountable |
| `storage_sata` | A SATA-attached drive (HAT or onboard) is present |
| `storage_nvme` | An NVMe drive is present (Pi 5 NVMe HAT, x86 m.2, etc.) |
| `storage_iscsi` | iSCSI initiator is configured and a target is reachable |
| `storage_cdrom` | A CD/DVD optical drive is present |
| `storage_emmc` | Embedded MMC is the boot device |

## Display / screen

| Name | What it asserts |
| --- | --- |
| `display_hdmi` | An HDMI display is attached |
| `display_dsi` | A DSI display is attached (Pi DSI ribbon cable) |
| `display_spi` | A small SPI display (TFT) is attached |
| `display_dpi` | DPI (parallel RGB) display is attached |
| `display_dual` | Two displays are attached (multi-monitor scenarios) |
| `display_touch` | The attached display is touch-capable |
| `display_hidpi` | The attached display is HiDPI (>= 200 ppi) |

## Input devices

| Name | What it asserts |
| --- | --- |
| `input_keyboard` | A USB / Bluetooth keyboard is attached |
| `input_mouse` | A USB / Bluetooth pointer is attached |
| `input_touch` | A touch surface is attached (covers display_touch + standalone tablets) |
| `input_rotary_encoder` | A rotary encoder (volume knob) is wired (typically on GPIO) |
| `input_buttons_gpio` | Physical buttons are wired to GPIO pins |
| `input_remote_ir` | An IR remote control is paired (LIRC keymap configured) |

## Bridges / wiring

| Name | What it asserts |
| --- | --- |
| `bridge_i2c` | I²C bus is enabled and devices respond to `i2cdetect` |
| `bridge_i2s` | I²S bus is enabled (typically for DAC HATs) |
| `bridge_spi` | SPI bus is enabled |
| `bridge_onewire` | 1-Wire bus is enabled (typically for DS18B20 temp sensors) |
| `bridge_gpio` | GPIO header is accessible (Pi 40-pin, etc.) |
| `bridge_can` | CAN bus interface is present |
| `bridge_uart` | A free UART (beyond console) is exposed |
| `bridge_pwm` | PWM channels are accessible |

## Communication devices

| Name | What it asserts |
| --- | --- |
| `comm_bluetooth` | Bluetooth radio is present and the daemon (`bluetoothd`) is running |
| `comm_wifi` | Wi-Fi radio is present |
| `comm_ethernet` | Ethernet interface is up |
| `comm_ir_receiver` | IR receiver is wired (LIRC, gpio-ir, USB-IR adapter) |
| `comm_ir_transmitter` | IR transmitter is wired (typically gpio-ir-tx or USB blaster) |
| `comm_zigbee` | Zigbee coordinator is attached (USB stick or HAT) |
| `comm_zwave` | Z-Wave controller is attached |
| `comm_lora` | LoRa radio is attached |
| `comm_proprietary_lirc` | A proprietary IR / remote stack is configured (LIRC userspace daemon) |
| `comm_modbus` | Modbus interface is wired (typically over UART or RS-485) |

## Real-time / timing hardware

| Name | What it asserts |
| --- | --- |
| `rtc_pirtc_hat` | A PiRTC HAT is attached and exposes `/dev/rtc0` |
| `rtc_ds3231_i2c` | A DS3231 RTC module is wired over I²C |
| `rtc_battery_backed` | The system has a battery-backed RTC (laptop / NUC class) |

## Sensors

| Name | What it asserts |
| --- | --- |
| `sensor_temperature` | Some temperature sensor is wired and readable |
| `sensor_humidity` | Some humidity sensor is wired and readable |
| `sensor_light` | Light / lux sensor is wired |
| `sensor_motion` | PIR / motion sensor is wired |
| `sensor_camera_csi` | Camera attached via CSI ribbon |
| `sensor_camera_usb` | USB UVC camera attached |

## Test fixtures

Capabilities that gate scenarios requiring synthetic test fixtures (helper plugins, scenario clients, infrastructure harnesses) rather than physical hardware. A target advertises these only when the fixture is actually installed and ready. Scenarios without their required fixture skip-with-reason; the bed test grows incrementally as fixtures land.

| Name | What the fixture is |
| --- | --- |
| `test_fixture_catalogue_corrupt` | Helper that mutates `/etc/evo/catalogue.toml` (and optionally the LKG) for corruption-recovery scenarios; restores at end. |
| `test_fixture_plugin_admin` | A small admitted plugin set the harness can flip enable/disable/uninstall against without disturbing production state. |
| `test_fixture_synthetic_flight_rack` | A flight-mode rack-class plugin with synthetic device-side emitter; consumer plugins subscribed to observe propagation. |
| `test_fixture_compose_apply_pair` | Composer respondent + delivery warden pair under reconciliation; supports crash-on-apply mode for failure scenarios. |
| `test_fixture_fast_path_warden` | Warden plugin Fast-Path-eligible with declared budget; client RTT timing harness alongside. |
| `test_fixture_prompt_responder` | Prompt-issuing plugin + scriptable consumer that answers / cancels / times out per scenario. |
| `test_fixture_ntp_isolation` | Network harness that blocks NTP traffic to drive TimeTrust transitions on demand. |
| `test_fixture_synthetic_appointment_plugin` | Appointment-registering plugin + observer that times fire latency and counts recurrences. |
| `test_fixture_synthetic_watch_plugin` | Watch-registering plugin + scriptable event source for cooldown / hysteresis / duration scenarios. |
| `test_fixture_coalescing_burst` | High-rate happening emitter + subscriber with configurable coalesce labels. |
| `test_fixture_live_reload_plugin` | In-process and OOP plugin variants implementing `prepare_for_live_reload` + `load_with_state`; supports state-blob size controls. |
| `test_fixture_synthetic_warden` | Generic warden plugin with declared `course_correct_verbs`; scenarios issue declared and undeclared verbs to verify gating. |
| `test_fixture_drift_plugin` | Plugin with intentional manifest / `describe()` mismatch; scenarios verify admission refusal at expected boundary. |
| `test_fixture_skew_plugin` | Plugin manifests with controlled `evo_min_version` values across the strict / warn-band / refuse zones. |
| `test_fixture_grammar_migration` | Scenario harness for catalogue swap + bulk subject registration + migration verb invocation + alias chain assertion. |
| `test_fixture_disk_full` | Loop-device or quota harness that fills the steward's storage volume to drive disk-full failure-mode scenarios. |
| `test_fixture_network_drop` | Synthetic client that issues a wire op and ungracefully closes mid-flight to drive session-cleanup scenarios. |
| `test_fixture_long_runner` | Marker capability declaring a target opted in for T4 stress / soak runs (multi-hour scenarios). Smoke / inspection-only targets do not declare this. |
| `test_fixture_capabilities_query` | Scriptable subscribe_capabilities client that captures the framework's capability surface (advertised wire ops, clock_trust state, codec list, etc.) for assertion. |

Adding a fixture: implement the helper script / synthetic plugin, install on the target, declare the matching `test_fixture_*` capability in the target descriptor. The scenarios that require it then run automatically on next harness invocation.

## Vendor-specific capabilities

Vendors declare board / product-specific capabilities under their own namespace by convention: `<vendor>_<capability>`. Example:

```toml
[[capabilities]]
name = "volumio_primo_xduoo_hat"
note = "Vendor-specific HAT for the Volumio Primo product"
```

Scenarios that target vendor hardware reference these directly.

---

## Adding a new capability

1. Decide the category (or pick "vendor-specific" if domain-distinct).
2. Pick a `lowercase_snake_case` name following the conventions above.
3. Add the capability to the relevant `acceptance/targets/<target>.toml` under `[[capabilities]]`.
4. (Optional but recommended) Append the capability + one-line description to this document so other targets and authors share the vocabulary.
5. Author scenarios that declare `requires_capability = "<name>"` to gate against it.

The vocabulary grows by use. There is no closed list; this document is the running roster.
