use spin::Once;
use virtio_drivers::{
    device::rng::VirtIORng,
    transport::{
        DeviceType,
        pci::{
            PciTransport, VirtioPciError,
            bus::{DeviceFunction, PciRoot},
        },
    },
};

use crate::{
    mutex::InterruptMutex,
    pci::{PortCam, virtio_type_at},
    println,
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
    FailedInit(virtio_drivers::Error),
    VirtioError(VirtioPciError),
}

/// Brings up the entropy source at `device_function`.
///
/// The caller must have already established that this function *is* an entropy
/// source. Building a transport for anything else and dropping it resets that
/// device.
fn init_rng_from_df(
    root: &mut PciRoot<PortCam>,
    device_function: &DeviceFunction,
) -> Result<(), DeviceErr> {
    match PciTransport::new::<KernelHal, _>(root, *device_function) {
        Ok(transport) => match VirtIORng::new(transport) {
            Ok(mut driver) => {
                driver.enable_interrupts();
                VIRTIO_RNG.call_once(|| InterruptMutex::new(driver));
                Ok(())
            }
            Err(err) => Err(DeviceErr::FailedInit(err)),
        },
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
                // Every other VirtIO device is left untouched: the network card
                // is already up by the time this runs, and merely building a
                // transport over it would reset it out from under its driver.
                if virtio_type_at(bus, device, function) != Some(DeviceType::EntropySource) {
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

pub trait FromRand {
    const SIZE_BYTES: usize;
    fn from_rand() -> Result<Self, virtio_drivers::Error>
    where
        Self: Sized;
}

macro_rules! impl_from_slice {
    ($($t:ty),*) => {
        $(
            impl FromRand for $t {
                const SIZE_BYTES: usize = (Self::BITS / 8) as usize;

                fn from_rand() -> Result<Self, virtio_drivers::Error> {
                    let mut arr: [u8; Self::SIZE_BYTES] = [0u8; Self::SIZE_BYTES];
                    get_random(&mut arr)?;
                    Ok(Self::from_ne_bytes(arr))
                }
            }
        )*
    };
}

impl_from_slice!(u8, u16, u32, u64, i8, i16, i32, i64);
