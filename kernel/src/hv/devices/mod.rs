//! The device model presented to Linux guests: a legacy PC just large enough
//! for a microVM.
//!
//! * local APIC (MMIO at 0xFEE00000, emulated through nested page faults)
//! * two cascaded 8259 PICs whose INTR output feeds LINT0 as ExtINT
//! * 8254 PIT (channel 0 -> IRQ 0, channel 2 for TSC calibration)
//! * 16550A UART at 0x3F8 on IRQ 4
//! * CMOS RTC, i8042 status stub (reset detection), PCI config stubs
//! * virtio-mmio network device at 0xD0000000 on IRQ 5
//!
//! Time inside the model is the guest-visible TSC.

#![allow(dead_code)]

pub mod ioapic;
pub mod lapic;
pub mod pic;
pub mod pit;
pub mod rtc;
pub mod uart;
pub mod vnet;
pub mod vq;

use alloc::sync::Arc;
use alloc::vec::Vec;

use ioapic::{Delivery, IoApic, IOAPIC_BASE};
use lapic::Lapic;
use pic::Pic;
use pit::Pit;
use rtc::Rtc;
use uart::Uart;
use vnet::{VirtioMmioNet, VNET_BASE, VNET_IRQ};

use crate::net::vmlink::VmLink;

pub const UART_BASE: u16 = 0x3F8;

/// An interrupt ready for delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pending {
    /// Fixed-priority interrupt from the local APIC (vector).
    Fixed(u8),
    /// ExtINT from the 8259 through LINT0 (vector).
    ExtInt(u8),
}

impl Pending {
    pub fn vector(&self) -> u8 {
        match self {
            Pending::Fixed(v) | Pending::ExtInt(v) => *v,
        }
    }
}

#[derive(Clone)]
pub struct DeviceModel {
    pub lapic: Lapic,
    pub pic: Pic,
    pub pit: Pit,
    pub uart: Uart,
    pub rtc: Rtc,
    pub ioapic: IoApic,
    pub vnet: Option<VirtioMmioNet>,
    pub tsc_hz: u64,
    /// Set when the guest asked for a reset/power-off through a port.
    pub reset_request: Option<&'static str>,
    pub apic_base_msr: u64,
    pub pat: u64,
    a20: bool,
    pub unhandled_io: u64,
    pub last_unhandled_port: u16,
    pub io_count: u64,
    /// Port I/O exits by device: pic, pit, uart, other.
    pub io_class: [u64; 4],
    /// MMIO exits by device: lapic, vnet, other.
    pub mmio_class: [u64; 3],
    /// Interrupts delivered by source: timer, uart, net, other.
    pub inj: [u64; 4],
}

impl DeviceModel {
    pub fn new(tsc_hz: u64, now: u64, link: Option<Arc<VmLink>>) -> Self {
        DeviceModel {
            lapic: Lapic::new(tsc_hz),
            pic: Pic::new(),
            pit: Pit::new(tsc_hz),
            uart: Uart::new(),
            rtc: Rtc::new(tsc_hz, now),
            ioapic: IoApic::new(),
            vnet: link.map(VirtioMmioNet::new),
            tsc_hz,
            reset_request: None,
            apic_base_msr: lapic::APIC_BASE | (1 << 11) | (1 << 8),
            pat: 0x0007_0406_0007_0406,
            a20: true,
            unhandled_io: 0,
            last_unhandled_port: 0,
            io_count: 0,
            io_class: [0; 4],
            mmio_class: [0; 3],
            inj: [0; 4],
        }
    }

    /// Hand an I/O APIC delivery to the local APIC.
    fn ioapic_deliver(&mut self, d: Option<Delivery>) {
        if let Some(d) = d {
            if d.level {
                self.lapic.inject_vector_level(d.vector);
            } else {
                self.lapic.inject_vector(d.vector);
            }
        }
    }

    /// Drive an interrupt line into both interrupt controllers; whichever
    /// the guest configured delivers it.
    fn drive_line(&mut self, irq: u8, high: bool) {
        self.pic.set_irq_level(irq, high);
        let d = self.ioapic.set_irq_level(irq, high);
        self.ioapic_deliver(d);
    }

    /// The local APIC finished a level-triggered interrupt: tell the I/O APIC.
    fn after_lapic_write(&mut self) {
        if let Some(v) = self.lapic.last_level_eoi.take() {
            let d = self.ioapic.eoi(v);
            self.ioapic_deliver(d);
        }
    }

    fn classify_port(port: u16) -> usize {
        match port {
            0x20 | 0x21 | 0xA0 | 0xA1 | 0x4D0 | 0x4D1 => 0,
            0x40..=0x43 | 0x61 => 1,
            0x3F8..=0x3FF => 2,
            _ => 3,
        }
    }

    fn all_ones(size: u8) -> u32 {
        match size {
            1 => 0xFF,
            2 => 0xFFFF,
            _ => 0xFFFF_FFFF,
        }
    }

    pub fn io_read(&mut self, port: u16, size: u8, now: u64) -> u32 {
        self.io_count += 1;
        self.io_class[Self::classify_port(port)] += 1;
        let v = self.io_read_inner(port, size, now);
        if (UART_BASE..UART_BASE + 8).contains(&port) {
            // Interrupt lines are edge-sensitive at the 8259: every
            // transition must reach it, not just the level sampled at the
            // next poll.
            let high = self.uart.irq_asserted();
            self.drive_line(4, high);
        }
        v
    }

    fn io_read_inner(&mut self, port: u16, size: u8, now: u64) -> u32 {
        match port {
            0x20 | 0x21 | 0xA0 | 0xA1 | 0x4D0 | 0x4D1 => self.pic.io_read(port) as u32,
            0x40..=0x43 | 0x61 => self.pit.io_read(port, now) as u32,
            0x70 | 0x71 => self.rtc.io_read(port, now) as u32,
            0x3F8..=0x3FF => self.uart.io_read(port - UART_BASE) as u32,
            0x60 => 0,
            0x64 => 0x00, // i8042 status: output buffer empty, input buffer free
            0x92 => {
                if self.a20 {
                    0x02
                } else {
                    0
                }
            }
            0xCF8..=0xCFF => Self::all_ones(size), // no PCI
            0x2F8..=0x2FF | 0x3E8..=0x3EF | 0x2E8..=0x2EF => Self::all_ones(size),
            _ => {
                self.unhandled_io += 1;
                self.last_unhandled_port = port;
                Self::all_ones(size)
            }
        }
    }

    pub fn io_write(&mut self, port: u16, size: u8, v: u32, now: u64) {
        self.io_count += 1;
        self.io_class[Self::classify_port(port)] += 1;
        self.io_write_inner(port, size, v, now);
        if (UART_BASE..UART_BASE + 8).contains(&port) {
            let high = self.uart.irq_asserted();
            self.drive_line(4, high);
        }
    }

    fn io_write_inner(&mut self, port: u16, size: u8, v: u32, now: u64) {
        let b = v as u8;
        match port {
            0x20 | 0x21 | 0xA0 | 0xA1 | 0x4D0 | 0x4D1 => self.pic.io_write(port, b),
            0x40..=0x43 | 0x61 => self.pit.io_write(port, b, now),
            0x70 | 0x71 => self.rtc.io_write(port, b),
            0x3F8..=0x3FF => self.uart.io_write(port - UART_BASE, b),
            0x64 => {
                if b == 0xFE {
                    self.reset_request = Some("keyboard controller reset (0xFE to port 0x64)");
                }
            }
            0xCF9 => {
                if b & 0x04 != 0 {
                    self.reset_request = Some("reset control register (port 0xCF9)");
                }
            }
            0x92 => self.a20 = b & 0x02 != 0,
            0x60 | 0x80 | 0xED | 0xEB | 0xF0 | 0xF1 | 0xCF8 | 0xCFC..=0xCFF => {}
            0x00..=0x1F | 0x81..=0x8F | 0xC0..=0xDF => {} // DMA controllers
            _ => {
                let _ = size;
                self.unhandled_io += 1;
                self.last_unhandled_port = port;
            }
        }
    }

    /// Advance timers and propagate interrupt lines.
    pub fn poll(&mut self, now: u64) {
        if self.pit.poll(now) {
            self.pic.pulse_irq(0);
            let d = self.ioapic.pulse(2);
            self.ioapic_deliver(d);
        }
        let uart_high = self.uart.irq_asserted();
        self.drive_line(4, uart_high);
        if let Some(high) = self.vnet.as_ref().map(|v| v.irq_level()) {
            self.drive_line(VNET_IRQ, high);
        }
        self.lapic.poll(now);
        self.lapic.set_extint(self.pic.pending_vector().is_some());
    }

    /// Earliest TSC at which a timer will need service.
    pub fn next_deadline(&self, now: u64) -> Option<u64> {
        let a = self.lapic.next_deadline();
        let b = self.pit.next_deadline(now);
        match (a, b) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (x, None) => x,
            (None, y) => y,
        }
    }

    /// The interrupt that would be delivered now (call `poll` first).
    pub fn pending(&self) -> Option<Pending> {
        if let Some(v) = self.lapic.pending_fixed() {
            return Some(Pending::Fixed(v));
        }
        if self.lapic.extint_deliverable() {
            return self.pic.pending_vector().map(Pending::ExtInt);
        }
        None
    }

    /// Acknowledge delivery of `p`.
    pub fn ack(&mut self, p: Pending) {
        match p {
            Pending::Fixed(v) => {
                let src = if v == self.lapic.timer_vector() {
                    0
                } else {
                    match self.ioapic.pin_for_vector(v) {
                        Some(0) | Some(2) => 0,
                        Some(4) => 1,
                        Some(VNET_IRQ) => 2,
                        _ => 3,
                    }
                };
                self.inj[src] += 1;
                self.lapic.ack_fixed(v)
            }
            Pending::ExtInt(v) => {
                let src = match self.pic.irq_for_vector(v) {
                    Some(0) => 0,
                    Some(4) => 1,
                    Some(VNET_IRQ) => 2,
                    _ => 3,
                };
                self.inj[src] += 1;
                self.pic.ack();
                self.lapic.set_extint(self.pic.pending_vector().is_some());
            }
        }
    }

    pub fn is_mmio(&self, gpa: u64) -> bool {
        (gpa >= lapic::APIC_BASE && gpa < lapic::APIC_BASE + 0x1000) || IoApic::contains(gpa) || (self.vnet.is_some() && VirtioMmioNet::contains(gpa))
    }

    pub fn mmio_read(&mut self, gpa: u64, size: u8, now: u64) -> u64 {
        if gpa >= lapic::APIC_BASE && gpa < lapic::APIC_BASE + 0x1000 {
            self.mmio_class[0] += 1;
            let v = self.lapic.read((gpa & 0xFFF) as u32, now) as u64;
            return match size {
                1 => v & 0xFF,
                2 => v & 0xFFFF,
                _ => v & 0xFFFF_FFFF,
            };
        }
        if VirtioMmioNet::contains(gpa) {
            self.mmio_class[1] += 1;
            if let Some(v) = self.vnet.as_mut() {
                return v.mmio_read(gpa - VNET_BASE, size) as u64;
            }
        }
        self.mmio_class[2] += 1;
        if IoApic::contains(gpa) {
            return self.ioapic.mmio_read(gpa - IOAPIC_BASE) as u64;
        }
        0
    }

    pub fn mmio_write(&mut self, gpa: u64, size: u8, v: u64, now: u64) {
        if gpa >= lapic::APIC_BASE && gpa < lapic::APIC_BASE + 0x1000 {
            self.mmio_class[0] += 1;
            self.lapic.write((gpa & 0xFFF) as u32, v as u32, now);
            self.after_lapic_write();
        } else if IoApic::contains(gpa) {
            self.mmio_class[2] += 1;
            let d = self.ioapic.mmio_write(gpa - IOAPIC_BASE, v as u32);
            self.ioapic_deliver(d);
        } else if VirtioMmioNet::contains(gpa) {
            self.mmio_class[1] += 1;
            if let Some(n) = self.vnet.as_mut() {
                n.mmio_write(gpa - VNET_BASE, size, v as u32);
            }
            // An interrupt acknowledge drops the line; the PIC must see it
            // go low before the next frame raises it again.
            self.sync_vnet_irq();
        }
    }

    /// The network device has a queue notification to service.
    pub fn vnet_kicked(&self) -> bool {
        self.vnet.as_ref().map(|v| v.kicked.is_some()).unwrap_or(false)
    }

    /// Propagate the network device's interrupt line to the controllers now.
    pub fn sync_vnet_irq(&mut self) {
        if let Some(high) = self.vnet.as_ref().map(|v| v.irq_level()) {
            self.drive_line(VNET_IRQ, high);
        }
    }

    /// MSRs owned by the device model.  `None` = not ours.
    pub fn msr_read(&mut self, msr: u32, now: u64) -> Option<u64> {
        match msr {
            0x1B => Some(self.apic_base_msr),
            0x277 => Some(self.pat),
            0x1A0 => Some(1), // fast strings
            0x6E0 => Some(self.lapic.tsc_deadline()),
            0xC001_0015 => Some(1 << 24), // HWCR: TSC counts at P0 frequency
            0x800..=0x8FF if self.lapic.x2apic => {
                let reg = (msr - 0x800) << 4;
                if reg == lapic::REG_ICR_LO {
                    Some(self.lapic.read(lapic::REG_ICR_LO, now) as u64 | (self.lapic.read(lapic::REG_ICR_HI, now) as u64) << 32)
                } else {
                    Some(self.lapic.read(reg, now) as u64)
                }
            }
            _ => None,
        }
    }

    pub fn msr_write(&mut self, msr: u32, v: u64, now: u64) -> bool {
        match msr {
            0x1B => {
                // Keep the base fixed; honour the enable bit, refuse x2APIC.
                self.apic_base_msr = (self.apic_base_msr & !(1 << 11)) | (v & (1 << 11));
                true
            }
            0x277 => {
                self.pat = v;
                true
            }
            0x6E0 => {
                self.lapic.set_tsc_deadline(v);
                true
            }
            0x800..=0x8FF if self.lapic.x2apic => {
                let reg = (msr - 0x800) << 4;
                if reg == lapic::REG_ICR_LO {
                    self.lapic.write(lapic::REG_ICR_HI, (v >> 32) as u32, now);
                }
                self.lapic.write(reg, v as u32, now);
                self.after_lapic_write();
                true
            }
            _ => false,
        }
    }

    pub fn push_serial_input(&mut self, b: u8) {
        self.uart.push_input(b);
    }

    pub fn take_serial_output(&mut self) -> Vec<u8> {
        self.uart.take_output()
    }

    pub fn debug_summary(&self, now: u64) -> alloc::string::String {
        let net = match &self.vnet {
            Some(v) => alloc::format!("\n  vnet[{}]", v.debug_summary()),
            None => alloc::string::String::new(),
        };
        alloc::format!(
            "lapic[{}]\n  ioapic[{}]\n  pic[{}]\n  pit[{}]\n  uart[{}]{}\n  io={} unhandled={} (last port {:#x}) reset={:?}",
            self.lapic.debug_summary(now),
            self.ioapic.debug_summary(),
            self.pic.debug_summary(),
            self.pit.debug_summary(now),
            self.uart.debug_summary(),
            net,
            self.io_count,
            self.unhandled_io,
            self.last_unhandled_port,
            self.reset_request
        )
    }
}
