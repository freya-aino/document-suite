# vs-infra's service: the blob store (Garage, as the S3 server). Part of the `stack`
# profile only; the credentials and endpoint the suite reads are in `devenv.nix`, so
# pointing it at your own S3 is a matter of overriding those.
{ ... }:

let
  s3 = {
    region = "garage";
    bucket = "visual-suite";
    # Fixed dev key, imported into Garage on start (Garage format: GK + 24 hex / 64 hex).
    keyName = "vs-dev";
    accessKey = "GK0123456789abcdef01234567";
    secretKey = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
  };
in
{
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
}
