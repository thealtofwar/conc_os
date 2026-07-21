use spin::{Mutex, Once};
use virtio_drivers::{device::net::VirtIONetRaw, transport::{DeviceType, Transport, pci::{PciTransport, VIRTIO_VENDOR_ID, bus::{DeviceFunction, PciRoot}}}};

use crate::{pci::{PortCam, pci_read_u32}, println, virtio::KernelHal};

type VirtioNetDriver = VirtIONetRaw<KernelHal, PciTransport, 8>;

static VIRTIO_NET: Once<Mutex<VirtioNetDriver>> = Once::new();

pub fn init_virtio_net_pci() -> bool {
    if VIRTIO_NET.r#try().is_some() {
        return true;
    }

    let mut root = PciRoot::new(PortCam);

    for bus in 0..=255 {
        for device in 0..32 {
            for function in 0..8 {
                let vendor = pci_read_u32(bus, device, function, 0) as u16;
                if vendor != VIRTIO_VENDOR_ID {
                    continue;
                }

                let device_function = DeviceFunction {
                    bus,
                    device,
                    function,
                };

                match PciTransport::new::<KernelHal, _>(&mut root, device_function) {
                    Ok(transport) => {
                        if transport.device_type() != DeviceType::Network {
                            println!(
                                "virtio device at {:02x}:{:02x}.{} is {:?}, skipping",
                                bus,
                                device,
                                function,
                                transport.device_type()
                            );
                            continue;
                        }

                        match VirtIONetRaw::new(transport) {
                            Ok(driver) => {
                                VIRTIO_NET.call_once(|| Mutex::new(driver));
                                println!(
                                    "initialized virtio network at {:02x}:{:02x}.{}",
                                    bus, device, function
                                );
                                return true;
                            }
                            Err(err) => {
                                println!(
                                    "failed to initialize virtio network at {:02x}:{:02x}.{}: {:?}",
                                    bus, device, function, err
                                );
                            }
                        }
                    }
                    Err(err) => {
                        println!(
                            "failed to build virtio transport at {:02x}:{:02x}.{}: {:?}",
                            bus, device, function, err
                        );
                    }
                }
            }
        }
    }

    false
}
