# Compiled from a box.toml static-site entry: the platform turns this into a
# real nginx virtual host. A generated module composing all the way down to a
# running system service is the whole point of the OS tier.
{ ... }:
{
  services.the-box.sites."hello" = {
    root = ./www;
  };
}
