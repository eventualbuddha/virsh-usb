# virsh-usb

A command-line tool for managing USB device attachment to virsh (libvirt/KVM) VMs with an intuitive interactive interface.

## Features

- Attach and detach physical USB devices to/from running VMs
- Create and manage named virtual USB flash drives (qcow2-backed)
- Interactive selection of VMs and devices — physical and virtual in one list
- Search physical devices by name or vendor:product ID
- Shows device attachment status
- Remembers your last used VM
- Color-coded output for better readability

## Prerequisites

- Linux system with libvirt/KVM installed
- `virsh` command-line tool
- `lsusb` utility (usually from the `usbutils` package)
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

### Physical USB Devices

```bash
# Attach a USB device (interactive mode)
virsh-usb attach

# Detach a USB device (interactive mode)
virsh-usb detach

# Check status of a USB device
virsh-usb status

# Specify VM and device by vendor:product ID
virsh-usb --vm myvm --device 0dd4:4105 attach

# Specify VM and search device by name
virsh-usb --vm myvm --device PaperHandler attach
```

### Virtual USB Flash Drives

Virtual drives are qcow2 disk images stored in libvirt's default storage pool (`/var/lib/libvirt/images/`). The guest OS sees them as USB mass storage devices and can format and mount them normally.

```bash
# Create a virtual drive (default size: 4G)
virsh-usb virtual create MyDrive
virsh-usb virtual create MyDrive --size 8G

# List all virtual drives and their attachment status
virsh-usb virtual list

# Delete a virtual drive (must be detached first)
virsh-usb virtual delete MyDrive

# Attach/detach non-interactively
virsh-usb --vm myvm --virtual-device MyDrive attach
virsh-usb --vm myvm --virtual-device MyDrive detach

# Check status
virsh-usb --vm myvm --virtual-device MyDrive status
```

### Interactive Mode

When you run commands without device flags, the tool prompts you interactively:

1. **VM Selection**: Choose from a list of all your VMs (defaults to last used)
2. **Device Selection**: Choose from a flat list of physical and virtual devices

```
[USB]  0dd4:4105 - PaperHandler (Bus 003 Device 006)
[USB]  046d:c52b - Logitech USB Receiver (Bus 001 Device 003) [attached]
[VIRT] MyDrive (8G)
[VIRT] BackupDrive (16G) [attached]
+ Create new virtual drive...
```

- **Attach**: shows all devices; selecting "Create new virtual drive..." prompts for a name and size
- **Detach**: shows only devices currently attached to the VM

## How It Works

The tool uses the virsh API to:
- Query and select VMs
- Parse VM XML configurations to check device attachments
- Generate XML definitions for USB passthrough and virtual disk attachment
- Attach/detach devices using `virsh attach-device` and `virsh detach-device`
- Create and delete virtual drive images using `virsh vol-create-as` and `virsh vol-delete`

Physical device information is retrieved using `lsusb`. Virtual drive metadata is stored in `~/.local/share/virsh-usb/drives.json`.

## Examples

### Attach a Physical USB Device

```
$ virsh-usb attach
🖥 Select a VM
  > my-windows-vm

🔌 Select a device
  > [USB]  0dd4:4105 - PaperHandler (Bus 003 Device 006)
    [VIRT] MyDrive (8G)
    + Create new virtual drive...

✓ Successfully attached PaperHandler (0dd4:4105) to my-windows-vm
```

### Create and Attach a Virtual Drive

```
$ virsh-usb virtual create WorkDrive --size 4G
✓ Created virtual drive WorkDrive (4G) at /var/lib/libvirt/images/WorkDrive.qcow2

$ virsh-usb --vm my-windows-vm --virtual-device WorkDrive attach
✓ Successfully attached virtual drive WorkDrive to my-windows-vm
```

### List Virtual Drives

```
$ virsh-usb virtual list
Virtual USB Flash Drives:

  WorkDrive            4G     attached to: my-windows-vm
  /var/lib/libvirt/images/WorkDrive.qcow2

  BackupDrive          16G    not attached
  /var/lib/libvirt/images/BackupDrive.qcow2
```

### Check Status

```
$ virsh-usb --vm my-windows-vm --device 0dd4:4105 status
🖥 VM (my-windows-vm): Running
🔌 PaperHandler (0dd4:4105): Connected (Bus 003 Device 006)
🔗 Attachment Status: Attached to VM
```

## Requirements for VM

Your VM must be running to attach or detach devices. The tool uses live attachment, so devices are added to the running VM without a restart.

## Permissions

You may need appropriate permissions to use virsh commands. If you're not in the `libvirt` group, add your user and log in again:

```bash
sudo usermod -aG libvirt $USER
```

## Configuration

The tool stores data in two locations:

- `~/.config/virsh-usb/last_vm` — last selected VM
- `~/.local/share/virsh-usb/drives.json` — virtual drive metadata

Virtual drive images are stored in libvirt's default storage pool, typically `/var/lib/libvirt/images/`.

## Troubleshooting

### "VM is not running"
The VM must be running (not paused or stopped) to attach or detach devices.

### "USB device not found"
Make sure the USB device is physically connected to your host. Run `lsusb` to verify.

### "Permission denied" / libvirt access errors
Ensure your user is in the `libvirt` group (see Permissions above).

### Virtual drive not found in storage pool
If you see an error about a missing volume, check what's in the default pool:
```bash
virsh vol-list default
```

## License

MIT License - see LICENSE file for details.

## Author

Brian Donovan

## Contributing

Contributions are welcome! Please feel free to submit issues or pull requests.
