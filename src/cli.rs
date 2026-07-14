//! Command-line interface: argument parsing, format auto-detection, and
//! wiring the sorter to files or stdio.
//!
//! Exit codes follow GNU sort where it has an opinion: `0` success (or
//! `--check` passed), `1` `--check` found disorder or a duplicate, `2`
//! usage or data errors.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter};
use std::path::PathBuf;

use crate::extract::Format;
use crate::keyspec::{parse_keyspec, KeySpec};
use crate::sorter::{self, CheckOutcome, Config};
use crate::value::MissingOrder;

const USAGE: &str = "\
sortyard — external merge sort for CSV/JSONL bigger than RAM, with typed keys

USAGE:
    sortyard [OPTIONS] [FILE]

    FILE may be '-' or omitted to read stdin. Sorted records go to stdout
    unless -o is given. CSV records are emitted verbatim (quotes intact).

OPTIONS:
    -k, --key <SPEC>        Sort key: field[:type][:flag...]  (repeatable;
                            later keys break ties). Types: str (default),
                            num, date. Flags: desc, asc, ci.
                            CSV fields are header names or 1-based column
                            numbers; JSONL fields are dot paths (user.id,
                            items.0.sku).
    -f, --format <FMT>      csv | jsonl | auto  (default: auto — by file
                            extension, else by the first byte)
    -d, --delimiter <CH>    CSV delimiter: one character, or \\t (default ,;
                            .tsv files default to \\t)
        --no-header         Headerless CSV: keys are 1-based column numbers
    -o, --output <FILE>     Write here instead of stdout
    -u, --unique            Keep the first record per distinct key
    -c, --check             Verify FILE is already sorted; exit 1 if not
    -r, --reverse           Reverse the total order
        --missing <WHERE>   first | last — where records with a missing key
                            sort (default: last)
        --lenient           Treat unparseable key values as missing instead
                            of failing
        --mem <SIZE>        Buffer this much before spilling a sorted run
                            to disk, e.g. 64M, 1G  (default: 256M)
        --tmp <DIR>         Spill directory (default: the system temp dir)
        --fan-in <N>        Max runs merged per pass, >= 2  (default: 64)
        --stats             Print records/runs/passes counters to stderr
    -h, --help              Show this help
    -V, --version           Show version

EXAMPLES:
    sortyard -k price:num:desc -k id orders.csv -o sorted.csv
    sortyard -k user.plan -k ts:date events.jsonl
    zcat export.csv.gz | sortyard -f csv -k 3:num --mem 1G -o sorted.csv
    sortyard --check -k ts:date events.jsonl
";

/// What the argument vector asked for.
#[derive(Debug)]
enum Parsed {
    Help,
    Version,
    Run(Box<Options>),
}

#[derive(Debug)]
struct Options {
    keys: Vec<KeySpec>,
    format: Option<Format>,
    delim: Option<u8>,
    no_header: bool,
    output: Option<PathBuf>,
    unique: bool,
    check: bool,
    reverse: bool,
    missing: MissingOrder,
    lenient: bool,
    mem: u64,
    tmp: Option<PathBuf>,
    fan_in: usize,
    stats: bool,
    file: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            keys: Vec::new(),
            format: None,
            delim: None,
            no_header: false,
            output: None,
            unique: false,
            check: false,
            reverse: false,
            missing: MissingOrder::Last,
            lenient: false,
            mem: 256 << 20,
            tmp: None,
            fan_in: 64,
            stats: false,
            file: None,
        }
    }
}

/// Entry point used by `main`; returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    match parse_args(args) {
        Ok(Parsed::Help) => {
            print!("{USAGE}");
            0
        }
        Ok(Parsed::Version) => {
            println!("sortyard {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Ok(Parsed::Run(opts)) => match execute(&opts) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("sortyard: {e}");
                2
            }
        },
        Err(e) => {
            eprintln!("sortyard: {e}");
            eprintln!("Try 'sortyard --help' for usage.");
            2
        }
    }
}

fn parse_args(args: &[String]) -> Result<Parsed, String> {
    let mut opts = Options::default();
    let mut it = args.iter().peekable();
    let mut positional_only = false;
    while let Some(arg) = it.next() {
        // `--opt=value` splits here; `--opt value` pulls the next arg.
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with('-') && !positional_only => (n, Some(v.to_string())),
            _ => (arg.as_str(), None),
        };
        let mut value = |flag: &str| -> Result<String, String> {
            if let Some(v) = &inline {
                return Ok(v.clone());
            }
            it.next()
                .cloned()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        let no_value = |flag: &str| -> Result<(), String> {
            match inline {
                Some(_) => Err(format!("{flag} does not take a value")),
                None => Ok(()),
            }
        };
        if positional_only || !name.starts_with('-') || name == "-" {
            if opts.file.is_some() {
                return Err(format!("unexpected extra argument '{arg}'"));
            }
            opts.file = Some(PathBuf::from(name));
            continue;
        }
        match name {
            "--" => {
                no_value("--")?;
                positional_only = true;
            }
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            "-k" | "--key" => opts.keys.push(parse_keyspec(&value("--key")?)?),
            "-f" | "--format" => {
                opts.format = match value("--format")?.as_str() {
                    "csv" => Some(Format::Csv),
                    "jsonl" | "ndjson" => Some(Format::Jsonl),
                    "auto" => None,
                    other => return Err(format!("unknown format '{other}' (csv, jsonl, auto)")),
                }
            }
            "-d" | "--delimiter" => opts.delim = Some(parse_delim(&value("--delimiter")?)?),
            "--no-header" => {
                no_value("--no-header")?;
                opts.no_header = true;
            }
            "-o" | "--output" => opts.output = Some(PathBuf::from(value("--output")?)),
            "-u" | "--unique" => {
                no_value(name)?;
                opts.unique = true;
            }
            "-c" | "--check" => {
                no_value(name)?;
                opts.check = true;
            }
            "-r" | "--reverse" => {
                no_value(name)?;
                opts.reverse = true;
            }
            "--missing" => {
                opts.missing = match value("--missing")?.as_str() {
                    "first" => MissingOrder::First,
                    "last" => MissingOrder::Last,
                    other => return Err(format!("--missing must be first or last, not '{other}'")),
                }
            }
            "--lenient" => {
                no_value("--lenient")?;
                opts.lenient = true;
            }
            "--mem" => opts.mem = parse_size(&value("--mem")?)?,
            "--tmp" => opts.tmp = Some(PathBuf::from(value("--tmp")?)),
            "--fan-in" => {
                let n: usize = value("--fan-in")?
                    .parse()
                    .map_err(|_| "--fan-in must be an integer".to_string())?;
                if n < 2 {
                    return Err("--fan-in must be at least 2".to_string());
                }
                opts.fan_in = n;
            }
            "--stats" => {
                no_value("--stats")?;
                opts.stats = true;
            }
            other => return Err(format!("unknown option '{other}'")),
        }
    }
    if opts.check && opts.output.is_some() {
        return Err("--check does not write output; drop -o/--output".to_string());
    }
    Ok(Parsed::Run(Box::new(opts)))
}

/// One literal character, or the escapes `\t` for tab.
fn parse_delim(s: &str) -> Result<u8, String> {
    let b = match s {
        "\\t" | "\t" | "tab" => b'\t',
        _ if s.len() == 1 && s.is_ascii() => s.as_bytes()[0],
        _ => return Err(format!("delimiter must be one ASCII character, got '{s}'")),
    };
    if b == b'"' || b == b'\n' || b == b'\r' {
        return Err("delimiter cannot be a quote or a newline".to_string());
    }
    Ok(b)
}

/// `1048576`, `64K`, `256M`, `1G` — binary units.
fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (digits, shift) = match s.as_bytes().last() {
        Some(b'K' | b'k') => (&s[..s.len() - 1], 10),
        Some(b'M' | b'm') => (&s[..s.len() - 1], 20),
        Some(b'G' | b'g') => (&s[..s.len() - 1], 30),
        _ => (s, 0),
    };
    let n: u64 = digits
        .trim()
        .parse()
        .map_err(|_| format!("invalid size '{s}' (use e.g. 64M, 1G)"))?;
    let bytes = n
        .checked_shl(shift)
        .filter(|_| n < (1 << 34))
        .ok_or_else(|| format!("size '{s}' is out of range"))?;
    if bytes == 0 {
        return Err("size must be at least 1 byte".to_string());
    }
    Ok(bytes)
}

/// Pick the format from the extension, else from the first content byte —
/// without consuming anything from the stream.
fn detect_format(file: Option<&PathBuf>, input: &mut dyn BufRead) -> Result<Format, String> {
    if let Some(ext) = file.and_then(|p| p.extension()).and_then(|e| e.to_str()) {
        match ext.to_ascii_lowercase().as_str() {
            "csv" | "tsv" => return Ok(Format::Csv),
            "jsonl" | "ndjson" | "json" => return Ok(Format::Jsonl),
            _ => {}
        }
    }
    let buf = input.fill_buf().map_err(|e| format!("read error: {e}"))?;
    let first = buf.iter().find(|b| !b" \t\r\n".contains(b));
    Ok(match first {
        Some(b'{') | Some(b'[') => Format::Jsonl,
        _ => Format::Csv,
    })
}

/// `.tsv` inputs default to tab unless -d overrides.
fn default_delim(file: Option<&PathBuf>) -> u8 {
    match file.and_then(|p| p.extension()).and_then(|e| e.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("tsv") => b'\t',
        _ => b',',
    }
}

fn execute(opts: &Options) -> Result<i32, String> {
    let file = opts.file.as_ref().filter(|p| p.as_os_str() != "-");
    let mut input: Box<dyn BufRead> = match file {
        Some(path) => Box::new(BufReader::with_capacity(
            1 << 16,
            File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?,
        )),
        None => Box::new(std::io::stdin().lock()),
    };
    let format = match opts.format {
        Some(f) => f,
        None => detect_format(file, input.as_mut())?,
    };
    let cfg = Config {
        format,
        specs: opts.keys.clone(),
        delim: opts.delim.unwrap_or_else(|| default_delim(file)),
        has_header: !opts.no_header,
        missing: opts.missing,
        lenient: opts.lenient,
        unique: opts.unique,
        reverse: opts.reverse,
        mem_limit: opts.mem,
        fan_in: opts.fan_in,
        tmp_dir: opts.tmp.clone().unwrap_or_else(std::env::temp_dir),
    };
    let input_name = file
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "-".to_string());

    if opts.check {
        return match sorter::check(input, &cfg)? {
            CheckOutcome::Sorted => Ok(0),
            CheckOutcome::Disorder { record, line } => {
                eprintln!("sortyard: {input_name}: disorder at record {record} (line {line})");
                Ok(1)
            }
            CheckOutcome::Duplicate { record, line } => {
                eprintln!("sortyard: {input_name}: duplicate key at record {record} (line {line})");
                Ok(1)
            }
        };
    }

    let stats = match &opts.output {
        Some(path) => {
            let f =
                File::create(path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
            let mut out = BufWriter::with_capacity(1 << 16, f);
            sorter::sort(input, &mut out, &cfg)?
        }
        None => {
            let stdout = std::io::stdout();
            let mut out = BufWriter::with_capacity(1 << 16, stdout.lock());
            sorter::sort(input, &mut out, &cfg)?
        }
    };
    if opts.stats {
        eprintln!("sortyard: format:          {}", format.name());
        eprintln!("sortyard: records read:    {}", stats.records_in);
        eprintln!("sortyard: records written: {}", stats.records_out);
        eprintln!("sortyard: bytes read:      {}", stats.bytes_in);
        eprintln!("sortyard: spilled runs:    {}", stats.runs_spilled);
        eprintln!("sortyard: merge passes:    {}", stats.merge_passes);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Parsed, String> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse_args(&owned)
    }

    fn parse_run(args: &[&str]) -> Options {
        match parse(args).unwrap() {
            Parsed::Run(o) => *o,
            _ => panic!("expected a run"),
        }
    }

    #[test]
    fn keys_accumulate_in_order() {
        let o = parse_run(&["-k", "price:num:desc", "--key", "id", "orders.csv"]);
        assert_eq!(o.keys.len(), 2);
        assert_eq!(o.keys[0].field, "price");
        assert!(o.keys[0].desc);
        assert_eq!(o.keys[1].field, "id");
        assert_eq!(o.file, Some(PathBuf::from("orders.csv")));
    }

    #[test]
    fn equals_form_and_split_form_agree() {
        let a = parse_run(&["--mem=64M"]);
        let b = parse_run(&["--mem", "64M"]);
        assert_eq!(a.mem, b.mem);
        assert_eq!(a.mem, 64 << 20);
    }

    #[test]
    fn dash_reads_stdin_and_double_dash_ends_options() {
        let o = parse_run(&["-"]);
        assert_eq!(o.file, Some(PathBuf::from("-")));
        let o = parse_run(&["--", "--weird-name.csv"]);
        assert_eq!(o.file, Some(PathBuf::from("--weird-name.csv")));
    }

    #[test]
    fn invalid_argument_shapes_are_errors() {
        assert!(parse(&["--frobnicate"]).is_err(), "unknown option");
        assert!(parse(&["a.csv", "b.csv"]).is_err(), "one input only");
        assert!(parse(&["-k"]).is_err(), "missing value");
        assert!(parse(&["-k", "x:bogus"]).is_err(), "bad key spec");
        assert!(parse(&["--fan-in", "1"]).is_err(), "fan-in must be >= 2");
        assert_eq!(parse_run(&["--fan-in", "8"]).fan_in, 8);
        let e = parse(&["--stats=yes"]).unwrap_err();
        assert!(e.contains("does not take a value"), "got: {e}");
        assert!(
            parse(&["--check=1"]).is_err(),
            "boolean flags take no value"
        );
    }

    #[test]
    fn check_conflicts_with_output() {
        let e = parse(&["--check", "-o", "out.csv"]).unwrap_err();
        assert!(e.contains("--check"), "got: {e}");
    }

    #[test]
    fn size_suffixes_are_binary_and_junk_is_rejected() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("64K").unwrap(), 64 << 10);
        assert_eq!(parse_size("256M").unwrap(), 256 << 20);
        assert_eq!(parse_size("2g").unwrap(), 2 << 30);
        assert!(parse_size("0").is_err());
        assert!(parse_size("lots").is_err());
        assert!(parse_size("99999999999G").is_err());
    }

    #[test]
    fn delimiters_accept_escapes_and_tsv_defaults_to_tab() {
        assert_eq!(parse_delim(",").unwrap(), b',');
        assert_eq!(parse_delim("\\t").unwrap(), b'\t');
        assert_eq!(parse_delim("tab").unwrap(), b'\t');
        assert_eq!(parse_delim("|").unwrap(), b'|');
        assert!(parse_delim("\"").is_err());
        assert!(parse_delim("ab").is_err());
        assert_eq!(default_delim(Some(&PathBuf::from("data.tsv"))), b'\t');
        assert_eq!(default_delim(Some(&PathBuf::from("data.csv"))), b',');
        assert_eq!(default_delim(None), b',');
    }

    #[test]
    fn format_detection_prefers_extension_then_content_without_consuming() {
        let mut csvish: &[u8] = b"{not json extension}";
        assert_eq!(
            detect_format(Some(&PathBuf::from("x.csv")), &mut csvish).unwrap(),
            Format::Csv
        );
        let mut json_body: &[u8] = b"  {\"a\": 1}\n";
        assert_eq!(detect_format(None, &mut json_body).unwrap(), Format::Jsonl);
        let mut empty: &[u8] = b"";
        assert_eq!(detect_format(None, &mut empty).unwrap(), Format::Csv);
        // Peeking must leave the stream untouched.
        let mut body: &[u8] = b"id\n1\n";
        assert_eq!(detect_format(None, &mut body).unwrap(), Format::Csv);
        let mut first = String::new();
        body.read_line(&mut first).unwrap();
        assert_eq!(first, "id\n", "the detected bytes must still be readable");
    }
}
