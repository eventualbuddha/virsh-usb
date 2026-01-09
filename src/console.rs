use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use std::process::Command;
use crate::vm::{attach_device, detach_device, show_status, select_vm, save_last_vm};
use crate::usb::{select_usb_device, find_device_by_name};

/// CLI tool to manage USB device attachment to virsh VMs
#[derive(Parser)]
#[command(name = "virsh-usb")]
#[command(about = "Manage USB device attachment to virsh VMs")]
pub struct Cli {
    /// Name of the virsh VM (if not provided, will prompt interactively)
    #[arg(long)]
    pub vm: Option<String>,

    /// USB device ID in format vendor:product (e.g., 0dd4:4105) or device name (e.g., "PaperHandler") (if not provided, will prompt interactively)
    #[arg(long)]
    pub device: Option<String>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Attach the USB device to the VM
    Attach,
    /// Detach the USB device from the VM
    Detach,
    /// Show current status
    Status,
}

pub fn run_command(args: &[&str]) -> Result<String> {
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


pub fn run() -> Result<()> {
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
