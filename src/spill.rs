//! On-disk sorted runs: length-prefixed binary records in a private
//! temporary directory.
//!
//! When the in-memory buffer reaches the `--mem` cap, it is sorted and
//! written out as a *run*. Each spilled record carries its already-encoded
//! key, so merging never re-parses CSV or JSON. The spill area is created
//! under the chosen temp directory and removed on drop, including on error
//! paths.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

/// One record flowing through the sorter: the encoded key, the input
/// sequence number (tie-breaker that makes the sort stable), and the raw
/// record bytes that will be emitted verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rec {
    pub key: Vec<u8>,
    pub seq: u64,
    pub raw: Vec<u8>,
}

impl Rec {
    /// Approximate heap footprint, used for the `--mem` accounting. The
    /// constant covers the three vector headers plus allocator slack.
    pub fn cost(&self) -> u64 {
        (self.key.len() + self.raw.len()) as u64 + 64
    }

    /// The one total order used everywhere: encoded key bytes, then input
    /// order. Distinct records never compare equal because `seq` is unique.
    pub fn cmp_order(&self, other: &Rec) -> std::cmp::Ordering {
        self.key.cmp(&other.key).then(self.seq.cmp(&other.seq))
    }
}

/// A private directory holding run files; deleted on drop.
pub struct SpillArea {
    dir: PathBuf,
    next_id: u64,
}

impl SpillArea {
    /// Create `sortyard-<pid>-<n>` under `base`, retrying on the unlikely
    /// name collision.
    pub fn create(base: &Path) -> Result<SpillArea, String> {
        let pid = std::process::id();
        for attempt in 0..64 {
            let dir = base.join(format!("sortyard-{pid}-{attempt}"));
            match fs::create_dir(&dir) {
                Ok(()) => return Ok(SpillArea { dir, next_id: 0 }),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("cannot create spill dir {}: {e}", dir.display())),
            }
        }
        Err(format!(
            "cannot create a spill directory under {}",
            base.display()
        ))
    }

    /// Write one sorted run. `records` must already be in `cmp_order`.
    pub fn write_run(&mut self, records: &[Rec]) -> Result<RunFile, String> {
        let path = self.dir.join(format!("run-{:06}.bin", self.next_id));
        self.next_id += 1;
        let file = File::create(&path)
            .map_err(|e| format!("cannot create run file {}: {e}", path.display()))?;
        let mut w = BufWriter::new(file);
        for rec in records {
            write_rec(&mut w, rec).map_err(|e| format!("cannot write run: {e}"))?;
        }
        w.flush().map_err(|e| format!("cannot write run: {e}"))?;
        Ok(RunFile { path })
    }

    /// Start a new run written incrementally (used by intermediate merge
    /// passes, where the merged output is itself a run).
    pub fn start_run(&mut self) -> Result<RunWriter, String> {
        let path = self.dir.join(format!("run-{:06}.bin", self.next_id));
        self.next_id += 1;
        let file = File::create(&path)
            .map_err(|e| format!("cannot create run file {}: {e}", path.display()))?;
        Ok(RunWriter {
            w: BufWriter::new(file),
            path,
        })
    }
}

impl Drop for SpillArea {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A finished run on disk.
pub struct RunFile {
    path: PathBuf,
}

impl RunFile {
    pub fn open(&self) -> Result<RunReader, String> {
        let file = File::open(&self.path)
            .map_err(|e| format!("cannot open run file {}: {e}", self.path.display()))?;
        Ok(RunReader {
            r: BufReader::new(file),
        })
    }

    /// Delete the backing file once a merge has consumed it, so peak disk
    /// usage stays near 2x input instead of growing with every pass.
    pub fn remove(self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Incremental writer for a run produced by an intermediate merge.
pub struct RunWriter {
    w: BufWriter<File>,
    path: PathBuf,
}

impl RunWriter {
    pub fn push(&mut self, rec: &Rec) -> Result<(), String> {
        write_rec(&mut self.w, rec).map_err(|e| format!("cannot write run: {e}"))
    }

    pub fn finish(mut self) -> Result<RunFile, String> {
        self.w
            .flush()
            .map_err(|e| format!("cannot write run: {e}"))?;
        Ok(RunFile { path: self.path })
    }
}

/// Streaming reader over one run, in the order it was written.
pub struct RunReader {
    r: BufReader<File>,
}

impl RunReader {
    pub fn next_rec(&mut self) -> Result<Option<Rec>, String> {
        // The key-length prefix doubles as the EOF probe: zero bytes here is
        // a clean end, anything partial is corruption worth reporting.
        let mut len4 = [0u8; 4];
        match read_exact_or_eof(&mut self.r, &mut len4) {
            Ok(false) => return Ok(None),
            Ok(true) => {}
            Err(e) => return Err(e),
        }
        let key_len = u32::from_le_bytes(len4) as usize;
        let mut head = [0u8; 12];
        self.r
            .read_exact(&mut head)
            .map_err(|_| "corrupt run file: truncated record header".to_string())?;
        let seq = u64::from_le_bytes(head[..8].try_into().unwrap());
        let raw_len = u32::from_le_bytes(head[8..].try_into().unwrap()) as usize;
        let mut key = vec![0u8; key_len];
        self.r
            .read_exact(&mut key)
            .map_err(|_| "corrupt run file: truncated key".to_string())?;
        let mut raw = vec![0u8; raw_len];
        self.r
            .read_exact(&mut raw)
            .map_err(|_| "corrupt run file: truncated record".to_string())?;
        Ok(Some(Rec { key, seq, raw }))
    }
}

fn write_rec(w: &mut BufWriter<File>, rec: &Rec) -> std::io::Result<()> {
    w.write_all(&(rec.key.len() as u32).to_le_bytes())?;
    w.write_all(&rec.seq.to_le_bytes())?;
    w.write_all(&(rec.raw.len() as u32).to_le_bytes())?;
    w.write_all(&rec.key)?;
    w.write_all(&rec.raw)
}

/// Fill `buf` completely, or report a clean EOF (`false`) if not even one
/// byte was available.
fn read_exact_or_eof(r: &mut BufReader<File>, buf: &mut [u8]) -> Result<bool, String> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => return Err("corrupt run file: truncated length prefix".to_string()),
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("cannot read run file: {e}")),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(key: &[u8], seq: u64, raw: &[u8]) -> Rec {
        Rec {
            key: key.to_vec(),
            seq,
            raw: raw.to_vec(),
        }
    }

    fn temp_base(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sortyard-spill-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn roundtrip_preserves_keys_seqs_and_raw_bytes() {
        let base = temp_base("roundtrip");
        let mut area = SpillArea::create(&base).unwrap();
        let records = vec![
            rec(b"a", 0, b"first,row"),
            rec(b"b", 1, b"quoted \"bytes\"\nwith newline"),
            rec(b"", 2, b""),
        ];
        let run = area.write_run(&records).unwrap();
        let mut reader = run.open().unwrap();
        for expected in &records {
            assert_eq!(reader.next_rec().unwrap().as_ref(), Some(expected));
        }
        assert!(
            reader.next_rec().unwrap().is_none(),
            "clean EOF after last record"
        );
        drop(area);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn incremental_writer_matches_batch_writer() {
        let base = temp_base("incremental");
        let mut area = SpillArea::create(&base).unwrap();
        let mut w = area.start_run().unwrap();
        w.push(&rec(b"k1", 7, b"data")).unwrap();
        w.push(&rec(b"k2", 8, b"more")).unwrap();
        let run = w.finish().unwrap();
        let mut r = run.open().unwrap();
        assert_eq!(r.next_rec().unwrap().unwrap().seq, 7);
        assert_eq!(r.next_rec().unwrap().unwrap().raw, b"more");
        assert!(r.next_rec().unwrap().is_none());
        drop(area);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn spill_area_cleans_up_on_drop() {
        let base = temp_base("cleanup");
        let mut area = SpillArea::create(&base).unwrap();
        area.write_run(&[rec(b"x", 0, b"y")]).unwrap();
        drop(area);
        let leftovers: Vec<_> = fs::read_dir(&base).unwrap().collect();
        assert!(leftovers.is_empty(), "spill dir must be removed on drop");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn truncated_run_is_reported_not_silently_shortened() {
        let base = temp_base("truncated");
        let mut area = SpillArea::create(&base).unwrap();
        let run = area.write_run(&[rec(b"key", 1, b"raw-bytes")]).unwrap();
        // Chop the tail off the only run file.
        let entry = fs::read_dir(base.join(format!("sortyard-{}-0", std::process::id())))
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let full = fs::read(entry.path()).unwrap();
        fs::write(entry.path(), &full[..full.len() - 4]).unwrap();
        let mut r = run.open().unwrap();
        let e = r.next_rec().unwrap_err();
        assert!(e.contains("corrupt run file"), "got: {e}");
        drop(area);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn rec_order_breaks_key_ties_by_input_sequence() {
        let a = rec(b"same", 1, b"first");
        let b = rec(b"same", 2, b"second");
        assert_eq!(a.cmp_order(&b), std::cmp::Ordering::Less);
        assert_eq!(b.cmp_order(&a), std::cmp::Ordering::Greater);
        let c = rec(b"other", 9, b"x");
        assert_eq!(
            c.cmp_order(&a),
            std::cmp::Ordering::Less,
            "key wins over seq"
        );
    }
}
