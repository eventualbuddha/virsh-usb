use anyhow::Result;

pub mod console;
pub mod vm;
pub mod usb;

fn main() -> Result<()> {
    console::run()
}
