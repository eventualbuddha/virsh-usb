# home-manager module for virsh-usb. Import it by path from a home.nix, or as
# this flake's `homeModules.default`.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.virsh-usb;
in

{
  options.programs.virsh-usb = {
    # Defaults on: importing the module is the opt-in.
    enable = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Install virsh-usb, the libvirt USB attach/detach CLI.";
    };

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./package.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ./package.nix { }";
      description = "The virsh-usb package to install.";
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];
  };
}
