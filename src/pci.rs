use virtio_drivers::transport::pci::bus::{ConfigurationAccess, DeviceFunction};
use x86_64::instructions::port::Port;

use crate::println;


pub fn pci_config_address(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    (1 << 31)
        | ((bus as u32) << 16)
        | ((device as u32) << 11)
        | ((function as u32) << 8)
        | ((offset as u32) & 0xfc)
}

pub fn pci_read_u32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    let address = pci_config_address(bus, device, function, offset);

    let mut addr_port = Port::<u32>::new(0xCF8);
    let mut data_port = Port::<u32>::new(0xCFC);

    unsafe {
        addr_port.write(address);
        data_port.read()
    }
}

pub fn pci_write_u32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    let address = pci_config_address(bus, device, function, offset);

    let mut addr_port = Port::<u32>::new(0xCF8);
    let mut data_port = Port::<u32>::new(0xCFC);

    unsafe {
        addr_port.write(address);
        data_port.write(value);
    }
}

#[derive(Clone)]
pub struct PortCam;

impl ConfigurationAccess for PortCam {
    fn read_word(&self, device_function: DeviceFunction, register_offset: u8) -> u32 {
        pci_read_u32(device_function.bus, device_function.device, device_function.function, register_offset)
    }
    fn write_word(&mut self, device_function: DeviceFunction, register_offset: u8, data: u32) {
        pci_write_u32(device_function.bus, device_function.device, device_function.function, register_offset, data)
    }
    unsafe fn unsafe_clone(&self) -> Self { PortCam }
}


pub fn enumerate_pci() {
    for bus in 0..=255 {
        for device in 0..32 {
            for function in 0..8 {
                let vendor = pci_read_u32(bus, device, function, 0) & 0xffff;

                if vendor != 0xffff {
                    let device_id =
                        (pci_read_u32(bus, device, function, 0) >> 16) & 0xffff;
                    
                    if vendor == 0x1af4 {
                        println!("virtio: ");
                        for bar in 0..6 {
                            let value = pci_read_u32(
                                bus,
                                device,
                                function,
                                0x10 + bar * 4,
                            );

                            println!("BAR{} = {:08x}", bar, value);
                        }
                    }

                    println!(
                        "PCI bus:{:02x} dev:{:02x} fn:{} vendor={:04x} device={:04x}",
                        bus, device, function, vendor, device_id
                    );
                }
            }
        }
    }
}
