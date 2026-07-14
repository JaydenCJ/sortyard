//! Turns raw records into order-preserving encoded sort keys.
//!
//! One extractor is built per run from the parsed key specs; it splits CSV
//! fields or parses the JSONL object exactly once per record, converts each
//! addressed field to a typed [`KeyValue`], and concatenates the encoded
//! parts. A global `--reverse` is applied here by inverting the finished key
//! bytes, so every later stage (sort, spill, merge, check) can compare keys
//! with a plain `memcmp` and stay policy-free.

use crate::csv::split_fields;
use crate::json::{self, Json};
use crate::keyspec::{resolve_csv_column, resolve_csv_index, ColRef, KeySpec};
use crate::value::{encode_into, parse_num, KeyType, KeyValue, MissingOrder};

/// Input format, decided by `--format` or auto-detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Csv,
    Jsonl,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Jsonl => "jsonl",
        }
    }
}

pub struct Extractor {
    format: Format,
    specs: Vec<KeySpec>,
    /// Resolved CSV column per spec (parallel to `specs`; CSV only).
    cols: Vec<ColRef>,
    /// Pre-split JSONL path per spec (parallel to `specs`; JSONL only).
    paths: Vec<Vec<String>>,
    delim: u8,
    missing: MissingOrder,
    lenient: bool,
    reverse: bool,
}

impl Extractor {
    pub fn new(
        format: Format,
        specs: Vec<KeySpec>,
        delim: u8,
        missing: MissingOrder,
        lenient: bool,
        reverse: bool,
    ) -> Extractor {
        let paths = match format {
            Format::Jsonl => specs.iter().map(|s| s.path()).collect(),
            Format::Csv => Vec::new(),
        };
        Extractor {
            format,
            specs,
            cols: Vec::new(),
            paths,
            delim,
            missing,
            lenient,
            reverse,
        }
    }

    /// Resolve CSV key selectors against the header record.
    pub fn resolve_csv_header(&mut self, header_raw: &[u8]) -> Result<(), String> {
        let header = split_fields(header_raw, self.delim);
        self.cols = self
            .specs
            .iter()
            .map(|s| resolve_csv_column(&s.field, &header))
            .collect::<Result<_, _>>()?;
        Ok(())
    }

    /// Resolve CSV key selectors as 1-based indices (`--no-header`).
    pub fn resolve_csv_headerless(&mut self) -> Result<(), String> {
        self.cols = self
            .specs
            .iter()
            .map(|s| resolve_csv_index(&s.field))
            .collect::<Result<_, _>>()?;
        Ok(())
    }

    /// Build the encoded key for one raw record. Errors name the offending
    /// field; the caller adds the record's location.
    pub fn key_for(&self, raw: &[u8]) -> Result<Vec<u8>, String> {
        let mut key = Vec::with_capacity(24);
        if self.specs.is_empty() {
            // No --key: the whole record, compared as a string. This mirrors
            // GNU sort's whole-line default but still quote-safe.
            let text = String::from_utf8_lossy(raw);
            encode_into(
                &mut key,
                &KeyValue::Str(text.into_owned()),
                false,
                self.missing,
            );
        } else {
            match self.format {
                Format::Csv => self.csv_key(raw, &mut key)?,
                Format::Jsonl => self.jsonl_key(raw, &mut key)?,
            }
        }
        if self.reverse {
            for b in &mut key {
                *b = !*b;
            }
        }
        Ok(key)
    }

    fn csv_key(&self, raw: &[u8], key: &mut Vec<u8>) -> Result<(), String> {
        debug_assert_eq!(self.cols.len(), self.specs.len(), "header not resolved");
        let fields = split_fields(raw, self.delim);
        for (spec, col) in self.specs.iter().zip(&self.cols) {
            let ColRef::Index(idx) = col;
            // An absent column and an empty field both mean "missing":
            // CSV has no other way to express null.
            let value = match fields.get(*idx).map(String::as_str) {
                None | Some("") => KeyValue::Missing,
                Some(text) => self.parse_field(spec, text)?,
            };
            encode_into(key, &value, spec.desc, self.missing);
        }
        Ok(())
    }

    fn parse_field(&self, spec: &KeySpec, text: &str) -> Result<KeyValue, String> {
        let folded;
        let text = if spec.ci {
            folded = text.to_ascii_lowercase();
            &folded
        } else {
            text
        };
        match KeyValue::parse_typed(text, spec.ty) {
            Ok(v) => Ok(v),
            Err(_) if self.lenient => Ok(KeyValue::Missing),
            Err(e) => Err(format!("field '{}': {e}", spec.field)),
        }
    }

    fn jsonl_key(&self, raw: &[u8], key: &mut Vec<u8>) -> Result<(), String> {
        let text = std::str::from_utf8(raw).map_err(|_| "invalid UTF-8".to_string())?;
        let doc = json::parse(text).map_err(|e| format!("invalid JSON: {e}"))?;
        for (spec, path) in self.specs.iter().zip(&self.paths) {
            let value = match json::lookup(&doc, path) {
                None | Some(Json::Null) => Ok(KeyValue::Missing),
                Some(v) => self.json_value(spec, v),
            };
            let value = match value {
                Ok(v) => v,
                Err(_) if self.lenient => KeyValue::Missing,
                Err(e) => return Err(format!("field '{}': {e}", spec.field)),
            };
            encode_into(key, &value, spec.desc, self.missing);
        }
        Ok(())
    }

    /// Convert a present JSON value to the declared key type.
    fn json_value(&self, spec: &KeySpec, v: &Json) -> Result<KeyValue, String> {
        match (spec.ty, v) {
            (KeyType::Num, Json::Num(n)) => Ok(KeyValue::Num(*n)),
            (KeyType::Num, Json::Str(s)) => parse_num(s).map(KeyValue::Num),
            (KeyType::Str, Json::Str(s)) => Ok(KeyValue::Str(if spec.ci {
                s.to_ascii_lowercase()
            } else {
                s.clone()
            })),
            // Scalars render the way JSON writes them, so `str` keys still
            // work on mixed columns.
            (KeyType::Str, Json::Num(n)) => Ok(KeyValue::Str(format_json_num(*n))),
            (KeyType::Str, Json::Bool(b)) => Ok(KeyValue::Str(b.to_string())),
            (KeyType::Date, Json::Str(s)) => {
                crate::datetime::parse_datetime(s.trim()).map(KeyValue::Date)
            }
            // A JSON number under a date key is Unix epoch *seconds*.
            (KeyType::Date, Json::Num(n)) => {
                let ms = n * 1000.0;
                if ms.is_finite() && ms.abs() < 9.0e15 {
                    Ok(KeyValue::Date(ms as i64))
                } else {
                    Err(format!("{n} is out of range for an epoch date"))
                }
            }
            (ty, other) => Err(format!(
                "cannot read a JSON {} as {}",
                json_kind(other),
                ty.name()
            )),
        }
    }
}

fn json_kind(v: &Json) -> &'static str {
    match v {
        Json::Null => "null",
        Json::Bool(_) => "boolean",
        Json::Num(_) => "number",
        Json::Str(_) => "string",
        Json::Arr(_) => "array",
        Json::Obj(_) => "object",
    }
}

/// Render a JSON number the shortest common way: integers without a trailing
/// `.0`, everything else via the standard float formatter.
fn format_json_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyspec::parse_keyspec;

    fn csv_extractor(specs: &[&str], header: &str) -> Extractor {
        let specs = specs.iter().map(|s| parse_keyspec(s).unwrap()).collect();
        let mut ex = Extractor::new(Format::Csv, specs, b',', MissingOrder::Last, false, false);
        ex.resolve_csv_header(header.as_bytes()).unwrap();
        ex
    }

    fn jsonl_extractor(specs: &[&str], lenient: bool) -> Extractor {
        let specs = specs.iter().map(|s| parse_keyspec(s).unwrap()).collect();
        Extractor::new(
            Format::Jsonl,
            specs,
            b',',
            MissingOrder::Last,
            lenient,
            false,
        )
    }

    #[test]
    fn csv_numeric_keys_order_by_magnitude_quoted_or_not() {
        let ex = csv_extractor(&["price:num"], "id,price");
        let cheap = ex.key_for(b"1,9.5").unwrap();
        let pricey = ex.key_for(b"2,80").unwrap();
        assert!(
            cheap < pricey,
            "9.5 must sort below 80 (bytewise it would not)"
        );
        let quoted = ex.key_for(b"1,\"42\"").unwrap();
        let bare = ex.key_for(b"2,42").unwrap();
        assert_eq!(quoted, bare, "quoting must not change the key");
    }

    #[test]
    fn csv_empty_and_absent_fields_are_missing() {
        let ex = csv_extractor(&["price:num"], "id,price");
        let empty = ex.key_for(b"1,").unwrap();
        let absent = ex.key_for(b"1").unwrap();
        assert_eq!(empty, absent);
        let present = ex.key_for(b"1,0").unwrap();
        assert!(present < empty, "missing-last: values sort before missing");
    }

    #[test]
    fn parse_errors_name_the_field_unless_lenient() {
        let ex = csv_extractor(&["price:num"], "id,price");
        let e = ex.key_for(b"1,twelve").unwrap_err();
        assert!(e.contains("field 'price'"), "got: {e}");
        assert!(e.contains("twelve"), "got: {e}");
        // --lenient downgrades the same failure to a missing key.
        let specs = vec![parse_keyspec("price:num").unwrap()];
        let mut lenient = Extractor::new(Format::Csv, specs, b',', MissingOrder::Last, true, false);
        lenient.resolve_csv_header(b"id,price").unwrap();
        let bad = lenient.key_for(b"1,twelve").unwrap();
        let missing = lenient.key_for(b"1,").unwrap();
        assert_eq!(bad, missing);
    }

    #[test]
    fn ci_flag_folds_ascii_case() {
        let ex = csv_extractor(&["city:ci"], "id,city");
        assert_eq!(
            ex.key_for(b"1,Tokyo").unwrap(),
            ex.key_for(b"2,tokyo").unwrap()
        );
    }

    #[test]
    fn multi_column_keys_break_ties_in_spec_order() {
        let ex = csv_extractor(&["qty:num", "id"], "id,qty");
        let a = ex.key_for(b"b,5").unwrap();
        let b = ex.key_for(b"a,7").unwrap();
        let c = ex.key_for(b"z,7").unwrap();
        assert!(a < b, "qty decides first");
        assert!(b < c, "id breaks the tie");
    }

    #[test]
    fn no_key_specs_compare_the_whole_record() {
        let ex = Extractor::new(
            Format::Csv,
            Vec::new(),
            b',',
            MissingOrder::Last,
            false,
            false,
        );
        assert!(ex.key_for(b"apple,1").unwrap() < ex.key_for(b"pear,0").unwrap());
    }

    #[test]
    fn reverse_inverts_the_whole_ordering() {
        let specs = vec![parse_keyspec("price:num").unwrap()];
        let mut ex = Extractor::new(Format::Csv, specs, b',', MissingOrder::Last, false, true);
        ex.resolve_csv_header(b"id,price").unwrap();
        let low = ex.key_for(b"1,1").unwrap();
        let high = ex.key_for(b"2,2").unwrap();
        assert!(high < low, "reverse: larger values sort first");
    }

    #[test]
    fn jsonl_nested_paths_and_array_indices_resolve() {
        let ex = jsonl_extractor(&["user.scores.1:num"], false);
        let lo = ex.key_for(br#"{"user": {"scores": [9, 1]}}"#).unwrap();
        let hi = ex.key_for(br#"{"user": {"scores": [0, 2]}}"#).unwrap();
        assert!(lo < hi, "the second array element is the key");
    }

    #[test]
    fn jsonl_null_and_absent_both_mean_missing() {
        let ex = jsonl_extractor(&["score:num"], false);
        let null = ex.key_for(br#"{"score": null}"#).unwrap();
        let absent = ex.key_for(br#"{"other": 1}"#).unwrap();
        assert_eq!(null, absent);
    }

    #[test]
    fn jsonl_date_keys_accept_iso_strings_and_epoch_seconds() {
        let ex = jsonl_extractor(&["ts:date"], false);
        let from_string = ex.key_for(br#"{"ts": "1970-01-02T00:00:00Z"}"#).unwrap();
        let from_number = ex.key_for(br#"{"ts": 86400}"#).unwrap();
        assert_eq!(from_string, from_number);
    }

    #[test]
    fn jsonl_str_keys_render_scalars_like_json_does() {
        let ex = jsonl_extractor(&["v"], false);
        assert_eq!(
            ex.key_for(br#"{"v": 7}"#).unwrap(),
            ex.key_for(br#"{"v": "7"}"#).unwrap()
        );
        assert_eq!(
            ex.key_for(br#"{"v": true}"#).unwrap(),
            ex.key_for(br#"{"v": "true"}"#).unwrap()
        );
    }

    #[test]
    fn jsonl_bad_data_is_described() {
        let ex = jsonl_extractor(&["tags:num"], false);
        let e = ex.key_for(br#"{"tags": ["a"]}"#).unwrap_err();
        assert!(e.contains("JSON array as num"), "got: {e}");
        let ex = jsonl_extractor(&["a"], false);
        let e = ex.key_for(b"{broken").unwrap_err();
        assert!(e.contains("invalid JSON"), "got: {e}");
    }
}
