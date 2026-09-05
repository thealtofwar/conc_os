//! Timekeeping: TSC-based uptime and a periodic APIC timer tick.
//!
//! Both the TSC and the APIC timer are calibrated against the legacy PIT,
//! which has a known 1.193182 MHz clock everywhere (QEMU included).

#![allow(dead_code)]

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::arch::cpu::{self, inb, outb};
use crate::arch::{apic, idt};

pub const TICK_HZ: u64 = 1000;

static TSC_PER_MS: AtomicU64 = AtomicU64::new(0);
static APIC_PER_MS: AtomicU64 = AtomicU64::new(0);
static BOOT_TSC: AtomicU64 = AtomicU64::new(0);
static TICKS: AtomicU64 = AtomicU64::new(0);
static TICK_HOOK: AtomicUsize = AtomicUsize::new(0);

const PIT_HZ: u64 = 1_193_182;

/// Measure TSC and APIC-timer ticks across a `ms` millisecond PIT countdown.
fn pit_measure(ms: u64) -> (u64, u64) {
    let count = (PIT_HZ * ms / 1000) as u16;
    // Gate channel 2 on, speaker off.
    let port61 = inb(0x61);
    outb(0x61, (port61 & !0x02) | 0x01);
    // Channel 2, lobyte/hibyte, mode 0 (interrupt on terminal count).
    outb(0x43, 0xB0);
    outb(0x42, count as u8);
    apic::timer_start_calibration();
    let t0 = cpu::rdtsc();
    outb(0x42, (count >> 8) as u8);
    // Wait for OUT to go high.
    while inb(0x61) & 0x20 == 0 {
        core::hint::spin_loop();
    }
    let t1 = cpu::rdtsc();
    let apic_ticks = apic::timer_elapsed() as u64;
    outb(0x61, port61 & !0x03);
    ((t1 - t0) / ms, apic_ticks / ms)
}

/// Longest gap between timer interrupts: keeps VM preemption and the
/// executor's fairness bounded even when no timer is due.
pub const MAX_TICK_GAP_US: u64 = 10_000;

/// TSC at which the one-shot timer is currently armed (0 = none).
static ARMED: AtomicU64 = AtomicU64::new(0);

fn tick_handler(_f: &mut idt::TrapFrame) {
    TICKS.fetch_add(1, Ordering::Relaxed);
    ARMED.store(0, Ordering::Relaxed);
    let hook = TICK_HOOK.load(Ordering::Acquire);
    if hook != 0 {
        let f: fn() = unsafe { core::mem::transmute(hook) };
        f();
    }
    // The hook re-arms for the next deadline; make sure something is armed.
    arm_timer_at(now() + us_to_tsc(MAX_TICK_GAP_US));
}

/// Make the timer interrupt fire at `deadline` (host TSC) unless it is
/// already armed for an earlier moment.  The timer is tickless: it fires
/// only when a deadline is due, or after `MAX_TICK_GAP_US` at the latest.
/// Safe from interrupt and task context.
pub fn arm_timer_at(deadline: u64) {
    crate::sync::without_interrupts(|| {
        let t = now();
        let cap = t + us_to_tsc(MAX_TICK_GAP_US);
        let deadline = deadline.min(cap);
        let cur = ARMED.load(Ordering::Relaxed);
        if cur != 0 && cur <= deadline && cur > t {
            return;
        }
        let delta = deadline.saturating_sub(t).max(1);
        // APIC bus ticks for `delta` TSC ticks.
        let apic_per_ms = APIC_PER_MS.load(Ordering::Relaxed).max(1);
        let tsc_per_ms = TSC_PER_MS.load(Ordering::Relaxed).max(1);
        let count = ((delta as u128 * apic_per_ms as u128) / tsc_per_ms as u128).clamp(1, u32::MAX as u128) as u32;
        apic::timer_oneshot(idt::VEC_TIMER, count);
        ARMED.store(deadline, Ordering::Relaxed);
    })
}

/// Calibrate clocks and start the periodic tick.  Interrupts may be enabled
/// by the caller afterwards.
pub fn init() {
    // Take the best of a few short measurements to shrug off emulation jitter.
    let mut best_tsc = u64::MAX;
    let mut best_apic = u64::MAX;
    for _ in 0..3 {
        let (t, a) = pit_measure(10);
        best_tsc = best_tsc.min(t);
        best_apic = best_apic.min(a);
    }
    TSC_PER_MS.store(best_tsc.max(1), Ordering::Relaxed);
    APIC_PER_MS.store(best_apic.max(1), Ordering::Relaxed);
    BOOT_TSC.store(cpu::rdtsc(), Ordering::Relaxed);

    idt::register_handler(idt::VEC_TIMER, tick_handler);
    // Tickless: the first interrupt comes after one nominal tick, later ones
    // whenever a timer is due (see `arm_timer_at`).
    arm_timer_at(now() + us_to_tsc(1_000_000 / TICK_HZ));
}

/// Install a function to run on every timer tick (in interrupt context).
pub fn set_tick_hook(f: fn()) {
    TICK_HOOK.store(f as usize, Ordering::Release);
}

pub fn tsc_per_ms() -> u64 {
    TSC_PER_MS.load(Ordering::Relaxed)
}

pub fn apic_ticks_per_ms() -> u64 {
    APIC_PER_MS.load(Ordering::Relaxed)
}

/// Convert a TSC delta to microseconds.
#[inline]
pub fn tsc_to_us(delta: u64) -> u64 {
    let per_ms = TSC_PER_MS.load(Ordering::Relaxed);
    if per_ms == 0 {
        return 0;
    }
    (delta / per_ms) * 1000 + (delta % per_ms) * 1000 / per_ms
}

/// Convert a TSC delta to nanoseconds.
#[inline]
pub fn tsc_to_ns(delta: u64) -> u64 {
    let per_ms = TSC_PER_MS.load(Ordering::Relaxed);
    if per_ms == 0 {
        return 0;
    }
    (delta / per_ms) * 1_000_000 + (delta % per_ms) * 1_000_000 / per_ms
}

#[inline]
pub fn us_to_tsc(us: u64) -> u64 {
    TSC_PER_MS.load(Ordering::Relaxed) * us / 1000
}

pub fn uptime_us() -> u64 {
    let boot = BOOT_TSC.load(Ordering::Relaxed);
    if boot == 0 {
        return 0;
    }
    tsc_to_us(cpu::rdtsc().wrapping_sub(boot))
}

pub fn uptime_ms() -> u64 {
    uptime_us() / 1000
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

pub fn busy_wait_us(us: u64) {
    let end = cpu::rdtsc() + us_to_tsc(us);
    while cpu::rdtsc() < end {
        core::hint::spin_loop();
    }
}

/// Monotonic timestamp in TSC cycles.
#[inline]
pub fn now() -> u64 {
    cpu::rdtsc()
}
