# Workspace-wide toolchain. Service-specific dependencies live in each service's
# devenv.nix (see `imports` in devenv.yaml).
{ pkgs, config, ... }:

{
  languages.rust = {
    enable = true;
    channel = "stable"; # workspace needs >= 1.80; musl target needs rust-overlay
  };

  packages = [ pkgs.cargo-nextest ];

  env = {
    # Read by vs-api and vs-worker (`--config`); points at the local services below.
    VS_CONFIG = "${config.devenv.root}/config/worker.dev.toml";
    RUST_LOG = "info,vs_=debug";
  };

  # Opt-in: `devenv --profile firecracker shell`
  profiles.firecracker.module = ./deploy/firecracker/devenv.nix;
}
