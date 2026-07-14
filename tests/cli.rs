//! End-to-end tests that exercise the compiled `sortyard` binary: typed CSV
//! and JSONL sorts, verbatim quoting, spill-to-disk with bounded fan-in,
//! check mode exit codes, and error reporting. Every test builds its own
//! fixtures under a temporary directory — offline, deterministic, no shared
//! state.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_sortyard")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("failed to run sortyard binary")
}

fn run_stdin(args: &[&str], input: &str) -> Output {
    let mut child = Command::new(bin())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn sortyard binary");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child
        .wait_with_output()
        .expect("failed to wait for sortyard")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn tempdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sortyard-cli-test-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, content).unwrap();
    path
}

#[test]
fn version_and_help_look_right() {
    let v = run(&["--version"]);
    assert!(v.status.success());
    assert_eq!(
        stdout(&v).trim(),
        format!("sortyard {}", env!("CARGO_PKG_VERSION"))
    );
    let h = run(&["--help"]);
    assert!(h.status.success());
    let text = stdout(&h);
    assert!(text.contains("USAGE:"), "help must show usage");
    assert!(text.contains("--fan-in"), "help must document options");
    assert!(text.contains("EXAMPLES:"), "help must show examples");
}

#[test]
fn csv_sorts_by_typed_key_descending_into_a_file() {
    let dir = tempdir("csv-desc");
    let input = write(
        &dir,
        "orders.csv",
        "id,item,price\nA-3,bolt,9.5\nA-1,girder,120\nA-2,plate,80\n",
    );
    let out_path = dir.join("sorted.csv");
    let out = run(&[
        "-k",
        "price:num:desc",
        input.to_str().unwrap(),
        "-o",
        out_path.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let sorted = fs::read_to_string(&out_path).unwrap();
    assert_eq!(
        sorted, "id,item,price\nA-1,girder,120\nA-2,plate,80\nA-3,bolt,9.5\n",
        "numeric desc order with the header kept on top"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn quoted_csv_records_are_emitted_verbatim() {
    // Embedded commas, doubled quotes and a quoted newline: the record must
    // come out byte-for-byte identical, just in a new position.
    let dir = tempdir("quoted");
    let input = write(
        &dir,
        "notes.csv",
        "id,note\n2,\"multi\nline, note\"\n3,\"say \"\"hi\"\"\"\n1,plain\n",
    );
    let out = run(&["-k", "id:num", input.to_str().unwrap()]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "id,note\n1,plain\n2,\"multi\nline, note\"\n3,\"say \"\"hi\"\"\"\n"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn jsonl_from_stdin_is_autodetected_and_sorted_by_nested_date() {
    let input = concat!(
        "{\"id\": \"c\", \"meta\": {\"ts\": \"2026-03-01T00:00:00Z\"}}\n",
        "{\"id\": \"a\", \"meta\": {\"ts\": \"2025-11-05T09:00:00+09:00\"}}\n",
        "{\"id\": \"b\", \"meta\": {\"ts\": \"2026-01-20\"}}\n",
    );
    let out = run_stdin(&["-k", "meta.ts:date"], input);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let ids: Vec<String> = stdout(&out).lines().map(|l| l[8..9].to_string()).collect();
    assert_eq!(ids, vec!["a", "b", "c"], "chronological across offsets");
}

#[test]
fn spilled_multipass_sort_matches_in_memory_and_passes_check() {
    // ~3000 records, a few hundred KB: with --mem 16K this needs dozens of
    // runs, and --fan-in 2 forces multi-pass merging. The plan must never
    // change the answer.
    let dir = tempdir("spill");
    let mut body = String::from("id,price,label\n");
    for i in 0..3000u64 {
        let price = (i * 7919) % 2503; // fixed pseudo-random walk
        body.push_str(&format!("row-{i:04},{price},\"item, {i}\"\n"));
    }
    let input = write(&dir, "big.csv", &body);
    let fast = dir.join("fast.csv");
    let spilled = dir.join("spilled.csv");
    let a = run(&[
        "-k",
        "price:num",
        "-k",
        "id",
        input.to_str().unwrap(),
        "-o",
        fast.to_str().unwrap(),
    ]);
    assert!(a.status.success(), "stderr: {}", stderr(&a));
    let b = run(&[
        "-k",
        "price:num",
        "-k",
        "id",
        "--mem",
        "16K",
        "--fan-in",
        "2",
        "--stats",
        "--tmp",
        dir.to_str().unwrap(),
        input.to_str().unwrap(),
        "-o",
        spilled.to_str().unwrap(),
    ]);
    assert!(b.status.success(), "stderr: {}", stderr(&b));
    assert_eq!(
        fs::read(&fast).unwrap(),
        fs::read(&spilled).unwrap(),
        "spill plan changed the output"
    );
    let stats = stderr(&b);
    assert!(stats.contains("records read:    3000"), "got: {stats}");
    let runs: u64 = stats
        .lines()
        .find_map(|l| l.strip_prefix("sortyard: spilled runs:"))
        .and_then(|v| v.trim().parse().ok())
        .expect("stats must report spilled runs");
    assert!(runs > 2, "16K buffer must spill many runs, got {runs}");
    let passes: u64 = stats
        .lines()
        .find_map(|l| l.strip_prefix("sortyard: merge passes:"))
        .and_then(|v| v.trim().parse().ok())
        .expect("stats must report merge passes");
    assert!(passes > 1, "fan-in 2 must force extra passes, got {passes}");
    // The tool's own verifier agrees with the sort it produced.
    let chk = run(&[
        "--check",
        "-k",
        "price:num",
        "-k",
        "id",
        spilled.to_str().unwrap(),
    ]);
    assert!(chk.status.success(), "check failed: {}", stderr(&chk));
    // No spill litter left behind.
    let leftovers: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("sortyard-")
        })
        .collect();
    assert!(leftovers.is_empty(), "spill dirs must be cleaned up");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn check_mode_exit_codes_follow_gnu_sort() {
    let dir = tempdir("check");
    let sorted = write(&dir, "sorted.csv", "id,price\na,1\nb,2\nc,30\n");
    let ok = run(&["--check", "-k", "price:num", sorted.to_str().unwrap()]);
    assert_eq!(ok.status.code(), Some(0), "stderr: {}", stderr(&ok));
    let unsorted = write(&dir, "unsorted.csv", "id,price\na,5\nb,2\nc,9\n");
    let bad = run(&["--check", "-k", "price:num", unsorted.to_str().unwrap()]);
    assert_eq!(bad.status.code(), Some(1));
    let msg = stderr(&bad);
    assert!(msg.contains("disorder at record 2 (line 3)"), "got: {msg}");
    // Usage errors are exit 2, distinct from disorder.
    let usage = run(&["--check", "-o", "x.csv", sorted.to_str().unwrap()]);
    assert_eq!(usage.status.code(), Some(2));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn unique_reverse_and_missing_flags_compose() {
    let input = "sku,qty\nbeta,1\nalpha,2\nbeta,3\n,9\n";
    let uniq = run_stdin(&["-f", "csv", "-k", "sku", "--unique"], input);
    assert!(uniq.status.success(), "stderr: {}", stderr(&uniq));
    assert_eq!(
        stdout(&uniq),
        "sku,qty\nalpha,2\nbeta,1\n,9\n",
        "first record per key wins; missing key sorts last"
    );
    let rev = run_stdin(&["-f", "csv", "-k", "sku", "--unique", "--reverse"], input);
    assert_eq!(
        stdout(&rev),
        "sku,qty\n,9\nbeta,1\nalpha,2\n",
        "--reverse flips the whole order, missing placement included"
    );
    let first = run_stdin(
        &["-f", "csv", "-k", "sku", "--unique", "--missing", "first"],
        input,
    );
    assert_eq!(stdout(&first), "sku,qty\n,9\nalpha,2\nbeta,1\n");
}

#[test]
fn data_errors_exit_2_with_location_unless_lenient() {
    let dir = tempdir("errors");
    let input = write(&dir, "bad.csv", "id,price\nok,1\noops,cheap\n");
    let out = run(&["-k", "price:num", input.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let msg = stderr(&out);
    assert!(msg.contains("record 2 (line 3)"), "got: {msg}");
    assert!(msg.contains("field 'price'"), "got: {msg}");
    let lenient = run(&["-k", "price:num", "--lenient", input.to_str().unwrap()]);
    assert_eq!(
        lenient.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&lenient)
    );
    assert_eq!(
        stdout(&lenient),
        "id,price\nok,1\noops,cheap\n",
        "lenient: the unparseable record sorts as missing (last)"
    );
    // Unknown key fields also fail fast, before any sorting.
    let unknown = run(&["-k", "cost:num", input.to_str().unwrap()]);
    assert_eq!(unknown.status.code(), Some(2));
    assert!(
        stderr(&unknown).contains("'cost' not found"),
        "got: {}",
        stderr(&unknown)
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn headerless_tsv_sorts_by_column_number() {
    let dir = tempdir("tsv");
    let input = write(&dir, "data.tsv", "gamma\t30\nalpha\t2\nbeta\t100\n");
    // .tsv implies the tab delimiter; --no-header switches keys to indices.
    let out = run(&["--no-header", "-k", "2:num", input.to_str().unwrap()]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "alpha\t2\ngamma\t30\nbeta\t100\n");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn jsonl_check_with_unique_reports_duplicates() {
    let sorted_with_dup = concat!(
        "{\"user\": {\"id\": 1}}\n",
        "{\"user\": {\"id\": 2}}\n",
        "{\"user\": {\"id\": 2}}\n",
    );
    let out = run_stdin(
        &["--check", "--unique", "-k", "user.id:num"],
        sorted_with_dup,
    );
    assert_eq!(out.status.code(), Some(1));
    let msg = stderr(&out);
    assert!(msg.contains("duplicate key at record 3"), "got: {msg}");
    // Without --unique the same stream passes.
    let ok = run_stdin(&["--check", "-k", "user.id:num"], sorted_with_dup);
    assert_eq!(ok.status.code(), Some(0), "stderr: {}", stderr(&ok));
}
