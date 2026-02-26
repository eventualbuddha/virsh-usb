use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use console::style;
use directories::ProjectDirs;
use inquire::Select;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// CLI tool to manage USB device attachment to virsh VMs
#[derive(Parser)]
#[command(name = "virsh-usb")]
#[command(about = "Manage USB device attachment to virsh VMs")]
struct Cli {
    /// Name of the virsh VM (if not provided, will prompt interactively)
    #[arg(long)]
    vm: Option<String>,

    /// Physical USB device ID in format vendor:product (e.g., 0dd4:4105) or device name
    #[arg(long)]
    device: Option<String>,

    /// Virtual USB drive name (for non-interactive scripting)
    #[arg(long)]
    virtual_device: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Attach the USB device to the VM
    Attach,
    /// Detach the USB device from the VM
    Detach,
    /// Show current status
    Status,
    /// Manage virtual USB flash drives
    Virtual {
        #[command(subcommand)]
        action: VirtualCommands,
    },
}

#[derive(Subcommand)]
enum VirtualCommands {
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

// ============================================================
// Virtual drive types
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

/// Unified device choice for interactive selection
#[derive(Debug, Clone)]
enum DeviceChoice {
    RealUsb(UsbDevice),
    Virtual(VirtualDrive, bool), // (drive, is_attached)
    CreateNew,
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
                        "[USB]  {} - {} {} {}",
                        id,
                        dev.name,
                        bus_info,
                        style("[attached]").green()
                    )
                } else {
                    write!(f, "[USB]  {} - {} {}", id, dev.name, bus_info)
                }
            }
            DeviceChoice::Virtual(drive, is_attached) => {
                if *is_attached {
                    write!(
                        f,
                        "[VIRT] {} ({}) {}",
                        style(&drive.name).cyan(),
                        drive.size,
                        style("[attached]").green()
                    )
                } else {
                    write!(f, "[VIRT] {} ({})", style(&drive.name).cyan(), drive.size)
                }
            }
            DeviceChoice::CreateNew => {
                write!(f, "{}", style("+ Create new virtual drive...").dim())
            }
        }
    }
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
            // Parse: Bus 003 Device 006: ID 0dd4:4105 Custom Engineering SPA PaperHandler
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
            if in_hostdev && let (Some(vendor), Some(product)) = (&current_vendor, &current_product)
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
    // Extract ID from: <vendor id='0x0dd4'/> or similar
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
    // Check if VM is running
    if !check_vm_running(vm_name)? {
        return Err(anyhow!("VM '{}' is not running", vm_name));
    }

    // Check if device is already attached
    if is_device_attached(vm_name, vendor_id, product_id)? {
        println!("Device is already attached to {}", vm_name);
        return Ok(());
    }

    // Find the device and get its name
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

    // Create XML for device attachment
    let xml_content = format!(
        r#"<hostdev mode='subsystem' type='usb' managed='yes'>
  <source>
    <vendor id='0x{vendor_id}'/>
    <product id='0x{product_id}'/>
  </source>
</hostdev>
"#
    );

    // Write XML to temporary file
    let temp_file = "/tmp/virsh-usb-attach.xml";
    fs::write(temp_file, &xml_content).context("Failed to write temporary XML file")?;

    // Attach the device
    let result = run_command(&["virsh", "attach-device", vm_name, temp_file, "--live"]);

    // Clean up temporary file
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
    // Check if VM is running
    if !check_vm_running(vm_name)? {
        return Err(anyhow!("VM '{}' is not running", vm_name));
    }

    // Check if device is attached
    if !is_device_attached(vm_name, vendor_id, product_id)? {
        println!("Device is not attached to {}", vm_name);
        return Ok(());
    }

    // Find the device and get its name
    let all_devices = get_all_usb_devices()?;
    let device_name = all_devices
        .iter()
        .find(|d| d.vendor_id == vendor_id && d.product_id == product_id)
        .map(|d| d.name.clone())
        .unwrap_or_else(|| "Unknown Device".to_string());

    // Create XML for device detachment
    let xml_content = format!(
        r#"<hostdev mode='subsystem' type='usb' managed='yes'>
  <source>
    <vendor id='0x{vendor_id}'/>
    <product id='0x{product_id}'/>
  </source>
</hostdev>
"#
    );

    // Write XML to temporary file
    let temp_file = "/tmp/virsh-usb-detach.xml";
    fs::write(temp_file, &xml_content).context("Failed to write temporary XML file")?;

    // Detach the device
    let result = run_command(&["virsh", "detach-device", vm_name, temp_file, "--live"]);

    // Clean up temporary file
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
    // Check VM status
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

    // Get device name
    let all_devices = get_all_usb_devices().unwrap_or_default();
    let device_name = all_devices
        .iter()
        .find(|d| d.vendor_id == vendor_id && d.product_id == product_id)
        .map(|d| d.name.clone())
        .unwrap_or_else(|| "Unknown Device".to_string());

    // Check if device is connected to host
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

    // Check if device is attached to VM
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

/// Ask libvirt for the absolute path of a volume in the default pool.
/// Uses virsh rather than a hardcoded path so we work with any pool layout.
fn get_vol_path(name: &str) -> Result<String> {
    let vol_name = format!("{}.qcow2", name);
    let output = run_command(&["virsh", "vol-path", &vol_name, "--pool", "default"])?;
    Ok(output.trim().to_string())
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

    // Find the default index based on last used VM
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
        // Parse: Bus 003 Device 006: ID 0dd4:4105 Custom Engineering SPA PaperHandler
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 6 {
            let bus = parts[1].to_string();
            let device = parts[3].trim_end_matches(':').to_string();

            // Extract vendor:product ID
            if let Some(id_part) = parts.get(5) {
                let id_parts: Vec<&str> = id_part.split(':').collect();
                if id_parts.len() == 2 {
                    let vendor_id = id_parts[0].to_string();
                    let product_id = id_parts[1].to_string();

                    // Get device name (everything after the ID)
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

    // Get list of attached devices for this VM
    let attached_devices = if let Some(vm) = vm_name {
        get_attached_devices(vm).unwrap_or_default()
    } else {
        vec![]
    };

    // Mark attached devices
    for device in &mut devices {
        device.attached = attached_devices
            .iter()
            .any(|(v, p)| v == &device.vendor_id && p == &device.product_id);
    }

    // Filter to only attached devices if detaching
    if filter_attached {
        devices.retain(|d| d.attached);
    }

    // Search for device by name (case-insensitive partial match)
    let name_lower = name.to_lowercase();
    let matches: Vec<_> = devices
        .iter()
        .filter(|d| d.name.to_lowercase().contains(&name_lower))
        .collect();

    match matches.len() {
        0 => Err(anyhow!("No USB device found matching name: '{}'", name)),
        1 => Ok((matches[0].vendor_id.clone(), matches[0].product_id.clone())),
        _ => {
            // Multiple matches, prompt user to select
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
// Virtual drive attachment helpers
// ============================================================

/// Parse a VM's XML and return all USB-attached virtual disks
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
/// Scans ALL disk target devices in the VM (not just USB ones) to avoid
/// conflicts with CDROMs, virtio disks, IDE disks, etc.
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
// Virtual drive lifecycle
// ============================================================

fn create_virtual_drive(name: &str, size: &str) -> Result<()> {
    // Validate name
    if name.is_empty() {
        return Err(anyhow!("Drive name cannot be empty"));
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return Err(anyhow!(
            "Drive name '{}' contains invalid characters. Use only letters, numbers, hyphens, and underscores.",
            name
        ));
    }

    // Check for duplicate name
    let mut drives = load_virtual_drives()?;
    if drives.iter().any(|d| d.name == name) {
        return Err(anyhow!("A virtual drive named '{}' already exists", name));
    }

    // Create the volume via libvirt's storage pool — libvirtd runs as root
    // so it can write to /var/lib/libvirt/images/ without needing sudo
    let vol_name = format!("{}.qcow2", name);
    run_command(&[
        "virsh", "vol-create-as", "default", &vol_name, size, "--format", "qcow2",
    ])?;

    // Save metadata
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
        "{} Created virtual drive {} ({}) at /var/lib/libvirt/images/{}",
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
        .ok_or_else(|| anyhow!("No virtual drive named '{}'", name))?;

    // Check if attached to any running VM
    let running_vms = run_command(&["virsh", "list", "--name"]).unwrap_or_default();
    let drive = &drives[idx];
    for vm in running_vms
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if is_virtual_drive_attached(vm, drive)? {
            return Err(anyhow!(
                "Virtual drive '{}' is currently attached to VM '{}'. Detach it first.",
                name,
                vm
            ));
        }
    }

    // Delete the volume via libvirt's storage pool
    let vol_name = format!("{}.qcow2", name);
    match run_command(&["virsh", "vol-delete", &vol_name, "--pool", "default"]) {
        Ok(_) => {}
        Err(e) => eprintln!("Warning: could not delete volume: {}", e),
    }

    // Remove metadata
    drives.remove(idx);
    save_virtual_drives(&drives)?;

    println!(
        "{} Deleted virtual drive {}",
        style("✓").green().bold(),
        style(name).cyan()
    );

    Ok(())
}

fn list_virtual_drives() -> Result<()> {
    let drives = load_virtual_drives()?;

    if drives.is_empty() {
        println!("No virtual drives found. Create one with:");
        println!("  virsh-usb virtual create <name>");
        return Ok(());
    }

    // Build attachment map: image_path_str → vm_name (for running VMs only)
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

    println!("Virtual USB Flash Drives:");
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
        .ok_or_else(|| anyhow!("No virtual drive named '{}'", drive_name))?;

    if !check_vm_running(vm_name)? {
        return Err(anyhow!("VM '{}' is not running", vm_name));
    }

    if is_virtual_drive_attached(vm_name, drive)? {
        println!(
            "Virtual drive '{}' is already attached to {}",
            drive_name, vm_name
        );
        return Ok(());
    }

    let image_path_str = get_vol_path(drive_name).with_context(|| {
        format!(
            "Virtual drive '{}' not found in libvirt storage pool. Try: virsh vol-list default",
            drive_name
        )
    })?;
    let target_dev = get_next_target_dev(vm_name)?;

    let xml_content = format!(
        "<disk type='file' device='disk'>\n  <driver name='qemu' type='qcow2'/>\n  <source file='{}'/>\n  <target dev='{}' bus='usb'/>\n</disk>\n",
        image_path_str, target_dev
    );

    let temp_file = "/tmp/virsh-usb-virtual-attach.xml";
    fs::write(temp_file, &xml_content).context("Failed to write temporary XML file")?;

    let result = run_command(&["virsh", "attach-device", vm_name, temp_file, "--live"]);
    let _ = fs::remove_file(temp_file);
    result?;

    println!(
        "{} Successfully attached virtual drive {} to {}",
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
        .ok_or_else(|| anyhow!("No virtual drive named '{}'", drive_name))?;

    if !check_vm_running(vm_name)? {
        return Err(anyhow!("VM '{}' is not running", vm_name));
    }

    let image_path_str = get_vol_path(drive_name).with_context(|| {
        format!(
            "Virtual drive '{}' not found in libvirt storage pool. Try: virsh vol-list default",
            drive_name
        )
    })?;

    let attachments = get_attached_virtual_devices(vm_name)?;
    let attachment = attachments
        .iter()
        .find(|a| a.source_file == image_path_str)
        .ok_or_else(|| {
            anyhow!(
                "Virtual drive '{}' is not attached to VM '{}'",
                drive_name,
                vm_name
            )
        })?;

    let xml_content = format!(
        "<disk type='file' device='disk'>\n  <driver name='qemu' type='qcow2'/>\n  <source file='{}'/>\n  <target dev='{}' bus='usb'/>\n</disk>\n",
        image_path_str, attachment.target_dev
    );

    let temp_file = "/tmp/virsh-usb-virtual-detach.xml";
    fs::write(temp_file, &xml_content).context("Failed to write temporary XML file")?;

    let result = run_command(&["virsh", "detach-device", vm_name, temp_file, "--live"]);
    let _ = fs::remove_file(temp_file);
    result?;

    println!(
        "{} Successfully detached virtual drive {} from {}",
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
        .ok_or_else(|| anyhow!("No virtual drive named '{}'", drive_name))?;

    // VM status
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

    // Drive info
    match get_vol_path(drive_name) {
        Ok(image_path_str) => println!(
            "{} Virtual Drive ({}): {} at {}",
            style("💾").cyan(),
            style(drive_name).cyan(),
            style(&drive.size).green(),
            style(&image_path_str).dim()
        ),
        Err(_) => println!(
            "{} Virtual Drive ({}): {}",
            style("💾").cyan(),
            style(drive_name).cyan(),
            style("Image missing from storage pool").red()
        ),
    }

    // Attachment status
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
// Unified interactive device selection
// ============================================================

fn select_device(vm_name: Option<&str>, filter_attached: bool) -> Result<DeviceChoice> {
    // Get real USB devices and mark which are attached
    let mut usb_devices = get_all_usb_devices().unwrap_or_default();
    let attached_usb = vm_name
        .map(|vm| get_attached_devices(vm).unwrap_or_default())
        .unwrap_or_default();
    for d in &mut usb_devices {
        d.attached = attached_usb
            .iter()
            .any(|(v, p)| v == &d.vendor_id && p == &d.product_id);
    }

    // Get virtual drives and their attachment status
    let virtual_drives = load_virtual_drives()?;
    let virtual_attachments = vm_name
        .map(|vm| get_attached_virtual_devices(vm).unwrap_or_default())
        .unwrap_or_default();

    // Build the flat choices list
    let mut choices: Vec<DeviceChoice> = vec![];

    if filter_attached {
        // Detach mode: only show attached devices
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
                choices.push(DeviceChoice::Virtual(drive, true));
            }
        }
    } else {
        // Attach / status mode: show all devices + virtual drives + create option
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
            choices.push(DeviceChoice::Virtual(drive, is_attached));
        }
        choices.push(DeviceChoice::CreateNew);
    }

    if choices.is_empty() {
        return Err(anyhow!(
            "{}",
            if filter_attached {
                "No devices or virtual drives are currently attached to the VM"
            } else {
                "No USB devices found and no virtual drives exist.\nCreate a virtual drive with: virsh-usb virtual create <name>"
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

    // Handle virtual subcommands — these don't need a VM selection
    if let Commands::Virtual { action } = &cli.command {
        return match action {
            VirtualCommands::Create { name, size } => create_virtual_drive(name, size),
            VirtualCommands::List => list_virtual_drives(),
            VirtualCommands::Delete { name } => delete_virtual_drive(name),
        };
    }

    // --device and --virtual-device are mutually exclusive
    if cli.device.is_some() && cli.virtual_device.is_some() {
        return Err(anyhow!("Cannot specify both --device and --virtual-device"));
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
        Virtual { name: String },
    }

    let selected = if let Some(vd_name) = cli.virtual_device {
        SelectedDevice::Virtual { name: vd_name }
    } else if let Some(device_spec) = cli.device {
        if device_spec.contains(':') {
            let parts: Vec<&str> = device_spec.splitn(2, ':').collect();
            if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
                SelectedDevice::RealUsb {
                    vendor_id: parts[0].to_string(),
                    product_id: parts[1].to_string(),
                }
            } else {
                return Err(anyhow!(
                    "Invalid device ID format. Expected: vendor:product (e.g., 0dd4:4105) or device name"
                ));
            }
        } else {
            let (vendor_id, product_id) =
                find_device_by_name(&device_spec, Some(&vm), filter_attached)?;
            SelectedDevice::RealUsb {
                vendor_id,
                product_id,
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
            DeviceChoice::Virtual(drive, _) => SelectedDevice::Virtual { name: drive.name },
            DeviceChoice::CreateNew => {
                let name = inquire::Text::new("Name for the new virtual drive:")
                    .prompt()
                    .context("Failed to get drive name")?;
                let size = inquire::Text::new("Size (e.g. 4G, 8G, 16G):")
                    .with_default("4G")
                    .prompt()
                    .context("Failed to get drive size")?;
                create_virtual_drive(&name, &size)?;
                SelectedDevice::Virtual { name }
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
        (Commands::Attach, SelectedDevice::Virtual { name }) => {
            attach_virtual_drive(&vm, &name)?
        }
        (Commands::Detach, SelectedDevice::Virtual { name }) => {
            detach_virtual_drive(&vm, &name)?
        }
        (Commands::Status, SelectedDevice::Virtual { name }) => {
            show_virtual_status(&vm, &name)?
        }
        (Commands::Virtual { .. }, _) => unreachable!(),
    }

    Ok(())
}
