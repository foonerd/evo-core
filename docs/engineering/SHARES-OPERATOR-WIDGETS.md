<!-- SPDX-License-Identifier: Apache-2.0 -->
<!-- Copyright (c) 2026 Just a Nerd -->

# Network Shares Operator Widgets — UI Team Contract

## Scope

Contract between the framework's NAS / SMB / NFS share-
management substrate and downstream widget implementations
for operator-facing share configuration, discovery, mount /
unmount, and server-side SMB (evo-as-upload-target).

The invariant the substrate honours: **an operator can point
evo at music wherever the music lives — local disk, NAS over
SMB, NAS over NFS, or files uploaded directly to the device —
without ever editing a shell file by hand.**

This document names the 8 widget kinds the reference UI is
expected to implement, which wire op(s) and subject(s) each
consumes, states + transitions, and the accessibility contract
each widget honours.

## Substrate

- **Schema**: `evo-catalogue-schemas/schemas/org.evoframework/networking/shares.v1.toml`
  declares the `networking.shares` shelf, its 9 wire ops, 5
  subjects, records, enums, widget-kind hints, and 11
  acceptance criteria.
- **Runtime primitives** (reference plugins in the vendor
  distribution's plugins/ tree):
  `org.evoframework.network.shares` handles share management +
  dialect probe + mount lifecycle;
  `org.evoframework.network.smb-server` handles evo-as-SMB-server.
  Both back their patterns on the reference `volumio-evo`
  implementation (~2,170 LoC across six modules).
- **Reference plugins**: `org.evoframework.network.shares` +
  `org.evoframework.network.smb-server` (both framework-reserved
  namespace, both stock `networking.shares`). Vendor
  distributions replacing the mount back-end (e.g. autofs
  instead of direct `mount.cifs`) ship an alternate plugin
  under their namespace with the wire-op surface
  stable.

## Wire ops the widgets consume

| Wire op | Purpose |
|---|---|
| `network.share.add` | Add + attempt initial mount. Returns share_id + initial mount outcome. |
| `network.share.edit` | Modify existing share record; does not remount. |
| `network.share.remove` | Unmount (lazy detach for busy CIFS) + delete record. |
| `network.share.mount` | Mount existing share; runs CIFS dialect probe if no persisted_vers. |
| `network.share.unmount` | Unmount; lazy detach for busy CIFS. |
| `network.discovery.refresh` | Trigger mDNS-SD + per-host smbclient sweep. |
| `network.smb_server.apply` | Apply new SMB-server settings. |
| `network.smb_server.user_add` | Add SMB user (password via user-interaction prompt → vault). |
| `network.smb_server.user_revoke` | Revoke existing SMB user. |

## Subjects the widgets subscribe to

| Subject | Cardinality | Republished on |
|---|---|---|
| `system_network_shares_configured` | singleton per node | every add / edit / remove |
| `system_network_shares_discovered` | singleton per node | every discovery.refresh + 5-min background |
| `network_share_state` | one instance per share_id | every state transition per share |
| `network_share_events` | one instance per share_id | every lifecycle event per share |
| `system_smb_server` | singleton per node | every smb_server.apply |

## Widget kinds

Ordered by implementation priority for the reference UI.

### `network.share.card.list` / `network.share.card.tile`

Configured-share cards (list mode = compact row per share; tile
mode = larger tile with activity-log preview).

**Consumes**: one `share_record` from
`system_network_shares_configured` + the corresponding
`network_share_state` subject for the share.

**Render**: alias, host / path, state badge (Mounted / Unmounted
/ Mounting / Failed), negotiated version chip (CIFS: `SMB 3.1.1`
/ NFS: `NFSv4.2`), mount_root chip (list only shows on hover;
tile shows always).

**States**: `mounted` (green badge, negotiated version chip) /
`unmounted` (grey badge, no version chip) / `mounting` (spinner,
current probe dialect for CIFS) / `failed` (red badge, reason
text below).

**Transitions**: state field on the subject changes → card
transitions with a subtle animation (500ms). Cross-state
transitions (e.g. mounting → mounted) animate the badge colour.

### `network.share.config.form`

Add / edit share form.

**Consumes**: nothing on mount (fields blank for add; pre-filled
from a share_record for edit). Dispatches `network.share.add`
or `network.share.edit` on submit.

**Render**:
- Alias text input.
- fstype segmented control (CIFS / NFS).
- Host text input (accepts IP or DNS name).
- Path text input (with an fstype-specific hint under —
  `Music` for CIFS, `/export/music` for NFS).
- Credentials-kind selector (Guest / Username + Password /
  Key file). On selecting Username + Password, dispatches the
  user-interaction prompt for the password; the returned
  SecretRef is submitted with the record (password never
  in-band).
- Advanced options text area (multi-line; hint text describes
  common options for the selected fstype).

**States**: `idle` / `submitting` / `succeeded` (auto-closes
after 500ms) / `failed` (renders the error inline; form stays
open for correction).

### `network.share.status.panel`

Full-detail per-share status panel — deeper than the card.

**Consumes**: `network_share_state` + `network_share_events` for
the share.

**Render**:
- Share alias + fstype badge in the header.
- State + negotiated version chip.
- mount_root chip (copyable).
- last_mounted_at as a `<n> ago` freshness indicator.
- Activity log ring buffer (newest-first list of
  mount_attempted / mounted / mount_failed / unmount_attempted
  / unmounted / connectivity_dropped / connectivity_restored
  events with timestamps + detail text).

**States**: `mounted` / `unmounted` / `mounting` / `failed` —
same as the card, with richer content.

### `network.discovered.nas.card.list` / `network.discovered.nas.card.tile`

Discovered-NAS device cards for the discovery surface.

**Consumes**: one `discovered_nas` record from
`system_network_shares_discovered`.

**Render**: NetBIOS/mDNS name, IP, negotiated_dialect chip
(compatibility hint), enumerated shares list. Tap opens
`network.share.config.form` pre-filled with host + a chosen
share path.

**States**: `discovered` (default) / `already_configured` (dimmed
+ badge saying `Already added`; tap opens the existing card).

**Refresh**: pull-to-refresh dispatches
`network.discovery.refresh`; auto-refresh every 5 minutes while
the surface is visible.

### `network.smb_server.settings.form`

evo-as-SMB-server settings form.

**Consumes**: `system_smb_server` subject.

**Render**:
- Enabled toggle (prominent — the whole form below dims when
  off).
- min_protocol selector (Default / SMB2_02 / SMB3_02) with
  hint text explaining the trade-off.
- extra_shares list editor:
  - Add row: name + path + guest_ok toggle.
  - Framework refuses paths outside the allowed prefix set
    with an inline error.
- Save button dispatches `network.smb_server.apply`.

**States**: `disabled` (toggle off — settings dimmed) / `enabled`
(form live) / `applying` (spinner) / `apply_succeeded` / `apply_partial`
(some settings applied, some refused — rendered inline per
row).

### `network.smb_users.list`

SMB user management.

**Consumes**: `system_smb_server.smb_users`.

**Render**:
- One row per smb_user_record with username +
  mapped_domain_identity (when present) + created_at freshness.
- Add-user button: dispatches the framework's user-interaction
  prompt for the password entry; on completion, dispatches
  `network.smb_server.user_add` with the SecretRef the vault
  returned.
- Revoke button per row: dispatches
  `network.smb_server.user_revoke` after confirmation prompt.

**States**: `idle` / `adding` (prompt open) / `revoking`
(confirmation open) / `succeeded` / `failed`.

## Accessibility contract

Every widget honours the accessibility invariants:

- **Screen-reader labels** for every actionable control. Share
  cards carry a compound label (`Family NAS, mounted via SMB
  3.1.1, mount root /mnt/NAS/family_nas`) so screen-reader
  operators receive the full state without visual density.
- **Keyboard navigation** through cards in configured-order;
  tab traverses cards + primary action; enter opens the status
  panel; escape closes.
- **Focus order** matches visual order.
- **Colour + shape** for state — the state badges use icon
  shape (checkmark / arrow-down / spinner / warning triangle)
  in addition to colour so colour-blind operators disambiguate.
- **Motion respect** — state-transition animations honour
  `prefers-reduced-motion`.
- **Live-region announcement** for discovery-refresh completion
  (`aria-live=polite`: `Discovery found <n> devices`).
- **Password entry safety** — every password field carries
  `autocomplete=new-password`, is `type=password`, and reads out
  as `Password field` to screen readers (never spoken back).
- **Confirmation gating** for destructive actions —
  network.share.remove + network.smb_server.user_revoke render a
  confirmation prompt before dispatching.

## Development flow

The reference plugins `org.evoframework.network.shares` +
`org.evoframework.network.smb-server` ship in the vendor
distribution's plugins/ tree. UI-side development:

1. Build the widget kinds against the schema shelf's declared
   subject shape + wire-op signatures.
2. Wire mock subject data at the WebSocket / gRPC subscription
   layer (`system_network_shares_configured` and friends can be
   fed synthetic data by a small test harness) for offline
   widget iteration.
3. Verify accessibility contract via screen-reader + keyboard-
   only interaction with the mocked surface.
4. Boot the framework on a dev machine with a reachable NAS.
5. Add a share via `network.share.add` (or via a config file the
   plugin reads at boot as a bootstrap path).
6. Verify the mount round-trip on the rig; observe
   `network_share_state` transitions on the wire.
7. Exercise discovery-refresh + evo-as-SMB-server flows.

## Runtime primitive history

The share-management runtime + evo-as-SMB-server runtime
originally landed as framework crates during v0.1.13
development; they were subsequently extracted to the two
reference plugins named above once the founding contract's
"anything serving a rack shelf lives in a plugin" rule was
enforced. The extracted implementations are functionally
equivalent to the earlier runtime primitives —
`NetworkSharesHandle` verbs, dialect probe ladder, mount
lifecycle, discovery cadence, subject-publisher wiring, and
sudoers scoping all preserved.

## Wire projection

REST + WebSocket + GraphQL projections for the subjects and
wire ops land in the framework's projection crates as the first
UI consumer ships against them.
