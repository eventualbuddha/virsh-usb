use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use console::style;
use inquire::Select;
use directories::ProjectDirs;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// CLI tool to manage USB device attachment to virsh VMs
#[derive(Parser)]
#[command(name = "virsh-usb")]
#[command(about = "Manage USB device attachment to virsh VMs")]
struct Cli {
    /// Name of the virsh VM (if not provided, will prompt interactively)
    #[arg(long)]
    vm: Option<String>,

    /// USB device ID in format vendor:product (e.g., 0dd4:4105) or device name (e.g., "PaperHandler") (if not provided, will prompt interactively)
    #[arg(long)]
    device: Option<String>,

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
}

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

fn is_device_attached(vm_name: &str, vendor_id: &str, product_id: &str) -> Result<bool> {
    let devices = get_attached_devices(vm_name)?;
    Ok(devices
        .iter()
        .any(|(v, p)| v == vendor_id && p == product_id))
}

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

#[derive(Debug, Clone)]
struct UsbDevice {
    bus: String,
    device: String,
    vendor_id: String,
    product_id: String,
    name: String,
    attached: bool,
}

impl std::fmt::Display for UsbDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let id_part = style(format!("{}:{}", self.vendor_id, self.product_id)).cyan();
        let bus_info = style(format!("(Bus {} Device {})", self.bus, self.device)).dim();

        if self.attached {
            write!(
                f,
                "{} - {} {} {}",
                id_part,
                self.name,
                bus_info,
                style("[attached]").green()
            )
        } else {
            write!(f, "{} - {} {}", id_part, self.name, bus_info)
        }
    }
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

fn find_device_by_name(name: &str, vm_name: Option<&str>, filter_attached: bool) -> Result<(String, String)> {
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
        0 => Err(anyhow!(
            "No USB device found matching name: '{}'",
            name
        )),
        1 => Ok((matches[0].vendor_id.clone(), matches[0].product_id.clone())),
        _ => {
            // Multiple matches, prompt user to select
            let device_strings: Vec<String> = matches.iter().map(|d| d.to_string()).collect();
            let selection = Select::new(
                &format!(
                    "{} Multiple devices match '{}'. Select one",
                    style("🔌").cyan(),
                    name
                ),
                device_strings
            )
            .with_starting_cursor(0)
            .prompt()
            .context("Failed to select USB device")?;

            // Find the device that matches the selected string
            let selected_device = matches.iter()
                .find(|d| d.to_string() == selection)
                .ok_or_else(|| anyhow!("Selected device not found"))?;

            Ok((selected_device.vendor_id.clone(), selected_device.product_id.clone()))
        }
    }
}

fn select_usb_device(vm_name: Option<&str>, filter_attached: bool) -> Result<(String, String)> {
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

    if devices.is_empty() {
        if filter_attached {
            return Err(anyhow!("No USB devices are currently attached to the VM"));
        } else {
            return Err(anyhow!("No USB devices found"));
        }
    }

    let device_strings: Vec<String> = devices.iter().map(|d| d.to_string()).collect();

    let prompt = if filter_attached {
        format!("{} Select a USB device to detach", style("🔌").yellow())
    } else {
        format!("{} Select a USB device", style("🔌").cyan())
    };

    let selection = Select::new(&prompt, device_strings)
        .with_starting_cursor(0)
        .prompt()
        .context("Failed to select USB device")?;

    // Find the device that matches the selected string
    let selected = devices.iter()
        .find(|d| d.to_string() == selection)
        .ok_or_else(|| anyhow!("Selected device not found"))?;

    Ok((selected.vendor_id.clone(), selected.product_id.clone()))
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Get VM name (from CLI or interactively)
    let vm = match cli.vm {
        Some(v) => v,
        None => select_vm()?,
    };

    // Save the selected VM for next time
    let _ = save_last_vm(&vm);

    // Get vendor and product IDs (from CLI or interactively)
    // For detach, filter to only show attached devices
    let filter_attached = matches!(cli.command, Commands::Detach);

    let (vendor_id, product_id) = match cli.device {
        Some(device_spec) => {
            // Try to parse as vendor:product format first
            if device_spec.contains(':') {
                let parts: Vec<&str> = device_spec.split(':').collect();
                if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
                    // Looks like vid:pid format
                    (parts[0].to_string(), parts[1].to_string())
                } else {
                    return Err(anyhow!(
                        "Invalid device ID format. Expected format: vendor:product (e.g., 0dd4:4105) or device name"
                    ));
                }
            } else {
                // Treat as device name
                find_device_by_name(&device_spec, Some(&vm), filter_attached)?
            }
        }
        None => select_usb_device(Some(&vm), filter_attached)?,
    };

    match cli.command {
        Commands::Attach => attach_device(&vm, &vendor_id, &product_id)?,
        Commands::Detach => detach_device(&vm, &vendor_id, &product_id)?,
        Commands::Status => show_status(&vm, &vendor_id, &product_id)?,
    }

    Ok(())
}
