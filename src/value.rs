//! Typed key values and their order-preserving byte encoding.
//!
//! Every extracted key part is normalized into a byte string whose plain
//! `memcmp` order equals the semantic order (numbers by magnitude, dates by
//! instant, strings lexicographically, missing values first or last). The
//! in-memory sort, the spill files and the k-way merge all compare these
//! bytes directly, so a record's key is parsed exactly once.

use std::str::FromStr;

/// Declared type of one key part (`str` is the default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyType {
    Str,
    Num,
    Date,
}

impl KeyType {
    pub fn name(self) -> &'static str {
        match self {
            KeyType::Str => "str",
            KeyType::Num => "num",
            KeyType::Date => "date",
        }
    }
}

impl FromStr for KeyType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "str" | "string" => Ok(KeyType::Str),
            "num" | "number" => Ok(KeyType::Num),
            "date" | "datetime" | "time" => Ok(KeyType::Date),
            other => Err(format!(
                "unknown key type '{other}' (expected str, num or date)"
            )),
        }
    }
}

/// Where records with a missing key part sort, relative to present values.
/// This placement is absolute: the per-key `desc` flag inverts value order
/// but never moves missing records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingOrder {
    First,
    Last,
}

/// One extracted, typed key part.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyValue {
    Missing,
    /// Finite or infinite float. NaN is rejected at parse time.
    Num(f64),
    /// UTC epoch milliseconds.
    Date(i64),
    Str(String),
}

impl KeyValue {
    /// Parse a raw text field according to the declared type. The caller has
    /// already decided the field is present (empty CSV fields and JSON nulls
    /// become [`KeyValue::Missing`] before this point).
    pub fn parse_typed(text: &str, ty: KeyType) -> Result<KeyValue, String> {
        match ty {
            KeyType::Str => Ok(KeyValue::Str(text.to_string())),
            KeyType::Num => parse_num(text).map(KeyValue::Num),
            KeyType::Date => crate::datetime::parse_datetime(text.trim()).map(KeyValue::Date),
        }
    }
}

/// Parse a number the way spreadsheets export them: optional surrounding
/// whitespace, standard float syntax. NaN is rejected because it has no
/// place in a total order.
pub fn parse_num(text: &str) -> Result<f64, String> {
    let t = text.trim();
    let n: f64 = t
        .parse()
        .map_err(|_| format!("cannot parse {t:?} as a number"))?;
    if n.is_nan() {
        return Err("NaN is not sortable".to_string());
    }
    Ok(n)
}

// Presence tags. Chosen so that missing-first < present < missing-last under
// unsigned byte comparison, whichever placement is configured for the run.
const TAG_MISSING_FIRST: u8 = 0x01;
const TAG_PRESENT: u8 = 0x40;
const TAG_MISSING_LAST: u8 = 0xfe;

/// Append the order-preserving encoding of one key part to `out`.
///
/// Layout: one presence tag byte, then (present values only) the encoded
/// value. `desc` inverts the value bytes — the tag is left alone so missing
/// placement stays absolute. Every encoding is self-delimiting (fixed width
/// for numbers/dates, 0x00 0x00 terminated for strings), so concatenated
/// parts compare part-by-part under a single `memcmp`.
pub fn encode_into(out: &mut Vec<u8>, value: &KeyValue, desc: bool, missing: MissingOrder) {
    let tag = match (value, missing) {
        (KeyValue::Missing, MissingOrder::First) => TAG_MISSING_FIRST,
        (KeyValue::Missing, MissingOrder::Last) => TAG_MISSING_LAST,
        _ => TAG_PRESENT,
    };
    out.push(tag);
    let start = out.len();
    match value {
        KeyValue::Missing => {}
        KeyValue::Num(n) => out.extend_from_slice(&encode_f64(*n)),
        KeyValue::Date(ms) => out.extend_from_slice(&((*ms as u64) ^ (1 << 63)).to_be_bytes()),
        KeyValue::Str(s) => encode_str(out, s.as_bytes()),
    }
    if desc {
        for b in &mut out[start..] {
            *b = !*b;
        }
    }
}

/// Map an f64 onto u64 so that unsigned big-endian comparison matches float
/// comparison: negative floats get all bits flipped, non-negative floats get
/// the sign bit set.
fn encode_f64(n: f64) -> [u8; 8] {
    let bits = n.to_bits();
    let ordered = if bits >> 63 == 1 {
        !bits
    } else {
        bits | (1 << 63)
    };
    ordered.to_be_bytes()
}

/// Escape 0x00 as 0x00 0x01 and terminate with 0x00 0x00. The terminator is
/// smaller than any escaped continuation, so "a" < "a\0" < "ab" holds and the
/// encoding stays prefix-free (required for multi-part keys).
fn encode_str(out: &mut Vec<u8>, bytes: &[u8]) {
    for &b in bytes {
        if b == 0 {
            out.push(0x00);
            out.push(0x01);
        } else {
            out.push(b);
        }
    }
    out.push(0x00);
    out.push(0x00);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(v: &KeyValue, desc: bool, missing: MissingOrder) -> Vec<u8> {
        let mut out = Vec::new();
        encode_into(&mut out, v, desc, missing);
        out
    }

    fn enc_asc(v: &KeyValue) -> Vec<u8> {
        enc(v, false, MissingOrder::Last)
    }

    #[test]
    fn num_encoding_orders_the_whole_float_line() {
        let values = [
            f64::NEG_INFINITY,
            -1e300,
            -2.5,
            -0.5,
            0.0,
            0.25,
            3.0,
            1e300,
            f64::INFINITY,
        ];
        for pair in values.windows(2) {
            let a = enc_asc(&KeyValue::Num(pair[0]));
            let b = enc_asc(&KeyValue::Num(pair[1]));
            assert!(a < b, "{} should encode below {}", pair[0], pair[1]);
        }
        assert_eq!(
            enc_asc(&KeyValue::Num(42.0)),
            enc_asc(&KeyValue::Num(42.0)),
            "equal numbers must encode byte-identically"
        );
    }

    #[test]
    fn date_encoding_orders_across_epoch() {
        let before = enc_asc(&KeyValue::Date(-86_400_000));
        let epoch = enc_asc(&KeyValue::Date(0));
        let after = enc_asc(&KeyValue::Date(86_400_000));
        assert!(before < epoch && epoch < after);
    }

    #[test]
    fn str_encoding_is_lexicographic_prefix_safe_and_nul_safe() {
        let a = enc_asc(&KeyValue::Str("a".into()));
        let ab = enc_asc(&KeyValue::Str("ab".into()));
        let b = enc_asc(&KeyValue::Str("b".into()));
        assert!(a < ab, "prefix must sort before its extension");
        assert!(ab < b);
        // "a" < "a\0" < "a\x01": the 0x00-escape must not reorder around
        // the terminator or around small real bytes.
        let a_nul = enc_asc(&KeyValue::Str("a\0".into()));
        let a_one = enc_asc(&KeyValue::Str("a\x01".into()));
        assert!(a < a_nul && a_nul < a_one);
    }

    #[test]
    fn desc_flag_reverses_numbers_and_string_prefix_pairs() {
        let two = enc(&KeyValue::Num(2.0), true, MissingOrder::Last);
        let ten = enc(&KeyValue::Num(10.0), true, MissingOrder::Last);
        assert!(ten < two, "descending: 10 must sort before 2");
        let a = enc(&KeyValue::Str("a".into()), true, MissingOrder::Last);
        let ab = enc(&KeyValue::Str("ab".into()), true, MissingOrder::Last);
        assert!(ab < a, "descending: the longer extension must come first");
    }

    #[test]
    fn missing_placement_is_absolute() {
        // First: below -inf. Last: above +inf. And desc inverts value bytes
        // only, so a missing part stays pinned wherever it was placed.
        let missing_first = enc(&KeyValue::Missing, false, MissingOrder::First);
        let lowest = enc(
            &KeyValue::Num(f64::NEG_INFINITY),
            false,
            MissingOrder::First,
        );
        assert!(missing_first < lowest);
        let missing_last = enc(&KeyValue::Missing, false, MissingOrder::Last);
        let highest = enc(&KeyValue::Num(f64::INFINITY), false, MissingOrder::Last);
        assert!(missing_last > highest);
        let missing_desc = enc(&KeyValue::Missing, true, MissingOrder::Last);
        let present_desc = enc(&KeyValue::Num(-1e300), true, MissingOrder::Last);
        assert!(missing_desc > present_desc);
    }

    #[test]
    fn multi_part_keys_compare_part_by_part() {
        // ("a", 2) vs ("ab", 1): the first part must decide even though the
        // second part of the shorter key holds a larger number.
        let mut short = Vec::new();
        encode_into(
            &mut short,
            &KeyValue::Str("a".into()),
            false,
            MissingOrder::Last,
        );
        encode_into(&mut short, &KeyValue::Num(2.0), false, MissingOrder::Last);
        let mut long = Vec::new();
        encode_into(
            &mut long,
            &KeyValue::Str("ab".into()),
            false,
            MissingOrder::Last,
        );
        encode_into(&mut long, &KeyValue::Num(1.0), false, MissingOrder::Last);
        assert!(short < long);
    }

    #[test]
    fn parse_num_accepts_export_shapes_and_rejects_junk() {
        assert_eq!(parse_num(" 42 ").unwrap(), 42.0);
        assert_eq!(parse_num("-0.5").unwrap(), -0.5);
        assert_eq!(parse_num("1e3").unwrap(), 1000.0);
        assert!(parse_num("12,50").is_err(), "locale commas are not numbers");
        assert!(parse_num("abc").is_err());
        assert!(
            parse_num("NaN").is_err(),
            "NaN has no place in a total order"
        );
        assert!(parse_num("").is_err());
    }

    #[test]
    fn key_type_parses_aliases() {
        assert_eq!("string".parse::<KeyType>().unwrap(), KeyType::Str);
        assert_eq!("number".parse::<KeyType>().unwrap(), KeyType::Num);
        assert_eq!("datetime".parse::<KeyType>().unwrap(), KeyType::Date);
        assert!("float".parse::<KeyType>().is_err());
    }
}
