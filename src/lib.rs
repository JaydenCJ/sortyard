//! sortyard — external merge sort for CSV/JSONL files bigger than RAM,
//! with typed multi-column keys (numbers, dates, nested JSON paths).
//!
//! The library crate exposes the parsing, key-encoding and merge layers so
//! they can be unit-tested and reused; the `sortyard` binary wires them
//! into a CLI (see `src/main.rs`).

pub mod cli;
pub mod csv;
pub mod datetime;
pub mod extract;
pub mod json;
pub mod keyspec;
pub mod merge;
pub mod sorter;
pub mod spill;
pub mod value;
