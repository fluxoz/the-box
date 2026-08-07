# The Box standard disk layout, parameterized by target device. The installer
# imports this with the device it resolved (see docs/install.md for the
# selection policy) and hands the result to disko.
#
# Layout: GPT, 1G ESP (label BOX-ESP), rest ext4 root (label box-root).
# box-os mounts by these labels, so the layout and the system config stay
# decoupled from device names.
{ device }:
{
  disko.devices.disk.main = {
    inherit device;
    type = "disk";
    content = {
      type = "gpt";
      partitions = {
        ESP = {
          size = "1G";
          type = "EF00";
          content = {
            type = "filesystem";
            format = "vfat";
            mountpoint = "/boot";
            mountOptions = [ "umask=0077" ];
            extraArgs = [ "-n" "BOX-ESP" ];
          };
        };
        root = {
          size = "100%";
          content = {
            type = "filesystem";
            format = "ext4";
            mountpoint = "/";
            extraArgs = [ "-L" "box-root" ];
          };
        };
      };
    };
  };
}
