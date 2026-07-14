//! K-way merge of sorted runs, with bounded fan-in.
//!
//! A binary heap keyed on `(encoded key, seq)` yields the global order from
//! any number of sorted sources. When more runs exist than `--fan-in`
//! allows open at once, intermediate passes merge groups of runs into new
//! runs until one final merge can stream straight to the output — the
//! classic polyphase-free external merge plan.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::spill::{Rec, RunFile, RunReader, SpillArea};

/// Heap entry: the record plus the index of the source it came from.
/// Ordering is *reversed* so `BinaryHeap` (a max-heap) pops the smallest.
struct Entry {
    rec: Rec,
    src: usize,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Entry {}
impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Entry {
    fn cmp(&self, other: &Self) -> Ordering {
        other.rec.cmp_order(&self.rec)
    }
}

/// Merge open runs, calling `emit` once per record in global order.
pub fn merge_streams(
    mut readers: Vec<RunReader>,
    mut emit: impl FnMut(Rec) -> Result<(), String>,
) -> Result<(), String> {
    let mut heap = BinaryHeap::with_capacity(readers.len());
    for (src, reader) in readers.iter_mut().enumerate() {
        if let Some(rec) = reader.next_rec()? {
            heap.push(Entry { rec, src });
        }
    }
    while let Some(Entry { rec, src }) = heap.pop() {
        if let Some(next) = readers[src].next_rec()? {
            heap.push(Entry { rec: next, src });
        }
        emit(rec)?;
    }
    Ok(())
}

/// Reduce `runs` with intermediate merge passes until at most `fan_in`
/// remain. Returns the surviving runs and the number of intermediate merges
/// performed. Consumed run files are deleted eagerly.
pub fn reduce_runs(
    area: &mut SpillArea,
    mut runs: Vec<RunFile>,
    fan_in: usize,
) -> Result<(Vec<RunFile>, u64), String> {
    let fan_in = fan_in.max(2);
    let mut passes = 0u64;
    while runs.len() > fan_in {
        // Merge the oldest `fan_in` runs into one new run appended at the
        // back; repeat until a single final merge is possible.
        let group: Vec<RunFile> = runs.drain(..fan_in.min(runs.len())).collect();
        let readers = group
            .iter()
            .map(RunFile::open)
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = area.start_run()?;
        merge_streams(readers, |rec| out.push(&rec))?;
        runs.push(out.finish()?);
        for consumed in group {
            consumed.remove();
        }
        passes += 1;
    }
    Ok((runs, passes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn rec(key: &[u8], seq: u64) -> Rec {
        Rec {
            key: key.to_vec(),
            seq,
            raw: format!("raw-{seq}").into_bytes(),
        }
    }

    fn temp_base(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sortyard-merge-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn collect(readers: Vec<RunReader>) -> Vec<Rec> {
        let mut out = Vec::new();
        merge_streams(readers, |r| {
            out.push(r);
            Ok(())
        })
        .unwrap();
        out
    }

    #[test]
    fn merges_two_runs_into_global_order() {
        let base = temp_base("two");
        let mut area = SpillArea::create(&base).unwrap();
        let r1 = area.write_run(&[rec(b"a", 0), rec(b"c", 2)]).unwrap();
        let r2 = area.write_run(&[rec(b"b", 1), rec(b"d", 3)]).unwrap();
        let merged = collect(vec![r1.open().unwrap(), r2.open().unwrap()]);
        let keys: Vec<&[u8]> = merged.iter().map(|r| r.key.as_slice()).collect();
        assert_eq!(keys, vec![b"a" as &[u8], b"b", b"c", b"d"]);
        drop(area);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn equal_keys_come_out_in_input_order() {
        // Stability across runs: seq must break ties even when the equal
        // records live in different files.
        let base = temp_base("stable");
        let mut area = SpillArea::create(&base).unwrap();
        let r1 = area.write_run(&[rec(b"k", 5)]).unwrap();
        let r2 = area.write_run(&[rec(b"k", 2), rec(b"k", 9)]).unwrap();
        let merged = collect(vec![r1.open().unwrap(), r2.open().unwrap()]);
        let seqs: Vec<u64> = merged.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, vec![2, 5, 9]);
        drop(area);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn empty_and_uneven_runs_are_fine() {
        let base = temp_base("uneven");
        let mut area = SpillArea::create(&base).unwrap();
        let r1 = area.write_run(&[]).unwrap();
        let r2 = area.write_run(&[rec(b"x", 1)]).unwrap();
        let merged = collect(vec![r1.open().unwrap(), r2.open().unwrap()]);
        assert_eq!(merged.len(), 1);
        drop(area);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn reduce_runs_respects_fan_in_and_counts_passes() {
        let base = temp_base("reduce");
        let mut area = SpillArea::create(&base).unwrap();
        // Seven single-record runs with fan-in 2 need intermediate passes.
        let runs: Vec<RunFile> = (0..7)
            .map(|i| {
                area.write_run(&[rec(format!("k{i}").as_bytes(), i)])
                    .unwrap()
            })
            .collect();
        let (survivors, passes) = reduce_runs(&mut area, runs, 2).unwrap();
        assert!(survivors.len() <= 2, "got {} survivors", survivors.len());
        assert!(
            passes >= 3,
            "7 runs at fan-in 2 need several passes, got {passes}"
        );
        // The surviving runs still contain all 7 records, in order.
        let readers = survivors
            .iter()
            .map(RunFile::open)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let merged = collect(readers);
        assert_eq!(merged.len(), 7);
        let seqs: Vec<u64> = merged.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, vec![0, 1, 2, 3, 4, 5, 6]);
        drop(area);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn reduce_runs_is_a_no_op_when_under_the_limit() {
        let base = temp_base("noop");
        let mut area = SpillArea::create(&base).unwrap();
        let runs = vec![
            area.write_run(&[rec(b"a", 0)]).unwrap(),
            area.write_run(&[rec(b"b", 1)]).unwrap(),
        ];
        let (survivors, passes) = reduce_runs(&mut area, runs, 16).unwrap();
        assert_eq!(survivors.len(), 2);
        assert_eq!(passes, 0);
        drop(area);
        let _ = fs::remove_dir_all(&base);
    }
}
