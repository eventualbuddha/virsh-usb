use std::fmt;

#[derive(Debug, Clone)]
pub struct UsbDevice {
    pub bus: String,
    pub device: String,
    pub vendor_id: String,
    pub product_id: String,
    pub name: String,
    pub attached: bool,
}

impl fmt::Display for UsbDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let id_part = console::style(format!("{}:{}", self.vendor_id, self.product_id)).cyan();
        let bus_info = console::style(format!("(Bus {} Device {})", self.bus, self.device)).dim();

        if self.attached {
            write!(
                f,
                "{} - {} {} {}",
                id_part,
                self.name,
                bus_info,
                console::style("[attached]").green()
            )
        } else {
            write!(f, "{} - {} {}", id_part, self.name, bus_info)
        }
    }
}

/// Parse `lsusb` output and return a list of devices, filtering out system root hubs.
pub fn parse_lsusb_output(output: &str) -> Vec<UsbDevice> {
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

                    // Filter out Linux Foundation root hubs (e.g., vendor 1d6b product 0002/0003)
                    let name_lower = name.to_lowercase();
                    if name_lower.contains("root hub") || name_lower.contains("linux foundation") || (vendor_id == "1d6b" && (product_id == "0002" || product_id == "0003")) {
                        continue;
                    }

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

    devices
}

/// Find a device (bus, device) in lsusb output for the given vendor/product, skipping root hubs.
use anyhow::{Context, Result, anyhow};
use inquire::Select;

pub fn find_usb_device_in_output(output: &str, vendor_id: &str, product_id: &str) -> Option<(String, String)> {
    for line in output.lines() {
        if line.contains(&format!("{}:{}", vendor_id, product_id)) {
            // Skip system root hubs like "Linux Foundation 3.0 root hub" or "2.0 root hub"
            let line_lower = line.to_lowercase();
            if line_lower.contains("root hub") || line_lower.contains("linux foundation") {
                continue;
            }

            // Parse: Bus 003 Device 006: ID 0dd4:4105 Custom Engineering SPA PaperHandler
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 4 {
                let bus = parts[1].to_string();
                let device = parts[3].trim_end_matches(':').to_string();
                return Some((bus, device));
            }
        }
    }

    None
}

/// Run `lsusb` and find a device's bus/device
pub fn find_usb_device(vendor_id: &str, product_id: &str) -> Result<Option<(String, String)>> {
    let output = crate::console::run_command(&["lsusb"])?;
    Ok(find_usb_device_in_output(&output, vendor_id, product_id))
}

/// Parse `virsh dumpxml` for attached hostdev usb devices
pub fn get_attached_devices(vm_name: &str) -> Result<Vec<(String, String)>> {
    let output = match crate::console::run_command(&["virsh", "dumpxml", vm_name]) {
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

pub fn is_device_attached(vm_name: &str, vendor_id: &str, product_id: &str) -> Result<bool> {
    let devices = get_attached_devices(vm_name)?;
    Ok(devices
        .iter()
        .any(|(v, p)| v == vendor_id && p == product_id))
}

pub fn get_all_usb_devices() -> Result<Vec<UsbDevice>> {
    let output = crate::console::run_command(&["lsusb"])?;
    Ok(parse_lsusb_output(&output))
}

pub fn find_device_by_name(name: &str, vm_name: Option<&str>, filter_attached: bool) -> Result<(String, String)> {
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
            "No USB device found matching name: '{}',",
            name
        )),
        1 => Ok((matches[0].vendor_id.clone(), matches[0].product_id.clone())),
        _ => {
            // Multiple matches, prompt user to select
            let device_strings: Vec<String> = matches.iter().map(|d| d.to_string()).collect();
            let selection = Select::new(
                &format!(
                    "{} Multiple devices match '{}'. Select one",
                    console::style("🔌").cyan(),
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

pub fn select_usb_device(vm_name: Option<&str>, filter_attached: bool) -> Result<(String, String)> {
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
        format!("{} Select a USB device to detach", console::style("🔌").yellow())
    } else {
        format!("{} Select a USB device", console::style("🔌").cyan())
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