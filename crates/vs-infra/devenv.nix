# vs-infra: how the suite reaches the blob store.
#
# The Garage *service* is in `devenv-stack.nix` (the `stack` profile). What stays here is
# the configuration pointing at it, because that is the part an external deployment
# replaces: the `AWS_*` variables `object_store` reads for anything S3-compatible.
{ pkgs, ... }:

{
  env = {
    # Defaults to the `stack` profile's Garage.
    AWS_ACCESS_KEY_ID = "GK0123456789abcdef01234567";
    AWS_SECRET_ACCESS_KEY = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    AWS_REGION = "garage";
    AWS_ENDPOINT = "http://127.0.0.1:3900";
    AWS_ALLOW_HTTP = "true";
  };
}
