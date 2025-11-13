# virsh-usb

A command-line tool for managing USB device attachment to virsh (libvirt/KVM) VMs with an intuitive interactive interface.

## Features

- Attach and detach USB devices to/from running VMs
- Interactive selection of VMs and USB devices
- Search devices by name or vendor:product ID
- Shows device attachment status
- Remembers your last used VM
- Color-coded output for better readability

## Prerequisites

- Linux system with libvirt/KVM installed
- `virsh` command-line tool
- `lsusb` utility (usually from `usbutils` package)
- Rust toolchain (for building from source)

## Installation

### From Source

```bash
cargo build --release
```

The binary will be available at `target/release/virsh-usb`.

Optionally, copy it to your PATH:

```bash
sudo cp target/release/virsh-usb /usr/local/bin/
```

## Usage

### Basic Commands

```bash
# Attach a USB device (interactive mode)
virsh-usb attach

# Detach a USB device (interactive mode)
virsh-usb detach

# Check status of a USB device
virsh-usb status
```

### With Command-Line Arguments

```bash
# Specify VM and device by vendor:product ID
virsh-usb --vm myvm --device 0dd4:4105 attach

# Specify VM and search device by name
virsh-usb --vm myvm --device PaperHandler attach

# Check status with specific VM and device
virsh-usb --vm myvm --device 0dd4:4105 status
```

### Interactive Mode

When you run commands without the `--vm` or `--device` flags, the tool will prompt you interactively:

1. **VM Selection**: Choose from a list of all your VMs (defaults to the last used VM)
2. **Device Selection**: Choose from a list of connected USB devices
   - For attach: shows all USB devices
   - For detach: shows only devices currently attached to the VM

## How It Works

The tool uses the virsh API to:
- Query running VMs
- Parse VM XML configurations to check device attachments
- Generate XML definitions for USB passthrough
- Attach/detach devices using `virsh attach-device` and `virsh detach-device`

Device information is retrieved using the `lsusb` command.

## Examples

### Attach a USB Device

```bash
$ virsh-usb attach
🖥 Select a VM
  > my-windows-vm
    my-linux-vm
    test-vm

🔌 Select a USB device
  > 0dd4:4105 - PaperHandler (Bus 003 Device 006)
    046d:c52b - Logitech USB Receiver (Bus 001 Device 003)

✓ Successfully attached PaperHandler (0dd4:4105) to my-windows-vm
```

### Check Status

```bash
$ virsh-usb --vm my-windows-vm --device 0dd4:4105 status
🖥 VM (my-windows-vm): Running
🔌 PaperHandler (0dd4:4105): Connected (Bus 003 Device 006)
🔗 Attachment Status: Attached to VM
```

## Requirements for VM

Your VM must be running to attach or detach USB devices. The tool uses "live" attachment, which means devices are attached to the running VM without needing to restart it.

## Permissions

You may need appropriate permissions to use virsh commands. If you're not in the `libvirt` group, you might need to run the tool with `sudo` or add your user to the group:

```bash
sudo usermod -aG libvirt $USER
```

## Configuration

The tool saves the last used VM in your system's config directory:
- Linux: `~/.config/virsh-usb/last_vm`

## Troubleshooting

### "VM is not running"
The VM must be running (not paused or stopped) to attach or detach devices.

### "USB device not found"
Make sure the USB device is physically connected to your host system. Run `lsusb` to verify.

### "Permission denied"
Ensure you have proper permissions to use virsh. Add your user to the `libvirt` group or use sudo.

## License

MIT License - see LICENSE file for details.

## Author

Brian Donovan

## Contributing

Contributions are welcome! Please feel free to submit issues or pull requests.
