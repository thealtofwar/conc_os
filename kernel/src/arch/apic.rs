//! Local APIC (xAPIC MMIO or x2APIC MSR mode) and its timer.

#![allow(dead_code)]

use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use crate::arch::cpu::{self, msr};

const MODE_NONE: u8 = 0;
const MODE_XAPIC: u8 = 1;
const MODE_X2APIC: u8 = 2;

static MODE: AtomicU8 = AtomicU8::new(MODE_NONE);
static MMIO_BASE: AtomicU64 = AtomicU64::new(0xFEE0_0000);

pub const REG_ID: u32 = 0x20;
pub const REG_VERSION: u32 = 0x30;
pub const REG_TPR: u32 = 0x80;
pub const REG_EOI: u32 = 0xB0;
pub const REG_LDR: u32 = 0xD0;
pub const REG_DFR: u32 = 0xE0;
pub const REG_SVR: u32 = 0xF0;
pub const REG_ESR: u32 = 0x280;
pub const REG_ICR_LO: u32 = 0x300;
pub const REG_ICR_HI: u32 = 0x310;
pub const REG_LVT_TIMER: u32 = 0x320;
pub const REG_LVT_THERMAL: u32 = 0x330;
pub const REG_LVT_PERF: u32 = 0x340;
pub const REG_LVT_LINT0: u32 = 0x350;
pub const REG_LVT_LINT1: u32 = 0x360;
pub const REG_LVT_ERROR: u32 = 0x370;
pub const REG_TIMER_ICR: u32 = 0x380;
pub const REG_TIMER_CCR: u32 = 0x390;
pub const REG_TIMER_DCR: u32 = 0x3E0;

const LVT_MASKED: u32 = 1 << 16;
const LVT_TIMER_PERIODIC: u32 = 1 << 17;

#[inline]
pub fn read(reg: u32) -> u32 {
    match MODE.load(Ordering::Relaxed) {
        MODE_X2APIC => cpu::rdmsr(msr::X2APIC_BASE + (reg >> 4)) as u32,
        _ => unsafe { core::ptr::read_volatile((MMIO_BASE.load(Ordering::Relaxed) + reg as u64) as *const u32) },
    }
}

#[inline]
pub fn write(reg: u32, v: u32) {
    match MODE.load(Ordering::Relaxed) {
        MODE_X2APIC => unsafe { cpu::wrmsr(msr::X2APIC_BASE + (reg >> 4), v as u64) },
        _ => unsafe { core::ptr::write_volatile((MMIO_BASE.load(Ordering::Relaxed) + reg as u64) as *mut u32, v) },
    }
}

/// Bring up the local APIC: enable it, mask every LVT entry, and point the
/// spurious vector at 0xFF.
pub fn init() {
    let base = cpu::rdmsr(msr::IA32_APIC_BASE);
    MMIO_BASE.store(base & 0x000F_FFFF_F000, Ordering::Relaxed);
    if base & (1 << 10) != 0 {
        MODE.store(MODE_X2APIC, Ordering::Relaxed);
    } else {
        MODE.store(MODE_XAPIC, Ordering::Relaxed);
    }
    if base & (1 << 11) == 0 {
        unsafe { cpu::wrmsr(msr::IA32_APIC_BASE, base | (1 << 11)) };
    }

    write(REG_TPR, 0);
    write(REG_LVT_TIMER, LVT_MASKED);
    write(REG_LVT_LINT0, LVT_MASKED);
    write(REG_LVT_LINT1, LVT_MASKED);
    write(REG_LVT_ERROR, LVT_MASKED);
    if read(REG_VERSION) >> 16 & 0xFF >= 4 {
        write(REG_LVT_PERF, LVT_MASKED);
    }
    write(REG_ESR, 0);
    write(REG_ESR, 0);
    write(REG_TIMER_DCR, 0b1011); // divide by 1
    write(REG_SVR, 0x100 | crate::arch::idt::VEC_SPURIOUS as u32);
    write(REG_EOI, 0);
}

pub fn is_x2apic() -> bool {
    MODE.load(Ordering::Relaxed) == MODE_X2APIC
}

pub fn id() -> u32 {
    let v = read(REG_ID);
    if is_x2apic() {
        v
    } else {
        v >> 24
    }
}

#[inline]
pub fn eoi() {
    write(REG_EOI, 0);
}

/// Start the timer counting down from `u32::MAX` in one-shot mode with the
/// interrupt masked (used for calibration).
pub fn timer_start_calibration() {
    write(REG_LVT_TIMER, LVT_MASKED);
    write(REG_TIMER_DCR, 0b1011);
    write(REG_TIMER_ICR, u32::MAX);
}

/// Ticks elapsed since `timer_start_calibration`.
pub fn timer_elapsed() -> u32 {
    u32::MAX - read(REG_TIMER_CCR)
}

/// Program a periodic timer interrupt on `vector` every `count` bus ticks.
pub fn timer_periodic(vector: u8, count: u32) {
    write(REG_TIMER_DCR, 0b1011);
    write(REG_LVT_TIMER, vector as u32 | LVT_TIMER_PERIODIC);
    write(REG_TIMER_ICR, count.max(1));
}

/// Program a single timer interrupt on `vector` after `count` bus ticks.
pub fn timer_oneshot(vector: u8, count: u32) {
    write(REG_TIMER_DCR, 0b1011);
    write(REG_LVT_TIMER, vector as u32);
    write(REG_TIMER_ICR, count.max(1));
}

pub fn timer_stop() {
    write(REG_LVT_TIMER, LVT_MASKED);
    write(REG_TIMER_ICR, 0);
}
