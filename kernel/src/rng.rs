use spin::Once;
use virtio_drivers::{
    device::rng::VirtIORng,
    transport::{
        DeviceType, Transport,
        pci::{
            PciTransport, VIRTIO_VENDOR_ID, VirtioPciError,
            bus::{DeviceFunction, PciRoot},
        },
    },
};

use crate::{
    mutex::InterruptMutex,
    pci::{PortCam, pci_read_u32},
    println,
    rng::DeviceErr::NotRNG,
    virtio::KernelHal,
};

pub type VirtioRngDriver = VirtIORng<KernelHal, PciTransport>;

static VIRTIO_RNG: Once<InterruptMutex<VirtioRngDriver>> = Once::new();

pub fn get_random(buf: &mut [u8]) -> Result<usize, virtio_drivers::Error> {
    VIRTIO_RNG
        .r#try()
        .expect("virtio rng must be enabled")
        .lock()
        .request_entropy(buf)
}

enum DeviceErr {
    NotRNG(DeviceType),
    FailedInit(virtio_drivers::Error),
    VirtioError(VirtioPciError),
}

fn init_rng_from_df(
    root: &mut PciRoot<PortCam>,
    device_function: &DeviceFunction,
) -> Result<(), DeviceErr> {
    match PciTransport::new::<KernelHal, _>(root, *device_function) {
        Ok(transport) => {
            if transport.device_type() != DeviceType::EntropySource {
                return Err(NotRNG(transport.device_type()));
            }
            match VirtIORng::new(transport) {
                Ok(mut driver) => {
                    driver.enable_interrupts();
                    VIRTIO_RNG.call_once(|| InterruptMutex::new(driver));
                    Ok(())
                }
                Err(err) => Err(DeviceErr::FailedInit(err)),
            }
        }
        Err(err) => Err(DeviceErr::VirtioError(err)),
    }
}

pub fn init_virtio_rng() -> bool {
    if VIRTIO_RNG.r#try().is_some() {
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

                match init_rng_from_df(&mut root, &device_function) {
                    Ok(_) => {
                        println!(
                            "initialized virtio rng at {:02x}:{:02x}.{}",
                            bus, device, function
                        );
                        return true;
                    }
                    Err(DeviceErr::FailedInit(err)) => {
                        println!(
                            "failed to initialize virtio rng at {:02x}:{:02x}.{}: {:?}",
                            bus, device, function, err
                        );
                    }
                    Err(DeviceErr::NotRNG(dtype)) => {
                        println!(
                            "virtio device at {:02x}:{:02x}.{} is {:?}, skipping",
                            bus, device, function, dtype
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
