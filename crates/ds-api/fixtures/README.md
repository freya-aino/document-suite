# Test keys

Throwaway RSA keypairs, generated once for the authentication tests and **not** secrets:
nothing outside `src/auth_tests.rs` and `src/http_tests.rs` reads them, and no deployment
trusts them. They are committed so that the tests sign real tokens and verify real
signatures rather than mocking the verifier out.

| File | What it is |
|---|---|
| `test-key-1.pem` | private key the tests sign valid tokens with |
| `test-jwks.json` | its public half, as the JWKS a realm would publish (`kid: test-key-1`) |
| `test-key-2.pem` | a second private key, for forged signatures and for key rotation |
| `test-jwks-key-2.json` | its public half (`kid: test-key-2`) |

Regenerating them (`ssh-keygen` writes a PKCS#8 private key, and the JWK is built from the
public blob):

```sh
ssh-keygen -q -t rsa -b 2048 -m PKCS8 -f test-key-1.pem -N ""
ssh-keygen -y -f test-key-1.pem > key.sshpub   # ssh-rsa <base64 of "ssh-rsa" | e | n>
```

Then `n` and `e` from that blob, base64url without padding, become the JWK's `n` and `e`.
