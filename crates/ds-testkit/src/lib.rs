//! Test doubles shared by the suite's tests and by the integration stack.
//!
//! This crate exists because the fake model tier had been written **three times** — in
//! `ds-suite`'s pipeline test, in `ds-adapt`'s chain test and in `ds-models`' own test
//! helpers — each with a different idea of what a model answers, and none reachable from
//! another crate. A fake that disagrees with itself across three files is a fake that
//! can make two tests pass for contradictory reasons.
//!
//! It is also the server the `fake` tier of `deploy/compose` runs, so the model tier a
//! pull request exercises is the same code the unit tests exercise.
//!
//! It deliberately has **no `ds-*` dependency**: it speaks the OpenAI-compatible wire
//! format and knows nothing of the suite's own types. That is what lets every crate
//! dev-depend on it, and it is also the point — a fake built from the suite's structs
//! would agree with the suite by construction and prove nothing about the parsers.

pub mod models;
