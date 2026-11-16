# vs-vectors: the vector store (Qdrant).
#
# devenv has no Qdrant module, and `pkgs.qdrant` is **not** usable: nixpkgs builds it from
# source and 1.18.2 fails to compile with the current toolchain — an LLVM intrinsic
# signature mismatch in its AVX-512 quantization code. Worse, naming it in `packages`
# makes the whole dev shell unbuildable, so Postgres and NATS stop starting too.
#
# So this runs the official image, the way the previous graph store did. `docker` comes
# from the host rather than nixpkgs, which keeps a multi-minute source build out of the
# shell; if it is missing, `devenv up` will say so rather than failing to evaluate.
#
# Collections are created by the `doc` worker at startup (`VectorStore::ensure_ready`),
# sized from the embedding model's dimensions, so nothing needs provisioning here.
{ config, ... }:

let
  # gRPC is what the client speaks (6334). 6333 is the REST/dashboard port, kept for
  # poking at collections by hand during development.
  grpcPort = 6334;
  restPort = 6333;
  image = "qdrant/qdrant:v1.18.2";
  name = "vs-dev-qdrant";
  storage = "${config.env.DEVENV_STATE}/qdrant";
in
{
  processes.qdrant.exec = ''
    if ! command -v docker >/dev/null; then
      echo "qdrant needs docker on PATH (nixpkgs' qdrant does not build; see devenv.nix)" >&2
      exit 1
    fi
    mkdir -p ${storage}
    # A previous run that was killed rather than stopped leaves the container behind, and
    # the port with it.
    docker rm -f ${name} >/dev/null 2>&1 || true
    exec docker run --rm --name ${name} \
      -p ${toString restPort}:6333 -p ${toString grpcPort}:6334 \
      -v ${storage}:/qdrant/storage \
      -e QDRANT__TELEMETRY_DISABLED=true \
      ${image}
  '';

  # `docker run --rm` cleans up on SIGTERM, but a hard kill of devenv does not reach it.
  processes.qdrant.process-compose.shutdown.command = "docker stop ${name}";

  env = {
    # Overrides `[qdrant] url` in config/worker.dev.toml. Note the gRPC port.
    VS__QDRANT__URL = "http://127.0.0.1:${toString grpcPort}";
  };
}
