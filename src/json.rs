//! Minimal recursive-descent JSON parser (std-only) with dot-path lookup.
//!
//! sortyard only needs to *read* keys out of JSONL records — records are
//! written back verbatim — so this parser favors correctness and clear
//! errors over speed tricks: full string-escape handling (including
//! surrogate pairs), strict number grammar, a recursion-depth guard against
//! hostile inputs, and byte offsets in every error message.

/// A parsed JSON value. Object members keep their original order and
/// duplicates; lookup returns the first match, like most JSON tooling.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

/// Maximum nesting depth. Deep enough for any real export, shallow enough
/// that a `[[[[…` bomb cannot blow the stack.
const MAX_DEPTH: usize = 128;

/// Parse a complete JSON document; trailing whitespace is allowed, trailing
/// content is an error.
pub fn parse(input: &str) -> Result<Json, String> {
    let mut p = Parser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    let value = p.value(0)?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(p.err("trailing content after JSON value"));
    }
    Ok(value)
}

/// Walk `root` along dot-separated path segments. Each segment is matched as
/// an object key first; on arrays, a decimal segment is used as a 0-based
/// index. Returns `None` when any hop is absent.
pub fn lookup<'a>(root: &'a Json, path: &[String]) -> Option<&'a Json> {
    let mut cur = root;
    for seg in path {
        cur = match cur {
            Json::Obj(members) => members.iter().find(|(k, _)| k == seg).map(|(_, v)| v)?,
            Json::Arr(items) => items.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn err(&self, msg: &str) -> String {
        format!("{msg} at byte {}", self.pos)
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn expect(&mut self, b: u8) -> Result<(), String> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err(&format!("expected '{}'", b as char)))
        }
    }

    fn literal(&mut self, word: &str, value: Json) -> Result<Json, String> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(self.err("invalid literal"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > MAX_DEPTH {
            return Err(self.err("nesting too deep"));
        }
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.err("unexpected character")),
            None => Err(self.err("unexpected end of input")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, String> {
        self.expect(b'{')?;
        let mut members = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Obj(members));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let val = self.value(depth + 1)?;
            members.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Obj(members));
                }
                _ => return Err(self.err("expected ',' or '}' in object")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(self.err("expected ',' or ']' in array")),
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    match self.peek() {
                        Some(b'"') => out.push('"'),
                        Some(b'\\') => out.push('\\'),
                        Some(b'/') => out.push('/'),
                        Some(b'b') => out.push('\u{8}'),
                        Some(b'f') => out.push('\u{c}'),
                        Some(b'n') => out.push('\n'),
                        Some(b'r') => out.push('\r'),
                        Some(b't') => out.push('\t'),
                        Some(b'u') => {
                            self.pos += 1;
                            out.push(self.unicode_escape()?);
                            continue;
                        }
                        _ => return Err(self.err("invalid escape")),
                    }
                    self.pos += 1;
                }
                Some(b) if b < 0x20 => return Err(self.err("raw control character in string")),
                Some(_) => {
                    // Copy one UTF-8 scalar (multi-byte sequences included).
                    let rest = &self.bytes[self.pos..];
                    let s = std::str::from_utf8(rest)
                        .map_err(|_| self.err("invalid UTF-8 in string"))?;
                    let ch = s.chars().next().unwrap();
                    out.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
    }

    /// `\uXXXX`, combining surrogate pairs into one scalar value.
    fn unicode_escape(&mut self) -> Result<char, String> {
        let hi = self.hex4()?;
        if (0xd800..=0xdbff).contains(&hi) {
            if self.peek() == Some(b'\\') && self.bytes.get(self.pos + 1) == Some(&b'u') {
                self.pos += 2;
                let lo = self.hex4()?;
                if !(0xdc00..=0xdfff).contains(&lo) {
                    return Err(self.err("invalid low surrogate"));
                }
                let code = 0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00);
                return char::from_u32(code).ok_or_else(|| self.err("invalid surrogate pair"));
            }
            return Err(self.err("lone high surrogate"));
        }
        char::from_u32(hi).ok_or_else(|| self.err("invalid \\u escape"))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let end = self.pos + 4;
        if end > self.bytes.len() {
            return Err(self.err("truncated \\u escape"));
        }
        let s = std::str::from_utf8(&self.bytes[self.pos..end])
            .map_err(|_| self.err("invalid \\u escape"))?;
        let v = u32::from_str_radix(s, 16).map_err(|_| self.err("invalid \\u escape"))?;
        self.pos = end;
        Ok(v)
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        // Integer part: '0' alone, or a nonzero digit run (no leading zeros).
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => self.digit_run(),
            _ => return Err(self.err("invalid number")),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("invalid number"));
            }
            self.digit_run();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.err("invalid number"));
            }
            self.digit_run();
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap();
        text.parse()
            .map(Json::Num)
            .map_err(|_| self.err("number out of range"))
    }

    fn digit_run(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(p: &str) -> Vec<String> {
        p.split('.').map(str::to_string).collect()
    }

    #[test]
    fn parses_scalars_and_nested_structures() {
        assert_eq!(parse("null").unwrap(), Json::Null);
        assert_eq!(parse("true").unwrap(), Json::Bool(true));
        assert_eq!(parse("-12.5e2").unwrap(), Json::Num(-1250.0));
        assert_eq!(parse("\"hi\"").unwrap(), Json::Str("hi".into()));
        let v = parse(r#"{"a": [1, {"b": null}], "c": {}}"#).unwrap();
        match v {
            Json::Obj(m) => assert_eq!(m.len(), 2),
            other => panic!("expected object, got {other:?}"),
        }
    }

    #[test]
    fn string_escapes_unicode_and_raw_multibyte_decode() {
        let v = parse(r#""line\nquote\"tab\t\\""#).unwrap();
        assert_eq!(v, Json::Str("line\nquote\"tab\t\\".into()));
        assert_eq!(parse(r#""é""#).unwrap(), Json::Str("é".into()));
        assert_eq!(
            parse(r#""😀""#).unwrap(),
            Json::Str("😀".into()),
            "surrogate pairs must combine"
        );
        assert_eq!(parse("\"日本語\"").unwrap(), Json::Str("日本語".into()));
        assert!(parse(r#""\ud83d""#).is_err(), "lone surrogate must fail");
    }

    #[test]
    fn number_grammar_is_strict() {
        assert!(parse("01").is_err(), "leading zeros are invalid JSON");
        assert!(parse("1.").is_err());
        assert!(parse(".5").is_err());
        assert!(parse("1e").is_err());
        assert!(parse("+1").is_err());
        assert_eq!(parse("0.5e+1").unwrap(), Json::Num(5.0));
    }

    #[test]
    fn trailing_content_is_rejected_and_errors_carry_offsets() {
        assert!(parse("{} {}").is_err());
        assert!(parse("1,").is_err());
        let e = parse(r#"{"a": }"#).unwrap_err();
        assert!(e.contains("at byte 6"), "got: {e}");
    }

    #[test]
    fn depth_guard_rejects_nesting_bombs() {
        let bomb = "[".repeat(4096) + &"]".repeat(4096);
        let e = parse(&bomb).unwrap_err();
        assert!(e.contains("nesting too deep"), "got: {e}");
    }

    #[test]
    fn lookup_walks_objects_and_arrays() {
        let v = parse(r#"{"user": {"tags": ["vip", "beta"], "id": 7}}"#).unwrap();
        assert_eq!(lookup(&v, &path("user.id")), Some(&Json::Num(7.0)));
        assert_eq!(
            lookup(&v, &path("user.tags.1")),
            Some(&Json::Str("beta".into()))
        );
        // An object key that *looks* numeric must match by name.
        let v = parse(r#"{"0": "zero"}"#).unwrap();
        assert_eq!(lookup(&v, &path("0")), Some(&Json::Str("zero".into())));
    }

    #[test]
    fn lookup_edge_cases_are_predictable() {
        let v = parse(r#"{"a": {"b": 1}}"#).unwrap();
        assert_eq!(lookup(&v, &path("a.c")), None);
        assert_eq!(
            lookup(&v, &path("a.b.c")),
            None,
            "cannot descend into a number"
        );
        assert_eq!(lookup(&v, &path("x")), None);
        let dup = parse(r#"{"k": 1, "k": 2}"#).unwrap();
        assert_eq!(
            lookup(&dup, &path("k")),
            Some(&Json::Num(1.0)),
            "duplicate keys resolve to the first"
        );
    }
}
