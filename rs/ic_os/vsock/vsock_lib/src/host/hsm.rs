use crate::host::command_utilities::handle_command_output;
use crate::protocol::Response;
use libusb::Device;
use std::io::{Error, ErrorKind, Write};
use tempfile::NamedTempFile;

// nitrokey:
const HSM_VENDOR: u16 = 8352;
const HSM_PRODUCT: u16 = 16944;

// the hard-coded domain name defined in the xml file for starting guestOS in virsh
const DOMAIN_NAME: &str = "guestos";

#[derive(Debug)]
struct HSMInfo {
    hsm_bus_num: u8,
    hsm_address: u8,
}

impl std::fmt::Display for HSMInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "HSMInfo {{ bus: {}, address: {} }}",
            self.hsm_bus_num, self.hsm_address
        )
    }
}

pub fn attach_hsm() -> Response {
    hsm_helper("attach-device")
}

pub fn detach_hsm() -> Response {
    hsm_helper("detach-device")
}

fn hsm_helper(command: &str) -> Response {
    let hsm_xml_file = create_hsm_xml_file()?;

    println!("Sending virsh command: {command}");
    let command_output = std::process::Command::new("virsh")
        .arg(command)
        .arg(DOMAIN_NAME)
        .arg("--file")
        .arg(hsm_xml_file.path())
        .output();

    handle_command_output(command_output)
}

fn create_hsm_xml_file() -> Result<NamedTempFile, String> {
    let hsm_info: HSMInfo = get_hsm_info().map_err(|_| "Could not get hsm info".to_string())?;

    println!("HSM found: {}", hsm_info);

    let xml: String = get_hsm_xml_string(&hsm_info);

    write_to_temp_file(&xml).map_err(|_| "Could not write to temp file".to_string())
}

fn get_hsm_info() -> Result<HSMInfo, Error> {
    let context = libusb::Context::new().map_err(|e| Error::new(ErrorKind::Other, e))?;

    let usb_devices = context
        .devices()
        .map_err(|e| Error::new(ErrorKind::Other, e))?;

    fn is_hsm_device(device: &Device) -> bool {
        println!(
            "Bus {:03} Device {:03} ID {:04x}:{:04x}",
            device.bus_number(),
            device.address(),
            device.device_descriptor().unwrap().vendor_id(),
            device.device_descriptor().unwrap().product_id()
        );

        let device_descriptor = match device.device_descriptor() {
            Ok(device_descriptor) => device_descriptor,
            Err(_) => {
                println!("Error: device.device_descriptor() returned error");
                return false;
            }
        };
        device_descriptor.vendor_id() == HSM_VENDOR && device_descriptor.product_id() == HSM_PRODUCT
    }

    println!("Iterating over attached devices to find hsm");
    // return the first usb device that satisfies the is_hsm_device filter
    let x = match usb_devices.iter().find(is_hsm_device) {
        Some(hsm_device) => Ok(HSMInfo {
            hsm_bus_num: hsm_device.bus_number(),
            hsm_address: hsm_device.address(),
        }),
        None => return Err(Error::new(ErrorKind::Other, "No HSM device found")),
    };
    x
}

// HSM_VENDOR and HSM_PRODUCT must be converted to hexadecimal for the attach/detach hsm virsh commands
fn get_hsm_xml_string(hsm_info: &HSMInfo) -> String {
    format!(
        "
<hostdev mode='subsystem' type='usb' managed='yes'>
    <source>
        <vendor id='{0:#06x}'/>
        <product id='{1:#06x}'/>
        <address bus='{2}' port='1' device='{3}'/>
    </source>
    <address type='usb' bus='0' port='2'/>
</hostdev>
",
        HSM_VENDOR, HSM_PRODUCT, hsm_info.hsm_bus_num, hsm_info.hsm_address
    )
}

fn write_to_temp_file(content: &str) -> Result<NamedTempFile, Error> {
    let mut file: NamedTempFile = NamedTempFile::new()?;
    write!(file, "{content}")?;
    Ok(file)
}

pub mod tests {
    #[test]
    fn get_hsm_xml_string() {
        use super::*;

        let hsm_info = HSMInfo {
            hsm_bus_num: 11u8,
            hsm_address: 12u8,
        };
        let actual = get_hsm_xml_string(&hsm_info);

        let expected: String = "
<hostdev mode='subsystem' type='usb' managed='yes'>
    <source>
        <vendor id='0x20a0'/>
        <product id='0x4230'/>
        <address bus='11' port='1' device='12'/>
    </source>
    <address type='usb' bus='0' port='2'/>
</hostdev>
"
        .to_string();
        assert_eq!(actual, expected)
    }
}
