# The `stack` profile: every service the suite talks to, in one command.
#
#   devenv --profile stack up        # Postgres, Garage, Qdrant, Keycloak
#   devenv --profile stack shell     # the same, plus the model-tier scripts
#
# The default shell deliberately has none of these — it builds the suite and nothing
# more, so another project can take the binaries into its own deployment without
# provisioning a database first. See `deploy/README.md`.
#
# Each service lives next to the component that talks to it; this file only collects
# them.
{ ... }:

{
  imports = [
    ./crates/ds-vault/devenv-stack.nix    # Postgres: ops, registry and vault schemas
    ./crates/ds-infra/devenv-stack.nix    # Garage (S3)
    ./crates/ds-vectors/devenv-stack.nix  # Qdrant
    ./crates/ds-models/devenv-stack.nix   # the LiteLLM model-tier harness
    ./deploy/keycloak/devenv.nix          # the realm ds-api verifies against
  ];
}
