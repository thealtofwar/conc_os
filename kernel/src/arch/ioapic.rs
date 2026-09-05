//! I/O APIC: routes legacy ISA IRQs (serial port) and PCI INTx lines to
//! local APIC vectors.

#![allow(dead_code)]

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::cpu::outb;

static BASE: AtomicU64 = AtomicU64::new(0xFEC0_0000);

const IOREGSEL: u64 = 0x00;
const IOWIN: u64 = 0x10;

fn read(reg: u32) -> u32 {
    let base = BASE.load(Ordering::Relaxed);
    unsafe {
        core::ptr::write_volatile((base + IOREGSEL) as *mut u32, reg);
        core::ptr::read_volatile((base + IOWIN) as *const u32)
    }
}

fn write(reg: u32, v: u32) {
    let base = BASE.load(Ordering::Relaxed);
    unsafe {
        core::ptr::write_volatile((base + IOREGSEL) as *mut u32, reg);
        core::ptr::write_volatile((base + IOWIN) as *mut u32, v);
    }
}

/// Number of redirection entries.
pub fn entries() -> u32 {
    ((read(1) >> 16) & 0xFF) + 1
}

/// Mask the legacy 8259 PICs and mask every IOAPIC redirection entry.
pub fn init() {
    // Remap and fully mask both PICs so stray legacy interrupts never reach
    // vectors 0..15 (which would look like CPU exceptions).
    outb(0x20, 0x11);
    outb(0xA0, 0x11);
    outb(0x21, 0xF0);
    outb(0xA1, 0xF8);
    outb(0x21, 0x04);
    outb(0xA1, 0x02);
    outb(0x21, 0x01);
    outb(0xA1, 0x01);
    outb(0x21, 0xFF);
    outb(0xA1, 0xFF);

    let n = entries();
    for i in 0..n {
        write(0x10 + 2 * i, 1 << 16); // masked
        write(0x11 + 2 * i, 0);
    }
}

/// Route global system interrupt `gsi` to `vector` on the boot CPU.
/// `level`/`active_low` select the trigger mode (ISA: edge/high; PCI INTx:
/// level/low).
pub fn route(gsi: u32, vector: u8, level: bool, active_low: bool) {
    let apic_id = crate::arch::apic::id();
    let mut lo = vector as u32;
    if active_low {
        lo |= 1 << 13;
    }
    if level {
        lo |= 1 << 15;
    }
    write(0x11 + 2 * gsi, apic_id << 24);
    write(0x10 + 2 * gsi, lo);
}

pub fn mask(gsi: u32) {
    let lo = read(0x10 + 2 * gsi);
    write(0x10 + 2 * gsi, lo | (1 << 16));
}
