use virtio_drivers::transport::{
    DeviceType,
    pci::{
        bus::{ConfigurationAccess, DeviceFunction, DeviceFunctionInfo, HeaderType},
        virtio_device_type,
    },
};
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

/// The type of VirtIO device at `bus:device.function`, or `None` if that
/// function is not a VirtIO device at all.
///
/// This reads configuration space directly instead of asking a
/// [`PciTransport`](virtio_drivers::transport::pci::PciTransport), because
/// building a transport is not free of side effects: dropping one resets the
/// device underneath it. A probe that builds a transport for every VirtIO
/// function it walks past, only to discard the ones of the wrong type, resets
/// those devices — including any that already have a driver attached and
/// running. Identify first, then build a transport only for the match.
pub fn virtio_type_at(bus: u8, device: u8, function: u8) -> Option<DeviceType> {
    let device_vendor = pci_read_u32(bus, device, function, 0);
    let class_revision = pci_read_u32(bus, device, function, 8);
    let bist_type_latency_cache = pci_read_u32(bus, device, function, 12);

    virtio_device_type(&DeviceFunctionInfo {
        vendor_id: device_vendor as u16,
        device_id: (device_vendor >> 16) as u16,
        class: (class_revision >> 24) as u8,
        subclass: (class_revision >> 16) as u8,
        prog_if: (class_revision >> 8) as u8,
        revision: class_revision as u8,
        header_type: HeaderType::from((bist_type_latency_cache >> 16) as u8 & 0x7f),
    })
}

#[derive(Clone)]
pub struct PortCam;

impl ConfigurationAccess for PortCam {
    fn read_word(&self, device_function: DeviceFunction, register_offset: u8) -> u32 {
        pci_read_u32(
            device_function.bus,
            device_function.device,
            device_function.function,
            register_offset,
        )
    }
    fn write_word(&mut self, device_function: DeviceFunction, register_offset: u8, data: u32) {
        pci_write_u32(
            device_function.bus,
            device_function.device,
            device_function.function,
            register_offset,
            data,
        )
    }
    unsafe fn unsafe_clone(&self) -> Self {
        PortCam
    }
}

pub fn enumerate_pci() {
    for bus in 0..=255 {
        for device in 0..32 {
            for function in 0..8 {
                let vendor = pci_read_u32(bus, device, function, 0) & 0xffff;

                if vendor != 0xffff {
                    let device_id = (pci_read_u32(bus, device, function, 0) >> 16) & 0xffff;

                    if vendor == 0x1af4 {
                        println!("virtio: ");
                        for bar in 0..6 {
                            let value = pci_read_u32(bus, device, function, 0x10 + bar * 4);

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
