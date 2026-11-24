//! `vs-mcp`: the suite's tools, as a Model Context Protocol server.
//!
//! Its own service and its own process, talking to `vs-suite` over HTTP like any other
//! client (`SPEC.md` has the reasoning). It links none of the suite's internals: no
//! database, no object store, no model registry, no migrations — so it cannot be the thing
//! that breaks them, and a deployment can run it in a different place, at a different
//! scale, or not at all.
//!
//! A library as well as a binary, so `tests/` can drive the protocol and the tools against
//! a suite served on a real socket — including the real `vs_api::router` — rather than
//! against a hand-rolled imitation of this crate's insides.

pub mod jsonrpc;
pub mod suite;
pub mod tools;
