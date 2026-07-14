//! `--key` specification parsing: `field[:type][:flag...]`.
//!
//! The field part names a CSV header, a 1-based CSV column index, or a
//! dot-separated JSONL path (`user.address.city`, `items.0.sku`). The
//! optional type is `str` (default), `num` or `date`; flags are `desc`
//! (descending) and `ci` (ASCII case-insensitive, `str` keys only).

use crate::value::KeyType;

/// One parsed `--key` specification, before column/path resolution.
#[derive(Debug, Clone, PartialEq)]
pub struct KeySpec {
    /// The raw field selector as written by the user.
    pub field: String,
    pub ty: KeyType,
    pub desc: bool,
    pub ci: bool,
}

impl KeySpec {
    /// The field selector split into JSONL path segments.
    pub fn path(&self) -> Vec<String> {
        self.field.split('.').map(str::to_string).collect()
    }
}

/// Parse one `--key` argument. The first `:`-separated segment is the field
/// selector; every following segment must be a type or a flag, in any order,
/// with at most one type.
pub fn parse_keyspec(spec: &str) -> Result<KeySpec, String> {
    let mut parts = spec.split(':');
    let field = parts.next().unwrap_or("").to_string();
    if field.is_empty() {
        return Err(format!("key spec '{spec}': empty field selector"));
    }
    let mut ty: Option<KeyType> = None;
    let mut desc = false;
    let mut ci = false;
    for part in parts {
        match part {
            "desc" | "descending" => desc = true,
            "asc" | "ascending" => desc = false,
            "ci" => ci = true,
            other => {
                let parsed: KeyType = other.parse().map_err(|_| bad_modifier(spec, other))?;
                if let Some(existing) = ty {
                    return Err(format!(
                        "key spec '{spec}': conflicting types '{}' and '{}'",
                        existing.name(),
                        parsed.name()
                    ));
                }
                ty = Some(parsed);
            }
        }
    }
    let ty = ty.unwrap_or(KeyType::Str);
    if ci && ty != KeyType::Str {
        return Err(format!(
            "key spec '{spec}': 'ci' only applies to str keys, not {}",
            ty.name()
        ));
    }
    Ok(KeySpec {
        field,
        ty,
        desc,
        ci,
    })
}

fn bad_modifier(spec: &str, part: &str) -> String {
    format!(
        "key spec '{spec}': unknown modifier '{part}' \
         (expected a type: str, num, date; or a flag: desc, asc, ci)"
    )
}

/// How a key spec addresses a CSV column once the header is known.
#[derive(Debug, Clone, PartialEq)]
pub enum ColRef {
    /// 0-based column index.
    Index(usize),
}

/// Resolve a key spec's field selector against a CSV header row. Exact
/// header names win; a selector that matches no header but is all digits is
/// taken as a 1-based column index.
pub fn resolve_csv_column(field: &str, header: &[String]) -> Result<ColRef, String> {
    if let Some(i) = header.iter().position(|h| h == field) {
        return Ok(ColRef::Index(i));
    }
    if let Ok(n) = field.parse::<usize>() {
        return index_col(n, field);
    }
    Err(format!(
        "key field '{field}' not found in header ({})",
        header.join(", ")
    ))
}

/// Resolve a selector for headerless CSV: it must be a 1-based index.
pub fn resolve_csv_index(field: &str) -> Result<ColRef, String> {
    match field.parse::<usize>() {
        Ok(n) => index_col(n, field),
        Err(_) => Err(format!(
            "key field '{field}': with --no-header, keys must be 1-based column numbers"
        )),
    }
}

fn index_col(n: usize, field: &str) -> Result<ColRef, String> {
    if n == 0 {
        Err(format!("key field '{field}': columns are numbered from 1"))
    } else {
        Ok(ColRef::Index(n - 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_field_defaults_to_ascending_str() {
        let k = parse_keyspec("name").unwrap();
        assert_eq!(k.field, "name");
        assert_eq!(k.ty, KeyType::Str);
        assert!(!k.desc);
        assert!(!k.ci);
    }

    #[test]
    fn type_and_flags_combine_in_any_order() {
        let a = parse_keyspec("price:num:desc").unwrap();
        let b = parse_keyspec("price:desc:num").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.ty, KeyType::Num);
        assert!(a.desc);
    }

    #[test]
    fn ci_flag_applies_to_str_keys_only() {
        let k = parse_keyspec("city:ci").unwrap();
        assert!(k.ci);
        assert_eq!(k.ty, KeyType::Str);
        let e = parse_keyspec("price:num:ci").unwrap_err();
        assert!(e.contains("only applies to str"), "got: {e}");
    }

    #[test]
    fn dotted_paths_stay_in_the_field_part() {
        let k = parse_keyspec("user.address.city:desc").unwrap();
        assert_eq!(k.field, "user.address.city");
        assert_eq!(k.path(), vec!["user", "address", "city"]);
        assert!(k.desc);
    }

    #[test]
    fn invalid_specs_are_rejected_with_guidance() {
        let e = parse_keyspec("x:num:date").unwrap_err();
        assert!(e.contains("conflicting types"), "got: {e}");
        let e = parse_keyspec("x:reverse").unwrap_err();
        assert!(e.contains("unknown modifier 'reverse'"), "got: {e}");
        assert!(e.contains("desc"), "the error should teach the fix: {e}");
        assert!(parse_keyspec("").is_err());
        assert!(parse_keyspec(":num").is_err());
    }

    #[test]
    fn header_names_win_over_numeric_interpretation() {
        // A header literally named "2" must resolve by name, not position;
        // a selector matching no header falls back to a 1-based index.
        let header = vec!["a".to_string(), "b".to_string(), "2".to_string()];
        assert_eq!(resolve_csv_column("2", &header).unwrap(), ColRef::Index(2));
        assert_eq!(resolve_csv_column("b", &header).unwrap(), ColRef::Index(1));
        assert_eq!(resolve_csv_column("1", &header).unwrap(), ColRef::Index(0));
        assert!(
            resolve_csv_column("0", &header).is_err(),
            "columns are 1-based"
        );
    }

    #[test]
    fn missing_header_names_list_the_candidates() {
        let header = vec!["id".to_string(), "price".to_string()];
        let e = resolve_csv_column("cost", &header).unwrap_err();
        assert!(e.contains("'cost' not found"), "got: {e}");
        assert!(e.contains("id, price"), "got: {e}");
    }

    #[test]
    fn headerless_mode_requires_numeric_selectors() {
        assert_eq!(resolve_csv_index("3").unwrap(), ColRef::Index(2));
        let e = resolve_csv_index("name").unwrap_err();
        assert!(e.contains("--no-header"), "got: {e}");
    }
}
