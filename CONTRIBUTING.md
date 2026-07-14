# Contributing to sortyard

Thanks for your interest in improving sortyard. Issues, discussions and pull requests are all welcome.

## Getting started

Prerequisites: Rust 1.75 or newer (stable toolchain).

```bash
git clone https://github.com/JaydenCJ/sortyard.git
cd sortyard
cargo build
cargo test
bash scripts/smoke.sh
```

`scripts/smoke.sh` generates a CSV with quoted commas/newlines and a JSONL export, sorts both with a deliberately tiny `--mem` so runs spill and merge in multiple passes, and verifies the results with `--check` and byte-comparisons. It finishes in well under a minute and must print `SMOKE OK`.

## Before you open a pull request

1. `cargo fmt` — formatting is enforced.
2. `cargo clippy --all-targets -- -D warnings` — clippy must be clean.
3. `cargo test` — unit tests and the CLI integration tests must pass.
4. `bash scripts/smoke.sh` — the smoke test must print `SMOKE OK`.
5. Add tests for behavior changes. Parsing, key encoding and merging live in pure modules (`csv`, `json`, `datetime`, `value`, `keyspec`, `merge`) that are easy to unit-test; please keep it that way.

## Ground rules

- Keep dependencies at zero. sortyard is std-only; adding a dependency needs a very strong justification in the PR description.
- No network calls, no telemetry, ever. Sorting reads the input, writes the output and touches its own spill directory — nothing else.
- Code comments and doc comments are written in English.
- Correctness invariants first: the sort must stay stable, records must be emitted verbatim, and the spill plan (`--mem`, `--fan-in`) must never change the output — only how it is produced. Tests guard all three; new features must not weaken them.

## Reporting bugs

Please include the `sortyard --version` output, the exact command line, the `--stats` output if the run completed, and a minimal input that reproduces the problem (a handful of records is usually enough — quoting and type edge cases matter more than volume).

## Security

If you find a security issue (e.g. a parsing crash on hostile input), please do not open a public issue. Use GitHub's private vulnerability reporting on this repository instead.
