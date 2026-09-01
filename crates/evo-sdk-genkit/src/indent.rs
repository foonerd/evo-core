// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Indented text emission helper.
//!
//! Per-language SDK generators emit code with nested
//! indentation. [`IndentWriter`] keeps the indent state once so
//! each emitter call doesn't re-roll it.

use std::fmt::Write;

/// Tiny indented-write helper.
///
/// Holds a target buffer, an indent unit (string, typically
/// four spaces or two spaces depending on the target
/// language's convention), and a current depth level. Each
/// [`Self::line`] write prepends the current indent and
/// appends a newline.
pub struct IndentWriter<'a> {
    buf: &'a mut String,
    unit: String,
    depth: usize,
}

impl<'a> IndentWriter<'a> {
    /// Construct an `IndentWriter` writing into the supplied
    /// buffer with the supplied indent unit. Common choices:
    /// `"    "` (4 spaces, used by Python and Rust SDKs),
    /// `"  "` (2 spaces, used by TS / Swift / Kotlin SDKs).
    pub fn new(buf: &'a mut String, unit: impl Into<String>) -> Self {
        Self {
            buf,
            unit: unit.into(),
            depth: 0,
        }
    }

    /// Append one indented line. Empty `line` arguments emit
    /// just a newline (no leading indent on blank lines).
    pub fn line(&mut self, line: &str) {
        if !line.is_empty() {
            for _ in 0..self.depth {
                self.buf.push_str(&self.unit);
            }
            self.buf.push_str(line);
        }
        self.buf.push('\n');
    }

    /// Append an indented `format_args!` line.
    pub fn line_fmt(&mut self, args: std::fmt::Arguments<'_>) {
        for _ in 0..self.depth {
            self.buf.push_str(&self.unit);
        }
        let _ = self.buf.write_fmt(args);
        self.buf.push('\n');
    }

    /// Indent the writer one level deeper for the body of a
    /// scope.
    pub fn indent(&mut self) {
        self.depth += 1;
    }

    /// Dedent the writer one level. Calling `dedent` at depth
    /// zero is a no-op (defensive — emitters should pair
    /// `indent`/`dedent` carefully but a stray extra dedent
    /// shouldn't panic the generator).
    pub fn dedent(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// Current indent depth (number of indent units written
    /// per line).
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Borrow the indent unit string.
    pub fn unit(&self) -> &str {
        &self.unit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_writer_with_no_lines_yields_empty_buffer() {
        let mut buf = String::new();
        let _w = IndentWriter::new(&mut buf, "    ");
        assert_eq!(buf, "");
    }

    #[test]
    fn top_level_line_has_no_indent() {
        let mut buf = String::new();
        {
            let mut w = IndentWriter::new(&mut buf, "    ");
            w.line("top");
        }
        assert_eq!(buf, "top\n");
    }

    #[test]
    fn indented_line_has_indent() {
        let mut buf = String::new();
        {
            let mut w = IndentWriter::new(&mut buf, "    ");
            w.line("top");
            w.indent();
            w.line("body");
        }
        assert_eq!(buf, "top\n    body\n");
    }

    #[test]
    fn nested_indent_writes_multiple_indent_units() {
        let mut buf = String::new();
        {
            let mut w = IndentWriter::new(&mut buf, "    ");
            w.indent();
            w.indent();
            w.line("deep");
        }
        assert_eq!(buf, "        deep\n");
    }

    #[test]
    fn dedent_back_to_top_level() {
        let mut buf = String::new();
        {
            let mut w = IndentWriter::new(&mut buf, "  ");
            w.line("a");
            w.indent();
            w.line("b");
            w.dedent();
            w.line("c");
        }
        assert_eq!(buf, "a\n  b\nc\n");
    }

    #[test]
    fn dedent_below_zero_is_noop() {
        let mut buf = String::new();
        {
            let mut w = IndentWriter::new(&mut buf, "  ");
            w.dedent();
            w.dedent();
            w.line("still_top");
        }
        assert_eq!(buf, "still_top\n");
    }

    #[test]
    fn empty_line_writes_only_newline() {
        let mut buf = String::new();
        {
            let mut w = IndentWriter::new(&mut buf, "    ");
            w.indent();
            w.line("a");
            w.line("");
            w.line("b");
        }
        assert_eq!(buf, "    a\n\n    b\n");
    }

    #[test]
    fn line_fmt_carries_format_args() {
        let mut buf = String::new();
        {
            let mut w = IndentWriter::new(&mut buf, "  ");
            w.indent();
            w.line_fmt(format_args!("count = {}", 42));
        }
        assert_eq!(buf, "  count = 42\n");
    }

    #[test]
    fn depth_and_unit_accessors() {
        let mut buf = String::new();
        let mut w = IndentWriter::new(&mut buf, "    ");
        assert_eq!(w.depth(), 0);
        assert_eq!(w.unit(), "    ");
        w.indent();
        assert_eq!(w.depth(), 1);
        w.indent();
        assert_eq!(w.depth(), 2);
        w.dedent();
        assert_eq!(w.depth(), 1);
    }
}
