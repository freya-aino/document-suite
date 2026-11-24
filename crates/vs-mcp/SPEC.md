# vs-mcp — Specification

A **Model Context Protocol** server that advertises the suite's work as LLM tools. Its own
binary and its own service: it talks to `vs-suite`'s HTTP API like any other client and
links none of the suite's internals — no database, no object store, no model registry, no
migrations. System spec: [`docs/SPEC.md`](../../docs/SPEC.md); the API it wraps:
[`vs-api/SPEC.md`](../vs-api/SPEC.md).

```
LLM client ──MCP (stdio or HTTP)──▶ vs-mcp ──HTTPS + the caller's token──▶ vs-suite
```

## Status

Implemented: JSON-RPC 2.0 framing, `initialize` with protocol-version negotiation, `ping`,
`tools/list`, `tools/call`, notifications, both transports (stdio and stateless Streamable
HTTP), twelve tools, the error mapping, and the limits. 26 unit tests, 11 tool tests and 7
transport tests pass without any service running; 4 more against real Postgres.

**It is tested against the real API, not against a picture of it.** `tests/live.rs` serves
the actual `vs_api::router` on a socket with real Keycloak-shaped tokens signed by the
fixture realm, and drives the tools through it. That test earned its place on its first
run: three of its four cases failed, and every failure was a true statement about the API
that a fake would have hidden — a `source_uri` upload returns no document hash key until
`skip_if_processed` makes the API read the object, and a document's access row is written
by **ingest**, not by accepting the job, so until a pipeline run exists there is nothing to
share. The tests now say so.

**Two people, one organization, through the tools.** The live test uploads as Alice and
reads as Bob: same organization, valid token, and still refused — then granted, then
readable, then revoked, then refused again. That is D14 proved end to end through the MCP
layer rather than asserted about it.

**The stdio transport is tested as a process.** `tests/transport.rs` runs the built binary
over pipes, because the thing most likely to break it cannot be caught in-process: one
stray line on stdout corrupts the stream, and the client's next parse fails on what looks
like a server bug. The test asserts the startup log is on stderr, that no token appears in
it, and that a closed stdin is a clean exit.

Remaining:
- [ ] No tool reads a document's **text** in the live test, because that needs a processed
      document, which needs models and a GPU. `tests/tools.rs` covers the reading, the
      ordering, the truncation and the furniture, against page results in the shape the API
      serves. What is untested is the one thing a real run would add: whether a real
      document's blocks make a useful answer at the size an LLM can hold.
- [ ] **No tool has been used by a model.** The descriptions are written for one and tested
      for shape, not for whether a model picks the right tool from them. That needs a
      session with an agent and a corpus, and it is the only item here that code does not
      close.
- [ ] No `search` tool. Retrieval reads Postgres and Qdrant directly
      ([`docs/STORES.md`](../../docs/STORES.md)) and the API exposes no search route, so a
      search tool would mean this service holding both stores' credentials and doing its own
      access control. That is a different service with a different security story; when the
      API gets a search route, this server gets a tool for it.
- [ ] No elicitation, sampling, resources or prompts. The suite's documents would make
      plausible MCP *resources*, but a resource list is a list of what the caller may read,
      and producing one means paging the whole corpus per session; `list_documents` answers
      the same question when asked.

## Why a separate service

The alternative is a transport inside `vs-suite`, which is what D12 would suggest. Against
it, three things, in order of weight:

1. **It is a different trust surface.** This server speaks to models and the clients that
   drive them, and the attacks on it are prompt-shaped: a document whose text tells the
   model to share it with somebody. Keeping it out of the process that holds the database
   credentials and the signing keys means a defect here cannot become a defect there.
2. **It scales and fails separately.** A model client that opens a hundred sessions should
   not be competing for the heap of the process running the pipeline, and an MCP server
   that needs a restart should not take page processing with it.
3. **It is optional.** A deployment that wants the API and no LLM tools runs one fewer
   binary, and nothing in the suite notices.

The cost is honest and small: one more hop, and a client of the API rather than of the
functions behind it. It buys the property that makes the rest of this document short —
this service cannot do anything a person with the same token could not do over HTTP.

## Credentials (decision D16)

**This service holds no credentials of its own, and will not act without the caller's.**

| Transport | Where the token comes from |
|---|---|
| stdio | `VS_API_TOKEN`, or `--token-file`; the process is the user's, so its environment is theirs |
| HTTP | the `Authorization` header of each MCP request, forwarded unchanged |

There is **no service-account fallback**, deliberately. A service account would mean every
tool call ran as this server, so every model user would read every document the server's
own identity could reach — which is precisely the per-person access control D14 exists to
provide, removed by a configuration default. A call without a token therefore fails, with a
message that says how to supply one.

Three consequences follow, and all three are properties rather than limitations:

- **No tenant argument, anywhere.** No tool takes an organization, a subject or an
  "act as": the organization is a verified claim (D11) and nothing a model can say changes
  it. `tests/tools.rs` asserts this of every tool's schema, so a tool added later cannot
  quietly introduce one.
- **This server cannot refresh a token.** An expired one is reported as such, with advice
  that says a fresh token is needed rather than implying the server can obtain it.
- **A token is never logged, stored or echoed.** Not at startup, not in the request log,
  not in an error. The transport test asserts the token does not appear in stderr.

## Tools

Twelve, by what a person actually asks for. Read-only unless marked.

| Tool | Does | Notes |
|---|---|---|
| `list_documents` | The documents this caller may read, newest first | Their view, not the organization's corpus |
| `get_document` | Page count, per-page class and workflow, block counts, entity and assertion counts | The cheap first call on an unfamiliar document |
| `get_document_text` | A page range's text in reading order, by block, with kinds | Tables as HTML, formulas as LaTeX; furniture omitted by default |
| `get_document_access` | How widely a document is shared, and its grants | What to call before sharing, and after a refusal |
| `get_job` | One job's state and page progress | |
| `get_batch` | A batch's state counts and a page of its jobs | |
| `wait_for_job` | Polls until `done`, `partial` or `failed`, or the wait runs out | Server-capped; a timeout is not a failure |
| `upload_document` | **Writes.** Submits an object already in the bucket; returns a job id | No local file: this server and the suite share no filesystem |
| `upload_batch` | **Writes.** Many documents, one request | One rate-limit charge instead of *n* |
| `share_document` | **Writes.** Sets visibility: private, department, workspace, organization | Owner or admin only; reversible; never leaves the organization |
| `grant_access` | **Writes.** Grants one person, department or workspace | Preferred over widening for everyone |
| `revoke_access` | **Writes.** Withdraws a grant | Does not change visibility |

### What is deliberately not advertised

The API has routes for erasing a person's documents, revoking tokens and sessions, and
uploading and activating DAG lists. None is a tool, and none will be:

- **Owner erasure** (`DELETE /v1/owners/{subject}`) is irreversible and is the kind of
  thing a prompt-injected document would ask for. A person erases a person.
- **Token and session revocation** is an incident-response action. A model deciding to log
  somebody out is not a feature.
- **DAG-list administration** changes how every page in the organization is routed. It is
  reviewed against a report (`vs-adapt`), not called by an assistant.

A unit test asserts these names stay absent, so "add a tool for it" has to be a decision
rather than an afternoon's convenience.

### Why `upload_document` takes an object key

A model asked to process a document naturally reaches for a path, and that cannot work:
this server runs wherever it runs and the suite reads from the bucket. Rather than accept a
path and fail in the suite, the tool's description says what the argument is and what to do
instead — put the file in `originals/<organization>/` or use the API's multipart route
directly. A tool that explains its own constraint costs one sentence; a tool that fails
obscurely costs the model a retry loop.

## Protocol

JSON-RPC 2.0, hand-written rather than from an SDK: this is an envelope, four methods and
five error codes, against a dependency whose own protocol revision would then have to be
tracked. `tests/transport.rs` holds it to the wire format.

| Revision | |
|---|---|
| `2025-06-18` | Preferred and advertised |
| `2025-03-26`, `2024-11-05` | Accepted and echoed when a client asks for them |

A client asking for a revision this server does not know is answered with `2025-06-18` and
decides for itself whether to continue — which is what the specification asks for, and the
opposite of echoing an unknown version back, which would claim support for anything.

**Batches are refused.** They were removed in `2025-06-18`, and half-supporting them is
worse than saying so: a client whose batch gets one reply has a harder problem than one
that is told plainly.

### Protocol errors versus tool failures

The distinction most worth getting right, because confusing it is how an MCP server becomes
useless to the thing calling it:

| | Reported as | Seen by |
|---|---|---|
| Unknown method, unparseable message, unknown tool, arguments that do not fit the schema | A JSON-RPC **error** | The client library |
| A document that may not be read, an expired token, a rate limit, an unreachable suite | A **result** with `isError: true` | **The model** |

So a 403 comes back as text the model can act on — the suite's own sentence, which already
names the remedy, followed by this server's, which names the tool that does it. Every error
variant carries that second sentence and a unit test asserts each one reads as advice.

### Transports

**stdio** — one message per line, requests on stdin, replies on stdout, handled in order.
Logs go to **stderr**, arranged before anything can log. Serial rather than concurrent on
purpose: the tools are I/O-bound against one API, an agent sends one call at a time, and a
serial loop cannot interleave two replies' bytes.

**Streamable HTTP** — `POST /mcp`, one message, one reply; `202` for a notification.
**Stateless**: no `Mcp-Session-Id` is assigned, because this server keeps nothing between
calls — the token is the only state a call needs. Copies therefore need no sticky sessions
and no shared store, the same reasoning D13 applied to the suite. `GET` and `DELETE` answer
`405`, which the specification permits for a server that offers no server-initiated
messages; nothing in this catalogue needs one, since every tool is a question with an
answer.

Two guards on the HTTP side:

- **`Origin`**, when present, must be on `--allowed-origin`. The specification calls this
  out for locally bound servers because of DNS rebinding: a page on any website can `POST`
  to `127.0.0.1`, and a server that answers has handed that page the tools. A request with
  no `Origin` — every non-browser client — is unaffected.
- **`MCP-Protocol-Version`**, when present, must be a revision this server implements. A
  wrong one means the client negotiated with a different server, and continuing would be
  guessing which dialect the next message is in.

## Limits

Set by the deployment, not by the caller: a model that asks for 500 pages gets the cap, and
is told it got the cap.

| Flag / variable | Default | |
|---|---|---|
| `--api` / `VS_API_URL` | `http://127.0.0.1:8080` | The suite |
| `--api-timeout` / `VS_MCP_API_TIMEOUT` | 60 s | Per API call, not per wait |
| `--max-text-chars` / `VS_MCP_MAX_TEXT_CHARS` | 60 000 | Text one read may return (~15k tokens) |
| `--max-pages` / `VS_MCP_MAX_PAGES` | 20 | Pages one read may fetch |
| `--max-wait` / `VS_MCP_MAX_WAIT` | 120 s | Longest `wait_for_job` waits |
| `--poll-interval` / `VS_MCP_POLL_INTERVAL` | 3 s | Between polls |

A truncated answer says it was truncated and where to resume. A clamped page count is not
an error: asking for too much once is a reasonable thing for a model to do.

## Running it

```sh
# One process per user, started by their client (Claude Desktop, an agent, an IDE).
VS_API_TOKEN=… vs-mcp --api https://suite.internal stdio

# A service. Every request carries its own caller's token.
vs-mcp --api https://suite.internal http --addr 0.0.0.0:8081 --allowed-origin https://app.internal
```

Client configuration, for the stdio form:

```jsonc
{ "mcpServers": {
    "visual-suite": {
      "command": "vs-mcp",
      "args": ["--api", "https://suite.internal", "stdio"],
      "env": { "VS_API_TOKEN_FILE": "/run/secrets/vs-token" } } } }
```

A token *file* rather than an environment variable on a shared machine: an environment is
readable through `/proc` by anyone who can read the process.

## Observability

`mcp_requests_total{method,outcome}`, `mcp_request_duration_seconds{method}`,
`mcp_tool_calls_total{tool,outcome}`, `mcp_tool_errors_total{tool}`,
`mcp_api_call_duration_seconds{path,status}`, `mcp_origin_refusals_total`. The HTTP mode
initialises `vs_infra::telemetry` and serves `/healthz`; the stdio mode logs to stderr and
serves nothing, having no port.

Neither mode logs a request body: a job spec carries the caller's subject and a page result
carries the document's text.

## Tests

| File | Needs | |
|---|---|---|
| unit tests in `src/` | nothing | The catalogue's shape, the protocol's codes, the token rules, block rendering |
| `tests/tools.rs` | nothing | Every tool against a suite served on a socket; token forwarding; the caps |
| `tests/transport.rs` | nothing | The HTTP endpoint, and the built binary over stdio pipes |
| `tests/live.rs` | Postgres | The real `vs_api::router`, real signed tokens, two people in one organization |

```sh
cargo test -p vs-mcp                  # everything that needs no service
cargo test -p vs-mcp -- --ignored     # the live API tests (devenv up)
```
