# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-07-13

### Added

- External merge sort engine: records buffer up to `--mem`, spill as sorted runs, and k-way merge with bounded `--fan-in` (intermediate passes when the run count exceeds it); a pure in-memory fast path when everything fits.
- Typed multi-column keys via `--key field[:type][:flag...]`: `str` (default), `num` (full float line, `-inf` to `+inf`, NaN rejected), `date` (offline ISO 8601 parsing with offsets, to UTC epoch milliseconds); flags `desc`, `asc`, `ci`.
- Order-preserving key encoding: every key is normalized once into bytes whose `memcmp` order equals the semantic order, so sort, spill and merge never re-parse a record.
- Quote-safe CSV: RFC 4180 reader where records may span lines (quoted newlines, `""` escapes, embedded delimiters); records are re-emitted verbatim, header stays on top; tolerant of stray quotes; custom delimiters via `-d`, `.tsv` defaults to tab, `--no-header` for column-number keys.
- JSONL keys as dot paths (`user.address.city`, `items.0.sku`) backed by a std-only strict JSON parser with surrogate-pair escapes and a nesting-depth guard.
- Missing-value policy: absent columns, empty CSV fields and JSON `null` sort `--missing first|last` (default last), unaffected by per-key `desc`; `--lenient` downgrades unparseable values to missing.
- Stable sort guaranteed by input-sequence tie-breaking, `--unique` (keep first record per key), `--reverse` (flip the total order), `--check` (verify order, GNU-sort-style exit codes 0/1/2), and `--stats` counters (records, spilled runs, merge passes).
- Format auto-detection by extension then first content byte, stdin/stdout streaming, `-o` output files, and spill directories that clean themselves up on success and on error.
- Test suite: 80 unit tests, 10 CLI integration tests (including a spill-and-multipass-merge equivalence check), and `scripts/smoke.sh`.

[0.1.0]: https://github.com/JaydenCJ/sortyard/releases/tag/v0.1.0
