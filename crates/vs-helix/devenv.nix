# vs-helix: HelixDB graph + vector store.
# The `helix` CLI deploys local instances (`helix init`, `helix push dev`; uses Docker).
# TODO: pin to Helix 2.3.5. nixpkgs ships CLI 2.3.4, which builds against upstream
# `main` (Helix 3.x, no HelixQL) and fails; see crates/vs-helix/SPEC.md.
# No instance is started automatically until the Helix project exists.
{ pkgs, ... }:

{
  packages = [ pkgs.helix-db ];

  env.VS__HELIX__URL = "http://127.0.0.1:6969";
}
