# ds-vectors: where the suite looks for Qdrant. The service itself is in
# `devenv-stack.nix` (the `stack` profile); override this to use your own.
{ ... }:

{
  # Overrides `[qdrant] url` in config/worker.dev.toml. Note the gRPC port, not 6333.
  env.DS__QDRANT__URL = "http://127.0.0.1:6334";
}
