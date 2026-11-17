# Keycloak — the development realm

`vs-api` verifies every request against a Keycloak realm and **refuses to start without
one** (`[auth]` in its config). This directory is the realm that makes the local stack
runnable; `devenv --profile stack up` imports it on every start.

```sh
devenv --profile stack up -d        # Keycloak on :8081, realm `koki`
vs-token                            # an access token for organization t1, with vs-admin
vs-token t2                         # organization t2, no admin role
```

Admin console: <http://127.0.0.1:8081> (`admin` / `admin`). Issuer:
`http://127.0.0.1:8081/realms/koki`, which is what `config/worker.dev.toml` points at.

## What the realm contains

| | |
|---|---|
| Realm | `koki`, `sslRequired: none` (http on loopback), 30-minute access tokens |
| Realm role | `vs-admin` — `auth.admin_role`: the DAG-list routes and `/metrics` |
| Client `vs-api` | confidential, secret `vs-api-dev-secret`; service account **and** password grant; service account and the `dev` user both hold `vs-admin` |
| Client `vs-api-t2` | confidential, secret `vs-api-t2-dev-secret`; service account only, organization `t2`, **no** role |
| User `dev` | password `dev`, holds `vs-admin` |

Each client carries an **audience mapper** adding `vs-api` to `aud`. Without one Keycloak
mints `aud: account` and every request is refused — it is the single most common way a
working realm still fails against this API.

## Why the organization is a hardcoded claim

In a real realm the organization comes from the person: their organization membership, or
a user attribute, mapped into the claim `auth.org_claim` names. Here it is an
`oidc-hardcoded-claim-mapper` on the client, because since Keycloak 24 the declarative
user profile **drops unmanaged user attributes by default** — a realm file that set
`organization` as a user attribute would import cleanly, mint tokens cheerfully, and leave
the claim out, which looks exactly like a bug in the API. Hardcoding it on the client
keeps this file independent of that setting, and it is why there are two clients rather
than two users: the client is what decides the organization here.

So: `vs-api` ⇒ organization `t1`, department `dept-a`, workspaces `[ws-red, ws-blue]`;
`vs-api-t2` ⇒ organization `t2`. Tenant isolation can be checked by hand by uploading with
one token and reading with the other — the second must answer `404`, and the admin routes
`403`.

## What a real realm needs instead

1. A mapper putting the person's organization into the claim `auth.org_claim` names
   (default `organization`). The value becomes the tenant, is hashed into every document
   and entity key, and must match `[A-Za-z0-9_-]{1,64}` — it is also a blob-key path
   segment. An organization id with a `/` or a `|` in it is refused rather than
   normalised.
2. An audience mapper for the API's `aud`.
3. The `auth.admin_role` role, granted to whoever administers DAG lists and to the
   Prometheus scrape's service account.
4. Department and workspace mappers if you want them recorded on jobs
   (`JobSpec::owner`); they are optional and nothing enforces them yet.

`crates/vs-api/src/auth_tests.rs` has a test (`#[ignore]`d, needs this realm running)
that asks for a token here and asserts the claims arrive as the verifier expects. It is
what notices if this file and the verifier drift apart.
