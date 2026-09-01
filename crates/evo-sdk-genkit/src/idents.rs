// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Identifier-case conversions.
//!
//! The wire schema's op ids are snake_case; per-language client
//! SDK generators convert to their target's convention. Each
//! converter is total — defensively skips empty segments from
//! malformed input, never panics.

/// Convert a snake_case identifier to camelCase.
///
/// `describe_capabilities` → `describeCapabilities`,
/// `request` → `request`, `step_up_auth_verify` →
/// `stepUpAuthVerify`. Empty segments from stray underscores
/// are dropped.
pub fn to_camel_case(snake: &str) -> String {
    let mut out = String::with_capacity(snake.len());
    let mut segments = snake.split('_').filter(|s| !s.is_empty());

    if let Some(first) = segments.next() {
        out.push_str(first);
    }
    for seg in segments {
        let mut chars = seg.chars();
        if let Some(c) = chars.next() {
            for u in c.to_uppercase() {
                out.push(u);
            }
            out.push_str(chars.as_str());
        }
    }
    out
}

/// Convert a snake_case identifier to PascalCase.
///
/// `describe_capabilities` → `DescribeCapabilities`,
/// `request` → `Request`, `step_up_auth_verify` →
/// `StepUpAuthVerify`.
pub fn to_pascal_case(snake: &str) -> String {
    let mut out = String::with_capacity(snake.len());
    for seg in snake.split('_').filter(|s| !s.is_empty()) {
        let mut chars = seg.chars();
        if let Some(c) = chars.next() {
            for u in c.to_uppercase() {
                out.push(u);
            }
            out.push_str(chars.as_str());
        }
    }
    out
}

/// Convert a snake_case identifier to SCREAMING_SNAKE_CASE.
///
/// `describe_capabilities` → `DESCRIBE_CAPABILITIES`,
/// `request` → `REQUEST`. Used for SDK constants such as the
/// canonical op id surface that consumers may switch against.
pub fn to_screaming_snake_case(snake: &str) -> String {
    snake.to_uppercase()
}

/// Convert any case to snake_case.
///
/// Idempotent on snake_case input. Splits on transitions
/// between lowercase / digit and uppercase, between uppercase
/// runs and lowercase, and on whitespace / hyphens. Result is
/// lowercased.
///
/// `DescribeCapabilities` → `describe_capabilities`,
/// `step_up_auth_verify` → `step_up_auth_verify`,
/// `XMLParser` → `xml_parser`.
pub fn to_snake_case(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 4);
    let chars: Vec<char> = input.chars().collect();
    let mut last_was_lower = false;
    let mut last_was_underscore = true; // suppress leading underscore

    for (i, &c) in chars.iter().enumerate() {
        if c == '_' || c == '-' || c.is_whitespace() {
            if !last_was_underscore && !out.is_empty() {
                out.push('_');
                last_was_underscore = true;
            }
            last_was_lower = false;
            continue;
        }

        if c.is_ascii_uppercase() {
            let next_is_lower = chars
                .get(i + 1)
                .map(|n| n.is_ascii_lowercase())
                .unwrap_or(false);
            if !last_was_underscore
                && !out.is_empty()
                && (last_was_lower || next_is_lower)
            {
                out.push('_');
            }
            for low in c.to_lowercase() {
                out.push(low);
            }
            last_was_lower = false;
            last_was_underscore = false;
        } else {
            out.push(c);
            last_was_lower = c.is_ascii_lowercase();
            last_was_underscore = false;
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // camelCase

    #[test]
    fn camel_single_segment() {
        assert_eq!(to_camel_case("request"), "request");
    }

    #[test]
    fn camel_two_segments() {
        assert_eq!(
            to_camel_case("describe_capabilities"),
            "describeCapabilities"
        );
    }

    #[test]
    fn camel_four_segments() {
        assert_eq!(to_camel_case("step_up_auth_verify"), "stepUpAuthVerify");
    }

    #[test]
    fn camel_skips_double_underscore() {
        assert_eq!(to_camel_case("foo__bar"), "fooBar");
    }

    #[test]
    fn camel_empty_yields_empty() {
        assert_eq!(to_camel_case(""), "");
    }

    // PascalCase

    #[test]
    fn pascal_single_segment() {
        assert_eq!(to_pascal_case("request"), "Request");
    }

    #[test]
    fn pascal_two_segments() {
        assert_eq!(
            to_pascal_case("describe_capabilities"),
            "DescribeCapabilities"
        );
    }

    #[test]
    fn pascal_four_segments() {
        assert_eq!(to_pascal_case("step_up_auth_verify"), "StepUpAuthVerify");
    }

    #[test]
    fn pascal_empty_yields_empty() {
        assert_eq!(to_pascal_case(""), "");
    }

    // SCREAMING_SNAKE_CASE

    #[test]
    fn screaming_uppercases_each_char() {
        assert_eq!(
            to_screaming_snake_case("describe_capabilities"),
            "DESCRIBE_CAPABILITIES"
        );
    }

    #[test]
    fn screaming_preserves_underscores() {
        assert_eq!(
            to_screaming_snake_case("step_up_auth_verify"),
            "STEP_UP_AUTH_VERIFY"
        );
    }

    // to_snake_case

    #[test]
    fn snake_from_camel() {
        assert_eq!(
            to_snake_case("describeCapabilities"),
            "describe_capabilities"
        );
    }

    #[test]
    fn snake_from_pascal() {
        assert_eq!(
            to_snake_case("DescribeCapabilities"),
            "describe_capabilities"
        );
    }

    #[test]
    fn snake_from_snake_is_idempotent() {
        assert_eq!(to_snake_case("step_up_auth_verify"), "step_up_auth_verify");
    }

    #[test]
    fn snake_acronym_run_splits_before_trailing_word() {
        assert_eq!(to_snake_case("XMLParser"), "xml_parser");
    }

    #[test]
    fn snake_handles_kebab_and_whitespace() {
        assert_eq!(
            to_snake_case("describe-capabilities"),
            "describe_capabilities"
        );
        assert_eq!(
            to_snake_case("describe capabilities"),
            "describe_capabilities"
        );
    }

    #[test]
    fn snake_from_empty_yields_empty() {
        assert_eq!(to_snake_case(""), "");
    }
}
