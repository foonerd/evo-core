//! Mock-steward testing harness (placeholder).
//!
//! This module will hold a mock steward that plugin authors can run in-process
//! to exercise their plugin without a real steward. It will drive the full
//! lifecycle (`describe`, `load`, `unload`, `health_check`, and the
//! kind-specific verbs) and capture outbound messages (`report_state`,
//! `announce_instance`, `retract_instance`, `report_custody_state`,
//! `request_user_interaction`) so tests can assert on them.
//!
//! TODO(evo-plugin-sdk pass 4): populate after contract traits and wire
//! protocol types exist. The harness depends on both and is therefore the
//! last SDK pass.
