use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use console::style;
use directories::ProjectDirs;
use inquire::Select;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// CLI tool to manage USB device attachment to virsh VMs
#[derive(Parser)]
#[command(name = "virsh-usb")]
#[command(about = "Manage USB device attachment to virsh VMs")]
struct Cli {
    /// Name of the virsh VM (if not provided, will prompt interactively)
    #[arg(long)]
    vm: Option<String>,

    /// Device: vid:pid for physical USB, or name for virtual storage/HID devices
    #[arg(long)]
    device: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Attach the device to the VM
    Attach,
    /// Detach the device from the VM
    Detach,
    /// Show current status
    Status,
    /// Manage virtual USB storage drives
    Storage {
        #[command(subcommand)]
        action: StorageCommands,
    },
    /// Manage virtual USB HID devices
    Hid {
        #[command(subcommand)]
        action: HidCommands,
    },
    /// Internal: run USB/IP HID daemon (do not call directly)
    #[command(hide = true)]
    HidDaemon {
        #[arg(long)]
        name: String,
        #[arg(long)]
        vid: String,
        #[arg(long)]
        pid: String,
        #[arg(long)]
        socket_path: String,
        #[arg(long)]
        pid_file: String,
        #[arg(long)]
        port_file: String,
    },
}

#[derive(Subcommand)]
enum StorageCommands {
    /// Create a new virtual USB flash drive
    Create {
        /// Name for the virtual drive
        name: String,
        /// Size of the drive (e.g., 4G, 8G, 512M)
        #[arg(long, default_value = "4G")]
        size: String,
    },
    /// List all virtual USB flash drives
    List,
    /// Delete a virtual USB flash drive
    Delete {
        /// Name of the virtual drive to delete
        name: String,
    },
}

#[derive(Subcommand)]
enum HidCommands {
    /// Create a new virtual USB HID device
    Create {
        /// Name for the HID device
        name: String,
        /// USB Vendor ID (e.g., 0x0c2e)
        #[arg(long)]
        vid: String,
        /// USB Product ID (e.g., 0x0b61)
        #[arg(long)]
        pid: String,
    },
    /// List all virtual USB HID devices
    List,
    /// Delete a virtual USB HID device
    Delete {
        /// Name of the HID device to delete
        name: String,
    },
    /// Type a string into the VM via the HID device
    Type {
        /// String to type
        text: String,
        /// Name of the VM (if not provided, will prompt interactively)
        #[arg(long)]
        vm: Option<String>,
        /// Name of the HID device (if not provided, will prompt interactively)
        #[arg(long)]
        device: Option<String>,
        /// Do not append Enter after typing
        #[arg(long)]
        no_enter: bool,
    },
}

// ============================================================
// Virtual storage types
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VirtualDrive {
    name: String,
    size: String,
    created_at_secs: u64,
}

/// Represents a virtual disk attachment found in a VM's XML
#[derive(Debug, Clone)]
struct VirtualAttachment {
    source_file: String,
    target_dev: String,
}

// ============================================================
// HID device types
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HidDevice {
    name: String,
    vid: String,
    pid: String,
}

// ============================================================
// Unified device choice for interactive selection
// ============================================================

#[derive(Debug, Clone)]
enum DeviceChoice {
    RealUsb(UsbDevice),
    Storage(VirtualDrive, bool),
    Hid(HidDevice, bool),
    CreateNewStorage,
    CreateNewHid,
}

impl std::fmt::Display for DeviceChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceChoice::RealUsb(dev) => {
                let id = style(format!("{}:{}", dev.vendor_id, dev.product_id)).cyan();
                let bus_info = style(format!("(Bus {} Device {})", dev.bus, dev.device)).dim();
                if dev.attached {
                    write!(
                        f,
                        "[USB]     {} - {} {} {}",
                        id,
                        dev.name,
                        bus_info,
                        style("[attached]").green()
                    )
                } else {
                    write!(f, "[USB]     {} - {} {}", id, dev.name, bus_info)
                }
            }
            DeviceChoice::Storage(drive, is_attached) => {
                if *is_attached {
                    write!(
                        f,
                        "[STORAGE] {} ({}) {}",
                        style(&drive.name).cyan(),
                        drive.size,
                        style("[attached]").green()
                    )
                } else {
                    write!(
                        f,
                        "[STORAGE] {} ({})",
                        style(&drive.name).cyan(),
                        drive.size
                    )
                }
            }
            DeviceChoice::Hid(device, is_attached) => {
                if *is_attached {
                    write!(
                        f,
                        "[HID]     {} ({}:{}) {}",
                        style(&device.name).cyan(),
                        device.vid,
                        device.pid,
                        style("[attached]").green()
                    )
                } else {
                    write!(
                        f,
                        "[HID]     {} ({}:{})",
                        style(&device.name).cyan(),
                        device.vid,
                        device.pid
                    )
                }
            }
            DeviceChoice::CreateNewStorage => {
                write!(f, "{}", style("+ Create new storage drive...").dim())
            }
            DeviceChoice::CreateNewHid => {
                write!(f, "{}", style("+ Create new HID device...").dim())
            }
        }
    }
}

// ============================================================
// Name sanitization
// ============================================================

/// Convert an arbitrary string into a valid device name (letters, digits, hyphens, underscores).
/// Invalid characters are replaced with hyphens; consecutive hyphens are collapsed; leading/trailing
/// hyphens are trimmed. Returns the original string unchanged if it is already valid.
fn sanitize_device_name(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();

    // Collapse consecutive hyphens
    let mut result = String::with_capacity(replaced.len());
    let mut prev_hyphen = false;
    for c in replaced.chars() {
        if c == '-' {
            if !prev_hyphen {
                result.push(c);
            }
            prev_hyphen = true;
        } else {
            result.push(c);
            prev_hyphen = false;
        }
    }

    result.trim_matches('-').to_string()
}

// ============================================================
// Core helpers
// ============================================================

fn run_command(args: &[&str]) -> Result<String> {
    if args.is_empty() {
        return Err(anyhow!("Command arguments cannot be empty"));
    }

    let output = Command::new(args[0])
        .args(&args[1..])
        .output()
        .context(format!("Failed to execute command: {}", args.join(" ")))?;

    if !output.status.success() {
        return Err(anyhow!(
            "Command failed: {}\nError: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn find_usb_device(vendor_id: &str, product_id: &str) -> Result<Option<(String, String)>> {
    let output = run_command(&["lsusb"])?;

    for line in output.lines() {
        if line.contains(&format!("{}:{}", vendor_id, product_id)) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 4 {
                let bus = parts[1].to_string();
                let device = parts[3].trim_end_matches(':').to_string();
                return Ok(Some((bus, device)));
            }
        }
    }

    Ok(None)
}

fn check_vm_running(vm_name: &str) -> Result<bool> {
    let output = run_command(&["virsh", "list", "--name"])?;
    Ok(output.lines().any(|line| line.trim() == vm_name))
}

fn get_attached_devices(vm_name: &str) -> Result<Vec<(String, String)>> {
    let output = match run_command(&["virsh", "dumpxml", vm_name]) {
        Ok(o) => o,
        Err(_) => return Ok(vec![]),
    };

    let mut devices = Vec::new();
    let mut in_hostdev = false;
    let mut current_vendor: Option<String> = None;
    let mut current_product: Option<String> = None;

    for line in output.lines() {
        let trimmed = line.trim();

        if trimmed.contains("<hostdev") && trimmed.contains("usb") {
            in_hostdev = true;
        } else if trimmed.contains("</hostdev>") {
            if in_hostdev
                && let (Some(vendor), Some(product)) = (&current_vendor, &current_product)
            {
                devices.push((vendor.clone(), product.clone()));
            }
            in_hostdev = false;
            current_vendor = None;
            current_product = None;
        } else if in_hostdev {
            if trimmed.contains("<vendor id=")
                && let Some(id) = extract_id(trimmed)
            {
                current_vendor = Some(id);
            } else if trimmed.contains("<product id=")
                && let Some(id) = extract_id(trimmed)
            {
                current_product = Some(id);
            }
        }
    }

    Ok(devices)
}

fn extract_id(line: &str) -> Option<String> {
    line.split('\'')
        .nth(1)
        .map(|s| s.trim_start_matches("0x").to_string())
}

/// Extract an XML attribute value from a line, e.g. file='/path/to/file' → "/path/to/file"
fn extract_attr_value(line: &str, attr: &str) -> Option<String> {
    let needle = format!("{}='", attr);
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}

fn is_device_attached(vm_name: &str, vendor_id: &str, product_id: &str) -> Result<bool> {
    let devices = get_attached_devices(vm_name)?;
    Ok(devices
        .iter()
        .any(|(v, p)| v == vendor_id && p == product_id))
}

// ============================================================
// Real USB attach / detach / status
// ============================================================

fn attach_device(vm_name: &str, vendor_id: &str, product_id: &str) -> Result<()> {
    if !check_vm_running(vm_name)? {
        return Err(anyhow!("VM '{}' is not running", vm_name));
    }

    if is_device_attached(vm_name, vendor_id, product_id)? {
        println!("Device is already attached to {}", vm_name);
        return Ok(());
    }

    let all_devices = get_all_usb_devices()?;
    let device = all_devices
        .iter()
        .find(|d| d.vendor_id == vendor_id && d.product_id == product_id)
        .ok_or_else(|| {
            anyhow!(
                "USB device {}:{} not found\nMake sure the USB device is connected",
                vendor_id,
                product_id
            )
        })?;

    let xml_content = format!(
        r#"<hostdev mode='subsystem' type='usb' managed='yes'>
  <source>
    <vendor id='0x{vendor_id}'/>
    <product id='0x{product_id}'/>
  </source>
</hostdev>
"#
    );

    let temp_file = "/tmp/virsh-usb-attach.xml";
    fs::write(temp_file, &xml_content).context("Failed to write temporary XML file")?;

    let result = run_command(&["virsh", "attach-device", vm_name, temp_file, "--live"]);
    let _ = fs::remove_file(temp_file);
    result?;

    println!(
        "{} Successfully attached {} {} to {}",
        style("✓").green().bold(),
        style(&device.name).cyan(),
        style(format!("({}:{})", vendor_id, product_id)).dim(),
        style(vm_name).yellow()
    );

    Ok(())
}

fn detach_device(vm_name: &str, vendor_id: &str, product_id: &str) -> Result<()> {
    if !check_vm_running(vm_name)? {
        return Err(anyhow!("VM '{}' is not running", vm_name));
    }

    if !is_device_attached(vm_name, vendor_id, product_id)? {
        println!("Device is not attached to {}", vm_name);
        return Ok(());
    }

    let all_devices = get_all_usb_devices()?;
    let device_name = all_devices
        .iter()
        .find(|d| d.vendor_id == vendor_id && d.product_id == product_id)
        .map(|d| d.name.clone())
        .unwrap_or_else(|| "Unknown Device".to_string());

    let xml_content = format!(
        r#"<hostdev mode='subsystem' type='usb' managed='yes'>
  <source>
    <vendor id='0x{vendor_id}'/>
    <product id='0x{product_id}'/>
  </source>
</hostdev>
"#
    );

    let temp_file = "/tmp/virsh-usb-detach.xml";
    fs::write(temp_file, &xml_content).context("Failed to write temporary XML file")?;

    let result = run_command(&["virsh", "detach-device", vm_name, temp_file, "--live"]);
    let _ = fs::remove_file(temp_file);
    result?;

    println!(
        "{} Successfully detached {} {} from {}",
        style("✓").green().bold(),
        style(&device_name).cyan(),
        style(format!("({}:{})", vendor_id, product_id)).dim(),
        style(vm_name).yellow()
    );

    Ok(())
}

fn show_status(vm_name: &str, vendor_id: &str, product_id: &str) -> Result<()> {
    let vm_running = check_vm_running(vm_name)?;
    println!(
        "{} VM ({}): {}",
        style("🖥").cyan(),
        style(vm_name).yellow(),
        if vm_running {
            style("Running").green()
        } else {
            style("Not running").red()
        }
    );

    let all_devices = get_all_usb_devices().unwrap_or_default();
    let device_name = all_devices
        .iter()
        .find(|d| d.vendor_id == vendor_id && d.product_id == product_id)
        .map(|d| d.name.clone())
        .unwrap_or_else(|| "Unknown Device".to_string());

    if let Some((bus, device)) = find_usb_device(vendor_id, product_id)? {
        println!(
            "{} {} {}: {} (Bus {} Device {})",
            style("🔌").cyan(),
            style(&device_name).cyan(),
            style(format!("({}:{})", vendor_id, product_id)).dim(),
            style("Connected").green(),
            bus,
            device
        );
    } else {
        println!(
            "{} {} {}: {}",
            style("🔌").cyan(),
            style(&device_name).cyan(),
            style(format!("({}:{})", vendor_id, product_id)).dim(),
            style("Not found").red()
        );
    }

    if vm_running {
        let attached = is_device_attached(vm_name, vendor_id, product_id)?;
        println!(
            "{} Attachment Status: {}",
            style("🔗").cyan(),
            if attached {
                style("Attached to VM").green()
            } else {
                style("Not attached to VM").yellow()
            }
        );
    }

    Ok(())
}

// ============================================================
// Config / persistence helpers
// ============================================================

fn get_config_file() -> Result<PathBuf> {
    let proj_dirs = ProjectDirs::from("", "", "virsh-usb")
        .ok_or_else(|| anyhow!("Could not determine config directory"))?;
    let config_dir = proj_dirs.config_dir();
    fs::create_dir_all(config_dir)?;
    Ok(config_dir.join("last_vm"))
}

fn save_last_vm(vm: &str) -> Result<()> {
    let config_file = get_config_file()?;
    fs::write(config_file, vm)?;
    Ok(())
}

fn load_last_vm() -> Option<String> {
    let config_file = get_config_file().ok()?;
    fs::read_to_string(config_file).ok()
}

fn get_data_dir() -> Result<PathBuf> {
    let proj_dirs = ProjectDirs::from("", "", "virsh-usb")
        .ok_or_else(|| anyhow!("Could not determine data directory"))?;
    let data_dir = proj_dirs.data_dir().to_path_buf();
    fs::create_dir_all(&data_dir)?;
    Ok(data_dir)
}

fn get_drives_file() -> Result<PathBuf> {
    Ok(get_data_dir()?.join("drives.json"))
}

fn load_virtual_drives() -> Result<Vec<VirtualDrive>> {
    let path = get_drives_file()?;
    if !path.exists() {
        return Ok(vec![]);
    }
    let content = fs::read_to_string(&path)
        .context(format!("Failed to read {}", path.display()))?;
    serde_json::from_str(&content).context(format!(
        "Failed to parse drives.json. The file may be corrupted. You can delete it at: {}",
        path.display()
    ))
}

fn save_virtual_drives(drives: &[VirtualDrive]) -> Result<()> {
    let path = get_drives_file()?;
    let content = serde_json::to_string_pretty(drives)?;
    fs::write(path, content)?;
    Ok(())
}

fn get_hid_devices_file() -> Result<PathBuf> {
    Ok(get_data_dir()?.join("hid-devices.json"))
}

fn load_hid_devices() -> Result<Vec<HidDevice>> {
    let path = get_hid_devices_file()?;
    if !path.exists() {
        return Ok(vec![]);
    }
    let content = fs::read_to_string(&path)
        .context(format!("Failed to read {}", path.display()))?;
    serde_json::from_str(&content).context(format!(
        "Failed to parse hid-devices.json. The file may be corrupted. You can delete it at: {}",
        path.display()
    ))
}

fn save_hid_devices(devices: &[HidDevice]) -> Result<()> {
    let path = get_hid_devices_file()?;
    let content = serde_json::to_string_pretty(devices)?;
    fs::write(path, content)?;
    Ok(())
}

/// Ask libvirt for the absolute path of a volume in the default pool.
fn get_vol_path(name: &str) -> Result<String> {
    let vol_name = format!("{}.qcow2", name);
    let output = run_command(&["virsh", "vol-path", &vol_name, "--pool", "default"])?;
    Ok(output.trim().to_string())
}

// HID daemon state file helpers
fn hid_pid_file(name: &str) -> Result<PathBuf> {
    Ok(get_data_dir()?.join(format!("hid-{}.pid", name)))
}

fn hid_port_file(name: &str) -> Result<PathBuf> {
    Ok(get_data_dir()?.join(format!("hid-{}.port", name)))
}

fn hid_sock_file(name: &str) -> Result<PathBuf> {
    Ok(get_data_dir()?.join(format!("hid-{}.sock", name)))
}

fn hid_vhci_port_file(name: &str) -> Result<PathBuf> {
    Ok(get_data_dir()?.join(format!("hid-{}.vhci-port", name)))
}

// ============================================================
// VM listing / selection
// ============================================================

fn get_all_vms() -> Result<Vec<String>> {
    let output = run_command(&["virsh", "list", "--all", "--name"])?;
    Ok(output
        .lines()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect())
}

fn select_vm() -> Result<String> {
    let vms = get_all_vms()?;

    if vms.is_empty() {
        return Err(anyhow!("No VMs found"));
    }

    let last_vm = load_last_vm();
    let default_index = last_vm
        .as_ref()
        .and_then(|vm| vms.iter().position(|v| v == vm))
        .unwrap_or(0);

    let selection = Select::new(&format!("{} Select a VM", style("🖥").cyan()), vms.clone())
        .with_starting_cursor(default_index)
        .prompt()
        .context("Failed to select VM")?;

    Ok(selection)
}

// ============================================================
// Real USB device listing
// ============================================================

#[derive(Debug, Clone)]
struct UsbDevice {
    bus: String,
    device: String,
    vendor_id: String,
    product_id: String,
    name: String,
    attached: bool,
}

fn get_all_usb_devices() -> Result<Vec<UsbDevice>> {
    let output = run_command(&["lsusb"])?;
    let mut devices = Vec::new();

    for line in output.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 6 {
            let bus = parts[1].to_string();
            let device = parts[3].trim_end_matches(':').to_string();

            if let Some(id_part) = parts.get(5) {
                let id_parts: Vec<&str> = id_part.split(':').collect();
                if id_parts.len() == 2 {
                    let vendor_id = id_parts[0].to_string();
                    let product_id = id_parts[1].to_string();
                    let name = parts[6..].join(" ");

                    devices.push(UsbDevice {
                        bus,
                        device,
                        vendor_id,
                        product_id,
                        name,
                        attached: false,
                    });
                }
            }
        }
    }

    Ok(devices)
}

fn find_device_by_name(
    name: &str,
    vm_name: Option<&str>,
    filter_attached: bool,
) -> Result<(String, String)> {
    let mut devices = get_all_usb_devices()?;

    let attached_devices = if let Some(vm) = vm_name {
        get_attached_devices(vm).unwrap_or_default()
    } else {
        vec![]
    };

    for device in &mut devices {
        device.attached = attached_devices
            .iter()
            .any(|(v, p)| v == &device.vendor_id && p == &device.product_id);
    }

    if filter_attached {
        devices.retain(|d| d.attached);
    }

    let name_lower = name.to_lowercase();
    let matches: Vec<_> = devices
        .iter()
        .filter(|d| d.name.to_lowercase().contains(&name_lower))
        .collect();

    match matches.len() {
        0 => Err(anyhow!("No USB device found matching name: '{}'", name)),
        1 => Ok((matches[0].vendor_id.clone(), matches[0].product_id.clone())),
        _ => {
            let device_strings: Vec<String> = matches
                .iter()
                .map(|d| {
                    format!(
                        "{}:{} - {} (Bus {} Device {})",
                        d.vendor_id, d.product_id, d.name, d.bus, d.device
                    )
                })
                .collect();
            let selection = Select::new(
                &format!(
                    "{} Multiple devices match '{}'. Select one",
                    style("🔌").cyan(),
                    name
                ),
                device_strings.clone(),
            )
            .with_starting_cursor(0)
            .prompt()
            .context("Failed to select USB device")?;

            let idx = device_strings
                .iter()
                .position(|s| s == &selection)
                .ok_or_else(|| anyhow!("Selected device not found"))?;

            Ok((matches[idx].vendor_id.clone(), matches[idx].product_id.clone()))
        }
    }
}

// ============================================================
// Virtual storage attachment helpers
// ============================================================

fn get_attached_virtual_devices(vm_name: &str) -> Result<Vec<VirtualAttachment>> {
    let output = match run_command(&["virsh", "dumpxml", vm_name]) {
        Ok(o) => o,
        Err(_) => return Ok(vec![]),
    };

    let mut attachments = Vec::new();
    let mut in_disk = false;
    let mut current_source: Option<String> = None;
    let mut current_target: Option<String> = None;
    let mut current_is_usb = false;

    for line in output.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("<disk")
            && trimmed.contains("type='file'")
            && trimmed.contains("device='disk'")
        {
            in_disk = true;
            current_source = None;
            current_target = None;
            current_is_usb = false;
        } else if trimmed == "</disk>" {
            if in_disk && current_is_usb
                && let (Some(src), Some(tgt)) = (current_source.take(), current_target.take())
            {
                attachments.push(VirtualAttachment {
                    source_file: src,
                    target_dev: tgt,
                });
            }
            in_disk = false;
            current_source = None;
            current_target = None;
            current_is_usb = false;
        } else if in_disk {
            if trimmed.contains("<source file=") {
                current_source = extract_attr_value(trimmed, "file");
            }
            if trimmed.contains("<target") && trimmed.contains("bus='usb'") {
                current_is_usb = true;
                current_target = extract_attr_value(trimmed, "dev");
            }
        }
    }

    Ok(attachments)
}

fn is_virtual_drive_attached(vm_name: &str, drive: &VirtualDrive) -> Result<bool> {
    let image_path_str = get_vol_path(&drive.name)?;
    let attachments = get_attached_virtual_devices(vm_name)?;
    Ok(attachments.iter().any(|a| a.source_file == image_path_str))
}

/// Find the next available sd* target device name for USB disks in a VM.
fn get_next_target_dev(vm_name: &str) -> Result<String> {
    let output = run_command(&["virsh", "dumpxml", vm_name]).unwrap_or_default();

    let used: std::collections::HashSet<String> = output
        .lines()
        .map(str::trim)
        .filter(|l| l.contains("<target") && l.contains("dev="))
        .filter_map(|l| extract_attr_value(l, "dev"))
        .collect();

    for c in b'a'..=b'z' {
        let name = format!("sd{}", c as char);
        if !used.contains(&name) {
            return Ok(name);
        }
    }

    Err(anyhow!(
        "No available USB disk target devices (all sda-sdz are in use)"
    ))
}

// ============================================================
// Virtual storage lifecycle
// ============================================================

fn create_virtual_drive(name: &str, size: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!("Drive name cannot be empty"));
    }
    let name = sanitize_device_name(name);
    if name.is_empty() {
        return Err(anyhow!("Drive name is empty after sanitization"));
    }
    let name = name.as_str();

    let mut drives = load_virtual_drives()?;
    if drives.iter().any(|d| d.name == name) {
        return Err(anyhow!("A storage drive named '{}' already exists", name));
    }
    let hid_devices = load_hid_devices()?;
    if hid_devices.iter().any(|d| d.name == name) {
        return Err(anyhow!(
            "An HID device named '{}' already exists. Device names must be unique across all types.",
            name
        ));
    }

    let vol_name = format!("{}.qcow2", name);
    run_command(&[
        "virsh", "vol-create-as", "default", &vol_name, size, "--format", "qcow2",
    ])?;

    let created_at_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    drives.push(VirtualDrive {
        name: name.to_string(),
        size: size.to_string(),
        created_at_secs,
    });
    save_virtual_drives(&drives)?;

    println!(
        "{} Created storage drive {} ({}) at /var/lib/libvirt/images/{}",
        style("✓").green().bold(),
        style(name).cyan(),
        size,
        vol_name
    );

    Ok(())
}

fn delete_virtual_drive(name: &str) -> Result<()> {
    let mut drives = load_virtual_drives()?;
    let idx = drives
        .iter()
        .position(|d| d.name == name)
        .ok_or_else(|| anyhow!("No storage drive named '{}'", name))?;

    let running_vms = run_command(&["virsh", "list", "--name"]).unwrap_or_default();
    let drive = &drives[idx];
    for vm in running_vms
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if is_virtual_drive_attached(vm, drive)? {
            return Err(anyhow!(
                "Storage drive '{}' is currently attached to VM '{}'. Detach it first.",
                name,
                vm
            ));
        }
    }

    let vol_name = format!("{}.qcow2", name);
    match run_command(&["virsh", "vol-delete", &vol_name, "--pool", "default"]) {
        Ok(_) => {}
        Err(e) => eprintln!("Warning: could not delete volume: {}", e),
    }

    drives.remove(idx);
    save_virtual_drives(&drives)?;

    println!(
        "{} Deleted storage drive {}",
        style("✓").green().bold(),
        style(name).cyan()
    );

    Ok(())
}

fn list_virtual_drives() -> Result<()> {
    let drives = load_virtual_drives()?;

    if drives.is_empty() {
        println!("No storage drives found. Create one with:");
        println!("  virsh-usb storage create <name>");
        return Ok(());
    }

    let running_vms: Vec<String> = run_command(&["virsh", "list", "--name"])
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    let mut attachment_map: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for vm in &running_vms {
        if let Ok(attachments) = get_attached_virtual_devices(vm) {
            for a in attachments {
                attachment_map.insert(a.source_file, vm.clone());
            }
        }
    }

    println!("Virtual USB Storage Drives:");
    println!();

    for drive in &drives {
        match get_vol_path(&drive.name) {
            Ok(image_path_str) => {
                let status = if let Some(vm) = attachment_map.get(&image_path_str) {
                    style(format!("attached to: {}", vm)).green().to_string()
                } else {
                    style("not attached").dim().to_string()
                };
                println!(
                    "  {:<20} {:<6} {}",
                    style(&drive.name).cyan(),
                    drive.size,
                    status,
                );
                println!("  {}", style(&image_path_str).dim());
            }
            Err(_) => {
                println!(
                    "  {:<20} {:<6} {}",
                    style(&drive.name).cyan(),
                    drive.size,
                    style("[image missing]").red(),
                );
            }
        }
        println!();
    }

    Ok(())
}

fn attach_virtual_drive(vm_name: &str, drive_name: &str) -> Result<()> {
    let drives = load_virtual_drives()?;
    let drive = drives
        .iter()
        .find(|d| d.name == drive_name)
        .ok_or_else(|| anyhow!("No storage drive named '{}'", drive_name))?;

    if !check_vm_running(vm_name)? {
        return Err(anyhow!("VM '{}' is not running", vm_name));
    }

    if is_virtual_drive_attached(vm_name, drive)? {
        println!(
            "Storage drive '{}' is already attached to {}",
            drive_name, vm_name
        );
        return Ok(());
    }

    let image_path_str = get_vol_path(drive_name).with_context(|| {
        format!(
            "Storage drive '{}' not found in libvirt storage pool. Try: virsh vol-list default",
            drive_name
        )
    })?;
    let target_dev = get_next_target_dev(vm_name)?;

    let xml_content = format!(
        "<disk type='file' device='disk'>\n  <driver name='qemu' type='qcow2'/>\n  <source file='{}'/>\n  <target dev='{}' bus='usb'/>\n</disk>\n",
        image_path_str, target_dev
    );

    let temp_file = "/tmp/virsh-usb-storage-attach.xml";
    fs::write(temp_file, &xml_content).context("Failed to write temporary XML file")?;

    let result = run_command(&["virsh", "attach-device", vm_name, temp_file, "--live"]);
    let _ = fs::remove_file(temp_file);
    result?;

    println!(
        "{} Successfully attached storage drive {} to {}",
        style("✓").green().bold(),
        style(drive_name).cyan(),
        style(vm_name).yellow()
    );

    Ok(())
}

fn detach_virtual_drive(vm_name: &str, drive_name: &str) -> Result<()> {
    let drives = load_virtual_drives()?;
    drives
        .iter()
        .find(|d| d.name == drive_name)
        .ok_or_else(|| anyhow!("No storage drive named '{}'", drive_name))?;

    if !check_vm_running(vm_name)? {
        return Err(anyhow!("VM '{}' is not running", vm_name));
    }

    let image_path_str = get_vol_path(drive_name).with_context(|| {
        format!(
            "Storage drive '{}' not found in libvirt storage pool. Try: virsh vol-list default",
            drive_name
        )
    })?;

    let attachments = get_attached_virtual_devices(vm_name)?;
    let attachment = attachments
        .iter()
        .find(|a| a.source_file == image_path_str)
        .ok_or_else(|| {
            anyhow!(
                "Storage drive '{}' is not attached to VM '{}'",
                drive_name,
                vm_name
            )
        })?;

    let xml_content = format!(
        "<disk type='file' device='disk'>\n  <driver name='qemu' type='qcow2'/>\n  <source file='{}'/>\n  <target dev='{}' bus='usb'/>\n</disk>\n",
        image_path_str, attachment.target_dev
    );

    let temp_file = "/tmp/virsh-usb-storage-detach.xml";
    fs::write(temp_file, &xml_content).context("Failed to write temporary XML file")?;

    let result = run_command(&["virsh", "detach-device", vm_name, temp_file, "--live"]);
    let _ = fs::remove_file(temp_file);
    result?;

    println!(
        "{} Successfully detached storage drive {} from {}",
        style("✓").green().bold(),
        style(drive_name).cyan(),
        style(vm_name).yellow()
    );

    Ok(())
}

fn show_virtual_status(vm_name: &str, drive_name: &str) -> Result<()> {
    let drives = load_virtual_drives()?;
    let drive = drives
        .iter()
        .find(|d| d.name == drive_name)
        .ok_or_else(|| anyhow!("No storage drive named '{}'", drive_name))?;

    let vm_running = check_vm_running(vm_name)?;
    println!(
        "{} VM ({}): {}",
        style("🖥").cyan(),
        style(vm_name).yellow(),
        if vm_running {
            style("Running").green()
        } else {
            style("Not running").red()
        }
    );

    match get_vol_path(drive_name) {
        Ok(image_path_str) => println!(
            "{} Storage Drive ({}): {} at {}",
            style("💾").cyan(),
            style(drive_name).cyan(),
            style(&drive.size).green(),
            style(&image_path_str).dim()
        ),
        Err(_) => println!(
            "{} Storage Drive ({}): {}",
            style("💾").cyan(),
            style(drive_name).cyan(),
            style("Image missing from storage pool").red()
        ),
    }

    if vm_running {
        let attached = is_virtual_drive_attached(vm_name, drive)?;
        println!(
            "{} Attachment Status: {}",
            style("🔗").cyan(),
            if attached {
                style("Attached to VM").green()
            } else {
                style("Not attached to VM").yellow()
            }
        );
    }

    Ok(())
}

// ============================================================
// HID keyboard report descriptor
// ============================================================

// Vendor-defined 8-byte raw input report — matches the Honeywell CM4680SR's
// raw HID mode where each report carries up to 8 bytes of barcode ASCII data.
const HID_SCANNER_REPORT_DESC: &[u8] = &[
    0x06, 0x00, 0xFF, // Usage Page (Vendor-Defined 0xFF00)
    0x09, 0x01,       // Usage (0x01)
    0xA1, 0x01,       // Collection (Application)
    0x09, 0x01,       //   Usage (0x01)
    0x15, 0x00,       //   Logical Minimum (0)
    0x26, 0xFF, 0x00, //   Logical Maximum (255)
    0x75, 0x08,       //   Report Size (8)
    0x95, 0x08,       //   Report Count (8) — 8 raw bytes per report
    0x81, 0x02,       //   Input (Data, Variable, Absolute)
    0xC0,             // End Collection
];

// ============================================================
// USB/IP HID daemon infrastructure
// ============================================================

// USB/IP protocol constants
const USBIP_VERSION: u16 = 0x0111;
const OP_REQ_DEVLIST: u16 = 0x8005;
const OP_REP_DEVLIST: u16 = 0x0005;
const OP_REQ_IMPORT: u16 = 0x8003;
const OP_REP_IMPORT: u16 = 0x0003;
const USBIP_CMD_SUBMIT: u32 = 0x00000001;
const USBIP_CMD_UNLINK: u32 = 0x00000002;
const USBIP_RET_SUBMIT: u32 = 0x00000003;
const USBIP_RET_UNLINK: u32 = 0x00000004;

// Virtual device bus/device numbers
const HID_BUSNUM: u32 = 1;
const HID_DEVNUM: u32 = 1;
const HID_BUSID: &str = "1-1";
const HID_SPEED: u32 = 2; // USB_SPEED_FULL

/// Strip optional `0x`/`0X` prefix and lowercase.
fn normalize_hex_id(s: &str) -> String {
    s.to_lowercase().trim_start_matches("0x").to_string()
}

fn is_hid_daemon_running(name: &str) -> bool {
    let Ok(pid_file) = hid_pid_file(name) else {
        return false;
    };
    let Ok(content) = fs::read_to_string(pid_file) else {
        return false;
    };
    let Ok(pid) = content.trim().parse::<u32>() else {
        return false;
    };
    PathBuf::from(format!("/proc/{}", pid)).exists()
}

fn read_hid_port(name: &str) -> Result<u16> {
    let port_file = hid_port_file(name)?;
    let content =
        fs::read_to_string(&port_file).context("Daemon port file not found — daemon not running?")?;
    content
        .trim()
        .parse::<u16>()
        .context("Invalid port in daemon port file")
}

fn cleanup_hid_state_files(name: &str) {
    let _ = hid_pid_file(name).map(|f| fs::remove_file(f));
    let _ = hid_port_file(name).map(|f| fs::remove_file(f));
    let _ = hid_sock_file(name).map(|f| fs::remove_file(f));
    let _ = hid_vhci_port_file(name).map(|f| fs::remove_file(f));
}

fn stop_hid_daemon(name: &str) {
    if let Ok(pid_file) = hid_pid_file(name) {
        if let Ok(content) = fs::read_to_string(&pid_file) {
            if let Ok(pid) = content.trim().parse::<u32>() {
                let _ = Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .status();
            }
        }
    }
    cleanup_hid_state_files(name);
}

/// Build a 18-byte USB device descriptor.
fn build_device_descriptor(vid: u16, pid: u16) -> Vec<u8> {
    let mut d = vec![
        18u8, // bLength
        0x01, // bDescriptorType = DEVICE
        0x00, 0x02, // bcdUSB = 2.00 (LE)
        0x00, // bDeviceClass
        0x00, // bDeviceSubClass
        0x00, // bDeviceProtocol
        64,   // bMaxPacketSize0
    ];
    d.extend_from_slice(&vid.to_le_bytes());
    d.extend_from_slice(&pid.to_le_bytes());
    d.extend_from_slice(&0x0100u16.to_le_bytes()); // bcdDevice
    d.push(1); // iManufacturer
    d.push(2); // iProduct
    d.push(0); // iSerialNumber
    d.push(1); // bNumConfigurations
    d
}

/// Build the full configuration descriptor (config + interface + HID + endpoint = 34 bytes).
fn build_config_descriptor() -> Vec<u8> {
    let report_desc_len = HID_SCANNER_REPORT_DESC.len() as u16;
    let total_len: u16 = 9 + 9 + 9 + 7;
    let mut d = Vec::new();
    // Configuration descriptor (9 bytes)
    d.push(9);
    d.push(0x02); // CONFIGURATION
    d.extend_from_slice(&total_len.to_le_bytes());
    d.extend_from_slice(&[1, 1, 0, 0x80, 50]);
    // Interface descriptor (9 bytes) — generic HID (no boot subclass/protocol)
    d.extend_from_slice(&[9, 0x04, 0, 0, 1, 0x03, 0x00, 0x00, 0]);
    // HID descriptor (9 bytes)
    d.push(9);
    d.push(0x21); // HID
    d.extend_from_slice(&0x0111u16.to_le_bytes()); // bcdHID 1.11
    d.push(0); // bCountryCode
    d.push(1); // bNumDescriptors
    d.push(0x22); // bDescriptorType = Report
    d.extend_from_slice(&report_desc_len.to_le_bytes());
    // Endpoint descriptor (7 bytes) — interrupt IN, EP1
    d.extend_from_slice(&[7, 0x05, 0x81, 0x03, 8, 0, 10]);
    d
}

fn build_lang_id_descriptor() -> Vec<u8> {
    vec![4, 0x03, 0x09, 0x04] // English US
}

fn build_string_descriptor(s: &str) -> Vec<u8> {
    let utf16: Vec<u16> = s.encode_utf16().collect();
    let len = 2 + utf16.len() * 2;
    let mut d = vec![len as u8, 0x03];
    for c in utf16 {
        d.extend_from_slice(&c.to_le_bytes());
    }
    d
}

/// Handle a USB control request. Returns response bytes for IN requests, empty vec for OUT ACKs.
fn handle_control_request(setup: &[u8; 8], vid: u16, pid: u16, device_name: &str) -> Vec<u8> {
    let bm_request_type = setup[0];
    let b_request = setup[1];
    let w_value = u16::from_le_bytes([setup[2], setup[3]]);
    let _w_index = u16::from_le_bytes([setup[4], setup[5]]);
    let w_length = u16::from_le_bytes([setup[6], setup[7]]) as usize;

    let direction_in = (bm_request_type & 0x80) != 0;
    let req_type = (bm_request_type >> 5) & 0x03; // 0=standard, 1=class, 2=vendor

    if direction_in {
        match (req_type, b_request) {
            // GET_DESCRIPTOR
            (0, 0x06) => {
                let desc_type = (w_value >> 8) as u8;
                let desc_index = (w_value & 0xFF) as u8;
                let data: Vec<u8> = match desc_type {
                    0x01 => build_device_descriptor(vid, pid),
                    0x02 => build_config_descriptor(),
                    0x03 => match desc_index {
                        0 => build_lang_id_descriptor(),
                        1 => build_string_descriptor("virsh-usb"),
                        2 => build_string_descriptor(device_name),
                        _ => return vec![],
                    },
                    0x22 => HID_SCANNER_REPORT_DESC.to_vec(),
                    _ => return vec![],
                };
                let len = data.len().min(w_length);
                data[..len].to_vec()
            }
            // GET_CONFIGURATION (bConfigurationValue)
            (0, 0x08) => vec![1u8],
            // GET_REPORT (HID class)
            (1, 0x01) => vec![0u8; 8.min(w_length)],
            // GET_IDLE
            (1, 0x02) => vec![0u8; 1.min(w_length)],
            _ => vec![],
        }
    } else {
        // OUT requests (SET_ADDRESS, SET_CONFIGURATION, SET_IDLE, SET_PROTOCOL, etc.) — ACK
        vec![]
    }
}

/// Clear the endpoint halt (EP_HALTED) flag for an endpoint on a USB device.
///
/// This calls USBDEVFS_CLEAR_HALT which sends a CLEAR_FEATURE(ENDPOINT_HALT)
/// control transfer to the device AND calls usb_reset_endpoint() in the kernel
/// to clear the endpoint's halt state bits.  This must be done before QEMU
/// claims the device so that its first INTERRUPT IN submission does not
/// encounter a stale EP_HALTED flag from the earlier host usbhid probe phase.
fn clear_ep_halt(bus: &str, dev: &str, endpoint: u32) {
    // USBDEVFS_CLEAR_HALT = _IOR('U', 21, unsigned int) = 0x80045515
    const USBDEVFS_CLEAR_HALT: libc::c_ulong = 0x80045515;
    let device_path = format!("/dev/bus/usb/{}/{}", bus, dev);
    let Ok(file) = std::fs::OpenOptions::new().write(true).open(&device_path) else {
        return;
    };
    unsafe {
        libc::ioctl(file.as_raw_fd(), USBDEVFS_CLEAR_HALT, &endpoint as *const u32);
    }
}

/// Build the 312-byte USB/IP device info block (path + busid + fields).
fn usbip_device_info(vid: u16, pid: u16) -> Vec<u8> {
    let mut d = Vec::new();
    // path (256 bytes)
    let path = "/sys/devices/platform/vhci_hcd.0/usb1/1-1";
    let mut path_buf = [0u8; 256];
    let bytes = path.as_bytes();
    path_buf[..bytes.len().min(255)].copy_from_slice(&bytes[..bytes.len().min(255)]);
    d.extend_from_slice(&path_buf);
    // busid (32 bytes)
    let mut busid_buf = [0u8; 32];
    busid_buf[..HID_BUSID.len()].copy_from_slice(HID_BUSID.as_bytes());
    d.extend_from_slice(&busid_buf);
    d.extend_from_slice(&HID_BUSNUM.to_be_bytes());
    d.extend_from_slice(&HID_DEVNUM.to_be_bytes());
    d.extend_from_slice(&HID_SPEED.to_be_bytes());
    d.extend_from_slice(&vid.to_be_bytes());
    d.extend_from_slice(&pid.to_be_bytes());
    d.extend_from_slice(&0x0100u16.to_be_bytes()); // bcdDevice
    d.push(0x00); // bDeviceClass
    d.push(0x00); // bDeviceSubClass
    d.push(0x00); // bDeviceProtocol
    d.push(1);    // bConfigurationValue
    d.push(1);    // bNumConfigurations
    d.push(1);    // bNumInterfaces
    d
}

fn send_devlist_reply(stream: &mut TcpStream, vid: u16, pid: u16) -> Result<()> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&USBIP_VERSION.to_be_bytes());
    buf.extend_from_slice(&OP_REP_DEVLIST.to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes()); // status
    buf.extend_from_slice(&1u32.to_be_bytes()); // num_exported_devices
    buf.extend_from_slice(&usbip_device_info(vid, pid));
    // Interface info (1 interface × 4 bytes)
    buf.extend_from_slice(&[0x03, 0x00, 0x00, 0x00]); // class=HID, subclass=None, proto=None
    stream.write_all(&buf)?;
    Ok(())
}

fn send_import_reply(stream: &mut TcpStream, vid: u16, pid: u16) -> Result<()> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&USBIP_VERSION.to_be_bytes());
    buf.extend_from_slice(&OP_REP_IMPORT.to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes()); // status
    buf.extend_from_slice(&usbip_device_info(vid, pid));
    stream.write_all(&buf)?;
    Ok(())
}

struct PendingEp1In {
    seqnum: u32,
    devid: u32,
    setup: [u8; 8],
}

fn send_ret_submit(
    stream: &mut TcpStream,
    seqnum: u32,
    devid: u32,
    direction: u32,
    ep: u32,
    setup: &[u8; 8],
    data: &[u8],
) -> bool {
    let mut ret = Vec::with_capacity(48 + data.len());
    ret.extend_from_slice(&USBIP_RET_SUBMIT.to_be_bytes());
    ret.extend_from_slice(&seqnum.to_be_bytes());
    ret.extend_from_slice(&devid.to_be_bytes());
    ret.extend_from_slice(&direction.to_be_bytes());
    ret.extend_from_slice(&ep.to_be_bytes());
    ret.extend_from_slice(&0u32.to_be_bytes()); // status
    ret.extend_from_slice(&(data.len() as u32).to_be_bytes()); // actual_length
    ret.extend_from_slice(&0u32.to_be_bytes()); // start_frame
    ret.extend_from_slice(&0u32.to_be_bytes()); // num_packets
    ret.extend_from_slice(&0u32.to_be_bytes()); // error_count
    ret.extend_from_slice(setup);
    ret.extend_from_slice(data);
    stream.write_all(&ret).is_ok()
}

fn send_ret_unlink(
    stream: &mut TcpStream,
    seqnum: u32,
    devid: u32,
    direction: u32,
    ep: u32,
) -> bool {
    let mut ret = [0u8; 48];
    ret[0..4].copy_from_slice(&USBIP_RET_UNLINK.to_be_bytes());
    ret[4..8].copy_from_slice(&seqnum.to_be_bytes());
    ret[8..12].copy_from_slice(&devid.to_be_bytes());
    ret[12..16].copy_from_slice(&direction.to_be_bytes());
    ret[16..20].copy_from_slice(&ep.to_be_bytes());
    // status=0, rest zeros
    stream.write_all(&ret).is_ok()
}

/// Pack a text string into 8-byte raw ASCII HID reports, zero-padded.
/// The `\r` appended by `hid_type` is packed inline with the data, matching
/// how the physical scanner embeds its CR terminator.
fn text_to_scanner_reports(text: &str) -> Vec<[u8; 8]> {
    let bytes = text.as_bytes();
    let mut reports = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let mut report = [0u8; 8];
        let n = (bytes.len() - i).min(8);
        report[..n].copy_from_slice(&bytes[i..i + n]);
        reports.push(report);
        i += 8;
    }
    reports
}

/// USB/IP transfer loop. Holds interrupt IN (EP1) submissions pending until
/// scan data arrives — matching how a real HID scanner NAKs idle polls.
fn handle_usb_transfers(
    stream: &mut TcpStream,
    vid: u16,
    pid: u16,
    device_name: &str,
    key_queue: Arc<Mutex<VecDeque<[u8; 8]>>>,
    notify_read_fd: i32,
) {
    let mut pending: Option<PendingEp1In> = None;

    loop {
        if let Some(ref pend) = pending {
            // EP1 IN is parked. Poll for a key-data notification or a CMD_UNLINK.
            let mut pfds = [
                libc::pollfd { fd: stream.as_raw_fd(), events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: notify_read_fd,     events: libc::POLLIN, revents: 0 },
            ];
            if unsafe { libc::poll(pfds.as_mut_ptr(), 2, -1) } <= 0 {
                continue; // EINTR or transient error — retry
            }

            if pfds[1].revents & libc::POLLIN != 0 {
                // Drain the notification pipe, then dequeue one report.
                let mut drain = [0u8; 64];
                unsafe { libc::read(notify_read_fd, drain.as_mut_ptr() as _, drain.len()) };
                if let Some(report) = key_queue.lock().unwrap().pop_front() {
                    if !send_ret_submit(stream, pend.seqnum, pend.devid, 1, 1, &pend.setup, &report) {
                        break;
                    }
                    pending = None;
                }
                // Queue was empty (stale notification) — loop back to poll.
                continue;
            }

            if pfds[0].revents & libc::POLLIN != 0 {
                // New command while EP1 IN is pending (typically CMD_UNLINK).
                let mut hdr = [0u8; 48];
                if stream.read_exact(&mut hdr).is_err() {
                    break;
                }
                let command  = u32::from_be_bytes(hdr[0..4].try_into().unwrap());
                let seqnum   = u32::from_be_bytes(hdr[4..8].try_into().unwrap());
                let devid    = u32::from_be_bytes(hdr[8..12].try_into().unwrap());
                let direction = u32::from_be_bytes(hdr[12..16].try_into().unwrap());
                let ep       = u32::from_be_bytes(hdr[16..20].try_into().unwrap());
                let buf_len  = u32::from_be_bytes(hdr[24..28].try_into().unwrap());
                let setup: [u8; 8] = hdr[40..48].try_into().unwrap();

                match command {
                    USBIP_CMD_UNLINK => {
                        // hdr[20..24] is cmd_unlink.seqnum — the CMD_SUBMIT seqnum to cancel.
                        let unlink_seqnum = u32::from_be_bytes(hdr[20..24].try_into().unwrap());
                        if unlink_seqnum == pend.seqnum {
                            pending = None;
                        }
                        if !send_ret_unlink(stream, seqnum, devid, direction, ep) {
                            break;
                        }
                    }
                    USBIP_CMD_SUBMIT => {
                        if direction == 0 && buf_len > 0 {
                            let mut buf = vec![0u8; buf_len as usize];
                            if stream.read_exact(&mut buf).is_err() {
                                break;
                            }
                        }
                        if ep == 0 {
                            let resp = handle_control_request(&setup, vid, pid, device_name);
                            if !send_ret_submit(stream, seqnum, devid, direction, ep, &setup, &resp) {
                                break;
                            }
                        }
                        // Ignore unexpected extra EP1 IN CMDs while one is already pending.
                    }
                    _ => break,
                }
            }
        } else {
            // No pending EP1 IN — blocking read for the next command.
            let mut hdr = [0u8; 48];
            if stream.read_exact(&mut hdr).is_err() {
                break;
            }
            let command   = u32::from_be_bytes(hdr[0..4].try_into().unwrap());
            let seqnum    = u32::from_be_bytes(hdr[4..8].try_into().unwrap());
            let devid     = u32::from_be_bytes(hdr[8..12].try_into().unwrap());
            let direction = u32::from_be_bytes(hdr[12..16].try_into().unwrap());
            let ep        = u32::from_be_bytes(hdr[16..20].try_into().unwrap());
            let buf_len   = u32::from_be_bytes(hdr[24..28].try_into().unwrap());
            let setup: [u8; 8] = hdr[40..48].try_into().unwrap();

            match command {
                USBIP_CMD_SUBMIT => {
                    if direction == 0 && buf_len > 0 {
                        let mut buf = vec![0u8; buf_len as usize];
                        if stream.read_exact(&mut buf).is_err() {
                            break;
                        }
                    }
                    let response: Vec<u8> = if ep == 0 {
                        handle_control_request(&setup, vid, pid, device_name)
                    } else if ep == 1 && direction == 1 {
                        // Interrupt IN: serve immediately if data is queued, else park.
                        match key_queue.lock().unwrap().pop_front() {
                            Some(report) => report.to_vec(),
                            None => {
                                pending = Some(PendingEp1In { seqnum, devid, setup });
                                continue;
                            }
                        }
                    } else {
                        vec![]
                    };
                    if !send_ret_submit(stream, seqnum, devid, direction, ep, &setup, &response) {
                        break;
                    }
                }
                USBIP_CMD_UNLINK => {
                    if !send_ret_unlink(stream, seqnum, devid, direction, ep) {
                        break;
                    }
                }
                _ => break,
            }
        }
    }
}

/// Main entry point for the USB/IP HID daemon process.
fn run_hid_daemon(
    name: &str,
    vid_str: &str,
    pid_str: &str,
    sock_path: &str,
    pid_file_path: &str,
    port_file_path: &str,
) -> Result<()> {
    let vid = u16::from_str_radix(
        vid_str
            .to_lowercase()
            .trim_start_matches("0x"),
        16,
    )
    .context("Invalid VID")?;
    let pid_val = u16::from_str_radix(
        pid_str
            .to_lowercase()
            .trim_start_matches("0x"),
        16,
    )
    .context("Invalid PID")?;

    // Bind TCP listener on a random port
    let tcp_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = tcp_listener.local_addr()?.port();

    // Write state files (parent is polling for port_file to know we're ready)
    fs::write(pid_file_path, format!("{}", std::process::id()))?;
    fs::write(port_file_path, format!("{}", port))?;

    // Unix socket for key injection IPC
    let sock_path_buf = PathBuf::from(sock_path);
    if sock_path_buf.exists() {
        let _ = fs::remove_file(&sock_path_buf);
    }
    let unix_listener = UnixListener::bind(&sock_path_buf)?;

    // Shared key report queue + pipe pair for waking the transfer loop.
    let key_queue: Arc<Mutex<VecDeque<[u8; 8]>>> = Arc::new(Mutex::new(VecDeque::new()));

    let mut pipe_fds = [0i32; 2];
    if unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(anyhow!("Failed to create notification pipe"));
    }
    let (notify_read_fd, notify_write_fd) = (pipe_fds[0], pipe_fds[1]);

    // Thread: handle Unix socket connections (key/scan data injection).
    let key_queue_ipc = Arc::clone(&key_queue);
    std::thread::spawn(move || {
        for stream in unix_listener.incoming() {
            if let Ok(mut stream) = stream {
                let kq = Arc::clone(&key_queue_ipc);
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let _ = stream.read_to_end(&mut buf);
                    if let Ok(text) = std::str::from_utf8(&buf) {
                        let reports = text_to_scanner_reports(text);
                        if reports.is_empty() {
                            return;
                        }
                        let mut queue = kq.lock().unwrap();
                        for report in reports {
                            queue.push_back(report);
                        }
                        drop(queue);
                        // Wake the transfer loop.
                        unsafe { libc::write(notify_write_fd, [1u8].as_ptr() as _, 1) };
                    }
                });
            }
        }
    });

    // Main loop: handle TCP connections sequentially
    let name = name.to_string();
    for stream in tcp_listener.incoming() {
        let Ok(mut stream) = stream else { continue };

        // Read 8-byte OP request header
        let mut header = [0u8; 8];
        if stream.read_exact(&mut header).is_err() {
            continue;
        }
        let code = u16::from_be_bytes([header[2], header[3]]);

        match code {
            OP_REQ_DEVLIST => {
                let _ = send_devlist_reply(&mut stream, vid, pid_val);
            }
            OP_REQ_IMPORT => {
                // Read 32-byte busid
                let mut busid_buf = [0u8; 32];
                if stream.read_exact(&mut busid_buf).is_err() {
                    continue;
                }
                let busid = std::str::from_utf8(&busid_buf)
                    .unwrap_or("")
                    .trim_end_matches('\0');

                if busid == HID_BUSID {
                    if send_import_reply(&mut stream, vid, pid_val).is_ok() {
                        let kq = Arc::clone(&key_queue);
                        handle_usb_transfers(&mut stream, vid, pid_val, &name, kq, notify_read_fd);
                    }
                } else {
                    // Reject with error status
                    let mut buf = Vec::new();
                    buf.extend_from_slice(&USBIP_VERSION.to_be_bytes());
                    buf.extend_from_slice(&OP_REP_IMPORT.to_be_bytes());
                    buf.extend_from_slice(&1u32.to_be_bytes()); // status = error
                    let _ = stream.write_all(&buf);
                }
            }
            _ => {}
        }
    }

    Ok(())
}

/// Find the vhci_hcd sysfs directory (handles both old and new paths).
fn find_vhci_sysfs_dir() -> Result<PathBuf> {
    let base = PathBuf::from("/sys/bus/platform/drivers/vhci_hcd");
    for candidate in &["vhci_hcd.0", "vhci_hcd"] {
        let p = base.join(candidate);
        if p.exists() {
            return Ok(p);
        }
    }
    Err(anyhow!(
        "vhci_hcd sysfs directory not found — is the vhci-hcd module loaded?\n  Try: sudo modprobe vhci-hcd"
    ))
}

/// Find a free vhci_hcd port (returns port number as u32).
fn find_free_vhci_port() -> Result<u32> {
    let vhci_dir = find_vhci_sysfs_dir()?;
    let status_path = vhci_dir.join("status");
    let content = fs::read_to_string(&status_path)
        .context("Failed to read vhci_hcd status")?;

    for line in content.lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        // Format: "hub port sta spd dev sockfd local_busid"
        // hub = "hs" or "ss", port = hex, sta = hex status
        if parts.len() >= 3 && parts[0] == "hs" {
            if let (Ok(port), Ok(status)) = (
                u32::from_str_radix(parts[1], 16),
                u32::from_str_radix(parts[2], 16),
            ) {
                if status == 0x04 {
                    // VDEV_ST_NULL = available
                    return Ok(port);
                }
            }
        }
    }
    Err(anyhow!("No free vhci_hcd ports available"))
}

/// Attach the socket FD to vhci_hcd so the device appears on the host USB bus.
fn attach_vhci(vhci_dir: &Path, rhport: u32, sockfd: i32) -> Result<()> {
    let devid = (HID_BUSNUM << 16) | HID_DEVNUM;
    let cmd = format!("{} {} {} {}\n", rhport, sockfd, devid, HID_SPEED);
    fs::write(vhci_dir.join("attach"), &cmd).with_context(|| {
        let attach_path = vhci_dir.join("attach");
        let perms = fs::metadata(&attach_path)
            .map(|m| format!("{:o}", <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::mode(&m.permissions())))
            .unwrap_or_else(|_| "unknown".to_string());
        format!(
            "Failed to write to vhci_hcd attach ({})\n  \
             Permissions: {}\n  \
             Make sure your user is in the 'plugdev' group and the udev rule is installed:\n  \
             sudo usermod -aG plugdev $USER\n  \
             echo 'SUBSYSTEM==\"platform\", DRIVER==\"vhci_hcd\", RUN+=\"/bin/sh -c \\'chown root:plugdev /sys%p/attach /sys%p/detach && chmod 0660 /sys%p/attach /sys%p/detach\\'\"' | sudo tee /etc/udev/rules.d/99-virsh-usb.rules",
            attach_path.display(), perms
        )
    })?;
    Ok(())
}

/// Detach the vhci port used by this HID device.
fn detach_vhci(name: &str) -> Result<()> {
    let port_file = hid_vhci_port_file(name)?;
    if !port_file.exists() {
        return Ok(());
    }
    let content = fs::read_to_string(&port_file)?;
    let port: u32 = content.trim().parse().context("Invalid vhci port in state file")?;

    let vhci_dir = find_vhci_sysfs_dir()?;
    fs::write(vhci_dir.join("detach"), format!("{}\n", port))
        .context("Failed to write to vhci_hcd detach")?;
    let _ = fs::remove_file(&port_file);
    Ok(())
}

// ============================================================
// HID device lifecycle
// ============================================================

fn is_hid_device_attached(vm_name: &str, device: &HidDevice) -> Result<bool> {
    if !is_hid_daemon_running(&device.name) {
        return Ok(false);
    }
    let vid = normalize_hex_id(&device.vid);
    let pid = normalize_hex_id(&device.pid);
    is_device_attached(vm_name, &vid, &pid)
}

fn create_hid_device(name: &str, vid: &str, pid: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!("Device name cannot be empty"));
    }
    let name = sanitize_device_name(name);
    if name.is_empty() {
        return Err(anyhow!("Device name is empty after sanitization"));
    }
    let name = name.as_str();

    let mut devices = load_hid_devices()?;
    if devices.iter().any(|d| d.name == name) {
        return Err(anyhow!("An HID device named '{}' already exists", name));
    }
    let drives = load_virtual_drives()?;
    if drives.iter().any(|d| d.name == name) {
        return Err(anyhow!(
            "A storage drive named '{}' already exists. Device names must be unique across all types.",
            name
        ));
    }

    devices.push(HidDevice {
        name: name.to_string(),
        vid: vid.to_string(),
        pid: pid.to_string(),
    });
    save_hid_devices(&devices)?;

    println!(
        "{} Created HID device {} ({}:{})",
        style("✓").green().bold(),
        style(name).cyan(),
        vid,
        pid
    );

    Ok(())
}

fn delete_hid_device(name: &str) -> Result<()> {
    let mut devices = load_hid_devices()?;
    let idx = devices
        .iter()
        .position(|d| d.name == name)
        .ok_or_else(|| anyhow!("No HID device named '{}'", name))?;

    let running_vms = run_command(&["virsh", "list", "--name"]).unwrap_or_default();
    for vm in running_vms.lines().map(str::trim).filter(|s| !s.is_empty()) {
        if is_hid_device_attached(vm, &devices[idx]).unwrap_or(false) {
            return Err(anyhow!(
                "HID device '{}' is currently attached to VM '{}'. Detach it first.",
                name,
                vm
            ));
        }
    }

    // Clean up any leftover state files
    cleanup_hid_state_files(name);

    devices.remove(idx);
    save_hid_devices(&devices)?;

    println!(
        "{} Deleted HID device {}",
        style("✓").green().bold(),
        style(name).cyan()
    );

    Ok(())
}

fn list_hid_devices() -> Result<()> {
    let devices = load_hid_devices()?;

    if devices.is_empty() {
        println!("No HID devices found. Create one with:");
        println!("  virsh-usb hid create <name> --vid <vid> --pid <pid>");
        return Ok(());
    }

    let running_vms: Vec<String> = run_command(&["virsh", "list", "--name"])
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    println!("Virtual USB HID Devices:");
    println!();

    for device in &devices {
        let mut attachment: Option<String> = None;
        for vm in &running_vms {
            if is_hid_device_attached(vm, device).unwrap_or(false) {
                attachment = Some(vm.clone());
                break;
            }
        }

        let daemon_status = if is_hid_daemon_running(&device.name) {
            style("daemon active").green().to_string()
        } else {
            style("daemon inactive").dim().to_string()
        };

        let status = if let Some(vm) = attachment {
            style(format!("attached to: {}", vm)).green().to_string()
        } else {
            style("not attached").dim().to_string()
        };

        println!(
            "  {:<20} {}:{:<12} {}  {}",
            style(&device.name).cyan(),
            device.vid,
            device.pid,
            daemon_status,
            status,
        );
        println!();
    }

    Ok(())
}

fn attach_hid_device(vm_name: &str, device_name: &str) -> Result<()> {
    let devices = load_hid_devices()?;
    let device = devices
        .iter()
        .find(|d| d.name == device_name)
        .ok_or_else(|| anyhow!("No HID device named '{}'", device_name))?;

    if !check_vm_running(vm_name)? {
        return Err(anyhow!("VM '{}' is not running", vm_name));
    }

    if is_hid_device_attached(vm_name, device)? {
        println!(
            "HID device '{}' is already attached to {}",
            device_name, vm_name
        );
        return Ok(());
    }

    let vid = normalize_hex_id(&device.vid);
    let pid = normalize_hex_id(&device.pid);

    // Load the vhci-hcd kernel module
    run_command(&["modprobe", "vhci-hcd"])
        .context("Failed to load vhci-hcd module — make sure it's available")?;

    // Start the USB/IP daemon if not already running
    if !is_hid_daemon_running(device_name) {
        let pid_file = hid_pid_file(device_name)?;
        let port_file = hid_port_file(device_name)?;
        let sock_file = hid_sock_file(device_name)?;

        let exe = fs::read_link("/proc/self/exe").context("Failed to find current executable")?;

        let log_path = format!("/tmp/virsh-usb-hid-{}.log", device_name);
        let log_file = std::fs::File::create(&log_path)
            .unwrap_or_else(|_| std::fs::File::open("/dev/null").unwrap());

        let child = Command::new(&exe)
            .args([
                "hid-daemon",
                "--name",
                device_name,
                "--vid",
                &device.vid,
                "--pid",
                &device.pid,
                "--socket-path",
                sock_file.to_str().unwrap(),
                "--pid-file",
                pid_file.to_str().unwrap(),
                "--port-file",
                port_file.to_str().unwrap(),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(log_file)
            .spawn()
            .context("Failed to spawn HID daemon")?;
        drop(child); // daemon is independent; parent will be reparented to init

        // Wait up to 5s for port file to appear
        let started = (0..50).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            hid_port_file(device_name)
                .ok()
                .map(|f| f.exists())
                .unwrap_or(false)
        });
        if !started {
            return Err(anyhow!(
                "HID daemon failed to start (port file not created within 5 seconds)"
            ));
        }
    }

    // Everything from here on must clean up the daemon on failure.
    let result = (|| -> Result<()> {
        // Connect to the daemon and perform USB/IP IMPORT handshake
        let port = read_hid_port(device_name)?;
        let stream = TcpStream::connect(format!("127.0.0.1:{}", port))
            .context("Failed to connect to HID daemon")?;

        // Send OP_REQ_IMPORT
        let mut import_req = Vec::new();
        import_req.extend_from_slice(&USBIP_VERSION.to_be_bytes());
        import_req.extend_from_slice(&OP_REQ_IMPORT.to_be_bytes());
        import_req.extend_from_slice(&0u32.to_be_bytes());
        let mut busid_buf = [0u8; 32];
        busid_buf[..HID_BUSID.len()].copy_from_slice(HID_BUSID.as_bytes());
        import_req.extend_from_slice(&busid_buf);
        (&stream).write_all(&import_req).context("Failed to send IMPORT request")?;

        // Read OP_REP_IMPORT header (8 bytes)
        let mut rep_hdr = [0u8; 8];
        (&stream).read_exact(&mut rep_hdr).context("Failed to read IMPORT reply")?;
        let status = u32::from_be_bytes([rep_hdr[4], rep_hdr[5], rep_hdr[6], rep_hdr[7]]);
        if status != 0 {
            return Err(anyhow!("USB/IP daemon rejected IMPORT (status={})", status));
        }
        // Read and discard 312-byte device info
        let mut dev_info = [0u8; 312];
        (&stream).read_exact(&mut dev_info).context("Failed to read device info")?;

        // Find a free vhci port and attach via sysfs
        let vhci_dir = find_vhci_sysfs_dir()?;
        let rhport = find_free_vhci_port()?;

        // Store vhci port for detach
        fs::write(hid_vhci_port_file(device_name)?, format!("{}", rhport))?;

        // Hand the socket FD to the kernel; it takes its own reference via fget()
        let sockfd = stream.into_raw_fd();
        let attach_result = attach_vhci(&vhci_dir, rhport, sockfd);
        // Close our userspace copy (kernel still holds its reference)
        drop(unsafe { TcpStream::from_raw_fd(sockfd) });
        attach_result?;

        // Wait up to 5s for device to appear in lsusb
        let mut usb_loc: Option<(String, String)> = None;
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if let Ok(Some(loc)) = find_usb_device(&vid, &pid) {
                usb_loc = Some(loc);
                break;
            }
        }
        let Some((usb_bus, usb_dev)) = usb_loc else {
            let _ = detach_vhci(device_name);
            return Err(anyhow!(
                "Device {}:{} did not appear on the host USB bus after vhci attachment",
                device.vid,
                device.pid
            ));
        };

        // Wait for udev rules to finish processing (unbind host kernel drivers,
        // set device file permissions for QEMU access).  Suppress output — a
        // timeout here is non-fatal; we proceed and let virsh sort it out.
        let _ = Command::new("udevadm")
            .args(["settle", "--timeout=3"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        // Clear the EP_HALTED flag on the interrupt IN endpoint before handing
        // the device to QEMU.  The initial host usbhid probe phase leaves the
        // endpoint in a halted state that persists across driver changes.  QEMU
        // uses USBDEVFS_SUBMITURB (not USBDEVFS_CLEAR_HALT) for the CLEAR_FEATURE
        // control transfer the guest sends, so the kernel never calls
        // usb_reset_endpoint() and the stale EP_HALTED bit blocks every
        // INTERRUPT IN URB before it reaches vhci_hcd, resulting in an infinite
        // CLEAR_FEATURE loop.  Calling USBDEVFS_CLEAR_HALT here sends the control
        // transfer to the daemon (which ACKs it) and has the kernel clear the bit.
        clear_ep_halt(&usb_bus, &usb_dev, 0x81);

        // Pass through to the guest via virsh hostdev
        attach_device(vm_name, &vid, &pid)
    })();

    if result.is_err() {
        stop_hid_daemon(device_name);
    }

    result
}

fn detach_hid_device(vm_name: &str, device_name: &str) -> Result<()> {
    let devices = load_hid_devices()?;
    let device = devices
        .iter()
        .find(|d| d.name == device_name)
        .ok_or_else(|| anyhow!("No HID device named '{}'", device_name))?;

    if !check_vm_running(vm_name)? {
        return Err(anyhow!("VM '{}' is not running", vm_name));
    }

    let vid = normalize_hex_id(&device.vid);
    let pid = normalize_hex_id(&device.pid);

    if !is_device_attached(vm_name, &vid, &pid)? && !is_hid_daemon_running(device_name) {
        println!(
            "HID device '{}' is not attached to {}",
            device_name, vm_name
        );
        return Ok(());
    }

    // Detach from guest
    if is_device_attached(vm_name, &vid, &pid)? {
        detach_device(vm_name, &vid, &pid)?;
    }

    // Detach vhci port
    detach_vhci(device_name)?;

    // Stop daemon
    stop_hid_daemon(device_name);

    println!(
        "{} Detached HID device {} from {}",
        style("✓").green().bold(),
        style(device_name).cyan(),
        style(vm_name).yellow(),
    );

    Ok(())
}

fn show_hid_status(vm_name: &str, device_name: &str) -> Result<()> {
    let devices = load_hid_devices()?;
    let device = devices
        .iter()
        .find(|d| d.name == device_name)
        .ok_or_else(|| anyhow!("No HID device named '{}'", device_name))?;

    let vm_running = check_vm_running(vm_name)?;
    println!(
        "{} VM ({}): {}",
        style("🖥").cyan(),
        style(vm_name).yellow(),
        if vm_running {
            style("Running").green()
        } else {
            style("Not running").red()
        }
    );

    println!(
        "{} HID Device ({}): {}:{}  daemon: {}",
        style("⌨").cyan(),
        style(device_name).cyan(),
        device.vid,
        device.pid,
        if is_hid_daemon_running(device_name) {
            style("active").green()
        } else {
            style("inactive").dim()
        }
    );

    if vm_running {
        let attached = is_hid_device_attached(vm_name, device)?;
        println!(
            "{} Attachment Status: {}",
            style("🔗").cyan(),
            if attached {
                style("Attached to VM").green()
            } else {
                style("Not attached to VM").yellow()
            }
        );
    }

    Ok(())
}

// ============================================================
// HID typing
// ============================================================

fn hid_type(vm_name: &str, device_name: &str, text: &str, no_enter: bool) -> Result<()> {
    if !check_vm_running(vm_name)? {
        return Err(anyhow!("VM '{}' is not running", vm_name));
    }

    let devices = load_hid_devices()?;
    let device = devices
        .iter()
        .find(|d| d.name == device_name)
        .ok_or_else(|| anyhow!("No HID device named '{}'", device_name))?;

    if !is_hid_device_attached(vm_name, device)? {
        return Err(anyhow!(
            "HID device '{}' is not attached to VM '{}'. Attach it first.",
            device_name,
            vm_name
        ));
    }

    let sock_file = hid_sock_file(device_name)?;
    let mut stream = UnixStream::connect(&sock_file)
        .context("Failed to connect to HID daemon socket")?;

    let mut payload = text.to_string();
    if !no_enter {
        payload.push('\r');
    }

    stream
        .write_all(payload.as_bytes())
        .context("Failed to send text to HID daemon")?;

    println!(
        "{} Typed {} character(s) via {} to {}",
        style("✓").green().bold(),
        text.chars().count(),
        style(device_name).cyan(),
        style(vm_name).yellow()
    );

    Ok(())
}

// ============================================================
// Unified interactive device selection
// ============================================================

fn select_device(vm_name: Option<&str>, filter_attached: bool) -> Result<DeviceChoice> {
    let mut usb_devices = get_all_usb_devices().unwrap_or_default();
    let attached_usb = vm_name
        .map(|vm| get_attached_devices(vm).unwrap_or_default())
        .unwrap_or_default();
    for d in &mut usb_devices {
        d.attached = attached_usb
            .iter()
            .any(|(v, p)| v == &d.vendor_id && p == &d.product_id);
    }

    let virtual_drives = load_virtual_drives()?;
    let virtual_attachments = vm_name
        .map(|vm| get_attached_virtual_devices(vm).unwrap_or_default())
        .unwrap_or_default();

    let hid_devices = load_hid_devices()?;

    // Filter HID device VID/PIDs out of the real USB list to avoid duplicates
    let hid_vid_pids: Vec<(String, String)> = hid_devices
        .iter()
        .map(|d| (normalize_hex_id(&d.vid), normalize_hex_id(&d.pid)))
        .collect();
    usb_devices.retain(|d| {
        !hid_vid_pids
            .iter()
            .any(|(v, p)| v == &d.vendor_id && p == &d.product_id)
    });

    let mut choices: Vec<DeviceChoice> = vec![];

    if filter_attached {
        for d in usb_devices {
            if d.attached {
                choices.push(DeviceChoice::RealUsb(d));
            }
        }
        for drive in virtual_drives {
            let Ok(image_path_str) = get_vol_path(&drive.name) else {
                continue;
            };
            let is_attached = virtual_attachments
                .iter()
                .any(|a| a.source_file == image_path_str);
            if is_attached {
                choices.push(DeviceChoice::Storage(drive, true));
            }
        }
        for device in hid_devices {
            let is_attached = is_hid_daemon_running(&device.name)
                && attached_usb.iter().any(|(v, p)| {
                    v == &normalize_hex_id(&device.vid) && p == &normalize_hex_id(&device.pid)
                });
            if is_attached {
                choices.push(DeviceChoice::Hid(device, true));
            }
        }
    } else {
        for d in usb_devices {
            choices.push(DeviceChoice::RealUsb(d));
        }
        for drive in virtual_drives {
            let is_attached = match get_vol_path(&drive.name) {
                Ok(image_path_str) => virtual_attachments
                    .iter()
                    .any(|a| a.source_file == image_path_str),
                Err(_) => false,
            };
            choices.push(DeviceChoice::Storage(drive, is_attached));
        }
        for device in hid_devices {
            let is_attached = is_hid_daemon_running(&device.name)
                && attached_usb.iter().any(|(v, p)| {
                    v == &normalize_hex_id(&device.vid) && p == &normalize_hex_id(&device.pid)
                });
            choices.push(DeviceChoice::Hid(device, is_attached));
        }
        choices.push(DeviceChoice::CreateNewStorage);
        choices.push(DeviceChoice::CreateNewHid);
    }

    if choices.is_empty() {
        return Err(anyhow!(
            "{}",
            if filter_attached {
                "No devices are currently attached to the VM"
            } else {
                "No USB devices found and no virtual devices exist."
            }
        ));
    }

    let prompt = if filter_attached {
        format!("{} Select a device to detach", style("🔌").yellow())
    } else {
        format!("{} Select a device", style("🔌").cyan())
    };

    Select::new(&prompt, choices)
        .with_starting_cursor(0)
        .prompt()
        .context("Failed to select device")
}

// ============================================================
// Entry point
// ============================================================

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Internal: HID daemon mode
    if let Commands::HidDaemon {
        name,
        vid,
        pid,
        socket_path,
        pid_file,
        port_file,
    } = &cli.command
    {
        return run_hid_daemon(name, vid, pid, socket_path, pid_file, port_file);
    }

    // Handle storage subcommands — no VM needed
    if let Commands::Storage { action } = &cli.command {
        return match action {
            StorageCommands::Create { name, size } => create_virtual_drive(name, size),
            StorageCommands::List => list_virtual_drives(),
            StorageCommands::Delete { name } => delete_virtual_drive(name),
        };
    }

    // Handle HID subcommands
    if let Commands::Hid { action } = &cli.command {
        return match action {
            HidCommands::Create { name, vid, pid } => create_hid_device(name, vid, pid),
            HidCommands::List => list_hid_devices(),
            HidCommands::Delete { name } => delete_hid_device(name),
            HidCommands::Type {
                text,
                vm,
                device,
                no_enter,
            } => {
                let vm_name = match vm {
                    Some(v) => v.clone(),
                    None => {
                        let selected = select_vm()?;
                        let _ = save_last_vm(&selected);
                        selected
                    }
                };
                let device_name = match device {
                    Some(d) => d.clone(),
                    None => {
                        let devices = load_hid_devices()?;
                        if devices.is_empty() {
                            return Err(anyhow!(
                                "No HID devices found. Create one with: virsh-usb hid create <name> --vid <vid> --pid <pid>"
                            ));
                        }
                        let display: Vec<String> = devices
                            .iter()
                            .map(|d| format!("{} ({}:{})", d.name, d.vid, d.pid))
                            .collect();
                        let selection = Select::new("Select a HID device:", display.clone())
                            .prompt()
                            .context("Failed to select HID device")?;
                        let idx = display.iter().position(|s| s == &selection).unwrap();
                        devices[idx].name.clone()
                    }
                };
                hid_type(&vm_name, &device_name, text, *no_enter)
            }
        };
    }

    // Get VM name (from CLI or interactively)
    let vm = match cli.vm {
        Some(v) => v,
        None => select_vm()?,
    };
    let _ = save_last_vm(&vm);

    let filter_attached = matches!(cli.command, Commands::Detach);

    // Resolve what device the user wants to operate on
    enum SelectedDevice {
        RealUsb { vendor_id: String, product_id: String },
        Storage { name: String },
        Hid { name: String },
    }

    let selected = if let Some(device_spec) = cli.device {
        // vid:pid format → real USB
        let parts: Vec<&str> = device_spec.splitn(2, ':').collect();
        let looks_like_vid_pid = parts.len() == 2
            && !parts[0].is_empty()
            && !parts[1].is_empty()
            && parts[0]
                .trim_start_matches("0x")
                .chars()
                .all(|c| c.is_ascii_hexdigit())
            && parts[1]
                .trim_start_matches("0x")
                .chars()
                .all(|c| c.is_ascii_hexdigit());

        if looks_like_vid_pid {
            SelectedDevice::RealUsb {
                vendor_id: parts[0].to_string(),
                product_id: parts[1].to_string(),
            }
        } else {
            // Named device: check storage and HID first (exact match), then USB by name
            let drives = load_virtual_drives()?;
            let hid_devices = load_hid_devices()?;
            let is_storage = drives.iter().any(|d| d.name == device_spec);
            let is_hid = hid_devices.iter().any(|d| d.name == device_spec);

            if is_storage && is_hid {
                return Err(anyhow!(
                    "Ambiguous device name '{}': matches both a storage drive and an HID device",
                    device_spec
                ));
            } else if is_storage {
                SelectedDevice::Storage { name: device_spec }
            } else if is_hid {
                SelectedDevice::Hid { name: device_spec }
            } else {
                let (vendor_id, product_id) =
                    find_device_by_name(&device_spec, Some(&vm), filter_attached)?;
                SelectedDevice::RealUsb {
                    vendor_id,
                    product_id,
                }
            }
        }
    } else {
        // Interactive selection from the combined list
        let choice = select_device(Some(&vm), filter_attached)?;
        match choice {
            DeviceChoice::RealUsb(dev) => SelectedDevice::RealUsb {
                vendor_id: dev.vendor_id,
                product_id: dev.product_id,
            },
            DeviceChoice::Storage(drive, _) => SelectedDevice::Storage { name: drive.name },
            DeviceChoice::Hid(device, _) => SelectedDevice::Hid { name: device.name },
            DeviceChoice::CreateNewStorage => {
                let raw = inquire::Text::new("Name for the new storage drive:")
                    .prompt()
                    .context("Failed to get drive name")?;
                let name = sanitize_device_name(&raw);
                if name != raw {
                    println!("Note: name sanitized to '{}'", style(&name).cyan());
                }
                let size = inquire::Text::new("Size (e.g. 4G, 8G, 16G):")
                    .with_default("4G")
                    .prompt()
                    .context("Failed to get drive size")?;
                create_virtual_drive(&name, &size)?;
                SelectedDevice::Storage { name }
            }
            DeviceChoice::CreateNewHid => {
                let raw = inquire::Text::new("Name for the new HID device:")
                    .prompt()
                    .context("Failed to get device name")?;
                let name = sanitize_device_name(&raw);
                if name != raw {
                    println!("Note: name sanitized to '{}'", style(&name).cyan());
                }
                let vid = inquire::Text::new("Vendor ID (e.g. 0x0c2e):")
                    .prompt()
                    .context("Failed to get vendor ID")?;
                let pid = inquire::Text::new("Product ID (e.g. 0x0b61):")
                    .prompt()
                    .context("Failed to get product ID")?;
                create_hid_device(&name, &vid, &pid)?;
                SelectedDevice::Hid { name }
            }
        }
    };

    match (&cli.command, selected) {
        (Commands::Attach, SelectedDevice::RealUsb { vendor_id, product_id }) => {
            attach_device(&vm, &vendor_id, &product_id)?
        }
        (Commands::Detach, SelectedDevice::RealUsb { vendor_id, product_id }) => {
            detach_device(&vm, &vendor_id, &product_id)?
        }
        (Commands::Status, SelectedDevice::RealUsb { vendor_id, product_id }) => {
            show_status(&vm, &vendor_id, &product_id)?
        }
        (Commands::Attach, SelectedDevice::Storage { name }) => {
            attach_virtual_drive(&vm, &name)?
        }
        (Commands::Detach, SelectedDevice::Storage { name }) => {
            detach_virtual_drive(&vm, &name)?
        }
        (Commands::Status, SelectedDevice::Storage { name }) => {
            show_virtual_status(&vm, &name)?
        }
        (Commands::Attach, SelectedDevice::Hid { name }) => attach_hid_device(&vm, &name)?,
        (Commands::Detach, SelectedDevice::Hid { name }) => detach_hid_device(&vm, &name)?,
        (Commands::Status, SelectedDevice::Hid { name }) => show_hid_status(&vm, &name)?,
        (Commands::Storage { .. }, _) | (Commands::Hid { .. }, _) | (Commands::HidDaemon { .. }, _) => {
            unreachable!()
        }
    }

    Ok(())
}
