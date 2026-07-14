#!/usr/bin/env bash
# Smoke test: builds sortyard, then drives the real CLI end to end — a
# quoted CSV big enough to force spills and multi-pass merging, a JSONL
# export with nested date keys, check mode, unique, and error handling.
# Self-contained: temp dirs only, no network, deterministic data.
set -euo pipefail

cd "$(dirname "$0")/.."

fail() { echo "SMOKE FAIL: $*" >&2; exit 1; }

echo "[smoke] building..."
cargo build --quiet
BIN=target/debug/sortyard

WORK=$(mktemp -d "${TMPDIR:-/tmp}/sortyard-smoke.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

# --- 1. version/help sanity ---------------------------------------------------
"$BIN" --version | grep -q '^sortyard 0\.1\.0$' || fail "--version mismatch"
"$BIN" --help | grep -q 'USAGE:' || fail "--help missing usage"

# --- 2. generate a deterministic CSV with hostile quoting ---------------------
# 20000 rows, ~1.2 MB: prices walk a fixed pseudo-random sequence, and every
# row hides a comma inside a quoted field. Row 137 also embeds a newline.
echo "[smoke] generating ${WORK}/big.csv"
awk 'BEGIN {
  print "id,item,price"
  for (i = 0; i < 20000; i++) {
    price = (i * 7919) % 9973
    if (i == 137)
      printf "row-%05d,\"two\nlines, quoted\",%d.%02d\n", i, price, i % 100
    else
      printf "row-%05d,\"item, nr %d\",%d.%02d\n", i, i, price, i % 100
  }
}' > "$WORK/big.csv"

# --- 3. external sort: tiny --mem forces spills, --fan-in 2 forces passes -----
echo "[smoke] sortyard -k price:num:desc -k id --mem 64K --fan-in 2 --stats"
"$BIN" -k price:num:desc -k id --mem 64K --fan-in 2 --stats \
  --tmp "$WORK" "$WORK/big.csv" -o "$WORK/sorted.csv" 2> "$WORK/stats.err"
grep -q 'records read:    20000' "$WORK/stats.err" || fail "stats missing record count"
RUNS=$(sed -n 's/^sortyard: spilled runs: *//p' "$WORK/stats.err")
PASSES=$(sed -n 's/^sortyard: merge passes: *//p' "$WORK/stats.err")
[ "${RUNS:-0}" -gt 4 ] || fail "expected many spilled runs, got '$RUNS'"
[ "${PASSES:-0}" -gt 1 ] || fail "expected multi-pass merge, got '$PASSES'"
echo "[smoke] spilled $RUNS runs, merged in $PASSES passes"

# The sorted file must contain the same records (header + 20000, one spans
# two physical lines) and keep the quoted record byte-identical.
LINES=$(wc -l < "$WORK/sorted.csv")
[ "$LINES" -eq 20002 ] || fail "sorted.csv has $LINES lines, want 20002"
grep -q 'lines, quoted",7819.37$' "$WORK/sorted.csv" || fail "quoted record mangled"
head -n 1 "$WORK/sorted.csv" | grep -q '^id,item,price$' || fail "header not on top"

# sortyard's own verifier must accept the output and reject the input.
"$BIN" --check -k price:num:desc -k id "$WORK/sorted.csv" || fail "check rejected sorted output"
if "$BIN" --check -k price:num:desc -k id "$WORK/big.csv" 2> "$WORK/chk.err"; then
  fail "check accepted unsorted input"
fi
grep -q 'disorder at record' "$WORK/chk.err" || fail "check did not pinpoint disorder"
echo "[smoke] --check: output sorted, input correctly rejected"

# The spill directory must be gone.
if ls "$WORK"/sortyard-* >/dev/null 2>&1; then fail "spill dir left behind"; fi

# --- 4. JSONL from stdin: nested date key, auto-detected format ---------------
echo "[smoke] jsonl nested-date sort via stdin"
printf '%s\n' \
  '{"id": "late",  "meta": {"ts": "2026-07-02T09:00:00+09:00"}}' \
  '{"id": "early", "meta": {"ts": "2026-07-01T20:00:00Z"}}' \
  '{"id": "mid",   "meta": {"ts": "2026-07-01T21:00:00-02:30"}}' \
  | "$BIN" -k meta.ts:date > "$WORK/events.out"
FIRST=$(head -n 1 "$WORK/events.out")
case "$FIRST" in *'"early"'*) : ;; *) fail "jsonl date order wrong: $FIRST" ;; esac
tail -n 1 "$WORK/events.out" | grep -q '"late"' || fail "jsonl date order wrong at tail"

# --- 5. unique + missing placement on the shipped example ---------------------
echo "[smoke] examples/orders.csv: --missing first, --unique"
"$BIN" -k ordered_at:date --missing first examples/orders.csv > "$WORK/orders.out"
sed -n 2p "$WORK/orders.out" | grep -q 'A-1004' || fail "missing date not first"
UNIQ=$("$BIN" -k qty:num -u examples/orders.csv | wc -l)
[ "$UNIQ" -eq 8 ] || fail "unique changed the record count unexpectedly"

# --- 6. errors: bad data exits 2 with a location; --lenient recovers ----------
printf 'id,price\nok,1\nbad,cheap\n' > "$WORK/bad.csv"
if "$BIN" -k price:num "$WORK/bad.csv" > /dev/null 2> "$WORK/bad.err"; then
  fail "unparseable number accepted"
fi
grep -q "record 2 (line 3).*field 'price'" "$WORK/bad.err" || fail "error lacks location"
"$BIN" -k price:num --lenient "$WORK/bad.csv" > /dev/null || fail "--lenient still failed"

echo "SMOKE OK"
