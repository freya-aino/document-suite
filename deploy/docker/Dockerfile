# syntax=docker/dockerfile:1.7
#
# The document-suite service as one image: `vs-suite`, which serves the HTTP API and runs
# the three worker roles in one process. See deploy/README.md for what the deployment has
# to provide around it, and § Configuration for every parameter.
#
#   docker build -f deploy/docker/Dockerfile -t document-suite:dev .
#
# Build it from the repository root: the context is the workspace, because the build
# compiles it.

# ───────────────────────────── pdfium ─────────────────────────────
# The rasterizer `dlopen`s libpdfium.so (pdfium-render, dynamic binding) and Debian has no
# package for it, so it comes from the usual prebuilt release, pinned by digest.
#
# 8066 is deliberate rather than "latest": it is the version nixpkgs gives the dev shell
# and CI, so a page renders in this image exactly as it does in the tests, and it exports
# the symbols the ABI feature pinned in the workspace Cargo.toml
# (`pdfium-render`/`pdfium_7543`) expects. Moving one without the other is how rendering
# breaks at load time rather than at build time. The library needs only glibc, libm,
# libpthread and libgcc_s — no libstdc++, no X — which is why the runtime stage below
# installs nothing for it.
FROM debian:bookworm-slim AS pdfium
ARG PDFIUM_RELEASE=chromium%2F8066
ARG PDFIUM_SHA256=0b43f405477cf2cfc4dbff06905093c3309756c6bca1fb9da99234a2ca97fed2
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL -o /tmp/pdfium.tgz \
      "https://github.com/bblanchon/pdfium-binaries/releases/download/${PDFIUM_RELEASE}/pdfium-linux-x64.tgz" \
 && echo "${PDFIUM_SHA256}  /tmp/pdfium.tgz" | sha256sum -c - \
 && mkdir -p /opt/pdfium \
 && tar xzf /tmp/pdfium.tgz -C /opt/pdfium lib/libpdfium.so

# ───────────────────────────── build ─────────────────────────────
# The newest stable, pinned: the policy is to track it (see `rust-version` in the
# workspace Cargo.toml), and pinning the exact release here is what keeps an image
# reproducible while the policy keeps it current. It may be ahead of `rust-version`, which
# is capped by what the dev shell provides — newer is always allowed.
FROM rust:1.100-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# Cache mounts rather than a dependency-only pre-build: 18 crates make the dummy-source
# trick more fragile than it is worth. `target` is a cache mount, so the binary has to be
# copied out inside the same step.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked -p vs-suite \
 && cp target/release/vs-suite /usr/local/bin/vs-suite

# ───────────────────────────── runtime ─────────────────────────────
FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.title="document-suite" \
      org.opencontainers.image.description="Visual document processing: HTTP API and worker roles in one service" \
      org.opencontainers.image.licenses="UNLICENSED"

# ca-certificates: Keycloak, S3 and the model endpoints are all TLS.
# curl: only for HEALTHCHECK below — drop both if your orchestrator probes /healthz itself.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --shell /usr/sbin/nologin --home-dir /scratch vs \
 && mkdir -p /scratch /etc/vs \
 && chown vs:vs /scratch

COPY --from=pdfium /opt/pdfium/lib/libpdfium.so /usr/lib/vs/libpdfium.so
COPY --from=build /usr/local/bin/vs-suite /usr/local/bin/vs-suite
COPY deploy/docker/worker.toml /etc/vs/worker.toml

ENV VS_CONFIG=/etc/vs/worker.toml \
    VS_API_ADDR=0.0.0.0:8080 \
    PDFIUM_DYNAMIC_LIB_PATH=/usr/lib/vs \
    LD_LIBRARY_PATH=/usr/lib/vs \
    TMPDIR=/scratch \
    RUST_LOG=info

EXPOSE 8080
USER vs
WORKDIR /scratch

# Every copy serves the API, so there is always an endpoint to probe. (This used to branch
# on `VS_ROLES`, for copies that only consumed the queue; there is no queue and no roles.)
HEALTHCHECK --interval=15s --timeout=3s --start-period=20s --retries=3 \
  CMD ["/bin/sh", "-c", "curl -fsS \"http://127.0.0.1:${VS_API_ADDR##*:}/healthz\" >/dev/null || exit 1"]

ENTRYPOINT ["/usr/local/bin/vs-suite"]
