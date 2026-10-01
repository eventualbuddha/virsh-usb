{
  description = "virsh-usb: attach USB devices (real and virtual) to libvirt VMs";

  inputs.nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        virsh-usb = pkgs.callPackage ./nix/package.nix { };
        default = virsh-usb;
      });

      # Installs the binary via home-manager. See nix/home-manager.nix.
      homeModules.default = ./nix/home-manager.nix;

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.rust-analyzer
            pkgs.clippy
            pkgs.rustfmt
          ];
        };
      });
    };
}
