# vs-worker: static musl binary for the Firecracker rootfs.
{ ... }:

{
  languages.rust.targets = [ "x86_64-unknown-linux-musl" ];

  scripts.vs-worker-static = {
    description = "Build the static worker binary for the microVM image";
    exec = ''cargo build --release --target x86_64-unknown-linux-musl -p vs-worker "$@"'';
  };
}
