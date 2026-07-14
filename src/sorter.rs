//! The engine: buffer, sort, spill, merge — plus the `--check` path.
//!
//! Records stream in, get their key encoded once, and accumulate in a
//! buffer bounded by `--mem`. A full buffer is sorted and spilled as a run;
//! at end of input either the buffer is emitted directly (everything fit)
//! or the runs are k-way merged, with intermediate passes when the run
//! count exceeds `--fan-in`. Output order is always `(encoded key, input
//! sequence)` — a stable sort by construction.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use crate::csv::{CsvReader, RawRecord};
use crate::extract::{Extractor, Format};
use crate::keyspec::KeySpec;
use crate::merge::{merge_streams, reduce_runs};
use crate::spill::{Rec, RunFile, SpillArea};
use crate::value::MissingOrder;

/// Everything the engine needs to know, assembled by the CLI.
pub struct Config {
    pub format: Format,
    pub specs: Vec<KeySpec>,
    pub delim: u8,
    /// CSV only: the first record is a header (resolved, preserved on top).
    pub has_header: bool,
    pub missing: MissingOrder,
    pub lenient: bool,
    pub unique: bool,
    pub reverse: bool,
    /// Buffer cap in bytes before a run is spilled.
    pub mem_limit: u64,
    /// Maximum runs merged in one pass.
    pub fan_in: usize,
    pub tmp_dir: PathBuf,
}

/// Counters reported by `--stats` and asserted on by the tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    pub records_in: u64,
    pub records_out: u64,
    pub bytes_in: u64,
    pub runs_spilled: u64,
    pub merge_passes: u64,
}

/// Result of `--check`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    Sorted,
    /// `record` counts data records (1-based); `line` is the physical line.
    Disorder {
        record: u64,
        line: u64,
    },
    /// Only reported with `--unique`: two records share a key.
    Duplicate {
        record: u64,
        line: u64,
    },
}

/// Unified record source: CSV records (which may span lines) or JSONL lines.
enum Source<R: BufRead> {
    Csv(CsvReader<R>),
    Jsonl { inner: R, line: u64 },
}

impl<R: BufRead> Source<R> {
    fn next(&mut self) -> Result<Option<RawRecord>, String> {
        match self {
            Source::Csv(r) => r.next_record(),
            Source::Jsonl { inner, line } => {
                let mut buf = Vec::new();
                let n = inner
                    .read_until(b'\n', &mut buf)
                    .map_err(|e| format!("read error: {e}"))?;
                if n == 0 {
                    return Ok(None);
                }
                let this_line = *line;
                *line += 1;
                if buf.last() == Some(&b'\n') {
                    buf.pop();
                }
                if buf.last() == Some(&b'\r') {
                    buf.pop();
                }
                Ok(Some(RawRecord {
                    bytes: buf,
                    line: this_line,
                }))
            }
        }
    }
}

/// Source, extractor and (for CSV) the header record to re-emit.
type Prepared<R> = (Source<R>, Extractor, Option<Vec<u8>>);

/// Shared setup for sort and check: build the extractor, open the source,
/// and consume the CSV header (returned so `sort` can re-emit it).
fn prepare<R: BufRead>(input: R, cfg: &Config) -> Result<Prepared<R>, String> {
    let mut extractor = Extractor::new(
        cfg.format,
        cfg.specs.clone(),
        cfg.delim,
        cfg.missing,
        cfg.lenient,
        cfg.reverse,
    );
    let mut source = match cfg.format {
        Format::Csv => Source::Csv(CsvReader::new(input, cfg.delim)),
        Format::Jsonl => Source::Jsonl {
            inner: input,
            line: 1,
        },
    };
    let mut header = None;
    if cfg.format == Format::Csv {
        if cfg.has_header {
            if let Some(rec) = source.next()? {
                extractor.resolve_csv_header(&rec.bytes)?;
                header = Some(rec.bytes);
            }
            // Empty input: nothing to resolve, nothing to sort.
        } else if !cfg.specs.is_empty() {
            extractor.resolve_csv_headerless()?;
        }
    }
    Ok((source, extractor, header))
}

fn locate(cfg: &Config, record: u64, line: u64) -> String {
    match cfg.format {
        Format::Csv => format!("record {record} (line {line})"),
        Format::Jsonl => format!("line {line}"),
    }
}

/// Sort `input` into `output` according to `cfg`.
pub fn sort<R: BufRead>(input: R, output: &mut dyn Write, cfg: &Config) -> Result<Stats, String> {
    let (mut source, extractor, header) = prepare(input, cfg)?;
    let mut stats = Stats::default();
    let mut emitter = Emitter {
        out: output,
        unique: cfg.unique,
        last_key: None,
        written: 0,
    };
    if let Some(h) = header {
        emitter.write_raw(&h)?;
    }

    let mut buffer: Vec<Rec> = Vec::new();
    let mut buffered_bytes = 0u64;
    let mut seq = 0u64;
    let mut area: Option<SpillArea> = None;
    let mut runs: Vec<RunFile> = Vec::new();

    while let Some(rec) = source.next()? {
        if rec.bytes.is_empty() {
            continue; // blank lines carry no record
        }
        stats.records_in += 1;
        stats.bytes_in += rec.bytes.len() as u64;
        let key = extractor
            .key_for(&rec.bytes)
            .map_err(|e| format!("{}: {e}", locate(cfg, stats.records_in, rec.line)))?;
        let entry = Rec {
            key,
            seq,
            raw: rec.bytes,
        };
        seq += 1;
        buffered_bytes += entry.cost();
        buffer.push(entry);
        if buffered_bytes >= cfg.mem_limit {
            if area.is_none() {
                area = Some(SpillArea::create(&cfg.tmp_dir)?);
            }
            let area = area.as_mut().expect("just created");
            buffer.sort_unstable_by(|a, b| a.cmp_order(b));
            runs.push(area.write_run(&buffer)?);
            stats.runs_spilled += 1;
            buffer.clear();
            buffered_bytes = 0;
        }
    }

    if runs.is_empty() {
        // Fast path: everything fit in memory; no disk I/O at all.
        buffer.sort_unstable_by(|a, b| a.cmp_order(b));
        for rec in buffer {
            emitter.emit(rec)?;
        }
    } else {
        let area = area.as_mut().expect("runs imply a spill area");
        if !buffer.is_empty() {
            buffer.sort_unstable_by(|a, b| a.cmp_order(b));
            runs.push(area.write_run(&buffer)?);
            stats.runs_spilled += 1;
            buffer.clear();
        }
        let (survivors, passes) = reduce_runs(area, runs, cfg.fan_in)?;
        stats.merge_passes = passes + 1; // + the final merge below
        let readers = survivors
            .iter()
            .map(RunFile::open)
            .collect::<Result<Vec<_>, _>>()?;
        merge_streams(readers, |rec| emitter.emit(rec))?;
    }
    emitter
        .out
        .flush()
        .map_err(|e| format!("cannot write output: {e}"))?;
    stats.records_out = emitter.written;
    Ok(stats)
}

/// Verify that `input` is already ordered by the configured keys.
pub fn check<R: BufRead>(input: R, cfg: &Config) -> Result<CheckOutcome, String> {
    let (mut source, extractor, _header) = prepare(input, cfg)?;
    let mut prev: Option<Vec<u8>> = None;
    let mut record = 0u64;
    while let Some(rec) = source.next()? {
        if rec.bytes.is_empty() {
            continue;
        }
        record += 1;
        let key = extractor
            .key_for(&rec.bytes)
            .map_err(|e| format!("{}: {e}", locate(cfg, record, rec.line)))?;
        if let Some(prev_key) = &prev {
            if *prev_key > key {
                return Ok(CheckOutcome::Disorder {
                    record,
                    line: rec.line,
                });
            }
            if cfg.unique && *prev_key == key {
                return Ok(CheckOutcome::Duplicate {
                    record,
                    line: rec.line,
                });
            }
        }
        prev = Some(key);
    }
    Ok(CheckOutcome::Sorted)
}

/// Writes records in arrival order, optionally dropping key-duplicates
/// (`--unique` keeps the first record of each key, i.e. the earliest by
/// input order thanks to the stable sort).
struct Emitter<'a> {
    out: &'a mut dyn Write,
    unique: bool,
    last_key: Option<Vec<u8>>,
    written: u64,
}

impl Emitter<'_> {
    fn emit(&mut self, rec: Rec) -> Result<(), String> {
        if self.unique {
            if self.last_key.as_deref() == Some(rec.key.as_slice()) {
                return Ok(());
            }
            self.last_key = Some(rec.key);
        }
        self.write_raw(&rec.raw)
    }

    fn write_raw(&mut self, raw: &[u8]) -> Result<(), String> {
        self.out
            .write_all(raw)
            .and_then(|()| self.out.write_all(b"\n"))
            .map_err(|e| format!("cannot write output: {e}"))?;
        self.written += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyspec::parse_keyspec;

    fn config(format: Format, specs: &[&str]) -> Config {
        Config {
            format,
            specs: specs.iter().map(|s| parse_keyspec(s).unwrap()).collect(),
            delim: b',',
            has_header: format == Format::Csv,
            missing: MissingOrder::Last,
            lenient: false,
            unique: false,
            reverse: false,
            mem_limit: 1 << 20,
            fan_in: 16,
            tmp_dir: std::env::temp_dir(),
        }
    }

    fn run_sort(input: &str, cfg: &Config) -> (String, Stats) {
        let mut out = Vec::new();
        let stats = sort(input.as_bytes(), &mut out, cfg).unwrap();
        (String::from_utf8(out).unwrap(), stats)
    }

    #[test]
    fn csv_sorts_by_typed_key_and_keeps_the_header_on_top() {
        let cfg = config(Format::Csv, &["price:num"]);
        let (out, stats) = run_sort("id,price\nb,20\na,3\nc,100\n", &cfg);
        assert_eq!(out, "id,price\na,3\nb,20\nc,100\n");
        assert_eq!(stats.records_in, 3);
        assert_eq!(stats.records_out, 4, "header + 3 data records");
        assert_eq!(stats.runs_spilled, 0, "small input stays in memory");
    }

    #[test]
    fn sort_is_stable_for_equal_keys() {
        let cfg = config(Format::Csv, &["grp"]);
        let (out, _) = run_sort("grp,val\nb,1\na,2\nb,3\na,4\n", &cfg);
        assert_eq!(out, "grp,val\na,2\na,4\nb,1\nb,3\n");
    }

    #[test]
    fn spilled_sort_produces_the_same_bytes_as_in_memory() {
        // The core external-sort invariant: the memory cap must never
        // change the answer, only the plan.
        let mut rows = String::from("id,price\n");
        for i in 0..500 {
            // A little pseudo-random walk, fixed seed by construction.
            let price = (i * 7919) % 997;
            rows.push_str(&format!("row{i},{price}\n"));
        }
        let cfg_mem = config(Format::Csv, &["price:num", "id"]);
        let mut cfg_spill = config(Format::Csv, &["price:num", "id"]);
        cfg_spill.mem_limit = 2_000; // forces many runs
        cfg_spill.fan_in = 3; // and multiple merge passes
        let (a, stats_a) = run_sort(&rows, &cfg_mem);
        let (b, stats_b) = run_sort(&rows, &cfg_spill);
        assert_eq!(a, b);
        assert_eq!(stats_a.runs_spilled, 0);
        assert!(
            stats_b.runs_spilled > 3,
            "got {} runs",
            stats_b.runs_spilled
        );
        assert!(stats_b.merge_passes > 1, "fan-in 3 must force extra passes");
    }

    #[test]
    fn quoted_csv_records_survive_the_spill_byte_for_byte() {
        let tricky = "id,note\n2,\"multi\nline, with ,commas\"\n1,\"say \"\"hi\"\"\"\n";
        let mut cfg = config(Format::Csv, &["id:num"]);
        cfg.mem_limit = 1; // spill after every record
        let (out, stats) = run_sort(tricky, &cfg);
        assert_eq!(
            out,
            "id,note\n1,\"say \"\"hi\"\"\"\n2,\"multi\nline, with ,commas\"\n"
        );
        assert_eq!(stats.runs_spilled, 2);
    }

    #[test]
    fn jsonl_sorts_by_nested_date_key() {
        let cfg = config(Format::Jsonl, &["meta.ts:date"]);
        let input = concat!(
            "{\"id\":1,\"meta\":{\"ts\":\"2026-03-01\"}}\n",
            "{\"id\":2,\"meta\":{\"ts\":\"2025-12-31T23:00:00Z\"}}\n",
            "{\"id\":3,\"meta\":{\"ts\":\"2026-01-15T09:30:00+09:00\"}}\n",
        );
        let (out, _) = run_sort(input, &cfg);
        let ids: Vec<&str> = out.lines().map(|l| &l[6..7]).collect();
        assert_eq!(ids, vec!["2", "3", "1"]);
    }

    #[test]
    fn unique_keeps_the_first_record_per_key_even_across_runs() {
        let mut cfg = config(Format::Csv, &["sku"]);
        cfg.unique = true;
        let (out, stats) = run_sort("sku,qty\nb,1\na,2\nb,3\na,4\n", &cfg);
        assert_eq!(out, "sku,qty\na,2\nb,1\n");
        assert_eq!(stats.records_out, 3);
        // Same answer when every record spills into its own run.
        cfg.mem_limit = 1;
        let (spilled, _) = run_sort("sku,qty\nb,1\na,2\nb,3\na,4\n", &cfg);
        assert_eq!(spilled, out);
    }

    #[test]
    fn blank_lines_are_dropped() {
        let cfg = config(Format::Jsonl, &["a:num"]);
        let (out, stats) = run_sort("{\"a\":2}\n\n{\"a\":1}\n\n", &cfg);
        assert_eq!(out, "{\"a\":1}\n{\"a\":2}\n");
        assert_eq!(stats.records_in, 2);
    }

    #[test]
    fn key_errors_carry_the_record_location() {
        let cfg = config(Format::Csv, &["price:num"]);
        let mut out = Vec::new();
        let e = sort("id,price\nok,1\nbad,oops\n".as_bytes(), &mut out, &cfg).unwrap_err();
        assert!(e.contains("record 2 (line 3)"), "got: {e}");
        let cfg = config(Format::Jsonl, &["a:num"]);
        let e = sort(b"{\"a\":1}\n{\"a\":\"x\"}\n".as_slice(), &mut out, &cfg).unwrap_err();
        assert!(e.contains("line 2"), "got: {e}");
    }

    #[test]
    fn missing_first_moves_gap_records_to_the_top() {
        let mut cfg = config(Format::Csv, &["score:num"]);
        cfg.missing = MissingOrder::First;
        let (out, _) = run_sort("id,score\na,5\nb,\nc,1\n", &cfg);
        assert_eq!(out, "id,score\nb,\nc,1\na,5\n");
    }

    #[test]
    fn empty_and_header_only_inputs_round_trip() {
        let cfg = config(Format::Csv, &[]);
        let (out, stats) = run_sort("", &cfg);
        assert_eq!(out, "");
        assert_eq!(stats.records_out, 0);
        let cfg = config(Format::Csv, &["id"]);
        let (out, stats) = run_sort("id,price\n", &cfg);
        assert_eq!(out, "id,price\n");
        assert_eq!(stats.records_in, 0);
    }

    #[test]
    fn check_accepts_sorted_and_pinpoints_disorder() {
        let cfg = config(Format::Csv, &["price:num"]);
        let ok = check("id,price\na,1\nb,2\n".as_bytes(), &cfg).unwrap();
        assert_eq!(ok, CheckOutcome::Sorted);
        let bad = check("id,price\na,5\nb,2\nc,9\n".as_bytes(), &cfg).unwrap();
        assert_eq!(bad, CheckOutcome::Disorder { record: 2, line: 3 });
        // Desc keys flip what "sorted" means.
        let cfg = config(Format::Csv, &["price:num:desc"]);
        let ok = check("id,price\na,9\nb,3\nc,1\n".as_bytes(), &cfg).unwrap();
        assert_eq!(ok, CheckOutcome::Sorted);
    }

    #[test]
    fn check_with_unique_flags_duplicates() {
        let mut cfg = config(Format::Csv, &["sku"]);
        cfg.unique = true;
        let dup = check("sku\na\nb\nb\n".as_bytes(), &cfg).unwrap();
        assert_eq!(dup, CheckOutcome::Duplicate { record: 3, line: 4 });
    }
}
