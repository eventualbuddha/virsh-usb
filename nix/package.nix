{
  lib,
  rustPlatform,
  installShellFiles,
  stdenv,
}:

rustPlatform.buildRustPackage (finalAttrs: {
  pname = "virsh-usb";
  version = "0.1.0";

  src = lib.cleanSource ../.;
  cargoLock.lockFile = ../Cargo.lock;

  # Host USB devices are enumerated straight from /sys/bus/usb, so there is no
  # dependency on `lsusb`/usbutils. At runtime it shells out to `virsh` and
  # `udevadm`; both are already on PATH on any NixOS host that has
  # `virtualisation.libvirtd.enable = true`, which is the only kind of host
  # this tool is useful on, so the binary is deliberately not wrapped.

  nativeBuildInputs = [ installShellFiles ];

  # Completions come from the binary itself (clap_complete, via the hidden
  # `completions` subcommand), so they can only be generated when the built
  # binary runs on the build machine.
  postInstall = lib.optionalString (stdenv.buildPlatform.canExecute stdenv.hostPlatform) ''
    installShellCompletion --cmd virsh-usb \
      --bash <($out/bin/virsh-usb completions bash) \
      --fish <($out/bin/virsh-usb completions fish) \
      --zsh <($out/bin/virsh-usb completions zsh)
  '';

  meta = {
    description = "Attach real and virtual USB devices to libvirt VMs";
    homepage = "https://github.com/eventualbuddha/virsh-usb";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = "virsh-usb";
  };
})
