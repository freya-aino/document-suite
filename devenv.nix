# The default shell: the toolchain and the native libraries the workspace needs to
# **build and unit-test** itself. No services — `devenv up` here starts nothing.
#
# That is the point. Another project embedding this suite wants its binaries, not its
# Postgres, so the default shell provisions nothing and every endpoint is a `VS__*`
# override away (`deploy/README.md`). The services are a profile:
#
#   devenv --profile stack up          # Postgres, NATS, Garage, Qdrant, Keycloak
#   devenv --profile firecracker shell # worker microVMs; needs /dev/kvm
#
# Component-specific *client* configuration (where to find a database, which S3
# credentials, which Qdrant) lives in each component's own devenv.nix, merged here
# through `imports` in devenv.yaml.
{ pkgs, config, ... }:

{
  languages.rust = {
    enable = true;
    channel = "stable"; # workspace needs >= 1.82 (Option::is_none_or)
  };

  packages = [ pkgs.cargo-nextest ];

  env = {
    # Read by vs-api and vs-worker (`--config`); its endpoints point at the stack profile.
    VS_CONFIG = "${config.devenv.root}/config/worker.dev.toml";
    RUST_LOG = "info,vs_=debug";
  };

  profiles.stack.module = ./devenv-stack.nix;
  profiles.firecracker.module = ./deploy/firecracker/devenv.nix;
}
