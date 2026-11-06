# vs-infra: task bus (NATS JetStream) and blob store (S3-compatible).
# Locally: nats-server with JetStream, Garage as the S3 server.
{ pkgs, ... }:

let
  s3 = {
    endpoint = "http://127.0.0.1:3900";
    region = "garage";
    bucket = "visual-suite";
    # Fixed dev key, imported into Garage on start (Garage format: GK + 24 hex / 64 hex).
    keyName = "vs-dev";
    accessKey = "GK0123456789abcdef01234567";
    secretKey = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
  };
in
{
  services.nats = {
    enable = true;
    port = 4222;
    jetstream.enable = true; # streams/consumers are created by vs-infra at startup
  };

  # devenv's garage module applies a single-node layout and creates the buckets.
  services.garage = {
    enable = true;
    s3Address = "127.0.0.1:3900";
    inherit (s3) region;
    buckets = [ s3.bucket ];
    afterStart = ''
      if ! $GARAGE key info ${s3.keyName} &>/dev/null; then
        $GARAGE key import --yes -n ${s3.keyName} ${s3.accessKey} ${s3.secretKey}
      fi
      $GARAGE bucket allow --read --write --owner ${s3.bucket} --key ${s3.keyName}
    '';
  };

  packages = [ pkgs.natscli ]; # `nats stream ls`, `nats consumer info VS_TASKS ...`

  env = {
    # object_store reads AWS_* from the environment.
    AWS_ACCESS_KEY_ID = s3.accessKey;
    AWS_SECRET_ACCESS_KEY = s3.secretKey;
    AWS_REGION = s3.region;
    AWS_ENDPOINT = s3.endpoint;
    AWS_ALLOW_HTTP = "true";
  };
}
