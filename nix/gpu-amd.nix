# The unified-memory inference layer: a Ryzen AI Max ("Strix Halo") machine
# becomes an inference Box. Composed by boxSystem when the box's channel says
# gpu = "amd" — the same axis as nvidia, different physics: this GPU lives on
# the CPU package and borrows system RAM, so the work here is letting it
# borrow nearly all of it. Everything is Mesa/RADV: no ROCm, no unfree
# packages, no kernel module beyond mainline amdgpu. Vulkan is the fast,
# stable path on this silicon (llama.cpp's RADV backend beats ROCm on it).
{ config, lib, pkgs, ... }:
{
  # Strix Halo's unified-memory handling landed across 6.15-6.18; AMD's own
  # ROCm system guide calls for >= 6.18.4 on generic distros for the merged
  # TTM patches. Track the latest kernel rather than the LTS default.
  boot.kernelPackages = lib.mkDefault pkgs.linuxPackages_latest;

  # Let the iGPU address nearly the whole memory as GTT instead of the stock
  # cap that strands models unable to load (the infamous "ROCm sees 16GB of
  # my 128GB"). Values follow the community-standard Strix Halo setup:
  # ~124GB addressable with OS headroom reserved. amd_iommu=off is measured
  # 5-12% faster on this chip. Keep the BIOS dedicated-VRAM carve-out small
  # (512MB); the whole point is dynamic sharing, not a static split.
  boot.kernelParams = [
    "amd_iommu=off"
    "amdgpu.gttsize=126976"
    "ttm.pages_limit=32505856"
  ];

  # Mesa userspace (RADV Vulkan) for compute; headless, no desktop.
  hardware.graphics.enable = true;

  # Vulkan compute containers need the render nodes, not CDI: a container
  # that says gpu = true gets /dev/dri handed through.
  services.the-box.gpuContainerDevices = [ "/dev/dri:/dev/dri" ];
}
