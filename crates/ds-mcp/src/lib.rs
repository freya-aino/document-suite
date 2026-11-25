//! `ds-mcp`: the suite's tools, as a Model Context Protocol server.
//!
//! Its own service and its own process, talking to `ds-suite` over HTTP like any other
//! client (`SPEC.md` has the reasoning). It links none of the suite's internals: no
//! database, no object store, no model registry, no migrations — so it cannot be the thing
//! that breaks them, and a deployment can run it in a different place, at a different
//! scale, or not at all.
//!
//! Two transports, one protocol implementation:
//!
//! ```text
//! ds-mcp stdio                       # one process per user, started by their client
//! ds-mcp http --addr 127.0.0.1:8081  # a service, with the caller's token per request
//! ```
//!
//! A library as well as a binary, so `tests/` can drive the protocol and the tools against
//! a suite served on a real socket — including the real `ds_api::router` — rather than
//! against a hand-rolled imitation of this crate's insides.

// The HTTP guards return a `Response` in the error position, so a refusal can carry its
// own status and headers — the same shape `ds-api` uses, and for the same reason.
#![allow(clippy::result_large_err)]

pub mod http;
pub mod jsonrpc;
pub mod server;
pub mod stdio;
pub mod suite;
pub mod tools;
