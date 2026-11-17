# vs-suite: the one binary. Building it for the microVM image is the only thing that
# needs a script — a glibc build that ships with its shared-library closure, including
# libpdfium.so (deploy/firecracker/SPEC.md).
{ ... }:

{
  scripts.vs-suite-release = {
    description = "Build the service binary for the microVM image";
    exec = ''cargo build --release -p vs-suite "$@"'';
  };
}
