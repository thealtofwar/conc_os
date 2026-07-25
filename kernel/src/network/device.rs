use spin::Once;
use virtio_drivers::{
    device::net::VirtIONet,
    transport::{
        DeviceType, Transport,
        pci::{
            PciTransport, VIRTIO_VENDOR_ID, VirtioPciError,
            bus::{DeviceFunction, PciRoot},
        },
    },
};

use crate::{
    apic::route_pci_interrupt,
    interrupts::VIRTIO_NET_VECTOR,
    mutex::InterruptMutex,
    pci::{PortCam, pci_read_u32},
    println,
    virtio::KernelHal,
};

pub type VirtioNetDriver = VirtIONet<KernelHal, PciTransport, 8>;

static VIRTIO_NET: Once<InterruptMutex<VirtioNetDriver>> = Once::new();

pub fn get_net_driver() -> &'static InterruptMutex<VirtioNetDriver> {
    VIRTIO_NET.r#try().expect("VIRTIO_NET must be initialized")
}

enum DeviceErr {
    NotNetwork(DeviceType),
    FailedInit(virtio_drivers::Error),
    VirtioError(VirtioPciError),
}

fn init_net_from_df(
    root: &mut PciRoot<PortCam>,
    device_function: &DeviceFunction,
) -> Result<(), DeviceErr> {
    match PciTransport::new::<KernelHal, _>(root, *device_function) {
        Ok(transport) => {
            if transport.device_type() != DeviceType::Network {
                return Err(DeviceErr::NotNetwork(transport.device_type()));
            }

            match VirtIONet::new(transport, 16384) {
                Ok(mut driver) => {
                    driver.enable_interrupts();
                    VIRTIO_NET.call_once(|| InterruptMutex::new(driver));
                    Ok(())
                }
                Err(err) => Err(DeviceErr::FailedInit(err)),
            }
        }
        Err(err) => Err(DeviceErr::VirtioError(err)),
    }
}

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

                match init_net_from_df(&mut root, &device_function) {
                    Ok(_) => {
                        println!(
                            "initialized virtio network at {:02x}:{:02x}.{}",
                            bus, device, function
                        );
                        let pci_reg = pci_read_u32(bus, device, function, 0x3C);
                        let gsi = (pci_reg & 0xff) as u8;
                        let pin = ((pci_reg >> 8) & 0xff) as u8;

                        let command_status = pci_read_u32(bus, device, function, 0x04);

                        let command = command_status as u16;

                        assert_eq!(command & (1 << 10), 0);

                        if pin == 0 {
                            panic!("VirtIO device has no interrupt pin");
                        }

                        println!("VirtIO: interrupt line={} pin={}", gsi, pin);
                        route_pci_interrupt(gsi, VIRTIO_NET_VECTOR);
                        return true;
                    }
                    Err(DeviceErr::NotNetwork(dtype)) => {
                        println!(
                            "virtio device at {:02x}:{:02x}.{} is {:?}, skipping",
                            bus, device, function, dtype
                        );
                    }
                    Err(DeviceErr::FailedInit(err)) => {
                        println!(
                            "failed to initialize virtio network at {:02x}:{:02x}.{}: {:?}",
                            bus, device, function, err
                        );
                    }
                    Err(DeviceErr::VirtioError(err)) => {
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
