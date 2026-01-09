use virsh_usb::usb::{find_usb_device_in_output, parse_lsusb_output};

const LSUSB_MIXED: &str = "\
Bus 001 Device 001: ID 1d6b:0002 Linux Foundation 2.0 root hub
Bus 002 Device 004: ID 0dd4:4105 Custom Engineering SPA PaperHandler
Bus 003 Device 005: ID 1d6b:0003 Linux Foundation 3.0 root hub
";

#[test]
fn parse_lsusb_filters_root_hubs() {
    let devices = parse_lsusb_output(LSUSB_MIXED);
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].vendor_id, "0dd4");
    assert_eq!(devices[0].product_id, "4105");
    assert_eq!(devices[0].name, "Custom Engineering SPA PaperHandler");
}

#[test]
fn find_usb_device_skips_root_hubs() {
    // Searching for 1d6b:0003 should return None because root hub is filtered
    assert_eq!(find_usb_device_in_output(LSUSB_MIXED, "1d6b", "0003"), None);

    // Searching for the real device should return its bus/device
    let found = find_usb_device_in_output(LSUSB_MIXED, "0dd4", "4105");
    assert!(found.is_some());
    let (bus, device) = found.unwrap();
    assert_eq!(bus, "002");
    assert_eq!(device, "004");
}
