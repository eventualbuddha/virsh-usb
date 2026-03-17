# virsh-usb

A command-line tool for managing USB device attachment to virsh (libvirt/KVM) VMs with an intuitive interactive interface.

## Features

- Attach and detach physical USB devices to/from running VMs
- Create and manage named virtual USB flash drives (qcow2-backed)
- Create and manage virtual USB HID keyboards with configurable VID/PID
- Type text into VMs via virtual HID keyboard
- Interactive selection of VMs and devices — physical, storage, and HID in one list
- Search physical devices by name or vendor:product ID
- Shows device attachment status
- Remembers your last used VM
- Color-coded output for better readability

## Prerequisites

- Linux system with libvirt/KVM installed
- `virsh` command-line tool
- `lsusb` utility (usually from the `usbutils` package)
- `vhci-hcd` kernel module (for virtual HID devices)
- Rust toolchain (for building from source)

## Installation

### From Source

```bash
cargo build --release
sudo cp target/release/virsh-usb /usr/local/bin/
```

### Permissions

Add yourself to the `libvirt` group for virsh access:

```bash
sudo usermod -aG libvirt $USER
```

For virtual HID devices, also add yourself to `plugdev` and install the udev rules:

```bash
sudo usermod -aG plugdev $USER

sudo tee /etc/udev/rules.d/99-virsh-usb.rules > /dev/null <<'EOF'
# Allow plugdev group to attach/detach vhci_hcd devices
SUBSYSTEM=="platform", DRIVER=="vhci_hcd", RUN+="/bin/sh -c 'chown root:plugdev /sys%p/attach /sys%p/detach && chmod 0660 /sys%p/attach /sys%p/detach'"
# Allow kvm group (libvirt-qemu) write access to vhci_hcd USB device files
SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", ENV{ID_PATH}=="platform-vhci_hcd*", GROUP="kvm", MODE="0660"
# Auto-unbind host kernel drivers from vhci_hcd interfaces so QEMU can claim them
ACTION=="bind", SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_interface", ENV{ID_PATH}=="platform-vhci_hcd*", RUN+="/bin/sh -c 'echo %k > /sys/bus/usb/drivers/usbhid/unbind 2>/dev/null; echo %k > /sys/bus/usb/drivers/hid-generic/unbind 2>/dev/null'"
EOF

sudo udevadm control --reload-rules

echo vhci-hcd | sudo tee /etc/modules-load.d/virsh-usb.conf
```

Log out and back in for group changes to take effect.

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
virsh-usb storage create MyDrive
virsh-usb storage create MyDrive --size 8G

# List all virtual drives and their attachment status
virsh-usb storage list

# Delete a virtual drive (must be detached first)
virsh-usb storage delete MyDrive

# Attach/detach non-interactively
virsh-usb --vm myvm --device MyDrive attach
virsh-usb --vm myvm --device MyDrive detach
```

### Virtual USB HID Keyboards

Virtual HID devices appear to the guest as a USB keyboard with your chosen VID/PID. Useful for presenting as a specific scanner, card reader, or input device.

```bash
# Create a virtual HID device
virsh-usb hid create MyScanner --vid 0x0c2e --pid 0x0b61

# List all virtual HID devices
virsh-usb hid list

# Delete a virtual HID device
virsh-usb hid delete MyScanner

# Type text into the VM (appends Enter by default)
virsh-usb hid type "Hello, world!"

# Type text without Enter
virsh-usb hid type "scan data" --no-enter

# Specify VM and device explicitly
virsh-usb hid type "Hello" --vm myvm --device MyScanner
```

Attach and detach HID devices using the standard commands:

```bash
virsh-usb --vm myvm --device MyScanner attach
virsh-usb --vm myvm --device MyScanner detach
```

### Interactive Mode

When you run commands without device flags, the tool prompts you interactively:

1. **VM Selection**: Choose from a list of all your VMs (defaults to last used)
2. **Device Selection**: Choose from a flat list of all device types

```
[USB]     0dd4:4105 - PaperHandler (Bus 003 Device 006)
[USB]     046d:c52b - Logitech USB Receiver (Bus 001 Device 003) [attached]
[STORAGE] MyDrive (8G)
[STORAGE] BackupDrive (16G) [attached]
[HID]     MyScanner (0c2e:0b61)
[HID]     MyReader (08fc:0012) [attached]
+ Create new storage drive...
+ Create new HID device...
```

- **Attach**: shows all devices; options to create new storage or HID devices inline
- **Detach**: shows only devices currently attached to the VM

## How It Works

### Physical USB Passthrough

Uses `virsh attach-device` / `virsh detach-device` with a USB hostdev XML definition. Device information is retrieved with `lsusb`.

### Virtual Storage Drives

Creates qcow2 disk images via `virsh vol-create-as` and attaches them as USB mass storage (`bus='usb'`) using `virsh attach-device`.

### Virtual HID Devices

Uses the USB/IP protocol and the `vhci-hcd` kernel module:

1. A daemon process implements a USB/IP server that emulates a HID keyboard with the configured VID/PID
2. The main process performs the USB/IP IMPORT handshake, then passes the socket to `vhci_hcd` via sysfs
3. The device appears in `lsusb` with the correct VID/PID
4. `virsh attach-device` passes it through to the guest VM
5. `virsh-usb hid type` sends text to the daemon via a Unix socket, which injects HID key reports

Daemon state files are stored in `~/.local/share/virsh-usb/`.

## Configuration

The tool stores data in:

- `~/.config/virsh-usb/last_vm` — last selected VM
- `~/.local/share/virsh-usb/drives.json` — virtual drive metadata
- `~/.local/share/virsh-usb/hids.json` — virtual HID device metadata
- `~/.local/share/virsh-usb/hid-<name>.*` — daemon state files (pid, port, socket, vhci port)

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

### HID device: "vhci_hcd sysfs directory not found"
The `vhci-hcd` module is not loaded. Load it manually:
```bash
sudo modprobe vhci-hcd
```
To load it automatically at boot, see the installation instructions above.

### HID device: "Failed to write to vhci_hcd attach"
Your user needs write access to the vhci_hcd sysfs files. Install the udev rule and add yourself to the `plugdev` group as described in the Permissions section, then reload the module:
```bash
sudo modprobe -r vhci-hcd && sudo modprobe vhci-hcd
```

## License

MIT License — see LICENSE file for details.

## Author

Brian Donovan
