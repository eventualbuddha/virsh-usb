use anyhow::{Context, Result, anyhow};
use console::style;
use inquire::Select;
use directories::ProjectDirs;
use std::fs;
use std::path::PathBuf;

use crate::console::run_command;
use crate::usb::{get_all_usb_devices, find_usb_device, is_device_attached};

pub fn check_vm_running(vm_name: &str) -> Result<bool> {
    let output = run_command(&["virsh", "list", "--name"])?;
    Ok(output.lines().any(|line| line.trim() == vm_name))
}

pub fn attach_device(vm_name: &str, vendor_id: &str, product_id: &str) -> Result<()> {
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

pub fn detach_device(vm_name: &str, vendor_id: &str, product_id: &str) -> Result<()> {
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

pub fn show_status(vm_name: &str, vendor_id: &str, product_id: &str) -> Result<()> {
    // Check VM status
    let vm_running = check_vm_running(vm_name)?;
    println!(
        "{} VM ({}): {}",
        style("🖥").cyan(),
        style(vm_name).yellow(),
        if vm_running { style("Running").green() } else { style("Not running").red() }
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
            if attached { style("Attached to VM").green() } else { style("Not attached to VM").yellow() }
        );
    }

    Ok(())
}

pub fn get_config_file() -> Result<PathBuf> {
    let proj_dirs = ProjectDirs::from("", "", "virsh-usb")
        .ok_or_else(|| anyhow!("Could not determine config directory"))?;
    let config_dir = proj_dirs.config_dir();
    fs::create_dir_all(config_dir)?;
    Ok(config_dir.join("last_vm"))
}

pub fn save_last_vm(vm: &str) -> Result<()> {
    let config_file = get_config_file()?;
    fs::write(config_file, vm)?;
    Ok(())
}

pub fn load_last_vm() -> Option<String> {
    let config_file = get_config_file().ok()?;
    fs::read_to_string(config_file).ok()
}

pub fn get_all_vms() -> Result<Vec<String>> {
    let output = run_command(&["virsh", "list", "--all", "--name"])?;
    Ok(output
        .lines()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect())
}

pub fn select_vm() -> Result<String> {
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
