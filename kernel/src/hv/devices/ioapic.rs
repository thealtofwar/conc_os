//! An I/O APIC (82093AA style, 24 inputs) at 0xFEC00000.
//!
//! With an MP table describing it, Linux leaves legacy PIC mode: interrupts
//! are delivered as fixed vectors straight into the local APIC and each one
//! costs the guest a single EOI instead of the four port accesses the 8259
//! needs, and the local APIC timer takes over from the PIT.  ISA inputs are
//! edge-triggered; level triggering is modelled too (remote IRR, EOI
//! register at 0x40, LAPIC EOI broadcast) for completeness.

pub const IOAPIC_BASE: u64 = 0xFEC0_0000;
pub const IOAPIC_ID: u8 = 1;
const PINS: usize = 24;

const RED_MASK: u64 = 1 << 16;
const RED_LEVEL: u64 = 1 << 15;
const RED_IRR: u64 = 1 << 14;
const RED_STATUS: u64 = 1 << 12;

/// An interrupt the I/O APIC wants the local APIC to take.
#[derive(Clone, Copy, Debug)]
pub struct Delivery {
    pub vector: u8,
    pub level: bool,
}

#[derive(Clone)]
pub struct IoApic {
    regsel: u32,
    redir: [u64; PINS],
    /// Current input levels.
    level: u32,
    pub delivered: u64,
}

impl Default for IoApic {
    fn default() -> Self {
        Self::new()
    }
}

impl IoApic {
    pub fn new() -> Self {
        IoApic { regsel: 0, redir: [RED_MASK; PINS], level: 0, delivered: 0 }
    }

    pub fn contains(gpa: u64) -> bool {
        gpa >= IOAPIC_BASE && gpa < IOAPIC_BASE + 0x1000
    }

    /// Is any entry unmasked, i.e. has the guest configured us at all?
    pub fn in_use(&self) -> bool {
        self.redir.iter().any(|e| e & RED_MASK == 0)
    }

    pub fn mmio_read(&self, off: u64) -> u32 {
        match off {
            0x00 => self.regsel,
            0x10 => self.read_reg(self.regsel),
            _ => 0,
        }
    }

    fn read_reg(&self, r: u32) -> u32 {
        match r {
            0 | 2 => (IOAPIC_ID as u32) << 24,
            1 => 0x20 | (((PINS - 1) as u32) << 16),
            0x10..=0x3F => {
                let i = ((r - 0x10) / 2) as usize;
                let e = self.redir[i];
                if r & 1 == 0 {
                    e as u32
                } else {
                    (e >> 32) as u32
                }
            }
            _ => 0,
        }
    }

    /// Returns a delivery if the write unmasked a pending level line or
    /// was an EOI for one that is still asserted.
    pub fn mmio_write(&mut self, off: u64, v: u32) -> Option<Delivery> {
        match off {
            0x00 => {
                self.regsel = v & 0xFF;
                None
            }
            0x10 => self.write_reg(self.regsel, v),
            0x40 => self.eoi(v as u8),
            _ => None,
        }
    }

    fn write_reg(&mut self, r: u32, v: u32) -> Option<Delivery> {
        match r {
            0x10..=0x3F => {
                let i = ((r - 0x10) / 2) as usize;
                let old = self.redir[i];
                let ro = RED_STATUS | RED_IRR;
                let new = if r & 1 == 0 {
                    (old & !0xFFFF_FFFF) | (v as u64 & !ro) | (old & ro)
                } else {
                    (old & 0xFFFF_FFFF) | ((v as u64) << 32)
                };
                self.redir[i] = new;
                if new & RED_MASK == 0 && new & RED_LEVEL != 0 && new & RED_IRR == 0 && self.level & (1 << i) != 0 {
                    return self.deliver(i);
                }
                None
            }
            _ => None,
        }
    }

    fn deliver(&mut self, pin: usize) -> Option<Delivery> {
        let e = self.redir[pin];
        if e & RED_MASK != 0 {
            return None;
        }
        let vector = e as u8;
        if vector < 16 {
            return None;
        }
        let level = e & RED_LEVEL != 0;
        if level {
            self.redir[pin] |= RED_IRR;
        }
        self.delivered += 1;
        Some(Delivery { vector, level })
    }

    /// Drive input `pin`.  Edge entries deliver on a rising edge; level
    /// entries while high and not already waiting for an EOI.
    pub fn set_irq_level(&mut self, pin: u8, high: bool) -> Option<Delivery> {
        if pin as usize >= PINS {
            return None;
        }
        let bit = 1u32 << pin;
        let was = self.level & bit != 0;
        if high {
            self.level |= bit;
        } else {
            self.level &= !bit;
        }
        let e = self.redir[pin as usize];
        let level = e & RED_LEVEL != 0;
        if high && !level && !was {
            return self.deliver(pin as usize);
        }
        if high && level && e & RED_IRR == 0 {
            return self.deliver(pin as usize);
        }
        None
    }

    /// A pulse: rising then falling edge.
    pub fn pulse(&mut self, pin: u8) -> Option<Delivery> {
        let d = self.set_irq_level(pin, true);
        self.set_irq_level(pin, false);
        d
    }

    /// End of interrupt for `vector`: level entries carrying it may fire
    /// again if their line is still high.
    pub fn eoi(&mut self, vector: u8) -> Option<Delivery> {
        let mut again = None;
        for i in 0..PINS {
            let e = self.redir[i];
            if e & RED_LEVEL != 0 && e as u8 == vector && e & RED_IRR != 0 {
                self.redir[i] &= !RED_IRR;
                if self.level & (1 << i) != 0 && again.is_none() {
                    again = self.deliver(i);
                }
            }
        }
        again
    }

    /// Which input an unmasked vector belongs to (for statistics).
    pub fn pin_for_vector(&self, vector: u8) -> Option<u8> {
        (0..PINS).find(|&i| self.redir[i] & RED_MASK == 0 && self.redir[i] as u8 == vector).map(|i| i as u8)
    }

    pub fn debug_summary(&self) -> alloc::string::String {
        let mut s = alloc::string::String::new();
        for i in 0..PINS {
            let e = self.redir[i];
            if e & RED_MASK == 0 {
                s.push_str(&alloc::format!(
                    "pin{}=v{:#x}{}{} ",
                    i,
                    e as u8,
                    if e & RED_LEVEL != 0 { "L" } else { "E" },
                    if e & RED_IRR != 0 { "!" } else { "" }
                ));
            }
        }
        if s.is_empty() {
            s.push_str("unused");
        }
        alloc::format!("{}delivered={}", s, self.delivered)
    }
}
