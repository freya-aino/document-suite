# vs-helix: HelixDB graph + vector store.
# The `helix` CLI deploys local instances (`helix init`, `helix push dev`; uses Docker).
# No instance is started automatically: the Helix project/schema for the suite is not
# written yet (target Helix version still open, see crates/vs-helix/SPEC.md).
{ pkgs, ... }:

{
  packages = [ pkgs.helix-db ];

  env.VS__HELIX__URL = "http://127.0.0.1:6969";
}
