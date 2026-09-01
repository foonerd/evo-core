// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Canonical wire-protocol schema.
//!
//! Every variant of the framework's `ClientRequest` enum has a
//! matching [`WireOp`] entry below, declaring the capability scope
//! and audit-emission timing the framework enforces uniformly
//! across every projection (REST, WebSocket, gRPC, GraphQL,
//! HTTP/3).
//!
//! ## Source of truth
//!
//! The wire protocol's typed types in `crate::server` are the
//! single source of truth for the dispatch surface; this module
//! is the source of truth for the projection-friendly annotation
//! layer that the schema-first generators consume. Adding a new
//! wire op extends both the `ClientRequest` enum (in
//! `crate::server`) and the [`canonical_schema`] table below; a
//! unit test enforces parity so the two stay aligned.
//!
//! ## Refinement
//!
//! Refining a single op's metadata is a direct edit on its entry
//! in [`canonical_schema`]. The annotation tier here describes
//! the projection-layer's view of each op (capability gate, audit
//! timing, one-line summary); the dispatch-layer's view (the
//! request payload shape, the response shape) is owned by the
//! typed enum in `crate::server`.

use evo_projection_core::{
    AuditTiming, CapabilityRequirement, WireOp, WireOpId,
};

/// Return the full set of wire ops the framework exposes.
///
/// The returned `Vec` carries one [`WireOp`] per `ClientRequest`
/// variant. Each entry declares:
///
/// - The op's stable identifier ([`WireOpId`]) — the snake-case
///   form of the variant name, matching the serde-derived `op`
///   tag in the wire envelope.
/// - The capability scope the caller must hold to dispatch
///   ([`CapabilityRequirement`]).
/// - The audit ledger emission timing ([`AuditTiming`]).
/// - A one-line summary surfaced in generated SDK docstrings,
///   OpenAPI summaries, and GraphQL field descriptions.
pub fn canonical_schema() -> Vec<WireOp> {
    let mut s = Schema::with_capacity(128);

    // Connection-level discovery and handshake. Anonymous-OK so
    // clients can negotiate capabilities before presenting any
    // credential.
    s.anonymous(
        "describe_capabilities",
        "Discover the wire-protocol version, supported op set, and named features.",
    );
    s.anonymous(
        "negotiate",
        "Connection handshake. Establish wire-protocol version, capability scope, and session context.",
    );

    // Subject-graph queries — read-scope, no audit emission.
    s.read(
        "project_subject",
        "subjects",
        "Compose a federated projection for one canonical subject.",
    );
    s.read(
        "describe_alias",
        "subjects",
        "Return alias metadata for a canonical subject id.",
    );
    s.read(
        "list_subjects",
        "subjects",
        "Enumerate every subject the steward knows of.",
    );
    s.read(
        "list_relations",
        "subjects",
        "Enumerate every relation predicate currently asserted.",
    );
    s.read(
        "enumerate_addressings",
        "subjects",
        "Enumerate every addressing currently asserted for a subject.",
    );
    s.read(
        "resolve_claimants",
        "subjects",
        "Resolve every plugin claimant for a subject.",
    );
    s.read(
        "project_rack",
        "subjects",
        "Compose a rack projection (every shelf in one rack).",
    );

    // Grammar / orphan migration — read for list; step-up for
    // mutations.
    s.read(
        "list_grammar_orphans",
        "subjects",
        "List subjects orphaned by a recent catalogue grammar change.",
    );
    s.step_up(
        "migrate_grammar_orphans",
        "subjects_admin",
        "Migrate orphaned subjects to a new grammar shape.",
    );
    s.step_up(
        "accept_grammar_orphans",
        "subjects_admin",
        "Mark orphaned subjects accepted in the new grammar.",
    );

    // Streaming subscriptions — read scope, no per-frame audit
    // (the subscription start is the auditable event if any).
    s.read(
        "subscribe_happenings",
        "subjects",
        "Subscribe to the durable happenings bus with optional cursor replay.",
    );
    s.read(
        "subscribe_subject",
        "subjects",
        "Subscribe to live state changes for one canonical subject.",
    );

    // Plugin dispatch — write scope. The plugin handles its own
    // dispatch-level audit via the framework's ledger primitive;
    // we do not double-emit at the projection layer.
    s.write(
        "request",
        "plugins",
        "Dispatch a plugin request on a specific shelf.",
    );
    s.write(
        "course_correct",
        "plugins",
        "Issue a course correction on a custody under a warden's care.",
    );
    s.write(
        "take_custody",
        "plugins",
        "Assign work to a warden plugin under custody.",
    );
    s.write(
        "release_custody",
        "plugins",
        "Release a custody held by a warden plugin.",
    );
    s.read(
        "list_active_custodies",
        "plugins",
        "Snapshot every currently-held custody on the steward.",
    );

    // Plugin inspection.
    s.read(
        "list_plugins",
        "plugins",
        "Enumerate every admitted plugin.",
    );
    s.read(
        "list_plugins_where",
        "plugins",
        "Filter admitted plugins by tag, kind, or admission state.",
    );
    s.read(
        "get_plugin_health",
        "plugins",
        "Aggregate health snapshot across every admitted plugin.",
    );
    s.read(
        "preview_dependency_cascade",
        "plugins",
        "Preview the cascade impact of disabling one plugin.",
    );

    // Plugin lifecycle mutations — privileged.
    s.step_up(
        "enable_plugin",
        "plugins_admin",
        "Enable an admitted plugin's dispatch.",
    );
    s.step_up(
        "disable_plugin",
        "plugins_admin",
        "Disable an admitted plugin's dispatch.",
    );
    s.step_up(
        "enable_plugins_where",
        "plugins_admin",
        "Bulk-enable admitted plugins matching a filter.",
    );
    s.step_up(
        "disable_plugins_where",
        "plugins_admin",
        "Bulk-disable admitted plugins matching a filter.",
    );
    s.step_up(
        "reload_plugin",
        "plugins_admin",
        "Hot-reload an admitted plugin (cold or live).",
    );
    s.step_up(
        "uninstall_plugin",
        "plugins_admin",
        "Uninstall an admitted plugin and remove its bundle.",
    );
    s.step_up(
        "install_plugin_from_url",
        "plugins_admin",
        "Install a plugin bundle from a remote URL.",
    );
    s.step_up(
        "purge_plugin_state",
        "plugins_admin",
        "Purge an admitted plugin's persistent state.",
    );

    // Plugin profiles.
    s.step_up(
        "put_plugin_profile",
        "plugins_admin",
        "Create or update a named plugin profile.",
    );
    s.read(
        "get_plugin_profile",
        "plugins",
        "Read one plugin profile by name.",
    );
    s.step_up(
        "delete_plugin_profile",
        "plugins_admin",
        "Delete a plugin profile by name.",
    );
    s.step_up(
        "set_active_plugin_profile",
        "plugins_admin",
        "Activate a named plugin profile.",
    );
    s.read(
        "list_plugin_profiles",
        "plugins",
        "Enumerate every plugin profile.",
    );

    // Plugin tags.
    s.write(
        "set_plugin_tag",
        "plugins_admin",
        "Set a tag on an admitted plugin.",
    );
    s.write(
        "delete_plugin_tag",
        "plugins_admin",
        "Delete a tag from an admitted plugin.",
    );
    s.read(
        "list_plugin_tags",
        "plugins",
        "Enumerate every tag currently asserted on admitted plugins.",
    );

    // Plugin registries.
    s.step_up(
        "register_plugin_registry",
        "plugins_admin",
        "Register a plugin registry source.",
    );
    s.step_up(
        "unregister_plugin_registry",
        "plugins_admin",
        "Unregister a plugin registry source.",
    );
    s.write(
        "refresh_plugin_registry",
        "plugins_admin",
        "Refresh the cached manifest for a registered plugin registry.",
    );
    s.read(
        "list_plugin_registries",
        "plugins",
        "Enumerate every registered plugin registry source.",
    );

    // Publisher trust.
    s.step_up(
        "grant_publisher_trust",
        "plugins_admin",
        "Grant trust to a plugin publisher's signing key.",
    );
    s.step_up(
        "revoke_publisher_trust",
        "plugins_admin",
        "Revoke trust from a plugin publisher's signing key.",
    );
    s.read(
        "list_publisher_trust",
        "plugins",
        "Enumerate every trusted plugin publisher.",
    );

    // Admission policies.
    s.step_up(
        "put_admission_policy",
        "plugins_admin",
        "Create or update a named admission policy.",
    );
    s.step_up(
        "delete_admission_policy",
        "plugins_admin",
        "Delete an admission policy by name.",
    );
    s.step_up(
        "set_active_admission_policy",
        "plugins_admin",
        "Activate a named admission policy.",
    );
    s.read(
        "get_admission_policy",
        "plugins",
        "Read one admission policy by name.",
    );
    s.read(
        "list_admission_policies",
        "plugins",
        "Enumerate every admission policy.",
    );
    s.read(
        "audit_against_policy",
        "plugins",
        "Audit a plugin's effective trust against a named policy.",
    );

    // Plugin capability revocations.
    s.step_up(
        "revoke_plugin_capability",
        "plugins_admin",
        "Revoke a specific capability from a plugin.",
    );
    s.step_up(
        "unrevoke_plugin_capability",
        "plugins_admin",
        "Restore a previously-revoked plugin capability.",
    );
    s.read(
        "list_plugin_capability_revocations",
        "plugins",
        "List active capability revocations for one plugin.",
    );
    s.read(
        "list_all_capability_revocations",
        "plugins",
        "List every active capability revocation across all plugins.",
    );

    // Catalogue / manifest reload.
    s.step_up(
        "reload_catalogue",
        "plugins_admin",
        "Reload the distribution catalogue from disk.",
    );
    s.step_up(
        "reload_manifest",
        "plugins_admin",
        "Reload one plugin's manifest from disk.",
    );

    // Appointments.
    s.write(
        "create_appointment",
        "plans",
        "Schedule a time-driven instruction for a plugin.",
    );
    s.write(
        "cancel_appointment",
        "plans",
        "Cancel a scheduled appointment.",
    );
    s.read(
        "project_appointment",
        "plans",
        "Project one appointment by id.",
    );
    s.read(
        "list_appointments",
        "plans",
        "Enumerate every scheduled appointment.",
    );

    // Watches.
    s.write(
        "create_watch",
        "plans",
        "Schedule a condition-driven instruction for a plugin.",
    );
    s.write("cancel_watch", "plans", "Cancel a scheduled watch.");
    s.read("project_watch", "plans", "Project one watch by id.");
    s.read("list_watches", "plans", "Enumerate every scheduled watch.");

    // User interaction prompts. Every op in this group is
    // capability-gated at the op layer by
    // `user_interaction_responder` — the single scope name that
    // defines the responder role. Gating on `ui` read/write
    // would have forced every scoped mint for a responder
    // session to carry `--scope ui` alongside
    // `--scope user_interaction_responder`, defeating least
    // privilege and producing a token twice the wire size for
    // no security or capability gain. The responder scope is
    // the honest authority for this group; a bearer holding it
    // has already been operator-issued for the responder role.
    s.write(
        "answer_user_interaction",
        "user_interaction_responder",
        "Submit an operator answer to an active user-interaction prompt.",
    );
    s.write(
        "cancel_user_interaction",
        "user_interaction_responder",
        "Cancel an active user-interaction prompt.",
    );
    s.read(
        "list_user_interactions",
        "user_interaction_responder",
        "Enumerate active user-interaction prompts.",
    );
    s.write(
        "release_user_interaction_responder",
        "user_interaction_responder",
        "Release the responder slot the calling bearer currently holds so a different bearer may claim without waiting for token TTL. Idempotent.",
    );

    // Plans (fire).
    s.step_up(
        "fire_plan",
        "plans_admin",
        "Fire a registered listening plan by id.",
    );

    // Audio topology (active topology).
    s.step_up(
        "publish_active_audio_topology",
        "audio_admin",
        "Publish a new active audio topology composition.",
    );
    s.step_up(
        "clear_active_audio_topology",
        "audio_admin",
        "Clear the active audio topology composition.",
    );
    s.read(
        "get_active_audio_topology",
        "audio",
        "Read the current active audio topology.",
    );
    s.read(
        "list_active_audio_topologies",
        "audio",
        "Enumerate every persisted active-audio-topology row.",
    );

    // Audio operator policies.
    s.write(
        "put_audio_operator_policy",
        "audio_admin",
        "Set the operator audio policy for one target.",
    );
    s.write(
        "delete_audio_operator_policy",
        "audio_admin",
        "Delete the operator audio policy for one target.",
    );
    s.read(
        "get_audio_operator_policy",
        "audio",
        "Read the operator audio policy for one target.",
    );
    s.read(
        "list_audio_operator_policies",
        "audio",
        "Enumerate every per-target operator audio policy.",
    );

    // Audio volume modes.
    s.write(
        "put_audio_volume_mode",
        "audio_admin",
        "Set the volume mode for one target.",
    );
    s.write(
        "delete_audio_volume_mode",
        "audio_admin",
        "Delete the volume mode for one target.",
    );
    s.read(
        "get_audio_volume_mode",
        "audio",
        "Read the volume mode for one target.",
    );
    s.read(
        "list_audio_volume_modes",
        "audio",
        "Enumerate every per-target volume mode.",
    );

    // Hardware profile overrides.
    s.write(
        "put_hardware_profile_override",
        "audio_admin",
        "Set the hardware profile override for one canonical identity.",
    );
    s.write(
        "delete_hardware_profile_override",
        "audio_admin",
        "Delete the hardware profile override for one canonical identity.",
    );
    s.read(
        "get_hardware_profile_override",
        "audio",
        "Read the hardware profile override for one canonical identity.",
    );
    s.read(
        "list_hardware_profile_overrides",
        "audio",
        "Enumerate every hardware profile override.",
    );

    // Reconciliation pairs.
    s.read(
        "list_reconciliation_pairs",
        "audio",
        "Enumerate every reconciliation pair in the steward.",
    );
    s.read(
        "project_reconciliation_pair",
        "audio",
        "Project one reconciliation pair by id.",
    );
    s.write(
        "reconcile_pair_now",
        "audio_admin",
        "Force a reconciliation pair to re-evaluate now.",
    );

    // Multi-room groups.
    s.write(
        "create_group",
        "multiroom",
        "Create a new multi-room group.",
    );
    s.write("delete_group", "multiroom", "Delete a multi-room group.");
    s.write("rename_group", "multiroom", "Rename a multi-room group.");
    s.write(
        "add_group_member",
        "multiroom",
        "Add a member device to a multi-room group.",
    );
    s.write(
        "remove_group_member",
        "multiroom",
        "Remove a member device from a multi-room group.",
    );
    s.read("get_group", "multiroom", "Read one multi-room group by id.");
    s.read(
        "list_groups",
        "multiroom",
        "Enumerate every multi-room group.",
    );
    s.read(
        "get_group_active_topology",
        "multiroom",
        "Read the active topology of a multi-room group.",
    );
    s.read(
        "list_group_active_topologies",
        "multiroom",
        "Enumerate the active topology of every multi-room group.",
    );
    s.write(
        "dispatch_to_group",
        "multiroom",
        "Dispatch a verb to every member of a multi-room group.",
    );
    s.read(
        "get_source_host",
        "multiroom",
        "Read the source-host election state for one group.",
    );
    s.read(
        "list_source_hosts",
        "multiroom",
        "Enumerate source-host election state for every group.",
    );
    s.read(
        "get_clock_sync",
        "multiroom",
        "Read the clock-sync state for one group.",
    );
    s.read(
        "list_clock_syncs",
        "multiroom",
        "Enumerate clock-sync state for every group.",
    );
    s.read(
        "list_discovered_peers",
        "multiroom",
        "Enumerate every peer the steward has discovered. Row carries \
         discovery fields plus chain-scope projection joined at read \
         time: presence_state, last_transition_at_ms, network.",
    );
    s.read(
        "roster_snap",
        "multiroom",
        "Gaze-triggered roster snap with marauder query. Returns a \
         truthful roster at the instant of composition; single \
         packet loss is not an absence claim.",
    );
    s.read(
        "list_gateway_plugins",
        "multiroom",
        "Enumerate admitted gateway plugins for external-ecosystem bridging.",
    );
    s.read(
        "list_audio_plane_connections",
        "multiroom",
        "Enumerate active audio-plane peer connections.",
    );
    s.write(
        "audio_plane_dial",
        "multiroom",
        "Operator-initiated dial of an audio-plane peer when \
         auto-discovery does not surface a dialable address.",
    );
    s.write(
        "move_group_member",
        "multiroom",
        "Atomic cross-group move; no visible solo intermediate. \
         Leader-of-source case composes the explicit-successor \
         protocol inline.",
    );
    s.write(
        "select_group_leader_successor",
        "multiroom",
        "Operator-explicit leader-successor selection. Atomically \
         pins the operator-chosen successor as source-host and removes \
         the departing leader.",
    );
    s.write(
        "cancel_group_leader_successor",
        "multiroom",
        "Cancel a pending leader-successor decision. The leader stays \
         in place; no group state changed.",
    );
    s.write(
        "pin_source_host",
        "multiroom",
        "Pin a specific device as the source-host for a multi-room \
         group. Election respects the pin while the device remains \
         a live member.",
    );
    s.write(
        "unpin_source_host",
        "multiroom",
        "Clear the source-host pin for a multi-room group. Election \
         resumes its standard canonical-min rule.",
    );
    s.read(
        "list_domain_members",
        "multiroom",
        "Enumerate every domain-membership row in the local trust \
         ledger, composed with live discovery state and the persistent \
         display-name cache.",
    );
    s.write(
        "admit_peer_to_domain",
        "multiroom",
        "Admit a device to the local domain trust ledger.",
    );
    s.write(
        "revoke_peer_from_domain",
        "multiroom",
        "Revoke a device's domain admission (soft revoke).",
    );
    s.write(
        "discard_peer_from_domain",
        "multiroom",
        "Operator-explicit discard of a peer from the domain. Appends a \
         signed entry to the domain witness chain; receivers project the \
         discard byte-equal. Irreversible.",
    );
    s.write(
        "bootstrap_domain",
        "multiroom",
        "Found a new multi-room domain by signing the genesis chain entry. \
         Refuses when the local chain already contains any entry. Operator-\
         gestured; the env-var path is retired.",
    );
    s.write(
        "join_domain",
        "multiroom",
        "Mark the local device as looking to join an existing domain. Emits \
         a happening so admitted-peer UIs can complete admission via \
         admit_peer_to_domain.",
    );
    s.write(
        "leave_domain",
        "multiroom",
        "Soft-leave the current domain. Discards the local chain log; the \
         per-device signing key persists so a subsequent join reuses the \
         same identity. Steward restart required.",
    );
    s.write(
        "factory_reset_domain",
        "multiroom",
        "Hard-reset the device's domain state. Discards the chain log and \
         the per-device signing key so the device returns to a fresh-from-\
         factory posture. Steward restart required.",
    );
    s.read(
        "get_chain_head",
        "multiroom",
        "Read the current domain witness chain head hash + length. The \
         freshness oracle for chain-substrate consumers.",
    );
    s.read(
        "domain_history",
        "multiroom",
        "Replay recent domain witness chain entries in chronological \
         order. Audit surface; bounded by an optional limit.",
    );
    s.write(
        "move_member",
        "multiroom",
        "Atomic move of a device between groups. Replaces the legacy \
         remove-then-add pair so the operator's intent is captured as one \
         chain entry.",
    );
    s.write(
        "set_group_leader",
        "multiroom",
        "Explicit operator-gestured leader handoff for a group. Appends a \
         signed leader-handoff entry to the domain witness chain.",
    );
    s.write(
        "trigger_reconnect",
        "multiroom",
        "Fire a reconnect storm against an absent peer. Every available \
         carrier is tried in parallel for the configured storm window.",
    );
    s.read(
        "export_chain",
        "multiroom",
        "Export the local domain witness chain as a portable signed \
         artefact for out-of-band transport.",
    );
    s.write(
        "import_chain",
        "multiroom",
        "Import a peer-supplied chain artefact; each witness is verified \
         and applied via deterministic linearisation.",
    );
    s.write(
        "update_peer_endpoints",
        "multiroom",
        "Refresh a peer's sticky endpoint list. Future reconnect storms \
         try the refreshed endpoints first.",
    );
    s.write(
        "declare_network_relay",
        "multiroom",
        "Declare this device as a chain-aware relay between named \
         networks. Receivers see the relay in their projection on the \
         next chain head advance.",
    );

    // Per-group operator-tunable latency budget. Distinct from
    // `set_group_leader` (which is the chain-substrate's explicit
    // leader-handoff op); this is the GroupStore-level `leader_ms`
    // field setter consumed by the multi-room plugin's renderer
    // every frame.
    s.step_up(
        "set_group_leader_ms",
        "plugins_admin",
        "Set the per-group multi-room latency budget in ms (range \
         10..=5000). Multi-room plugin reads it every render frame.",
    );

    // Per-device operator-gestured role substrate. The multi-room
    // plugin observes RoleStore via subscription and reconfigures
    // DAC, capture, and audio-plane connections in place. Values:
    // `source` / `receiver` / `auto`. Substrate-empty default is
    // `auto`.
    s.step_up(
        "set_device_role",
        "plugins_admin",
        "Set the per-device multi-room role (`source` / `receiver` / \
         `auto`). Idempotent on unchanged value.",
    );
    s.read(
        "get_device_role",
        "plugins_admin",
        "Read the operator-declared role for a device (`auto` when \
         no explicit gesture).",
    );
    s.read(
        "list_device_roles",
        "plugins_admin",
        "List every device with an explicit operator-gestured role.",
    );
    s.step_up(
        "clear_device_role",
        "plugins_admin",
        "Clear the operator-declared role for a device; return to \
         `auto`. Idempotent on devices already at default.",
    );

    // 5-carrier reconnect storm. Distinct from the chain-
    // substrate's `trigger_reconnect`: this is the LAN-
    // presence-substrate reconnect entry point, the operator-
    // canonical surface for restoring an unreachable peer.
    s.step_up(
        "reconnect_peer",
        "plugins_admin",
        "Operator-gestured 5-carrier reconnect storm against an \
         unreachable peer; 30 s deadline; first-carrier-wins.",
    );

    // Plugin-lifecycle operator gestures. Distinct from the legacy
    // `reload_plugin` (which serves `lifecycle.hot_reload` Live /
    // Restart mechanisms): these route through the per-plugin
    // `lifecycle.mode` surface (`reactive-only` / `reload-cleanable`
    // / `frozen`).
    s.step_up(
        "plugin_reload",
        "plugins_admin",
        "Operator-gestured plugin reload via the lifecycle \
         coordinator; routes through the per-plugin `lifecycle.mode` \
         surface.",
    );
    s.step_up(
        "plugin_restore",
        "plugins_admin",
        "Operator-gestured recovery of a degraded plugin; clears the \
         degraded slot and resets failure counters.",
    );

    // Device identity.
    s.read(
        "get_device_identity",
        "system",
        "Read this device's persistent identity.",
    );
    s.write(
        "set_device_display_name",
        "system_admin",
        "Set this device's operator-facing display name.",
    );
    s.write(
        "reset_device_display_name",
        "system_admin",
        "Reset this device's display name to its default \
         (re-seeded from the OS hostname when sane, or \
         `evo-<short>` fallback). Clears any prior operator \
         override; `name_source` returns to `Auto`.",
    );

    // Updates.
    s.write(
        "check_updates_now",
        "updates_admin",
        "Trigger a manifest fetch from every registered update source.",
    );
    s.step_up(
        "apply_update",
        "updates_admin",
        "Apply a pending update (core or plugins).",
    );
    s.read(
        "list_update_inventory",
        "updates",
        "Enumerate the current update inventory across every source.",
    );
    s.read(
        "get_auto_apply_policies",
        "updates",
        "Read the auto-apply policy set.",
    );
    s.step_up(
        "set_auto_apply_policy",
        "updates_admin",
        "Set the auto-apply policy for a target.",
    );
    s.read(
        "get_update_channels",
        "updates",
        "Read the active update channel for core and plugins.",
    );
    s.step_up(
        "set_update_channel",
        "updates_admin",
        "Set the active update channel for a target.",
    );
    s.step_up_on_dispatch(
        "request_steward_restart",
        "system_admin",
        "Request a graceful steward restart via the execve coordinator.",
    );

    // Step-up auth.
    s.write(
        "step_up_auth_verify",
        "auth",
        "Verify operator credentials and issue a step-up auth session.",
    );
    s.write(
        "step_up_auth_revoke",
        "auth",
        "Revoke an active step-up auth session.",
    );

    // Session-trust wire surface (kiosk mint + pair-once +
    // kiosk password set).
    s.write(
        "mint_local_kiosk_session",
        "auth",
        "Mint an operator bearer for the device-local kiosk shell \
         via SO_PEERCRED against the compositor UID allowlist.",
    );
    s.write(
        "pair_begin",
        "auth",
        "Begin a remote-browser pair-once ceremony; framework \
         publishes an 8-digit code to the kiosk display for the \
         operator to type back through pair_complete.",
    );
    s.write(
        "pair_complete",
        "auth",
        "Complete a pair-once ceremony; mints a paired-device bearer \
         on the correct code.",
    );
    s.write(
        "pair_authenticate",
        "auth",
        "Establish browser trust by authenticating with the OS \
         password; issues a paired-device bearer in one round-trip. \
         Consumer path.",
    );
    s.read(
        "pair_list",
        "plugins",
        "List every paired-device entry the framework tracks.",
    );
    s.step_up(
        "pair_revoke",
        "plugins_admin",
        "Revoke a paired device by id.",
    );
    s.write(
        "set_kiosk_password",
        "auth",
        "Set the operator's kiosk password via the compositor UID \
         allowlist; writes an argon2id PHC hash to the distribution-\
         configured shared-secret file.",
    );

    // UI artefact admission + activation.
    s.step_up(
        "activate_theme",
        "plugins_admin",
        "Activate an admitted theme artefact.",
    );
    s.step_up(
        "activate_ui_shell",
        "plugins_admin",
        "Activate an admitted UI shell artefact.",
    );
    s.read(
        "describe_active_ui_selection",
        "plugins",
        "Read the currently-active theme and UI shell selection.",
    );
    s.read(
        "describe_ui_stockings",
        "plugins",
        "Enumerate every admitted UI stocking across every shelf.",
    );
    s.write(
        "record_wizard_step_completion",
        "ui",
        "Renderer-driven wizard step acknowledgement; advances the persisted resume cursor and (for consent steps) appends an evo.consent ledger entry.",
    );

    // Configuration migration.
    s.step_up(
        "export_migration_bundle",
        "system_admin",
        "Export the device's configuration substrate as a migration bundle.",
    );
    s.step_up(
        "import_migration_bundle",
        "system_admin",
        "Apply a configuration migration bundle to the device.",
    );

    // Per-credential management. The operator manages named
    // bearer credentials with custom scopes and operator-set
    // expiry policy (never / seconds) through the UI tier
    // toggle screen; each op composes the framework's
    // CredentialStore + RevocationList primitives.
    s.write(
        "create_bearer_token",
        "auth",
        "Mint a named operator-issued bearer credential with operator-set scopes and expiry.",
    );
    s.read(
        "list_bearer_tokens",
        "auth",
        "List the operator-managed bearer credential inventory (token bytes excluded; metadata only).",
    );
    s.write(
        "revoke_bearer_token",
        "auth",
        "Revoke a previously-minted bearer credential by token id; revocation persists across steward restarts.",
    );
    s.step_up(
        "reset_credentials_to_open",
        "system_admin",
        "Recovery gesture: purge every credential record + revocation entry and flip the steward to Open tier on next boot.",
    );

    // Plugin credential vault — third-party API keys / service
    // passwords / OAuth tokens the framework holds on behalf of
    // plugins. The three ops are the operator UI's CRUD surface;
    // never a get op — values leave the vault only via the
    // plugin-side LoadContext handle.
    s.write(
        "credential_put",
        "credentials",
        "Store an operator-supplied credential value for a plugin; overwrites any prior entry under the same (plugin_id, key).",
    );
    s.write(
        "credential_delete",
        "credentials",
        "Remove a credential for a plugin. Idempotent — deleting an already-absent entry succeeds silently.",
    );
    s.read(
        "credential_list_keys",
        "credentials",
        "Enumerate a plugin's credential inventory. Returns key_hash + metadata + timestamps only — value bytes are never surfaced.",
    );

    // Online-provider per-source enable + priority. The three ops
    // are the operator UI's CRUD surface for the framework-wide
    // per-provider config store: list to render Settings, set
    // gestures publish on the change bus so plugin reactors
    // re-resolve their local view live.
    s.read(
        "online_providers_list",
        "online_providers",
        "Read every registered online provider's enable + priority ordered (priority ascending, provider_id ascending).",
    );
    s.write(
        "online_providers_set_enabled",
        "online_providers",
        "Toggle the enable flag on one online provider. Publishes on the framework's online-provider-config change bus so plugin reactors re-resolve live.",
    );
    s.write(
        "online_providers_set_priority",
        "online_providers",
        "Set the cascade priority (0..=999) on one online provider. Publishes on the framework's online-provider-config change bus so plugin reactors re-resolve live.",
    );

    s.into_inner()
}

/// Return the runtime-side list of supported wire-op ids that
/// the steward's `describe_capabilities` response advertises.
///
/// **Single source of truth.** The returned `Vec<String>` is
/// derived from [`canonical_schema`] so the runtime response,
/// the schema-discipline registry, the projection-layer's
/// generated SDK docstrings, OpenAPI summaries, GraphQL field
/// descriptions, and capability-discovery probes consumed by
/// UI shells all see the SAME op set by construction.
///
/// Adding a new wire op is exactly one registry edit: a new
/// entry in [`canonical_schema`]. The runtime `Capabilities`
/// response, the schema-discipline tests, and every downstream
/// schema-first generator pick up the change without a second
/// edit. There is no parallel static array to keep in sync;
/// drift between the two is structurally impossible.
///
/// Prior shape held a parallel `SUPPORTED_OPS: &[&str]` static
/// array in `server.rs` that the response read directly. Every
/// commit that landed a new wire op had to remember to edit
/// both registries; many forgot, so 27 ops shipped with
/// dispatch handlers but were silently missing from the
/// runtime `describe_capabilities` response, breaking
/// capability-discovery for any UI shell that depended on
/// probing before consuming a new op. The single-source-of-
/// truth refactor eliminates the failure mode by removing the
/// parallel registry.
pub fn runtime_op_ids() -> Vec<String> {
    canonical_schema()
        .into_iter()
        .map(|op| op.id.as_str().to_string())
        .collect()
}

/// Internal builder used by [`canonical_schema`] to keep the
/// per-entry call sites compact.
struct Schema(Vec<WireOp>);

impl Schema {
    fn with_capacity(n: usize) -> Self {
        Self(Vec::with_capacity(n))
    }

    fn push(
        &mut self,
        id: &str,
        cap: CapabilityRequirement,
        audit: AuditTiming,
        summary: &str,
    ) {
        self.0.push(WireOp::new(
            WireOpId::new(id)
                .expect("canonical_schema entry id must be valid snake_case"),
            cap,
            audit,
            summary.to_string(),
        ));
    }

    /// Anonymous-OK op (no capability gate, no audit emission).
    fn anonymous(&mut self, id: &str, summary: &str) {
        self.push(id, CapabilityRequirement::None, AuditTiming::None, summary);
    }

    /// Read scope, no audit. Default for queries.
    fn read(&mut self, id: &str, scope: &str, summary: &str) {
        self.push(
            id,
            CapabilityRequirement::read(scope),
            AuditTiming::None,
            summary,
        );
    }

    /// Write scope, audit on every dispatch outcome. Default for
    /// state-mutating ops that do not require step-up.
    fn write(&mut self, id: &str, scope: &str, summary: &str) {
        self.push(
            id,
            CapabilityRequirement::write(scope),
            AuditTiming::Always,
            summary,
        );
    }

    /// Step-up scope, audit on every dispatch outcome. Default
    /// for privileged operations.
    fn step_up(&mut self, id: &str, scope: &str, summary: &str) {
        self.push(
            id,
            CapabilityRequirement::step_up(scope),
            AuditTiming::Always,
            summary,
        );
    }

    /// Step-up scope, audit on dispatch entry only. Used for
    /// long-running mutations whose attempt is the auditable
    /// event regardless of body outcome (steward restart).
    fn step_up_on_dispatch(&mut self, id: &str, scope: &str, summary: &str) {
        self.push(
            id,
            CapabilityRequirement::step_up(scope),
            AuditTiming::OnDispatch,
            summary,
        );
    }

    fn into_inner(self) -> Vec<WireOp> {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The number of variants in the `ClientRequest` enum at the
    /// time this schema was authored. The
    /// `schema_count_matches_client_request_variant_count` test
    /// asserts the schema matches; bumping the enum and not
    /// bumping this number flags a missing schema entry.
    const EXPECTED_SCHEMA_COUNT: usize = 176;

    #[test]
    fn schema_count_matches_client_request_variant_count() {
        let schema = canonical_schema();
        assert_eq!(
            schema.len(),
            EXPECTED_SCHEMA_COUNT,
            "canonical_schema count drifted from ClientRequest \
             variant count; sync the schema entries with the enum"
        );
    }

    #[test]
    fn every_wire_op_id_is_unique() {
        let schema = canonical_schema();
        let mut seen: HashSet<&str> = HashSet::new();
        for op in &schema {
            assert!(
                seen.insert(op.id.as_str()),
                "duplicate WireOpId in canonical_schema: {}",
                op.id
            );
        }
    }

    #[test]
    fn every_wire_op_id_is_snake_case() {
        let schema = canonical_schema();
        for op in &schema {
            let id = op.id.as_str();
            let first = id.chars().next().unwrap();
            assert!(
                first.is_ascii_lowercase(),
                "WireOpId '{id}' must start with lowercase"
            );
            for c in id.chars() {
                assert!(
                    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_',
                    "WireOpId '{id}' contains non-snake-case char '{c}'"
                );
            }
        }
    }

    #[test]
    fn describe_capabilities_is_anonymous() {
        let schema = canonical_schema();
        let op = schema
            .iter()
            .find(|o| o.id.as_str() == "describe_capabilities")
            .expect("describe_capabilities must be in canonical schema");
        assert_eq!(op.capability, CapabilityRequirement::None);
        assert_eq!(op.audit, AuditTiming::None);
    }

    #[test]
    fn list_plugins_is_read_scope() {
        let schema = canonical_schema();
        let op = schema
            .iter()
            .find(|o| o.id.as_str() == "list_plugins")
            .expect("list_plugins must be in canonical schema");
        assert_eq!(op.capability, CapabilityRequirement::read("plugins"));
        assert_eq!(op.audit, AuditTiming::None);
    }

    #[test]
    fn set_update_channel_is_step_up_audited() {
        let schema = canonical_schema();
        let op = schema
            .iter()
            .find(|o| o.id.as_str() == "set_update_channel")
            .expect("set_update_channel must be in canonical schema");
        assert_eq!(
            op.capability,
            CapabilityRequirement::step_up("updates_admin")
        );
        assert_eq!(op.audit, AuditTiming::Always);
    }

    #[test]
    fn request_steward_restart_audits_on_dispatch() {
        let schema = canonical_schema();
        let op = schema
            .iter()
            .find(|o| o.id.as_str() == "request_steward_restart")
            .expect("request_steward_restart must be in canonical schema");
        assert_eq!(
            op.capability,
            CapabilityRequirement::step_up("system_admin")
        );
        assert_eq!(op.audit, AuditTiming::OnDispatch);
    }

    #[test]
    fn every_step_up_op_audits_always_or_on_dispatch() {
        let schema = canonical_schema();
        for op in &schema {
            if op.capability.requires_step_up() {
                assert!(
                    matches!(
                        op.audit,
                        AuditTiming::Always | AuditTiming::OnDispatch
                    ),
                    "step-up op '{}' must audit; current timing is {:?}",
                    op.id,
                    op.audit,
                );
            }
        }
    }

    #[test]
    fn every_read_op_is_unaudited() {
        let schema = canonical_schema();
        for op in &schema {
            if matches!(op.capability, CapabilityRequirement::Read { .. }) {
                assert_eq!(
                    op.audit,
                    AuditTiming::None,
                    "read-scope op '{}' should not audit; current \
                     timing is {:?}",
                    op.id,
                    op.audit,
                );
            }
        }
    }

    #[test]
    fn every_summary_is_non_empty_and_under_a_sentence() {
        let schema = canonical_schema();
        for op in &schema {
            assert!(
                !op.summary.is_empty(),
                "WireOp '{}' has empty summary",
                op.id
            );
            // Summaries are one sentence; allow up to 200 chars
            // before flagging the entry as too verbose for
            // generated SDK docstrings / OpenAPI summaries.
            assert!(
                op.summary.len() <= 200,
                "WireOp '{}' summary exceeds 200 chars ({} chars)",
                op.id,
                op.summary.len(),
            );
        }
    }
}
