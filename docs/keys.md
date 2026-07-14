# Key specs and sort semantics

This is the reference for `--key` (`-k`) and the ordering rules the engine
guarantees. The short form is `field[:type][:flag...]` — everything after
the first `:` is a modifier, in any order.

## Field selectors

| Input format | Selector | Meaning |
|---|---|---|
| CSV (default) | `price` | Column whose header is exactly `price` |
| CSV | `3` | 1-based column number, if no header is named `3` |
| CSV `--no-header` | `3` | 1-based column number (names are not available) |
| JSONL | `user.address.city` | Dot path through nested objects |
| JSONL | `items.0.sku` | Array hops use 0-based decimal segments |

Notes:

- Exact header names always win over the numeric interpretation, so a CSV
  with a header literally called `2` still resolves by name.
- On arrays, a path segment must be a decimal index; on objects it is always
  a key, even if it looks numeric.
- A field selector cannot contain `:` (it starts the modifier list) and dots
  inside JSON keys cannot be escaped in 0.1.0 — see the roadmap.

## Types

| Type | Accepts | Order |
|---|---|---|
| `str` (default) | any text | byte-wise lexicographic; `ci` folds ASCII case |
| `num` | float syntax incl. `1e3`, `-0.5`, `inf` | numeric; NaN is rejected |
| `date` | ISO 8601 (`2026-07-01`, `2026-07-01T14:30:05.25+09:00`, `/` dates) | chronological, offsets normalized to UTC |

JSONL values convert naturally: JSON numbers under a `num` key are used
directly; under a `date` key they are Unix epoch **seconds**; under a `str`
key scalars render the way JSON writes them (`7`, `true`). Type mismatches
(an array under a `num` key, `"twelve"` under `num`) are hard errors naming
the record — or missing values with `--lenient`.

## Missing values

A key part is *missing* when the CSV column is absent or empty, or the JSON
path is absent or `null`. Missing parts sort after all present values by
default; `--missing first` pins them before. This placement is absolute: a
per-key `desc` flag reverses value order but never moves missing records.
`--reverse` flips the entire output including missing placement, like
reading the file backwards.

## Multi-key ordering and stability

Keys compare in the order given: `-k user.plan -k ts:date` orders by plan,
then chronologically inside each plan. Records that compare equal on every
key keep their input order (the sort is stable), which also defines
`--unique`: the *first* record of each distinct key survives.

## How keys are compared internally

Each record's key is parsed once and normalized into an order-preserving
byte string (numbers become sign-corrected big-endian bits, dates become
epoch milliseconds, strings get a terminator that sorts prefixes first).
After that, every comparison — the in-memory sort, the spilled runs, the
k-way merge, `--check` — is a plain byte compare. That is why the spill
plan (`--mem`, `--fan-in`) can never change the output, a property the test
suite asserts directly.
