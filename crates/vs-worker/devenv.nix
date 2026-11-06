# vs-worker: glibc build for the Firecracker rootfs (ships with its shared-library
# closure, incl. libpdfium.so; see deploy/firecracker/SPEC.md).
{ ... }:

{
  scripts.vs-worker-release = {
    description = "Build the worker binary for the microVM image";
    exec = ''cargo build --release -p vs-worker "$@"'';
  };
}
