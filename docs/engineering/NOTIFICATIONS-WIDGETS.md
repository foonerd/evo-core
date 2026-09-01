<!-- SPDX-License-Identifier: Apache-2.0 -->
<!-- Copyright (c) 2026 Just a Nerd -->

# Notifications Widgets — UI Team Contract

## Scope

Contract between the notifications shelf's back-end (the
reference plugin `org.evoframework.system.notifications`,
framework-reserved namespace) and downstream widget
implementations. The plugin publishes the
`system_notifications_active` subject on every state
transition; UI widgets subscribe once and render reactively.

This document names the widget kinds the framework's reference
UI is expected to implement, their states and transitions, the
accessibility contract each widget honours, and the reference-
data emitter available for local development.

## Substrate

- **Schema**: `evo-catalogue-schemas/schemas/org.evoframework/system/notifications.v1.toml`
  declares the shelf, verbs, subject, records, enums, widget-kind
  hints, and 9 acceptance criteria.
- **Publisher**: reference plugin
  `org.evoframework.system.notifications` (source in the
  vendor distribution's plugins/ tree). The plugin's
  `NotificationDispatcher::attach_subject_publisher` wires the
  runtime to the plugin's `LoadContext.subject_announcer`
  handle; every send / cancel / set_base_mode /
  set_quiet_hours / auto-dismiss-prune fires a
  fire-and-forget republish.
- **Wire subject type**: `system_notifications_active`
  (singleton per node).
- **Wire addressing**: scheme `evo.system.notifications.active`,
  value `local`.
- **Envelope**: full-snapshot JSON on every republish; consumers
  need not track deltas.

## Widget kinds

The following widget kinds compose the operator-facing
notification surface. Each is independently addressable — a
surface may render one, several, or all. The reference UI
implements every kind.

### `evo.notifications.banner`

Single-notification overlay card. Consumes one
`active_notification` record.

**Render**: title (translated from `title_key`), body (from
`body_key` when present, hidden when absent), source-plugin
attribution, priority indicator (colour band per level), one
button per action.

**States**:

- `idle` — no notifications; widget hidden.
- `presenting` — a notification is displayed; auto-dismiss
  timer counting down when `auto_dismiss_after_ms` is present.
- `dismissed` — operator or plugin cancelled; widget animates
  out.

**Transitions**: `idle → presenting` on subject envelope
containing a new entry. `presenting → dismissed` on entry
removal from the envelope or auto-dismiss expiry.

### `evo.notifications.tray`

Stacked list of every active notification. Consumes the full
`active` array.

**Render**: rows ordered `(priority desc, sent_at desc)`.
Critical entries pin to the top. Group-coalesced entries render
as one row with `group_count` badge. Empty state renders `No
active notifications`.

**States**: `empty` (0 entries) / `non-empty` (1+ entries).

**Transitions**: subject envelope updates recompute the row set.
Handle stability guaranteed across republishes.

### `evo.notifications.dnd_toggle`

Two-state boolean toggle mapping the base mode. Off = `chime`
or `voice`; on = `display_only`. Dispatches
`system.notifications.set_base_mode`.

**States**: reflects `base_mode` from the subject reactively.

### `evo.notifications.mode_selector`

Three-way segmented control (`display_only`, `chime`, `voice`).
Reads `base_mode` from the subject; dispatches
`system.notifications.set_base_mode` on selection change.

### `evo.notifications.quiet_hours_editor`

Form with two time-of-day pickers (start, end) plus a
downgrade-mode selector. Reads `quiet_hours_policy` to seed the
form; dispatches `system.notifications.set_quiet_hours` on save.

**Empty state**: `start_minute == 0 && end_minute == 0` renders
`Quiet hours off; tap to enable`.

**Wrap-midnight state**: `end_minute < start_minute` — the
window crosses midnight. Widget renders this explicitly
(`22:00 → 07:00 (crosses midnight)`) so the operator sees the
intent.

### `evo.notifications.quiet_hours_indicator`

Compact indicator surfacing whether the current wall-clock
minute falls inside the quiet-hours window. Reads
`quiet_hours_active`.

**States**: `off` (policy inert) / `outside_window` /
`inside_window`.

### `evo.notifications.group_collapse`

Collapsible list variant for grouped notifications. Consumes
entries whose `group_with` matches a specific group id. Renders
the group's summary row with the `group_count` badge; expands
to show every coalesced member.

## Accessibility contract

Every widget honours the accessibility invariants:

- **Screen-reader labels** for every actionable control.
  Buttons carry `aria-label` matching `label_key`'s translation.
  Priority indicators carry `aria-label` describing the priority
  class (`critical alert`, `important warning`, `routine
  notification`).
- **Keyboard navigation** through the tray + banner in
  focus-order: newest-first, tab to next action, enter to
  activate. Escape dismisses the focused banner.
- **Focus order** matches visual order. Group-coalesced entries
  expose the count as `<n> more <group-name> notifications`
  when the operator tabs onto the group row.
- **No focus trap** on banners. Auto-dismissing banners return
  focus to whatever was focused before the banner appeared.
- **Colour contrast** meets the framework's chosen accessibility
  bar (see accessibility ADR when finalised). Priority
  indicators use both colour AND shape so colour-blind operators
  see priority.
- **Motion respect**: banners animate in / out; widgets read the
  operator's `prefers-reduced-motion` preference and disable
  the animation when set.

## Reference-data emitter

For local development without real hardware events, the
reference plugin ships a rotating-sample emitter in its
`src/demo.rs` module.

**Enable**: set `EVO_NOTIFICATIONS_DEMO=1` before booting the
framework. The plugin's `Plugin::load` reads the env-var on
each admission cycle and spawns the emitter task when set.

**What it emits**: a six-step rotation on a 5-second cadence
exercising every envelope shape variant —

1. Doorbell chime (Routine, group=`doorbell`, chime audio,
   `answer` + `snooze_5m` actions, auto-dismiss 30s).
2. Doorbell chime again — same group, exercises coalescing.
3. Motion alert (Important, Warning, `view_camera` action,
   auto-dismiss 60s).
4. Update available (Info, no auto-dismiss — pinned).
5. Alarm firing (Critical, Alert, voice payload, no
   auto-dismiss).
6. Backup complete (Info, no actions, auto-dismiss 8s).

Then cancels the first three and cycles back to step 1.

**Not for production**: the env-gate is deliberate; production
builds MUST NOT set the variable. A distribution's production
build script may unset the variable explicitly as an extra
belt-and-braces gate.

## Development flow

1. Enable the demo emitter (`EVO_NOTIFICATIONS_DEMO=1`).
2. Boot the framework.
3. Subscribe your UI-side WebSocket / gRPC consumer to the
   subject: type = `system_notifications_active`, addressing =
   (`evo.system.notifications.active`, `local`).
4. Render envelopes as they arrive; verify every widget
   variant against the six rotating samples.
5. Test operator interactions:
   - dispatch `system.notifications.set_base_mode` on
     mode-selector change; verify `base_mode` in the next
     envelope matches.
   - dispatch `system.notifications.set_quiet_hours` on
     editor save; verify `quiet_hours_policy` +
     `quiet_hours_active` update.
   - dispatch `system.notifications.cancel` when operator
     dismisses a tray entry; verify the entry disappears
     from the next envelope.

## Wire projection

REST + WebSocket + GraphQL projections for the subject and
verbs land in the framework's projection crates. Consumers
that prefer a REST snapshot over subject subscription can
`GET` a JSON representation of the subject state at any time;
consumers that want push-mode use the WebSocket subscription.

Projection endpoint URLs are documented in the projection
crates' respective READMEs and stabilise when the first
consumer ships against them.
