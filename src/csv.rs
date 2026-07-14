//! Streaming RFC 4180 CSV reader that preserves raw record bytes.
//!
//! This is the part GNU sort cannot do: a quoted field may contain the
//! delimiter, doubled quotes (`""`) and even newlines, so a *record* is not
//! a *line*. The reader returns each record's raw bytes untouched (records
//! are re-emitted verbatim after sorting) plus a separate field splitter
//! used only for key extraction. Both run the same quote state machine, so
//! record boundaries and field values can never disagree.

use std::io::BufRead;

/// Quote state while scanning a record, byte by byte.
///
/// Quotes are special only when a field *starts* with one (tolerant mode:
/// `it"s,fine` splits as `it"s` | `fine`, like most real-world consumers).
/// Inside a quoted field, `""` decodes to a literal quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldState {
    /// At the start of a field.
    Start,
    /// Inside an unquoted field; quotes here are literal bytes.
    Unquoted,
    /// Inside a quoted field; delimiter and newline are literal here.
    Quoted,
    /// Just saw a quote inside a quoted field: either the `""` escape or
    /// the closing quote, decided by the next byte.
    AfterQuote,
}

/// Advance the quote state machine by one byte. Newline handling is the
/// caller's job (the reader ends the record, the splitter never sees one
/// outside quotes).
fn step(state: FieldState, b: u8, delim: u8) -> FieldState {
    match state {
        FieldState::Start => match b {
            b'"' => FieldState::Quoted,
            _ if b == delim => FieldState::Start,
            _ => FieldState::Unquoted,
        },
        FieldState::Unquoted => {
            if b == delim {
                FieldState::Start
            } else {
                FieldState::Unquoted
            }
        }
        FieldState::Quoted => {
            if b == b'"' {
                FieldState::AfterQuote
            } else {
                FieldState::Quoted
            }
        }
        FieldState::AfterQuote => match b {
            b'"' => FieldState::Quoted,
            _ if b == delim => FieldState::Start,
            _ => FieldState::Unquoted,
        },
    }
}

/// One raw CSV record: the exact bytes between record terminators, and the
/// 1-based physical line on which it started (for error messages).
#[derive(Debug)]
pub struct RawRecord {
    pub bytes: Vec<u8>,
    pub line: u64,
}

/// Streaming record reader. Handles quoted newlines, CRLF and LF
/// terminators, and reports unterminated quotes with the offending line.
pub struct CsvReader<R: BufRead> {
    inner: R,
    delim: u8,
    /// Physical line number of the *next* byte to be read (1-based).
    line: u64,
}

impl<R: BufRead> CsvReader<R> {
    pub fn new(inner: R, delim: u8) -> Self {
        CsvReader {
            inner,
            delim,
            line: 1,
        }
    }

    /// Next record, or `None` at end of input. The record terminator is
    /// stripped (both `\n` and `\r\n`); bytes inside the record — including
    /// quoted newlines — are preserved exactly.
    pub fn next_record(&mut self) -> Result<Option<RawRecord>, String> {
        let mut buf: Vec<u8> = Vec::new();
        let start_line = self.line;
        let mut state = FieldState::Start;
        loop {
            let chunk = self
                .inner
                .fill_buf()
                .map_err(|e| format!("read error: {e}"))?;
            if chunk.is_empty() {
                // EOF: flush whatever is pending (a final record without a
                // trailing newline is legal).
                if state == FieldState::Quoted {
                    return Err(format!(
                        "line {start_line}: unterminated quoted field at end of input"
                    ));
                }
                if buf.is_empty() {
                    return Ok(None);
                }
                strip_trailing_cr(&mut buf);
                return Ok(Some(RawRecord {
                    bytes: buf,
                    line: start_line,
                }));
            }
            for (i, &b) in chunk.iter().enumerate() {
                if b == b'\n' {
                    self.line += 1;
                    if state != FieldState::Quoted {
                        buf.extend_from_slice(&chunk[..i]);
                        self.inner.consume(i + 1);
                        strip_trailing_cr(&mut buf);
                        return Ok(Some(RawRecord {
                            bytes: buf,
                            line: start_line,
                        }));
                    }
                }
                state = step(state, b, self.delim);
            }
            let len = chunk.len();
            buf.extend_from_slice(chunk);
            self.inner.consume(len);
        }
    }
}

fn strip_trailing_cr(buf: &mut Vec<u8>) {
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
}

/// Split a raw record into unquoted field values, for key extraction only.
///
/// Runs the same state machine as the reader: quoted fields decode `""` and
/// keep delimiters/newlines literal; stray quotes inside unquoted fields
/// stay literal. Invalid UTF-8 is replaced only in this extracted copy —
/// the raw record bytes are emitted untouched.
pub fn split_fields(raw: &[u8], delim: u8) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field: Vec<u8> = Vec::new();
    let mut state = FieldState::Start;
    for &b in raw {
        match state {
            FieldState::Start | FieldState::Unquoted | FieldState::AfterQuote => {
                if b == delim {
                    fields.push(String::from_utf8_lossy(&field).into_owned());
                    field.clear();
                } else if b != b'"' {
                    field.push(b);
                } else if state != FieldState::Start {
                    // Unquoted: a stray mid-field quote is literal.
                    // AfterQuote: the second half of a `""` escape.
                    // Only a quote at Start opens a section (not content).
                    field.push(b);
                }
            }
            FieldState::Quoted => {
                // A quote here may close the field or start a `""` escape;
                // the AfterQuote state decides on the next byte.
                if b != b'"' {
                    field.push(b);
                }
            }
        }
        state = step(state, b, delim);
    }
    fields.push(String::from_utf8_lossy(&field).into_owned());
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn records(input: &str) -> Vec<Vec<u8>> {
        let mut r = CsvReader::new(Cursor::new(input.as_bytes().to_vec()), b',');
        let mut out = Vec::new();
        while let Some(rec) = r.next_record().unwrap() {
            out.push(rec.bytes);
        }
        out
    }

    fn fields(raw: &str) -> Vec<String> {
        split_fields(raw.as_bytes(), b',')
    }

    #[test]
    fn records_split_on_lf_crlf_and_missing_final_newline() {
        let expect = vec![b"a,b".to_vec(), b"c,d".to_vec()];
        assert_eq!(records("a,b\nc,d\n"), expect);
        assert_eq!(records("a,b\r\nc,d\r\n"), expect);
        assert_eq!(records("a,b\nc,d"), expect, "final record without newline");
    }

    #[test]
    fn quoted_newlines_keep_the_record_together() {
        // This is the case that silently corrupts data under GNU sort.
        let recs = records("id,note\n1,\"line one\nline two\"\n2,plain\n");
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[1], b"1,\"line one\nline two\"".to_vec());
        // CRLF inside the quotes is field content, preserved byte-for-byte.
        let recs = records("1,\"a\r\nb\"\r\n");
        assert_eq!(recs[0], b"1,\"a\r\nb\"".to_vec());
    }

    #[test]
    fn doubled_quotes_do_not_end_the_field() {
        let recs = records("1,\"say \"\"hi\"\",ok\"\n2,x\n");
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0], b"1,\"say \"\"hi\"\",ok\"".to_vec());
    }

    #[test]
    fn stray_quotes_in_unquoted_fields_stay_literal() {
        // `it"s` must not swallow the following newline into the record...
        let recs = records("1,it\"s fine\n2,next\n");
        assert_eq!(recs.len(), 2);
        // ...and the splitter must agree, keeping the quote as content.
        assert_eq!(fields("it\"s,fine"), vec!["it\"s", "fine"]);
    }

    #[test]
    fn record_start_lines_are_tracked_across_quoted_newlines() {
        let mut r = CsvReader::new(Cursor::new(b"a\n\"x\ny\"\nb\n".to_vec()), b',');
        let l1 = r.next_record().unwrap().unwrap().line;
        let l2 = r.next_record().unwrap().unwrap().line;
        let l3 = r.next_record().unwrap().unwrap().line;
        assert_eq!((l1, l2, l3), (1, 2, 4), "record 3 starts on line 4");
    }

    #[test]
    fn unterminated_quote_is_an_error_with_line_number() {
        let mut r = CsvReader::new(Cursor::new(b"ok\n2,\"broken\n".to_vec()), b',');
        r.next_record().unwrap();
        let e = r.next_record().unwrap_err();
        assert!(e.contains("line 2"), "got: {e}");
        assert!(e.contains("unterminated"), "got: {e}");
    }

    #[test]
    fn split_decodes_quotes_delimiters_and_newlines() {
        assert_eq!(
            fields(r#"1,"Widget, large","say ""hi""""#),
            vec!["1", "Widget, large", "say \"hi\""]
        );
        assert_eq!(
            split_fields(b"1,\"line one\nline two\"", b','),
            vec!["1", "line one\nline two"]
        );
    }

    #[test]
    fn split_keeps_empty_fields_and_honors_custom_delimiters() {
        assert_eq!(fields("a,,c,"), vec!["a", "", "c", ""]);
        assert_eq!(fields(""), vec![""]);
        assert_eq!(fields("\"\",b"), vec!["", "b"]);
        assert_eq!(
            split_fields(b"a\tb\t\"c\td\"", b'\t'),
            vec!["a", "b", "c\td"]
        );
    }
}
