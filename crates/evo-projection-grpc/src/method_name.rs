// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Wire-op id → gRPC method name conversion.
//!
//! Wire op ids are snake_case (e.g. `describe_capabilities`);
//! gRPC method names are PascalCase (e.g.
//! `DescribeCapabilities`). The conversion splits on underscore
//! and capitalises each segment.

/// Convert a snake_case wire op id to PascalCase.
///
/// Examples:
///
/// - `describe_capabilities` → `DescribeCapabilities`
/// - `set_update_channel` → `SetUpdateChannel`
/// - `request` → `Request`
/// - `step_up_auth_verify` → `StepUpAuthVerify`
///
/// Numbers ride along verbatim: `project_subject_v2` →
/// `ProjectSubjectV2`. Empty segments (e.g. from a stray double
/// underscore) are skipped; the [`WireOpId`] validator already
/// refuses inputs with leading underscores or double
/// underscores, so this is defence in depth rather than a
/// load-bearing rule.
///
/// [`WireOpId`]: evo_projection_core::WireOpId
pub fn pascal_case_method_name(snake: &str) -> String {
    let mut out = String::with_capacity(snake.len());
    for segment in snake.split('_').filter(|s| !s.is_empty()) {
        let mut chars = segment.chars();
        if let Some(first) = chars.next() {
            for c in first.to_uppercase() {
                out.push(c);
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
    fn single_segment_capitalises() {
        assert_eq!(pascal_case_method_name("request"), "Request");
    }

    #[test]
    fn two_segments_concatenate() {
        assert_eq!(
            pascal_case_method_name("describe_capabilities"),
            "DescribeCapabilities"
        );
    }

    #[test]
    fn three_segments_concatenate() {
        assert_eq!(
            pascal_case_method_name("set_update_channel"),
            "SetUpdateChannel"
        );
    }

    #[test]
    fn four_segments_concatenate() {
        assert_eq!(
            pascal_case_method_name("step_up_auth_verify"),
            "StepUpAuthVerify"
        );
    }

    #[test]
    fn digits_ride_along() {
        assert_eq!(
            pascal_case_method_name("project_subject_v2"),
            "ProjectSubjectV2"
        );
    }

    #[test]
    fn already_lowercase_alone_stays() {
        assert_eq!(pascal_case_method_name("negotiate"), "Negotiate");
    }

    #[test]
    fn empty_string_yields_empty() {
        assert_eq!(pascal_case_method_name(""), "");
    }

    #[test]
    fn defence_in_depth_skips_double_underscore() {
        // WireOpId validator refuses these, but we must not
        // panic if one slips through.
        assert_eq!(pascal_case_method_name("foo__bar"), "FooBar");
    }

    #[test]
    fn defence_in_depth_skips_trailing_underscore() {
        assert_eq!(pascal_case_method_name("foo_"), "Foo");
    }
}
