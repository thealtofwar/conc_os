use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::instructions::port::Port;

use crate::{apic, interrupts::TIMER_VECTOR, println};

/// Timer ticks per second. 1 kHz gives the millisecond resolution TCP wants
/// without the tick itself costing anything measurable.
pub const TICK_HZ: u32 = 1000;

/// Prescaler applied to the LAPIC's input clock. Encoded per SDM Vol.3
/// 11.5.4: the value lives in bits 3 and 1:0, with bit 2 unused.
const DIVIDE_BY_16: u32 = 0b0011;

const LVT_TIMER: u64 = 0x320;
const INITIAL_COUNT: u64 = 0x380;
const CURRENT_COUNT: u64 = 0x390;
const DIVIDE_CONFIG: u64 = 0x3E0;

const LVT_MASKED: u32 = 1 << 16;
const LVT_PERIODIC: u32 = 0b01 << 17;

const PIT_FREQ: u32 = 1_193_182;
/// PIT counts for the calibration window. Longer is more accurate; 10 ms is
/// well inside the 16-bit divisor and short enough not to stall boot.
const CALIBRATION_MS: u32 = 10;

static TICKS: AtomicU64 = AtomicU64::new(0);

/// Milliseconds since the timer was started.
///
/// Monotonic and never wraps in practice: at 1 kHz a `u64` outlasts the
/// machine by a wide margin.
pub fn now_ms() -> u64 {
    TICKS.load(Ordering::Relaxed) * 1000 / TICK_HZ as u64
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Called from the timer interrupt. Nothing else may call this.
pub(crate) fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

/// Measures the LAPIC timer's input frequency against PIT channel 2.
///
/// Channel 2 is used rather than channel 0 because its gate is software
/// controlled and its output is readable from port 0x61, so the measurement
/// needs no interrupt — which matters here, since the PICs are masked.
///
/// Returns LAPIC ticks per second at [`DIVIDE_BY_16`].
///
/// # Safety
///
/// The caller must hold exclusive use of PIT channel 2 and of the local APIC
/// timer registers; both are clobbered without being saved. Interrupts should
/// also be disabled, not for soundness but because anything servicing an IRQ
/// inside the polling loop inflates the measurement.
unsafe fn calibrate() -> u32 {
    let mut port61 = Port::<u8>::new(0x61);
    let mut command = Port::<u8>::new(0x43);
    let mut channel2 = Port::<u8>::new(0x42);

    unsafe {
        // Gate low and speaker disconnected: the counter is held while it is
        // being programmed, and nothing reaches the speaker when it runs. Bits
        // 2 and 3 are SERR/IOCHK enables, so they are preserved rather than
        // cleared along with them.
        let speaker_disable = port61.read() & 0xFC;

        port61.write(speaker_disable);

        // Channel 2, lobyte/hibyte, mode 0 (interrupt on terminal count),
        // binary. Mode 0 drives OUT low now and high when the count expires,
        // which is the edge polled for below.
        command.write(0xB0);

        let divisor = (PIT_FREQ * CALIBRATION_MS / 1000) as u16;
        channel2.write(divisor as u8);
        channel2.write((divisor >> 8) as u8);

        apic::write_register(DIVIDE_CONFIG, DIVIDE_BY_16);
        // Masked: this run is only a stopwatch, and no handler is installed yet.
        apic::write_register(LVT_TIMER, LVT_MASKED);

        // Raising the gate starts the PIT. The LAPIC timer starts on the very
        // next store, so the two windows differ by a few cycles at most.
        let start_pit = port61.read() | 0x01;

        port61.write(start_pit);
        apic::write_register(INITIAL_COUNT, u32::MAX);

        // Bit 5 is channel 2's OUT line, high once the count has expired.
        while port61.read() & 0x20 == 0 {
            core::hint::spin_loop();
        }

        let remaining = apic::read_register(CURRENT_COUNT);
        apic::write_register(INITIAL_COUNT, 0); // writing zero halts the timer

        // Drop the gate so the channel is left as it was found.
        let gate_low = port61.read() & 0xFE;

        port61.write(gate_low);

        let elapsed = u32::MAX - remaining;

        // Scaled up before dividing, so the result is not quantised to whole
        // kHz. Widened first because the intermediate does not fit in 32 bits.
        (elapsed as u64 * 1000 / CALIBRATION_MS as u64) as u32
    }
}

pub fn init_timer() {
    // Sound only because the timer is brought up once, during `init`, with
    // interrupts still masked and nothing else yet driving the PIT.
    let hz = unsafe { calibrate() };
    let count = hz / TICK_HZ;

    println!("LAPIC timer: {} Hz input, {} counts per tick", hz, count);

    assert!(
        count > 0,
        "calibration produced a zero reload; an initial count of 0 disables the timer"
    );

    unsafe {
        apic::write_register(DIVIDE_CONFIG, DIVIDE_BY_16);
        apic::write_register(LVT_TIMER, TIMER_VECTOR as u32 | LVT_PERIODIC);
        apic::write_register(INITIAL_COUNT, count);
    }
}
