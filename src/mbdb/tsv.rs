//! PostgreSQL COPY-text streaming parser for MusicBrainz dump members.
//!
//! The dump TSVs are PostgreSQL `COPY ... TO STDOUT` text output, **not**
//! plain TSV (plan §2/§9): fields are tab-separated and rows newline
//! terminated, backslash escapes carry embedded control characters,
//! booleans are `t`/`f`, **NULL is the exact field `\N`**, and a genuinely
//! empty field is a real empty string (all verified against the 2026-09-01
//! sample dump).
//!
//! Escape sequences handled — the full set COPY *output* can emit, plus the
//! octal form COPY *input* accepts:
//!
//! - `\\` → backslash, `\t` → TAB, `\n` → LF, `\r` → CR, `\b` → backspace,
//!   `\f` → form feed, `\v` → vertical tab
//! - `\NNN` (1–3 octal digits) → the byte
//! - any other backslashed character keeps **both** characters (COPY
//!   input's conservative fallback; unreachable from real dumps because
//!   COPY output only emits the named escapes)
//! - a trailing backslash at end of data is a literal backslash
//!
//! NULL is recognized only when the **raw** field is exactly `\N` — an
//! escaped `\\N` is the two-character literal `\N`, and `\N` embedded in a
//! longer field is ordinary text.
//!
//! Parsing is byte-oriented: [`Field::Value`] carries the unescaped bytes
//! and UTF-8 validation is the caller's policy (the loader routes malformed
//! rows to the rejections log, never dropping them silently).
//!
//! [`TsvReader`] streams rows over any [`BufRead`] with constant memory
//! (one row buffered at a time) — the loader chain reads the decompressed
//! member stream and batches rows into DuckDB without materializing the
//! multi-GB members.

use std::io::{self, BufRead};

/// One parsed COPY-text field.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Field {
    /// PostgreSQL NULL — the raw field was exactly `\N`.
    Null,
    /// A real value, possibly the empty string — the raw field with all
    /// backslash escapes resolved.
    Value(Vec<u8>),
}

impl Field {
    /// The unescaped bytes, or `None` for NULL.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Field::Null => None,
            Field::Value(bytes) => Some(bytes),
        }
    }

    /// Whether this field is PostgreSQL NULL.
    pub fn is_null(&self) -> bool {
        matches!(self, Field::Null)
    }
}

/// Parse one raw COPY-text field (the bytes between a tab or terminator).
///
/// The whole-field `\N` NULL rule is applied **before** unescaping, so
/// `\\N` and `ab\Ncd` remain ordinary text.
pub fn parse_field(raw: &[u8]) -> Field {
    if raw == b"\\N" {
        return Field::Null;
    }
    Field::Value(unescape(raw))
}

/// Resolve the backslash escapes in a raw COPY-text field.
fn unescape(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let byte = raw[i];
        if byte != b'\\' {
            out.push(byte);
            i += 1;
            continue;
        }
        i += 1;
        let Some(&escape) = raw.get(i) else {
            // Backslash at end of data: no escape follows — literal.
            out.push(b'\\');
            break;
        };
        match escape {
            b'\\' => {
                out.push(b'\\');
                i += 1;
            }
            b'b' => {
                out.push(0x08);
                i += 1;
            }
            b'f' => {
                out.push(0x0c);
                i += 1;
            }
            b'n' => {
                out.push(b'\n');
                i += 1;
            }
            b'r' => {
                out.push(b'\r');
                i += 1;
            }
            b't' => {
                out.push(b'\t');
                i += 1;
            }
            b'v' => {
                out.push(0x0b);
                i += 1;
            }
            b'0'..=b'7' => {
                // Octal escape, 1–3 digits.
                let mut value: u32 = 0;
                let mut digits = 0;
                while digits < 3 {
                    let Some(&digit) = raw.get(i) else { break };
                    if !matches!(digit, b'0'..=b'7') {
                        break;
                    }
                    value = value * 8 + u32::from(digit - b'0');
                    digits += 1;
                    i += 1;
                }
                out.push(value as u8);
            }
            _ => {
                // Unrecognized escape: keep both characters (COPY input's
                // fallback — never emitted by realistic dump data).
                out.push(b'\\');
                out.push(escape);
                i += 1;
            }
        }
    }
    out
}

/// One parsed data row: the COPY-text line split into fields in ship order.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Row {
    /// Parsed fields in column order. Checking the count against the pinned
    /// column manifests is the loader's job.
    pub fields: Vec<Field>,
}

impl Row {
    /// Parse one physical COPY-text line (terminator already stripped).
    ///
    /// An empty line is a row with a single empty field, exactly as
    /// PostgreSQL's COPY input treats it.
    pub fn parse(line: &[u8]) -> Row {
        let fields = line.split(|&byte| byte == b'\t').map(parse_field).collect();
        Row { fields }
    }

    /// Field at `index`, `None` when the row has fewer columns.
    pub fn field(&self, index: usize) -> Option<&Field> {
        self.fields.get(index)
    }

    /// Number of fields in the row.
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Whether the row has no fields (unreachable for a parsed COPY row,
    /// which always carries at least one field).
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Re-render the row as COPY text, exactly as a dump member would ship
    /// it (tabs between fields, escapes re-applied, `\N` for NULL).
    ///
    /// Used by the loader to log rejected rows: the bytes are byte-identical
    /// to what the source TSV contained.
    pub fn to_copy_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, field) in self.fields.iter().enumerate() {
            if i > 0 {
                out.push(b'\t');
            }
            match field {
                Field::Null => out.extend_from_slice(b"\\N"),
                Field::Value(bytes) => out.extend_from_slice(&escape_copy(bytes)),
            }
        }
        out.push(b'\n');
        out
    }
}

/// Escape a value the way COPY *output* does — the inverse of `unescape`
/// for every byte COPY escapes: backslash, tab, and the control characters.
///
/// Any other byte passes through unchanged.
pub fn escape_copy(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for &byte in bytes {
        match byte {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            0x0b => out.extend_from_slice(b"\\v"),
            _ => out.push(byte),
        }
    }
    out
}

/// Interpret a COPY-text boolean: `t` → `Some(true)`, `f` → `Some(false)`.
///
/// Anything else returns `None` so the loader can route the row to the
/// rejections log. MusicBrainz dumps always use the canonical COPY output
/// forms `t`/`f` (the plan pins this against the sample dump).
pub fn parse_bool(bytes: &[u8]) -> Option<bool> {
    match bytes {
        b"t" => Some(true),
        b"f" => Some(false),
        _ => None,
    }
}

/// Streaming row reader over any [`BufRead`] byte source.
///
/// Constant memory: one row is buffered at a time and the buffer is reused
/// across rows. Line endings `\n` and `\r\n` are accepted (a trailing `\r`
/// is stripped), matching COPY input tolerance; a final row without a
/// terminator is still yielded. Reads are terminal: once the underlying
/// source errors, the iterator is exhausted after reporting the error.
pub struct TsvReader<R> {
    inner: R,
    buf: Vec<u8>,
    finished: bool,
}

impl<R: BufRead> TsvReader<R> {
    /// Wrap a byte source (e.g. a decompressed member stream) in a row
    /// reader.
    pub fn new(inner: R) -> Self {
        TsvReader {
            inner,
            buf: Vec::new(),
            finished: false,
        }
    }
}

impl<R: BufRead> Iterator for TsvReader<R> {
    type Item = io::Result<Row>;

    fn next(&mut self) -> Option<io::Result<Row>> {
        if self.finished {
            return None;
        }
        self.buf.clear();
        match self.inner.read_until(b'\n', &mut self.buf) {
            Ok(0) => {
                self.finished = true;
                None
            }
            Ok(_) => {
                // Strip the terminator: optional `\n`, then one optional
                // preceding `\r` (CRLF tolerance).
                if self.buf.last() == Some(&b'\n') {
                    self.buf.pop();
                }
                if self.buf.last() == Some(&b'\r') {
                    self.buf.pop();
                }
                Some(Ok(Row::parse(&self.buf)))
            }
            Err(error) => {
                self.finished = true;
                Some(Err(error))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Shorthand: a plain value field.
    fn v(bytes: &[u8]) -> Field {
        Field::Value(bytes.to_vec())
    }

    /// Collect a reader over the given input into parsed rows.
    fn rows_from(input: &[u8]) -> Vec<Row> {
        let reader = TsvReader::new(Cursor::new(input.to_vec()));
        reader.map(|row| row.unwrap()).collect()
    }

    #[test]
    fn null_is_exact_field_backslash_n() {
        assert_eq!(parse_field(b"\\N"), Field::Null);
        // An escaped backslash followed by N is the literal two-char field.
        assert_eq!(parse_field(b"\\\\N"), v(b"\\N"));
        // `\N` embedded in a larger field is ordinary text, not NULL.
        assert_eq!(parse_field(b"ab\\Ncd"), v(b"ab\\Ncd"));
    }

    #[test]
    fn empty_field_is_empty_string_not_null() {
        assert_eq!(parse_field(b""), v(b""));
    }

    #[test]
    fn backslash_escapes_for_control_characters() {
        assert_eq!(parse_field(b"\\\\"), v(b"\\"));
        assert_eq!(parse_field(b"a\\tb"), v(b"a\tb"));
        assert_eq!(parse_field(b"a\\nb"), v(b"a\nb"));
        assert_eq!(parse_field(b"a\\rb"), v(b"a\rb"));
        assert_eq!(parse_field(b"\\b"), v(&[0x08]));
        assert_eq!(parse_field(b"\\f"), v(&[0x0c]));
        assert_eq!(parse_field(b"\\v"), v(&[0x0b]));
    }

    #[test]
    fn octal_escapes() {
        assert_eq!(parse_field(b"\\101"), v(b"A")); // 0o101 = 0x41
        assert_eq!(parse_field(b"\\0"), v(&[0]));
        assert_eq!(parse_field(b"\\377"), v(&[0xff]));
        // Up to three digits only: the fourth is literal.
        assert_eq!(parse_field(b"\\1014"), v(b"A4"));
    }

    #[test]
    fn trailing_backslash_is_literal() {
        assert_eq!(parse_field(b"abc\\"), v(b"abc\\"));
    }

    #[test]
    fn unrecognized_escape_keeps_both_characters() {
        assert_eq!(parse_field(b"a\\q"), v(b"a\\q"));
    }

    #[test]
    fn row_split_on_tabs() {
        let row = Row::parse(b"1\t\\N\t\\t\\t\\n");
        assert_eq!(row.len(), 3);
        assert_eq!(row.field(0), Some(&v(b"1")));
        assert_eq!(row.field(1), Some(&Field::Null));
        // Escape sequences resolved inside a field that survived the split.
        assert_eq!(row.field(2), Some(&v(b"\t\t\n")));
        assert!(row.field(3).is_none());
    }

    #[test]
    fn blank_row_is_single_empty_field_row() {
        let row = Row::parse(b"");
        assert_eq!(row.len(), 1);
        assert_eq!(row.field(0), Some(&v(b"")));
    }

    #[test]
    fn bool_values() {
        assert_eq!(parse_bool(b"t"), Some(true));
        assert_eq!(parse_bool(b"f"), Some(false));
        assert_eq!(parse_bool(b"true"), None);
        assert_eq!(parse_bool(b"1"), None);
        assert_eq!(parse_bool(b""), None);
    }

    #[test]
    fn reader_lf_line_endings() {
        let rows = rows_from(b"a\tb\n\\N\tc\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].fields, vec![v(b"a"), v(b"b")]);
        assert_eq!(rows[1].fields, vec![Field::Null, v(b"c")]);
    }

    #[test]
    fn reader_crlf_line_endings() {
        let rows = rows_from(b"a\tb\r\nc\td\r\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].fields[1], v(b"b"));
        assert_eq!(rows[1].fields[0], v(b"c"));
    }

    #[test]
    fn reader_final_unterminated_row_is_yielded() {
        let rows = rows_from(b"a\tb\nc");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].fields[0], v(b"c"));
    }

    #[test]
    fn reader_empty_input_yields_nothing() {
        assert!(rows_from(b"").is_empty());
    }

    #[test]
    fn reader_blank_line_yields_single_empty_field_row() {
        let rows = rows_from(b"\n");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 1);
        assert_eq!(rows[0].fields[0], v(b""));
    }

    #[test]
    fn reader_escaped_newline_does_not_split_row() {
        // A field containing an escaped newline must stay one physical row.
        let rows = rows_from(b"1\tline1\\nline2\t\\N\n");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].fields[1], v(b"line1\nline2"));
    }

    /// COPY *output* escapement — the inverse of `unescape` for the bytes
    /// COPY actually escapes. Round-tripping through it reproduces the dump
    /// bytes for the original value.
    #[test]
    fn escape_then_parse_round_trips() {
        // The values that COPY output escapes must survive a full
        // escape → parse cycle verbatim (realistic dump field contents).
        let originals: &[&[u8]] = &[
            b"plain",
            b"with\ttab",
            b"back\\slash",
            b"new\nline",
            b"cr\r",
            &[0x08],
            &[0x0c],
            &[0x0b],
            &[
                b'a', b'l', b'l', b'\\', b'\t', b'\n', b'\r', 0x08, 0x0c, 0x0b, b' ', b't', b'o',
                b'g', b'e', b't', b'h', b'e', b'r',
            ],
            b"",
        ];
        for original in originals {
            let escaped = escape_copy(original);
            assert_eq!(parse_field(&escaped), v(original));
        }
    }

    #[test]
    fn to_copy_bytes_reconstructs_the_shipped_row() {
        // Rendering a parsed row reproduces the source TSV line verbatim,
        // so rejections logged by the loader show exactly what the dump
        // contained.
        let row = Row::parse(b"1\t\\N\ta\\\\b\t\\t");
        assert_eq!(row.to_copy_bytes(), b"1\t\\N\ta\\\\b\t\\t\n");
    }

    #[test]
    fn reader_handles_realistic_artist_row() {
        // Shape of a real schema-31 `artist` row: id, gid, name, sort_name,
        // begin/end dates, type/area FKs, comment with escaped tab, etc.
        let raw = b"12345\t1a2b3c4d-1111-2222-3333-444444444444\tU2\tU2\t\
                    1976\t5\t1\t\\N\t\\N\t\\N\t2\t\\N\t1\t\\tTabbed\\tTitle\
                    \t\\N\t0\t2026-09-01 12:34:56.789+00\tf\t\\N\t\\N\n";
        let rows = rows_from(raw);
        assert_eq!(rows.len(), 1);
        let fields = &rows[0].fields;
        assert_eq!(fields[0], v(b"12345"));
        assert_eq!(fields[1], v(b"1a2b3c4d-1111-2222-3333-444444444444"));
        assert_eq!(fields[2], v(b"U2"));
        assert_eq!(fields[8], Field::Null);
        // begin_date_year(4) type(10) area(11) gender(12) … comment(13)
        assert_eq!(fields[13], v(b"\tTabbed\tTitle"));
        assert_eq!(fields[14], Field::Null);
        assert_eq!(fields[15], v(b"0"));
        assert_eq!(fields[16], v(b"2026-09-01 12:34:56.789+00"));
        assert_eq!(fields[17], v(b"f"));
        assert_eq!(fields[18], Field::Null);
        assert_eq!(fields[19], Field::Null);
    }
}
