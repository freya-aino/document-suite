//! Finding the stack, and skipping rather than failing when it is not there.

/// Where the stack is, and what it is serving.
#[derive(Debug, Clone)]
pub struct Stack {
    /// The public API, e.g. `http://127.0.0.1:57080`.
    pub base_url: String,
    /// The unauthenticated metrics port. `GET /metrics` on the API is behind the admin
    /// role on purpose, so the harness scrapes this one.
    pub metrics_url: String,
    /// Keycloak's token endpoint, reached from the host.
    pub token_url: String,
    /// `fake`, `cpu` or `gpu`. What a measurement may be read as depends on it.
    pub tier: String,
}

impl Stack {
    /// Reads the environment `deploy/compose/stack.sh env` prints, with the defaults it
    /// uses, so `cargo test` works after `eval "$(stack.sh env)"` or without it.
    pub fn from_env() -> Self {
        let var = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        Self {
            base_url: var("DS_ITEST_BASE_URL", "http://127.0.0.1:57080"),
            metrics_url: var("DS_ITEST_METRICS_URL", "http://127.0.0.1:57464/metrics"),
            token_url: var("DS_ITEST_TOKEN_URL", "http://127.0.0.1:57081/realms/koki/protocol/openid-connect/token"),
            tier: var("DS_ITEST_TIER", "fake"),
        }
    }

    /// Is it answering?
    pub async fn reachable(&self) -> bool {
        let Ok(c) = reqwest::Client::builder().timeout(std::time::Duration::from_secs(3)).build() else {
            return false;
        };
        matches!(c.get(format!("{}/healthz", self.base_url)).send().await, Ok(r) if r.status().is_success())
    }

    /// Only a `fake` tier may be asserted on for anything but plumbing, and only a
    /// non-fake tier may be read as saying anything about a model.
    pub fn models_are_real(&self) -> bool {
        self.tier != "fake"
    }
}

/// The stack, or `None` with a note on stderr.
///
/// This is the repository's first runtime reachability gate — everywhere else `#[ignore]`
/// is the only one. It earns the exception because `cargo test --workspace --
/// --include-ignored` is a documented developer command, and it must not fail for someone
/// who simply has no stack running. A test that *is* reached still fails loudly.
pub async fn stack_or_skip() -> Option<Stack> {
    let s = Stack::from_env();
    if s.reachable().await {
        return Some(s);
    }
    eprintln!("skipping: no stack at {} — start one with `deploy/compose/stack.sh up`", s.base_url);
    None
}
