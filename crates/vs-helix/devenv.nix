# vs-helix: HelixDB graph + vector store, accessed only through HelixQL.
# Pinned to Helix 2.3.5 (CLI and server): HelixQL exists only on the 2.x line, and CLIs
# before 2.3.5 (nixpkgs ships 2.3.4) build against upstream `main` (3.x) and fail.
# See crates/vs-helix/SPEC.md.
{ pkgs, ... }:

let
  helix = pkgs.helix-db.overrideAttrs (final: prev: {
    version = "2.3.5";
    src = pkgs.fetchFromGitHub {
      owner = "HelixDB";
      repo = "helix-db";
      tag = "v${final.version}";
      hash = "sha256-9vysmNCFFKfafyvyIQHBO6Qrs2v2aYwQBTUABFWqQY8=";
    };
    cargoHash = "sha256-b2ZIjMLizJk1Felcg9/AZaumgMB9OtWM0c+HCWLVt1I=";
  });
in
{
  packages = [
    helix
    # `helix check` cargo-checks the generated query code, which links openssl
    pkgs.pkg-config
    pkgs.openssl
  ];

  env.VS__HELIX__URL = "http://127.0.0.1:6969";

  # Local instance (Docker): builds the project in crates/vs-helix/helix and starts it.
  scripts.vs-helix-up = {
    description = "Build and start the local HelixDB instance (helix push dev)";
    # Stop first: if the port is still taken, helix 2.3.5 maps the next free host port
    # but leaves the server on the configured one, and the instance is unreachable.
    exec = ''cd "$DEVENV_ROOT/crates/vs-helix/helix" && { helix stop dev >/dev/null 2>&1 || true; } && helix push dev "$@"'';
  };
  scripts.vs-helix-down = {
    description = "Stop the local HelixDB instance";
    exec = ''cd "$DEVENV_ROOT/crates/vs-helix/helix" && helix stop dev "$@"'';
  };
}
