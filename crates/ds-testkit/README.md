# ds-testkit — the test doubles

One fake model tier, shared.

It exists because the fake had been written **three times** — in `ds-suite`'s pipeline
test, in `ds-adapt`'s chain test and in `ds-models`' own test helpers — each with a
different idea of what a model answers, and none reachable from another crate. A fake that
disagrees with itself across three files can make two tests pass for contradictory
reasons.

It is also the server the `fake` tier of [`deploy/compose`](../../deploy/compose/README.md)
runs, so the model tier a pull request exercises is the code the unit tests exercise.

## What it is not

A model. **The answers are written here**, so any fidelity number measured against this
crate is a measurement of this crate. It proves the suite is wired — upload, auth, ingest,
routing, the executor, both stores, read-back — and nothing about how well a page is
parsed. Extraction fidelity is measured against a real model tier.

## Two decisions worth knowing

**Dispatch is on the task, read from the prompt, not on the model id.** The id cannot
decide: one registry entry serves several tasks — `mineru-2.5` is declared for
`page_parse` *and* `layout` — so a parse request and a layout request arrive at the same
model with different jobs. Each of the suite's prompts opens with a distinctive sentence,
and that is what is matched. An unrecognised prompt is a 400 with the prompt's opening in
it, not a guess: a fake that silently answers the wrong shape sends the caller into the
Markdown fallback, which looks like a model problem for a long time.
`every_prompt_the_suite_ships_is_recognised` reads the real prompt files, so rewording one
fails here rather than three crates away.

**The answers are derived from the request** wherever a real one would be. Extraction
quotes are copied out of the window the prompt showed, so grounding keeps the claim;
verification echoes back the ids it was given, so a verdict cannot verify an assertion
nobody emitted. Plausible constants would let the grounding and verification paths be
skipped entirely while every assertion still passed — which is what the two tests
`extraction_quotes_the_block_it_was_shown` and
`verification_answers_only_the_ids_it_was_given` hold in place.

## No `ds-*` dependency

Deliberate, and in both directions. `ds-models`, `ds-api`, `ds-suite` and `ds-adapt` all
dev-depend on this crate, so a dependency the other way would be a cycle. And a fake built
out of the suite's own structs would agree with the suite by construction, proving nothing
about the parsers — this one speaks the wire format and knows none of the suite's types.

## Using it

```rust
let (base_url, _handle) = ds_testkit::models::spawn().await;
// point a registry's `endpoints` at `base_url`
```

On a real socket rather than as a fake client, for the reason both of the tests it
replaces gave: a fake client skips the transport, the registry's health ejection and the
parser — the three things most likely to be wrong.

As a service: `cargo run -p ds-testkit --bin ds-fake-models`, or the image
`deploy/docker/Dockerfile.fakemodels`. It listens on `DS_FAKE_MODELS_ADDR`
(default `0.0.0.0:8000`, matching the vLLM pools' in-container port) and serves `/health`.

## One defect it had, which the scanned corpus found

Axum's default request body limit is 2 MB, and a page does not fit. A 200-dpi A4 render is
around 1 MB of PNG, base64 in a JSON message adds a third, and a page that has been
through a conditioner is a re-encode that can be larger again. On a scanned corpus **every
`conditioned-e2e` candidate failed** — 38 of 528 runs — with
`413 length limit exceeded`, which the client reports as "model call failed" and which
therefore reads as a model problem rather than a transport one.

A real VLM server accepts multi-megabyte images, so the default made this double
**stricter than production** — the worst kind of difference for a test double to have, and
one that nothing had noticed because the clean corpus's renders are smaller.
`MAX_REQUEST_BYTES` and `a_page_sized_request_is_not_refused` are the fix and the guard.
