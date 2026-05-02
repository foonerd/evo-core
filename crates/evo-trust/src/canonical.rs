//! Canonical TOML re-serialiser for the signing payload.
//!
//! Whitespace, comments, key ordering, and TOML quoting style on
//! disk are the operator's and the editor's choice — none of them
//! is semantic. Signatures must survive that variability, so the
//! signing payload is not the raw bytes of `manifest.toml` but the
//! deterministic re-serialisation produced by [`canonicalise`]:
//!
//! - Tables emitted in lexicographic order of their fully-qualified
//!   path.
//! - Keys within a table emitted in lexicographic order.
//! - Strings written as basic strings (`"..."`) only; literal and
//!   multi-line forms collapse to basic. Non-printable characters
//!   are `\u`-escaped.
//! - Integers, floats, booleans, datetimes in their TOML canonical
//!   form (`Display` of [`toml::Value`]).
//! - Arrays preserve declaration order.
//! - Inline tables are expanded to standard `[table.path]` form.
//! - LF line endings, no BOM, no trailing whitespace, exactly one
//!   trailing newline.
//!
//! The output is recoverable by humans inspecting the bundle and
//! deterministic across operator tooling. This module is the only
//! blessed implementation.
//!
//! ## What is not canonical here
//!
//! - **Empty inline tables** within an array of tables expand to a
//!   single `[[...]]` header with no body, which `toml` parses
//!   identically. The header itself is the canonical form.
//! - **Floats** delegate to `toml::Value`'s display, which uses a
//!   minimal-digits round-trippable form. `NaN`, `+inf`, `-inf` are
//!   refused with [`CanonicalError::NonFiniteFloat`]: signing a
//!   payload containing them would be ambiguous.
//! - **Datetimes** delegate to `toml::Value`'s display. Their
//!   canonical TOML form is well-defined.

use std::fmt::Write;

use thiserror::Error;
use toml::Value;

/// Errors the canonical encoder may produce.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CanonicalError {
    /// The input bytes were not valid UTF-8. Manifests are TOML;
    /// non-UTF-8 input is rejected before parsing.
    #[error("manifest is not valid UTF-8: {0}")]
    InvalidUtf8(#[from] std::str::Utf8Error),

    /// The input bytes did not parse as TOML.
    #[error("manifest is not valid TOML: {0}")]
    Parse(#[from] toml::de::Error),

    /// A float field was non-finite (`NaN`, `+inf`, or `-inf`).
    /// Canonical TOML cannot represent these unambiguously and the
    /// signing payload would not be deterministic.
    #[error("non-finite float in manifest: {path}")]
    NonFiniteFloat {
        /// Dotted path to the offending field.
        path: String,
    },
}

/// Canonicalise the TOML in `input` per the rules in this module's
/// header.
///
/// Parses the input, then re-emits a deterministic byte sequence.
/// Two manifests that round-trip to structurally equal
/// [`toml::Value`]s produce identical canonical bytes regardless
/// of the on-disk formatting that produced them.
pub fn canonicalise(input: &[u8]) -> Result<Vec<u8>, CanonicalError> {
    let s = std::str::from_utf8(input)?;
    let parsed: Value = toml::from_str(s)?;
    let mut out = String::new();
    emit_root(&parsed, &mut out)?;
    Ok(out.into_bytes())
}

/// Emit the top-level table, root scalars first then nested tables
/// in lexicographic order.
fn emit_root(v: &Value, out: &mut String) -> Result<(), CanonicalError> {
    let table = match v {
        Value::Table(t) => t,
        _ => {
            // A top-level non-table is a TOML protocol error in
            // practice — `toml::from_str` already refuses such input
            // — but be defensive in case a future toml version eases
            // it.
            return Ok(());
        }
    };
    emit_table(&[], table, out)
}

/// Emit one table at `path`, with its scalar/array fields first
/// followed by its sub-tables (recursively).
fn emit_table(
    path: &[String],
    table: &toml::map::Map<String, Value>,
    out: &mut String,
) -> Result<(), CanonicalError> {
    let mut scalars: Vec<(&String, &Value)> = Vec::new();
    let mut subtables: Vec<(&String, &toml::map::Map<String, Value>)> =
        Vec::new();
    let mut array_of_tables: Vec<(&String, &Vec<Value>)> = Vec::new();

    for (k, val) in table {
        match val {
            Value::Table(t) => subtables.push((k, t)),
            Value::Array(a)
                if a.iter().all(|e| matches!(e, Value::Table(_))) =>
            {
                array_of_tables.push((k, a));
            }
            _ => scalars.push((k, val)),
        }
    }
    scalars.sort_by(|a, b| a.0.cmp(b.0));
    subtables.sort_by(|a, b| a.0.cmp(b.0));
    array_of_tables.sort_by(|a, b| a.0.cmp(b.0));

    if !path.is_empty() && (!scalars.is_empty() || !subtables.is_empty()) {
        writeln!(out, "[{}]", join_dotted(path)).expect("string write");
    }
    for (k, val) in &scalars {
        let mut field_path = path.to_vec();
        field_path.push((*k).clone());
        write!(out, "{} = ", canonical_key(k)).expect("string write");
        emit_value(&field_path, val, out)?;
        out.push('\n');
    }
    if !scalars.is_empty()
        && (!subtables.is_empty() || !array_of_tables.is_empty())
    {
        out.push('\n');
    }
    for (i, (k, t)) in subtables.iter().enumerate() {
        let mut child = path.to_vec();
        child.push((*k).clone());
        if i > 0 {
            out.push('\n');
        }
        emit_table(&child, t, out)?;
    }
    if !subtables.is_empty() && !array_of_tables.is_empty() {
        out.push('\n');
    }
    for (i, (k, arr)) in array_of_tables.iter().enumerate() {
        let mut child = path.to_vec();
        child.push((*k).clone());
        if i > 0 {
            out.push('\n');
        }
        for (j, elem) in arr.iter().enumerate() {
            if j > 0 {
                out.push('\n');
            }
            writeln!(out, "[[{}]]", join_dotted(&child)).expect("string write");
            if let Value::Table(elem_table) = elem {
                emit_table_body(&child, elem_table, out)?;
            }
        }
    }

    Ok(())
}

/// Emit the contents of an inline table within an array element,
/// without the header line (the caller printed it).
fn emit_table_body(
    path: &[String],
    table: &toml::map::Map<String, Value>,
    out: &mut String,
) -> Result<(), CanonicalError> {
    let mut scalars: Vec<(&String, &Value)> = Vec::new();
    let mut subtables: Vec<(&String, &toml::map::Map<String, Value>)> =
        Vec::new();
    let mut array_of_tables: Vec<(&String, &Vec<Value>)> = Vec::new();
    for (k, val) in table {
        match val {
            Value::Table(t) => subtables.push((k, t)),
            Value::Array(a)
                if a.iter().all(|e| matches!(e, Value::Table(_))) =>
            {
                array_of_tables.push((k, a));
            }
            _ => scalars.push((k, val)),
        }
    }
    scalars.sort_by(|a, b| a.0.cmp(b.0));
    subtables.sort_by(|a, b| a.0.cmp(b.0));
    array_of_tables.sort_by(|a, b| a.0.cmp(b.0));

    for (k, val) in &scalars {
        let mut field_path = path.to_vec();
        field_path.push((*k).clone());
        write!(out, "{} = ", canonical_key(k)).expect("string write");
        emit_value(&field_path, val, out)?;
        out.push('\n');
    }
    if !subtables.is_empty() || !array_of_tables.is_empty() {
        out.push('\n');
    }
    for (i, (k, t)) in subtables.iter().enumerate() {
        let mut child = path.to_vec();
        child.push((*k).clone());
        if i > 0 {
            out.push('\n');
        }
        emit_table(&child, t, out)?;
    }
    if !subtables.is_empty() && !array_of_tables.is_empty() {
        out.push('\n');
    }
    for (i, (k, arr)) in array_of_tables.iter().enumerate() {
        let mut child = path.to_vec();
        child.push((*k).clone());
        if i > 0 {
            out.push('\n');
        }
        for (j, elem) in arr.iter().enumerate() {
            if j > 0 {
                out.push('\n');
            }
            writeln!(out, "[[{}]]", join_dotted(&child)).expect("string write");
            if let Value::Table(elem_table) = elem {
                emit_table_body(&child, elem_table, out)?;
            }
        }
    }
    Ok(())
}

/// Emit a non-table TOML value as a canonical scalar / array.
fn emit_value(
    path: &[String],
    v: &Value,
    out: &mut String,
) -> Result<(), CanonicalError> {
    match v {
        Value::String(s) => {
            out.push('"');
            for c in s.chars() {
                emit_string_char(c, out);
            }
            out.push('"');
        }
        Value::Integer(i) => {
            write!(out, "{i}").expect("string write");
        }
        Value::Float(f) => {
            if !f.is_finite() {
                return Err(CanonicalError::NonFiniteFloat {
                    path: join_dotted(path),
                });
            }
            // toml::Value::Float's Display emits a round-trippable
            // form; `1.0` rather than `1`.
            write!(out, "{}", Value::Float(*f)).expect("string write");
        }
        Value::Boolean(b) => {
            write!(out, "{b}").expect("string write");
        }
        Value::Datetime(d) => {
            write!(out, "{d}").expect("string write");
        }
        Value::Array(a) => {
            out.push('[');
            for (i, elem) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                let mut child = path.to_vec();
                child.push(format!("[{i}]"));
                emit_value(&child, elem, out)?;
            }
            out.push(']');
        }
        Value::Table(_) => {
            // Inline tables in canonical form expand to standard
            // `[table.path]` form, which is emitted by
            // `emit_table`. Arriving here means a table appeared as
            // a value rather than a sub-table — the top-level
            // walker routes tables to `emit_table`, so this branch
            // is unreachable for well-formed input.
            // Defensive: emit the table contents as an inline
            // table on a single line.
            out.push('{');
            if let Value::Table(t) = v {
                let mut keys: Vec<&String> = t.keys().collect();
                keys.sort();
                for (i, k) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write!(out, "{} = ", canonical_key(k))
                        .expect("string write");
                    let mut child = path.to_vec();
                    child.push((*k).clone());
                    emit_value(&child, &t[*k], out)?;
                }
            }
            out.push('}');
        }
    }
    Ok(())
}

/// Emit one Unicode scalar inside a basic string. Printable ASCII
/// passes through; the standard escape set (`"`, `\`, control
/// chars) gets short escapes; everything else is `\uXXXX` (or
/// `\UXXXXXXXX` for code points above U+FFFF).
fn emit_string_char(c: char, out: &mut String) {
    match c {
        '"' => out.push_str("\\\""),
        '\\' => out.push_str("\\\\"),
        '\u{08}' => out.push_str("\\b"),
        '\t' => out.push_str("\\t"),
        '\n' => out.push_str("\\n"),
        '\u{0C}' => out.push_str("\\f"),
        '\r' => out.push_str("\\r"),
        c if c.is_ascii() && (c as u32) >= 0x20 => out.push(c),
        c if (c as u32) <= 0xFFFF => {
            write!(out, "\\u{:04X}", c as u32).expect("string write");
        }
        c => {
            write!(out, "\\U{:08X}", c as u32).expect("string write");
        }
    }
}

/// Render a TOML key. Bare keys (`[A-Za-z0-9_-]+`) emit as-is;
/// anything else is quoted as a basic string.
fn canonical_key(k: &str) -> String {
    let bare = !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        k.to_string()
    } else {
        let mut s = String::with_capacity(k.len() + 2);
        s.push('"');
        for c in k.chars() {
            emit_string_char(c, &mut s);
        }
        s.push('"');
        s
    }
}

/// Join a dotted path to a header form. Each segment is rendered
/// via [`canonical_key`] so reserved characters round-trip safely.
fn join_dotted(path: &[String]) -> String {
    path.iter()
        .map(|s| canonical_key(s))
        .collect::<Vec<_>>()
        .join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_under_key_reorder() {
        let a = b"\
[plugin]
name = \"org.example.plugin\"
version = \"0.1.0\"
contract = 1
";
        let b = b"\
[plugin]
contract = 1
version = \"0.1.0\"
name = \"org.example.plugin\"
";
        let ca = canonicalise(a).unwrap();
        let cb = canonicalise(b).unwrap();
        assert_eq!(ca, cb);
    }

    #[test]
    fn deterministic_under_whitespace_changes() {
        let a = b"name = \"x\"\n\nvalue = 42\n";
        let b = b"\
# leading comment
   name   =   \"x\"
   value=42
";
        let ca = canonicalise(a).unwrap();
        let cb = canonicalise(b).unwrap();
        assert_eq!(ca, cb);
    }

    #[test]
    fn deterministic_under_dotted_vs_table_form() {
        let dotted = b"a.b.c = 1\n";
        let nested = b"[a.b]\nc = 1\n";
        let ca = canonicalise(dotted).unwrap();
        let cn = canonicalise(nested).unwrap();
        assert_eq!(ca, cn);
    }

    #[test]
    fn arrays_preserve_order() {
        let a = b"list = [3, 1, 2]\n";
        let out = String::from_utf8(canonicalise(a).unwrap()).unwrap();
        assert!(
            out.contains("[3, 1, 2]"),
            "arrays preserve declaration order: {out:?}"
        );
    }

    #[test]
    fn nested_tables_lexicographic() {
        let input = b"\
[zeta]
key = 1

[alpha]
key = 2
";
        let out = String::from_utf8(canonicalise(input).unwrap()).unwrap();
        let alpha = out.find("[alpha]").unwrap();
        let zeta = out.find("[zeta]").unwrap();
        assert!(
            alpha < zeta,
            "nested tables emit lexicographically: {out:?}"
        );
    }

    #[test]
    fn array_of_tables_round_trip() {
        let input = b"\
[[items]]
name = \"a\"

[[items]]
name = \"b\"
";
        let canonical = canonicalise(input).unwrap();
        let s = String::from_utf8(canonical.clone()).unwrap();
        assert!(s.contains("[[items]]"), "header preserved: {s}");
        // Re-canonicalise: the second pass must equal the first.
        let again = canonicalise(&canonical).unwrap();
        assert_eq!(canonical, again, "canonical form is idempotent");
    }

    #[test]
    fn rejects_non_finite_float() {
        // toml itself can't represent NaN, but we still defend:
        // construct a value programmatically.
        let v = Value::Float(f64::NAN);
        let mut out = String::new();
        let r = emit_value(&["bad".into()], &v, &mut out);
        assert!(matches!(r, Err(CanonicalError::NonFiniteFloat { .. })));
    }

    #[test]
    fn non_ascii_string_escapes_unicode() {
        let input = b"name = \"a\xc3\xa9b\"\n"; // "aéb"
        let out = String::from_utf8(canonicalise(input).unwrap()).unwrap();
        assert!(
            out.contains("\\u00E9"),
            "non-ASCII chars use \\u escape: {out}"
        );
    }

    #[test]
    fn ends_with_single_newline() {
        let input = b"name = \"x\"\n";
        let out = canonicalise(input).unwrap();
        assert_eq!(out.last(), Some(&b'\n'));
        // No trailing blank line.
        assert!(!out.ends_with(b"\n\n"));
    }
}
