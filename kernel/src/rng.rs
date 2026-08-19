use core::sync::atomic::{Ordering, compiler_fence};

use rand_chacha::{
    ChaCha20Rng,
    rand_core::{RngCore, SeedableRng},
};
use raw_cpuid::CpuId;
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

/// Which hardware sources seeded the CSPRNG that [`get_random`] draws from.
///
/// Every variant but [`EntropyMode::Insecure`] serves bytes from the same
/// ChaCha20 construction; they differ only in what went into the seed. The
/// device is not a separate path any more, because a DRBG seeded from the
/// device is strictly better than calling the device per request: it survives
/// the device going away, it costs no host round-trip, and it cannot be starved
/// by a host that rate-limits entropy requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntropyMode {
    /// Seeded from both the virtio entropy source and RDRAND.
    VirtioAndRdrand,
    /// Seeded from the virtio entropy source alone.
    Virtio,
    /// Seeded from RDRAND alone. This is what OCI gives you.
    Rdrand,
    /// No hardware source at all: TSC-seeded xorshift. Predictable; see
    /// [`Xorshift64`].
    Insecure,
}

static CSPRNG: Once<InterruptMutex<Csprng>> = Once::new();
static ENTROPY_MODE: Once<EntropyMode> = Once::new();

/// Blocks handed out before fresh hardware entropy is stirred back into the key.
const RESEED_INTERVAL: u32 = 1024;

/// Overwrites `buf` in a way the optimiser is not permitted to elide.
///
/// A plain `buf.fill(0)` on a buffer that is never read again is dead code and
/// LLVM deletes it, which is exactly the case for every buffer worth clearing.
/// The volatile writes have to be emitted, and the fence stops them being sunk
/// past the end of the value's life.
fn zeroize(buf: &mut [u8]) {
    for byte in buf.iter_mut() {
        // SAFETY: `byte` comes from a live mutable borrow, so it is valid and
        // properly aligned for a one-byte write.
        unsafe { core::ptr::write_volatile(byte, 0) };
    }

    compiler_fence(Ordering::SeqCst);
}

/// Draws 32 bytes from the virtio entropy source, if one came up.
///
/// A short read is treated as failure rather than padded: seed material that is
/// partly zero is worse than admitting the source did not deliver.
fn virtio_bytes() -> Option<[u8; 32]> {
    let mut out = [0u8; 32];

    match VIRTIO_RNG.r#try()?.lock().request_entropy(&mut out) {
        Ok(n) if n == out.len() => Some(out),
        _ => {
            zeroize(&mut out);
            None
        }
    }
}

/// Collects seed material from every hardware source present.
///
/// Sources are XORed together rather than chosen between. XOR of independent
/// sources is at least as strong as the best of them, so a source that turns
/// out to be degraded cannot drag the seed below what the other would have
/// given on its own. That argument rests on the sources being independent,
/// which is why the mode names them rather than blending them away.
fn gather_seed() -> Option<([u8; 32], EntropyMode)> {
    let virtio = virtio_bytes();
    let rdrand = rdrand_bytes();

    let mode = match (virtio.is_some(), rdrand.is_some()) {
        (true, true) => EntropyMode::VirtioAndRdrand,
        (true, false) => EntropyMode::Virtio,
        (false, true) => EntropyMode::Rdrand,
        (false, false) => return None,
    };

    let mut seed = [0u8; 32];
    // flatten flattens the [Option<T>] into [T]
    for mut source in [virtio, rdrand].into_iter().flatten() {
        for (s, b) in seed.iter_mut().zip(source.iter()) {
            *s ^= *b;
        }

        zeroize(&mut source);
    }

    Some((seed, mode))
}

/// Whether this CPU advertises RDRAND.
fn has_rdrand() -> bool {
    CpuId::new()
        .get_feature_info()
        .is_some_and(|info| info.has_rdrand())
}

/// Draws 32 bytes from RDRAND.
///
/// Intel documents RDRAND as able to fail transiently once the on-chip pool is
/// drained, and asks callers to retry roughly ten times before treating that as
/// a real failure. Returning `None` is the honest answer when it keeps failing:
/// silently substituting something weaker is how a CSPRNG stops being one.
///
/// # Safety
///
/// The caller must have confirmed RDRAND support with [`has_rdrand`].
#[target_feature(enable = "rdrand")]
unsafe fn rdrand_bytes_unchecked() -> Option<[u8; 32]> {
    let mut out = [0u8; 32];

    for chunk in out.chunks_exact_mut(8) {
        let mut word = 0u64;

        let ok = (0..10).any(|_| core::arch::x86_64::_rdrand64_step(&mut word) == 1);
        if !ok {
            return None;
        }

        chunk.copy_from_slice(&word.to_le_bytes());
        // SAFETY: `word` is a live local; this is the same volatile-write
        // reasoning as `zeroize`, which cannot be reused here because the
        // scratch value is a `u64` rather than a byte slice.
        unsafe { core::ptr::write_volatile(&mut word, 0) };
    }

    compiler_fence(Ordering::SeqCst); // guard the last write_volatile and ensure that it's zeroized
    Some(out)
}

fn rdrand_bytes() -> Option<[u8; 32]> {
    if !has_rdrand() {
        return None;
    }

    // SAFETY: RDRAND support was just confirmed via CPUID.
    unsafe { rdrand_bytes_unchecked() }
}

/// ChaCha20 driven as a CSPRNG with fast key erasure.
///
/// The stream cipher itself is `rand_chacha`, the reference implementation
/// behind the `rand` ecosystem. Only the construction around it lives here:
/// every 64 bytes drawn from the cipher are split in half, the first 32
/// becoming the seed of a freshly constructed instance and only the remaining
/// 32 being handed out.
///
/// That buys forward secrecy, which a plain counter-mode stream does not have.
/// `rand_chacha` keeps its key for the life of the instance, so anything that
/// read the state could rewind the counter and reproduce every byte ever
/// issued; re-seeding from the cipher's own output destroys the state that
/// produced earlier bytes. It costs half the throughput and a re-key every 32
/// bytes, which is irrelevant at the rate a DHCP client asks for exchange ids.
struct Csprng {
    inner: ChaCha20Rng,
    buf: [u8; 32],
    buf_pos: usize,
    blocks: u32,
}

impl Csprng {
    fn new(mut seed: [u8; 32]) -> Self {
        let inner = ChaCha20Rng::from_seed(seed);
        zeroize(&mut seed);

        Self {
            inner,
            buf: [0; 32],
            // Equal to buf.len(), so the first fill refills before reading.
            buf_pos: 32,
            blocks: 0,
        }
    }

    fn refill(&mut self) {
        let mut block = [0u8; 64];
        self.inner.fill_bytes(&mut block);

        let mut next_seed = [0u8; 32];
        next_seed.copy_from_slice(&block[..32]);

        self.buf.copy_from_slice(&block[32..]);
        self.buf_pos = 0;
        zeroize(&mut block);

        self.blocks = self.blocks.wrapping_add(1);
        if self.blocks >= RESEED_INTERVAL {
            self.blocks = 0;

            // XOR into the seed rather than replacing it. If a hardware source
            // is ever degraded or hostile, the result is no weaker than the
            // state we already had; replacing outright would hand it full
            // control. This takes the virtio lock while holding the CSPRNG
            // lock, which is the module's only nesting and matches the order
            // `init_entropy` uses, so it cannot deadlock.
            if let Some((mut fresh, _)) = gather_seed() {
                for (s, f) in next_seed.iter_mut().zip(fresh.iter()) {
                    *s ^= *f;
                }

                zeroize(&mut fresh);
            }
        }

        self.inner = ChaCha20Rng::from_seed(next_seed);
        zeroize(&mut next_seed);
    }

    fn fill(&mut self, buf: &mut [u8]) {
        for byte in buf.iter_mut() {
            if self.buf_pos == self.buf.len() {
                self.refill();
            }

            *byte = self.buf[self.buf_pos];
            self.buf_pos += 1;
        }
    }
}

/// Probes for entropy hardware and seeds the CSPRNG from whatever is there.
///
/// This is the only entry point the rest of the kernel needs: it brings up the
/// virtio device if the host has one, folds it and RDRAND into a single seed,
/// and reports what it managed to find.
pub fn init_entropy() -> EntropyMode {
    init_virtio_rng();

    let mode = match gather_seed() {
        Some((mut seed, mode)) => {
            CSPRNG.call_once(|| InterruptMutex::new(Csprng::new(seed)));
            zeroize(&mut seed);
            mode
        }
        None => EntropyMode::Insecure,
    };

    *ENTROPY_MODE.call_once(|| mode)
}

/// Non-cryptographic entropy, for hosts that expose no virtio entropy source.
///
/// OCI's paravirtualized shapes give you virtio-blk and virtio-net but no
/// entropy source, and the DHCP client needs an exchange id before it can put
/// anything on the wire. This is enough to keep transaction ids from
/// colliding. It is not unguessable, so anything that needs real entropy must
/// check [`entropy_is_hardware_backed`] first.
struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    const fn new() -> Self {
        Self { state: 0 }
    }

    fn next(&mut self) -> u64 {
        if self.state == 0 {
            // Seeded on first use rather than at init: by the time anything
            // asks for entropy the TSC has advanced an amount that depends on
            // how long device probing took. The `| 1` matters because zero is
            // a fixed point of xorshift.
            self.state = unsafe { core::arch::x86_64::_rdtsc() } | 1;
        }

        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }
}

static FALLBACK: InterruptMutex<Xorshift64> = InterruptMutex::new(Xorshift64::new());

/// Fills `buf` with entropy from the seeded CSPRNG.
///
/// This used to `expect` the virtio device, which took the whole kernel down on
/// any host without one: `init_virtio_rng` would report `false` quite happily,
/// and then DHCP would panic here a moment later asking for an exchange id.
pub fn get_random(buf: &mut [u8]) -> Result<usize, virtio_drivers::Error> {
    if let Some(csprng) = CSPRNG.r#try() {
        csprng.lock().fill(buf);
        return Ok(buf.len());
    }

    let mut fallback = FALLBACK.lock();
    for chunk in buf.chunks_mut(8) {
        let bytes = fallback.next().to_ne_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }

    Ok(buf.len())
}

/// Which hardware sources seeded the generator [`get_random`] draws from.
pub fn entropy_mode() -> EntropyMode {
    ENTROPY_MODE
        .r#try()
        .copied()
        .unwrap_or(EntropyMode::Insecure)
}

/// Whether [`get_random`] is safe for values an attacker must not predict.
///
/// Anything security-sensitive should check this and refuse, rather than
/// quietly accepting whatever the host happened to leave us with.
pub fn entropy_is_cryptographic() -> bool {
    entropy_mode() != EntropyMode::Insecure
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

fn init_virtio_rng() -> bool {
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
