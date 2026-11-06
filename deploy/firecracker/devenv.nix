# deploy/firecracker: run worker microVMs locally. Opt-in profile:
#   devenv --profile firecracker shell
# Needs /dev/kvm on the host.
{ pkgs, ... }:

{
  packages = [
    pkgs.firecracker
    pkgs.e2fsprogs # mkfs.ext4 for the rootfs image
  ];

  enterShell = ''
    [ -w /dev/kvm ] || echo "warning: /dev/kvm missing or not writable; firecracker will not start" >&2
  '';
}
