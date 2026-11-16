# vs-vectors: the vector store (Qdrant).
#
# devenv has no Qdrant module, so this runs the binary as a process. Collections are
# created by the `doc` worker at startup (`VectorStore::ensure_ready`), sized from the
# embedding model's dimensions — nothing needs to be provisioned here.
{ pkgs, lib, config, ... }:

let
  # gRPC is what the client speaks (6334). 6333 is the REST/dashboard port, kept for
  # poking at collections by hand during development.
  grpcPort = 6334;
  restPort = 6333;
  storage = "${config.env.DEVENV_STATE}/qdrant";
in
{
  processes.qdrant.exec = ''
    mkdir -p ${storage}/storage ${storage}/snapshots
    export QDRANT__STORAGE__STORAGE_PATH=${storage}/storage
    export QDRANT__STORAGE__SNAPSHOTS_PATH=${storage}/snapshots
    export QDRANT__SERVICE__HTTP_PORT=${toString restPort}
    export QDRANT__SERVICE__GRPC_PORT=${toString grpcPort}
    # Telemetry off: this is a development instance and should not phone home.
    export QDRANT__TELEMETRY_DISABLED=true
    exec ${lib.getExe pkgs.qdrant}
  '';

  env = {
    # Overrides `[qdrant] url` in config/worker.dev.toml. Note the gRPC port.
    VS__QDRANT__URL = "http://127.0.0.1:${toString grpcPort}";
  };
}
