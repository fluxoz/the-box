# The BYO-GPU layer: a spare PC with an NVIDIA card becomes an inference Box.
#
# Composed by boxSystem when the box's channel says gpu = "nvidia" — the same
# axis pattern as boards, and for the same reason: a channel update must
# rebuild the machine with the hardware layer it actually has, or the next
# boot loses the GPU. Everything here is headless compute: no desktop, no X,
# just the kernel module, the userspace, and CDI so containers can say
# `--device nvidia.com/gpu=all`.
{ config, lib, pkgs, ... }:
{
  # The driver is unfree; allow exactly NVIDIA's packages and nothing else
  # (nixpkgs ships the driver as several nvidia-* derivations — kernel
  # modules, userspace, settings — and the split has changed names before,
  # so match the family rather than a list that rots).
  nixpkgs.config.allowUnfreePredicate = pkg:
    lib.hasPrefix "nvidia-" (lib.getName pkg) || lib.getName pkg == "nvidia-x11";

  hardware.graphics.enable = true;
  # The video driver list is what actually loads the kernel module, desktop or
  # not — NixOS's nvidia plumbing hangs off it even on a headless machine.
  services.xserver.videoDrivers = [ "nvidia" ];
  hardware.nvidia = {
    # The proprietary module covers every card back to Maxwell — the honest
    # default for "the spare PC in the closet". Turing-and-newer owners can
    # flip to the open module later; both serve CUDA the same way.
    open = false;
    modesetting.enable = true;
    # No fine-grained power management on an always-on server.
    powerManagement.enable = false;
  };

  # CDI: generates the nvidia.com/gpu device specs podman hands to containers.
  hardware.nvidia-container-toolkit.enable = true;
}
