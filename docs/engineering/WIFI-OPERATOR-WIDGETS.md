<!-- SPDX-License-Identifier: Apache-2.0 -->
<!-- Copyright (c) 2026 Just a Nerd -->

# WiFi Operator Widgets — UI Team Contract

## Scope

Contract between the framework's WiFi / network substrate and
downstream widget implementations for operator-facing network
management: scanning, connection setup, captive-portal handoff,
security posture, preferred-interface selection, flight-mode,
radio inventory.

The framework substrate is fully shipped — schema shelf,
reference plugin, subject publisher. This document names the
widget kinds the reference UI is expected to implement, which
wire op(s) each consumes, states and transitions, and the
accessibility contract each widget honours.

## Substrate

- **Schema**: `evo-catalogue-schemas/schemas/org.evoframework/networking/link.v1.toml`
  declares the `networking.link` shelf, its 16 wire ops, and
  the acceptance criteria.
- **Reference plugin**: `org.evoframework.network` in the
  audio reference distribution. NetworkManager back-end via
  D-Bus on systemd hosts. Vendor distributions replacing the
  plugin (systemd-networkd, iwd, alternate init systems) keep
  the wire-op surface stable.
- **Published subject**: `network_connectivity_state`
  (singleton per node) — carries the runtime supervisor's
  current reachability state (online / portal / limited /
  offline), critical-recovery flag, last-observations snapshot,
  and captive-portal URL when present. Republished on every
  reachability transition.
- **Cardinality**: one plugin per device per shelf. A device
  admits exactly one `networking.link` plugin at boot.

## Wire ops the widgets consume

| Wire op | Purpose |
|---|---|
| `network.nm.status` | Current link state, active connection details, captive-portal status, signal metrics. Read-side snapshot. |
| `network.nm.scan` | Trigger a WiFi scan; returns the resulting access-point list with signal / security info. Read-side (side effect: RF scan). |
| `network.nm.intent.get` | Current connection intent (declared profile, autoconnect state, security parameters). |
| `network.nm.intent.set` | Update the connection intent without applying. |
| `network.nm.intent.apply` | Apply the current intent (connect / disconnect / reconfigure per the declared profile). |
| `network.nm.captive.status` | Captive-portal detection state (none / detected / in-flow / completed). |
| `network.nm.captive.start` | Initiate a captive-portal flow. Returns a session token consumed by submit/complete. |
| `network.nm.captive.submit` | Submit captive-portal credentials (typically routed via user-interaction prompt). |
| `network.nm.captive.complete` | Mark the captive-portal session complete. Plugin verifies connectivity and returns final status. |
| `network.nm.security.status` | Network-security posture (encryption type, weak-cipher warnings, MAC randomisation state). |
| `network.nm.security.harden` | Apply a hardened network-security configuration (per declared policy). |
| `network.nm.flight_mode.get` | Flight-mode state for the wireless rack-class. |
| `network.nm.flight_mode.set` | Set flight-mode for the wireless rack-class. Emits `FlightModeChanged` happening. |
| `network.nm.supervisor.status` | Runtime supervisor state (reachability + critical-recovery flag + last-observations snapshot + captive-portal URL). |
| `network.nm.wifi_devices` | Live WiFi radio inventory — one row per kernel netdev with PHY, supported interface modes, concurrent managed+AP capability, per-band support (2.4 / 5 / 6 GHz), connection class (onboard vs USB), virtual-AP-vif flag. Drives the preferred-interface picker. |
| `network.refresh_connectivity` | On-demand connectivity-state refresh; returns the freshly-published `networking.link.connectivity` subject value. |

## Widget kinds

The following widget kinds compose the operator-facing WiFi
surface. Each is independently addressable — a surface may
render one, several, or all. Ordered by implementation priority
for the reference UI.

### `evo.network.connectivity_status_pill`

Compact status pill surfacing the current network state.

**Consumes**: `network_connectivity_state` subject (push);
`network.nm.refresh_connectivity` for operator-triggered
refresh.

**Render**: colour-coded pill (green online / amber portal /
amber limited / red offline) with the active connection's
SSID (WiFi) or interface name (Ethernet) alongside. Tap
opens the full network surface.

**States**: `online` / `portal` / `limited` / `offline` /
`unknown` (subject not yet subscribed).

### `evo.network.scanner`

WiFi access-point scanner with live-refresh.

**Consumes**: `network.nm.scan` on operator tap or auto-refresh.
Returns access-point list (SSID, signal_dbm, security type,
frequency band, is_hidden, is_current).

**Render**: sortable list; per-row signal-strength icon (0-4
bars) + lock icon (for secured networks) + current-connection
badge. Empty state renders `No networks in range`. Refresh
button + auto-refresh timer (every 15s while surface visible).

**States**: `scanning` (spinner) / `results` / `empty` /
`refused` (scan denied — radio off, permission missing).

### `evo.network.ssid_picker`

Wraps the scanner as a chooser for a specific target
connection.

**Consumes**: same as scanner. On selection, dispatches
`network.nm.intent.set` with the chosen SSID + prompts for
credentials.

**Render**: scanner list with tap-to-select. Selected row
transitions to a credentials form (SSID pre-filled, password
input, remember-network toggle, hidden-SSID checkbox if the
selected network is hidden).

**States**: `browsing` / `selected` / `submitting` /
`connected` / `failed`.

### `evo.network.hidden_ssid_form`

Operator enters SSID + credentials for a hidden network the
scanner cannot see.

**Consumes**: `network.nm.intent.set` + `network.nm.intent.apply`
on submit.

**Render**: SSID text input, security-type selector (WPA2 /
WPA3 / open / enterprise), password input (or enterprise
credential set), remember-network toggle.

**States**: `idle` / `submitting` / `connected` / `failed`.

### `evo.network.connection_wizard`

Guided setup for a fresh device with no configured network.
Composes scanner + ssid_picker + captive_portal_handoff.

**Consumes**: multiple wire ops as above.

**Render**: multi-step wizard —
1. Scanner → pick network
2. Credential entry (or hidden-SSID form)
3. Connecting (progress indicator; auto-transitions on state
   change)
4. Captive-portal handoff if detected
5. Connected + summary

**States**: one per step + `failed` (rewind to appropriate step).

### `evo.network.captive_portal_handoff`

Surfaces the captive-portal login flow the connected network
requires. Device-proxied — the operator's remote LAN browser
NEVER touches the venue portal URL; it iframes a same-origin
session URL served by the framework's captive-session
endpoint.

**Consumes**:

- `network.nm.captive.status` — detection state (`none` /
  `probe_detected` / `authenticated` / `failed`) + `portal_url`
  used by the operator-facing "you need to sign in" banner
  before the session opens.
- `network.nm.captive.session.start` — opens a device-proxied
  captive session; returns `{session_id, session_url}` where
  `session_url` is the same-origin
  `/api/v1/network/captive/session/{sid}/…` the widget iframes.
- `network.nm.captive.session.close` — closes the session and
  re-runs reachability so `network.nm.captive.status` reports
  the fresh verdict.

**Render**:

1. When `captive.phase` transitions to `probe_detected`, show
   a banner: "This network requires sign-in". One CTA button
   labelled "Open sign-in".
2. On CTA: dispatch `network.nm.captive.session.start`. On
   `{session_id, session_url}` success, render an `<iframe>`
   whose `src` IS `session_url`. The iframe is same-origin
   (device management HTTPS plane), so browser security
   applies normally.
3. Iframe hosts the real portal bytes — the device fetches
   upstream over the captive-carrying interface
   (SO_BINDTODEVICE), rewrites `Location` + absolute URLs +
   strips `Set-Cookie` at the framework layer, hands
   same-origin bytes to the browser. The operator sees the
   actual portal, JS SPA or otherwise.
4. When the portal completes admission (browser navigates out
   of the iframe, or an operator "Done" tap fires), dispatch
   `network.nm.captive.session.close`. The response carries
   the fresh `connectivity` verdict; on `full` the widget
   transitions to `completed` and hides.

**NEVER**:

- `window.open(user_portal_url)` — the operator's browser
  cannot reach the venue on the captive-carrying interface.
- `<iframe src="<venue_url>">` — same reason; also the venue
  portal will refuse a cross-origin frame (X-Frame-Options).
- Opening the venue URL through `OpenMechanism::SystemBrowser`
  or `OpenMechanism::InAppWebview`; those variants are for
  non-captive flows only. Captive uses
  `OpenMechanism::DeviceProxiedSession`.

**States**: `none` (widget hidden) / `detected` (banner + CTA) /
`in_flow` (iframe open) / `completed` (transient, then hidden) /
`failed` (banner with retry).

### `evo.network.preferred_interface_picker`

Operator picks which WiFi radio the framework prefers for STA
mode (station / client) — critical on devices with multiple
radios (onboard + USB, dual-band split).

**Consumes**: `network.nm.wifi_devices` for the inventory;
`network.nm.intent.set` to persist the preference.

**Render**: one row per WiFi radio with PHY name, supported
bands (2.4 / 5 / 6 GHz), connection class (onboard /
USB / PCIe), concurrent-AP-capable badge. Selection is
mutually exclusive.

**States**: `single_radio` (no picker needed — status readout
only) / `multi_radio` (picker rendered).

### `evo.network.country_band_selector`

Regulatory-domain + band selector. Country selector filters
which bands are legal in the operator's region; band selector
restricts the radio's scan surface.

**Consumes**: `network.nm.intent.set` (regulatory field).

**Render**: country combobox (ISO-3166 alpha-2), per-band
toggle set (2.4 / 5 / 6 GHz). Bands unavailable in the selected
country render disabled with a tooltip explaining why.

**States**: `default` (operator has not customised) /
`customised`.

### `evo.network.security_hardening_toggle`

Compact toggle to apply / retract the framework's hardened
network-security configuration (MAC randomisation on,
weak-cipher rejection, per the plugin's declared policy).

**Consumes**: `network.nm.security.status` (state) +
`network.nm.security.harden` (apply).

**Render**: two-state toggle + a details expander showing the
current cipher / MAC posture. Hint text describes what the
hardened preset changes.

**States**: `hardened` / `unhardened` / `partial` (some
elements applied, others not).

### `evo.network.flight_mode_switch`

Wireless flight-mode toggle. Coordinates with the framework's
flight-mode signal so every wireless-rack plugin reacts
together.

**Consumes**: `network.nm.flight_mode.get` (state) +
`network.nm.flight_mode.set` (toggle). Successful set emits
`Happening::FlightModeChanged { rack_class: "wireless", on }`
from the steward post-dispatch hook (single story with the
plugin-local verb — no separate `flight_mode.*` shelf required
on the audio catalogue).

**Render**: prominent toggle with an icon; when engaged, a
dimming overlay grays out the WiFi surface. Text confirms
the wireless class the switch affects.

**States**: `on` (flight mode) / `off`.

### `evo.network.ethernet_ipv4_form`

Ethernet DHCP vs static addressing.

**Consumes**: `network.nm.intent.get` / `.set` (with `apply: true`)
fields `ethernet.ipv4_mode`, `ethernet.ipv4_address` (CIDR),
`ethernet.ipv4_gateway`, `ethernet.ipv4_dns[]`.

**Render**: mode select (DHCP / Static); address / gateway / DNS
inputs enabled only in Static mode. Apply persists + applies.

**States**: `dhcp` / `static` / `submitting` / `failed`.

### `evo.network.wifi_sta_ipv4_form`

Wi‑Fi STA DHCP vs static addressing (same shape as ethernet,
fields under `wifi.sta_ipv4_*`).

**Consumes**: `network.nm.intent.get` / `.set` + apply.

**States**: `dhcp` / `static` / `submitting` / `failed`.

### `evo.network.hotspot_settings`

Operator hotspot / AP configuration for concurrent STA+AP
(`ap0` when PHY allows) or exclusive AP role.

**Consumes**: `network.nm.intent.get` / `.set` fields
`fallback.hotspot_enabled`, `wifi.ap_ssid`, `wifi.ap_channel`,
`wifi.ap_band`, optional `ap_psk` sidecar.

**Render**: enable toggle, SSID, password, channel. Status
should surface when AP is up on a virtual vif.

**States**: `disabled` / `enabled` / `submitting` / `failed`.

### `evo.network.wifi_devices_inventory`

Diagnostic surface listing every WiFi radio on the host with
capabilities.

**Consumes**: `network.nm.wifi_devices`.

**Render**: full-detail list — one row per radio with PHY
name, kernel netdev, supported interface modes (station / AP /
mesh / monitor), concurrent-managed-plus-AP capability, per-
band support, connection class, virtual-AP-vif flag. Not
operator-facing per se; surfaces via the diagnostics shelf.

**States**: `none` (no WiFi radios — Ethernet-only device) /
`one` / `many`.

## Accessibility contract

Every widget honours the accessibility invariants:

- **Screen-reader labels** for every actionable control.
  Scanner rows carry SSID + signal + security state as one
  compound label (`"Home network, strong signal, WPA3
  secured, currently connected"`).
- **Keyboard navigation** through scanner rows in signal-
  strength order; enter selects; escape retreats to the
  previous wizard step.
- **Focus order** matches visual order.
- **Colour + shape** for security state — the lock icon
  disambiguates from colour-only signalling; band tags are
  text as well as chip colour.
- **Motion respect** — scanner refresh animations honour
  `prefers-reduced-motion`.
- **Live-region announcement** for scanner refresh
  completion (`aria-live=polite`) so screen-reader operators
  hear the result count.

## Development flow

The framework substrate is complete; no reference-data emitter
ships (unlike notifications, real WiFi is easy to exercise on
any dev laptop):

1. Boot the framework on a dev machine with WiFi hardware.
2. Ensure `org.evoframework.network` is admitted.
3. Subscribe your WebSocket / gRPC consumer to
   `network_connectivity_state`; dispatch the wire ops above
   as widgets require.
4. Test captive-portal flows against a café / hotel WiFi or a
   locally-configured captive-portal router.
5. Test flight-mode using `rfkill` on the dev machine.
6. Test preferred-interface with a USB WiFi adapter alongside
   the onboard radio, or by faking the wifi_devices response
   via the plugin's dev fixtures.

## Wire projection

REST + WebSocket + GraphQL projections for the subject and
wire ops land in the framework's projection crates as the
first UI consumer ships against them. Consumers that prefer a
REST snapshot over subject subscription can `GET` a JSON
representation at any time; consumers that want push-mode
use the WebSocket subscription.

## Follow-on substrate work (not shipped in this doc)

- Push-mode subjects for scanner results and captive-portal
  transitions — today the widgets poll `network.nm.scan` and
  `network.nm.captive.status`. A subject-per-scan and a
  subject-per-captive-session would let widgets subscribe
  instead of poll, matching the notifications pattern.
- Reference-data emitter for CI / off-hardware widget
  testing. Real WiFi covers most local development; CI runs
  need a fake surface.
- Country-code catalogue with band-availability lookup —
  today the `country_band_selector` widget owns this
  catalogue; a shared source-of-truth would let vendor
  distributions extend without forking the widget.
