// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Wire op id → GraphQL field name conversion.
//!
//! Wire op ids are snake_case (`describe_capabilities`);
//! GraphQL fields are camelCase (`describeCapabilities`). The
//! conversion lowercases the first segment and capitalises the
//! remainder.

/// Convert a snake_case wire op id to camelCase.
///
/// Examples:
///
/// - `describe_capabilities` → `describeCapabilities`
/// - `set_update_channel` → `setUpdateChannel`
/// - `request` → `request`
/// - `step_up_auth_verify` → `stepUpAuthVerify`
///
/// Digits in the wire op id ride along verbatim:
/// `project_subject_v2` → `projectSubjectV2`. Empty segments
/// from a stray underscore are skipped; the
/// [`WireOpId`](evo_projection_core::WireOpId) validator
/// already refuses inputs with leading or repeated
/// underscores, so this is defence in depth.
pub fn camel_case_field_name(snake: &str) -> String {
    let mut out = String::with_capacity(snake.len());
    let mut segments = snake.split('_').filter(|s| !s.is_empty());

    if let Some(first) = segments.next() {
        out.push_str(first);
    }

    for segment in segments {
        let mut chars = segment.chars();
        if let Some(c) = chars.next() {
            for u in c.to_uppercase() {
                out.push(u);
            }
            out.push_str(chars.as_str());
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_segment_stays_lowercase() {
        assert_eq!(camel_case_field_name("request"), "request");
    }

    #[test]
    fn two_segments_camelcase() {
        assert_eq!(
            camel_case_field_name("describe_capabilities"),
            "describeCapabilities"
        );
    }

    #[test]
    fn three_segments_camelcase() {
        assert_eq!(
            camel_case_field_name("set_update_channel"),
            "setUpdateChannel"
        );
    }

    #[test]
    fn four_segments_camelcase() {
        assert_eq!(
            camel_case_field_name("step_up_auth_verify"),
            "stepUpAuthVerify"
        );
    }

    #[test]
    fn digits_ride_along() {
        assert_eq!(
            camel_case_field_name("project_subject_v2"),
            "projectSubjectV2"
        );
    }

    #[test]
    fn empty_string_yields_empty() {
        assert_eq!(camel_case_field_name(""), "");
    }

    #[test]
    fn defence_in_depth_double_underscore() {
        assert_eq!(camel_case_field_name("foo__bar"), "fooBar");
    }

    #[test]
    fn defence_in_depth_trailing_underscore() {
        assert_eq!(camel_case_field_name("foo_"), "foo");
    }

    #[test]
    fn subscribe_prefix_stays_lowercase_first() {
        assert_eq!(
            camel_case_field_name("subscribe_happenings"),
            "subscribeHappenings"
        );
    }
}
