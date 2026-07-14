# sortyard examples

Two small fixtures that exercise exactly the cases byte-oriented tools get
wrong:

- `orders.csv` — quoted fields with embedded commas and doubled quotes,
  mixed-magnitude prices, ISO timestamps with different UTC offsets, and one
  record with a missing date.
- `events.jsonl` — nested objects (`user.plan`, `metrics.latency_ms`),
  timezone-offset timestamps, and one record where the key is absent.

Run everything from the repository root:

```bash
cargo build --quiet
alias sortyard=target/debug/sortyard

# Most expensive order first — 9.5 must not beat 80
sortyard -k price:num:desc examples/orders.csv

# Chronological, offsets normalized; the record without a date on top
sortyard -k ordered_at:date --missing first examples/orders.csv

# JSONL: group by plan, slowest request first inside each plan
sortyard -k user.plan -k metrics.latency_ms:num:desc examples/events.jsonl

# Verify a sort you just produced (exit 0 = sorted, 1 = disorder)
sortyard -k ts:date examples/events.jsonl -o /tmp/by-ts.jsonl
sortyard --check -k ts:date /tmp/by-ts.jsonl && echo sorted

# Force the external path on a tiny file, just to watch it work
sortyard -k price:num --mem 256 --fan-in 2 --stats examples/orders.csv >/dev/null
```

The quoted records come back byte-for-byte identical — only their position
changes. Try the first command with `sort -t, -k3 -n` to see the difference:
GNU sort splits `"Hex bolt, M8"` at the embedded comma and sorts the wrong
column.
