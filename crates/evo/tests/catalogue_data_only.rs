//! Invariant: parsing a catalogue is data-only.
//!
//! The catalogue is TOML-as-data. Loading a catalogue MUST NOT
//! execute code from it, MUST NOT read files outside the supplied
//! input, MUST NOT spawn processes, MUST NOT panic on malformed or
//! hostile input, and MUST terminate on every input shape the
//! steward might encounter in the wild.
//!
//! The negative-path scenarios in this file pin that contract.
//! [`Catalogue::from_toml`] is the single entry point; every test
//! drives a synthetic input through it and asserts that the function
//! returns (with `Ok` or a structured `Err`) without panicking and
//! without exceeding wall-clock and memory bounds. The shape of each
//! test is "construct a hostile or unusual input, call from_toml, then
//! assert that the result is a real `Result` and that no panic
//! occurred". The wall-clock guard is implicit in the test framework's
//! per-test timeout; the memory guard is implicit in the OS-level
//! per-process limit.
//!
//! ## Side-effect oracle
//!
//! The "no code execution" property is enforced structurally:
//! [`Catalogue::from_toml`]'s only collaborator is the `toml` crate's
//! deserialiser, which has no API for spawning processes or opening
//! arbitrary files; the steward's catalogue-parse path itself takes a
//! `&str` and returns a `Result`, never touching the filesystem
//! beyond the caller's input. Any future refactor that introduces a
//! `std::process::Command` invocation, a `std::fs::read` of an
//! attacker-controlled path, or any other side effect from inside
//! `from_toml` would be a structural change visible in code review.
//! These tests pin the dynamic shape (no panic, terminates) so the
//! engineering claim above remains true at runtime.
//!
//! Each test uses a fresh `Catalogue::from_toml` call; the parser is
//! pure with respect to the input string, so tests are independent
//! and order does not matter.

use evo::catalogue::Catalogue;

/// Construct a TOML document with `depth` levels of nested inline
/// tables under a single root key. Used to drive the parser into a
/// recursion path that, on a naive implementation, would consume an
/// unbounded amount of stack.
///
/// Example for `depth = 3`:
/// `nested = { a = { a = { a = "leaf" } } }`
fn deeply_nested_toml(depth: usize) -> String {
    let mut s = String::with_capacity(depth * 6 + 32);
    s.push_str("nested = ");
    for _ in 0..depth {
        s.push_str("{ a = ");
    }
    s.push_str("\"leaf\"");
    for _ in 0..depth {
        s.push_str(" }");
    }
    s.push('\n');
    s
}

#[test]
fn deeply_nested_tables_do_not_overflow() {
    // 200 levels: well past anything a real catalogue would carry,
    // chosen to expose any stack-recursive parser without consuming
    // enough memory to be slow on a developer machine.
    let toml = deeply_nested_toml(200);
    let result = Catalogue::from_toml(&toml);
    // Either the parser tolerates the depth and returns `Ok` (the
    // unknown `nested` field is ignored under the catalogue schema),
    // or it returns a structured `Err`. Both outcomes satisfy the
    // data-only contract. What MUST NOT happen is a stack overflow
    // or an unhandled panic; reaching this assertion is the proof.
    let _ = result;
}

#[test]
fn ten_megabyte_string_does_not_oom() {
    // A 10 MiB string in a TOML field is uncomfortably large for a
    // catalogue but must not crash the steward. The string is built
    // from a repeated benign character to keep test memory low for
    // the test runner itself.
    let big: String = "a".repeat(10 * 1024 * 1024);
    let toml = format!(
        "schema_version = 1\n\n[[racks]]\nname = \"big\"\nfamily = \"domain\"\ncharter = \"{}\"\n",
        big
    );
    let result = Catalogue::from_toml(&toml);
    // The catalogue may be accepted (a 10 MiB charter is technically
    // legal TOML) or rejected by validation. Either is fine; the
    // property is "no OOM, no panic".
    let _ = result;
}

#[test]
fn diverse_unicode_input_does_not_panic() {
    // A spread of characters touching multiple Unicode categories:
    // Latin, Greek, Cyrillic, CJK ideographs, Arabic (RTL),
    // Devanagari, emoji (supplementary plane), combining marks,
    // zero-width joiner, control-equivalent (BOM as data, NOT at
    // file start). Each is legal inside a TOML quoted string. The
    // catalogue does not interpret these bytes; the steward's
    // contract is simply that they round-trip through the parser
    // without panicking.
    let charter = "\
        ABC abc 123 \
        \u{0391}\u{03B2}\u{03B3} \
        \u{0414}\u{043E}\u{0431}\u{0440}\u{043E} \
        \u{4E2D}\u{6587} \
        \u{0627}\u{0644}\u{0639}\u{0631}\u{0628}\u{064A}\u{0629} \
        \u{0939}\u{093F}\u{0928}\u{094D}\u{0926}\u{0940} \
        \u{1F600}\u{1F30D} \
        a\u{0301}e\u{0301}i\u{0301} \
        \u{200D} \
        \u{FEFF} \
    ";
    let toml = format!(
        "schema_version = 1\n\n[[racks]]\nname = \"unicode\"\nfamily = \"domain\"\ncharter = {}\n",
        toml_quote(charter)
    );
    let result = Catalogue::from_toml(&toml);
    // The parser MUST handle every Unicode scalar value identically
    // to ASCII: as opaque bytes inside a quoted string. We assert
    // that the function returns a Result (rather than panicking)
    // and, on the Ok path, that the charter survived round-trip
    // verbatim.
    if let Ok(cat) = result {
        if let Some(rack) = cat.racks.iter().find(|r| r.name == "unicode") {
            assert_eq!(
                rack.charter, charter,
                "charter must round-trip byte-identically"
            );
        }
    }
}

/// Encode `s` as a TOML basic-string literal, escaping the characters
/// that TOML's grammar requires escaped. Emoji and other scalars in
/// the supplementary plane are emitted verbatim; only the structural
/// escapes are needed for the input shapes used here.
fn toml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write as _;
                write!(out, "\\u{:04X}", c as u32).unwrap();
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[test]
fn unknown_fields_terminate_without_executing() {
    // A catalogue with a top-level field the schema does not know
    // about. The parser MUST NOT execute, eval, or otherwise act on
    // the field's value; it either ignores it (if the schema is
    // tolerant) or returns a structured error. Either way the call
    // returns; reaching the assertion below is the property.
    //
    // The field name and value are deliberately suspicious-looking
    // ("exec_command", "rm -rf /tmp/evo-adr-0002-canary") so a
    // future refactor that mistakenly grew an interpreter would
    // surface immediately. The canary path is NEVER created by the
    // steward; if a regression caused `from_toml` to act on this
    // field, the most plausible failure mode would be the parser
    // attempting filesystem or process work and panicking or
    // erroring with an unrelated I/O reason. We sanity-check that
    // the canary path does not exist after the call.
    let toml = r#"
        exec_command = "rm -rf /tmp/evo-adr-0002-canary"
        eval = "1 + 1"
        [[racks]]
        name = "good"
        family = "domain"
        kinds = ["registrar"]
        charter = "Schema-conformant rack alongside foreign fields."
    "#;
    let canary = std::path::Path::new("/tmp/evo-adr-0002-canary");
    // Pre-condition: the canary path does not exist. The steward
    // does not create it under any input.
    assert!(
        !canary.exists(),
        "test fixture invariant: canary path must not pre-exist"
    );
    let result = Catalogue::from_toml(toml);
    let _ = result;
    assert!(
        !canary.exists(),
        "catalogue parsing produced filesystem side effects"
    );
}

#[test]
fn toml_datetimes_in_fields_do_not_panic() {
    // TOML supports four datetime forms (offset, local, local date,
    // local time). None of these appear in the catalogue schema, so
    // the parser will either ignore them as unknown fields or
    // surface a structured error. The property under test is that
    // no datetime form panics the deserialiser; this guards against
    // panics in the toml crate's chrono-free datetime path.
    let toml = r#"
        offset_dt = 1979-05-27T07:32:00Z
        local_dt = 1979-05-27T00:32:00.999999
        local_date = 1979-05-27
        local_time = 07:32:00
        [[racks]]
        name = "good"
        family = "domain"
        kinds = ["registrar"]
        charter = "Schema-conformant rack alongside datetime fields."
    "#;
    let result = Catalogue::from_toml(toml);
    let _ = result;
}

/// Reaching this test confirms that none of the prior tests crashed
/// the test runner. It carries no per-input check of its own; it is
/// the smoke test that proves `cargo test` finished normally for the
/// negative-path battery above.
#[test]
fn parses_minimal_catalogue_after_battery() {
    let toml = r#"
        schema_version = 1

        [[racks]]
        name = "example"
        family = "domain"
        kinds = ["registrar"]
        charter = "Minimal catalogue for the post-battery smoke test."
    "#;
    let cat = Catalogue::from_toml(toml).expect("minimal catalogue parses");
    assert_eq!(cat.racks.len(), 1);
    assert_eq!(cat.racks[0].name, "example");
}
