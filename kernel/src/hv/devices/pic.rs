//! Dual 8259A programmable interrupt controller (legacy PIC mode).
//!
//! IRQs 0-7 on the master, 8-15 on the slave cascaded into master IRQ 2.
//! Edge-triggered inputs as on ISA; the slave's INT output is evaluated
//! dynamically rather than latched into the master's IRR.

#[derive(Clone, Copy, Default, Debug)]
struct Chip {
    irr: u8,
    isr: u8,
    imr: u8,
    icw_step: u8,
    icw4_needed: bool,
    single: bool,
    auto_eoi: bool,
    vector_base: u8,
    read_isr: bool,
    special_mask: bool,
    initialised: bool,
}

impl Chip {
    /// Highest-priority IRQ that may interrupt the CPU, given the in-service
    /// state.  `extra_irr` ORs in the cascade line.
    fn pending(&self, extra_irr: u8) -> Option<u8> {
        let cand = (self.irr | extra_irr) & !self.imr;
        for irq in 0..8u8 {
            let bit = 1u8 << irq;
            if self.isr & bit != 0 && !self.special_mask {
                return None; // an equal or higher priority interrupt is in service
            }
            if cand & bit != 0 {
                return Some(irq);
            }
        }
        None
    }

    fn ack(&mut self, irq: u8) {
        let bit = 1u8 << irq;
        self.irr &= !bit;
        if !self.auto_eoi {
            self.isr |= bit;
        }
    }

    fn eoi_nonspecific(&mut self) {
        for irq in 0..8u8 {
            let bit = 1u8 << irq;
            if self.isr & bit != 0 {
                self.isr &= !bit;
                return;
            }
        }
    }

    fn reset(&mut self, icw1: u8) {
        self.imr = 0;
        self.isr = 0;
        self.irr = 0;
        self.icw_step = 1;
        self.icw4_needed = icw1 & 1 != 0;
        self.single = icw1 & 2 != 0;
        self.auto_eoi = false;
        self.read_isr = false;
        self.special_mask = false;
        self.initialised = false;
    }

    fn write_cmd(&mut self, v: u8) {
        if v & 0x10 != 0 {
            self.reset(v);
        } else if v & 0x08 != 0 {
            // OCW3
            if v & 0x02 != 0 {
                self.read_isr = v & 0x01 != 0;
            }
            if v & 0x40 != 0 {
                self.special_mask = v & 0x20 != 0;
            }
        } else {
            // OCW2
            match v >> 5 {
                0b001 | 0b101 => self.eoi_nonspecific(),
                0b011 | 0b111 => self.isr &= !(1u8 << (v & 7)),
                _ => {}
            }
        }
    }

    fn write_data(&mut self, v: u8) {
        match self.icw_step {
            1 => {
                self.vector_base = v & 0xF8;
                self.icw_step = if self.single {
                    if self.icw4_needed {
                        3
                    } else {
                        0
                    }
                } else {
                    2
                };
                if self.icw_step == 0 {
                    self.initialised = true;
                }
            }
            2 => {
                // ICW3: cascade wiring, fixed in our model.
                self.icw_step = if self.icw4_needed { 3 } else { 0 };
                if self.icw_step == 0 {
                    self.initialised = true;
                }
            }
            3 => {
                self.auto_eoi = v & 0x02 != 0;
                self.icw_step = 0;
                self.initialised = true;
            }
            _ => self.imr = v,
        }
    }

    fn read_cmd(&self) -> u8 {
        if self.read_isr {
            self.isr
        } else {
            self.irr
        }
    }
}

#[derive(Clone)]
pub struct Pic {
    master: Chip,
    slave: Chip,
    elcr: [u8; 2],
    /// Last observed level of each IRQ input, for edge detection.
    level: u16,
    pub delivered: u64,
}

impl Default for Pic {
    fn default() -> Self {
        Self::new()
    }
}

impl Pic {
    pub fn new() -> Self {
        Pic { master: Chip::default(), slave: Chip::default(), elcr: [0; 2], level: 0, delivered: 0 }
    }

    fn chip_mut(&mut self, irq: u8) -> (&mut Chip, u8) {
        if irq < 8 {
            (&mut self.master, irq)
        } else {
            (&mut self.slave, irq - 8)
        }
    }

    /// Drive an IRQ input; a rising edge latches a request.
    pub fn set_irq_level(&mut self, irq: u8, high: bool) {
        let bit = 1u16 << irq;
        let was = self.level & bit != 0;
        if high && !was {
            let (c, i) = self.chip_mut(irq);
            c.irr |= 1 << i;
        }
        if high {
            self.level |= bit;
        } else {
            self.level &= !bit;
        }
    }

    /// A short pulse (timer tick).
    pub fn pulse_irq(&mut self, irq: u8) {
        let (c, i) = self.chip_mut(irq);
        c.irr |= 1 << i;
    }

    fn slave_int(&self) -> bool {
        self.slave.initialised && self.slave.pending(0).is_some()
    }

    /// Vector that an INTA cycle would return right now, if any.
    pub fn pending_vector(&self) -> Option<u8> {
        if !self.master.initialised {
            return None;
        }
        let cascade = if self.slave_int() { 1 << 2 } else { 0 };
        let irq = self.master.pending(cascade)?;
        if irq == 2 && cascade != 0 {
            let s = self.slave.pending(0)?;
            Some(self.slave.vector_base + s)
        } else {
            Some(self.master.vector_base + irq)
        }
    }

    /// Which IRQ line a vector belongs to (for statistics).
    pub fn irq_for_vector(&self, v: u8) -> Option<u8> {
        let m = v.wrapping_sub(self.master.vector_base);
        if m < 8 {
            return Some(m);
        }
        let s = v.wrapping_sub(self.slave.vector_base);
        if s < 8 {
            return Some(8 + s);
        }
        None
    }

    /// INTA: acknowledge and return the vector.
    pub fn ack(&mut self) -> Option<u8> {
        let cascade = if self.slave_int() { 1 << 2 } else { 0 };
        let irq = self.master.pending(cascade)?;
        self.delivered += 1;
        if irq == 2 && cascade != 0 {
            let s = self.slave.pending(0)?;
            self.slave.ack(s);
            self.master.ack(2);
            Some(self.slave.vector_base + s)
        } else {
            self.master.ack(irq);
            Some(self.master.vector_base + irq)
        }
    }

    pub fn io_read(&mut self, port: u16) -> u8 {
        match port {
            0x20 => self.master.read_cmd(),
            0x21 => self.master.imr,
            0xA0 => self.slave.read_cmd(),
            0xA1 => self.slave.imr,
            0x4D0 => self.elcr[0],
            0x4D1 => self.elcr[1],
            _ => 0xFF,
        }
    }

    pub fn io_write(&mut self, port: u16, v: u8) {
        match port {
            0x20 => self.master.write_cmd(v),
            0x21 => self.master.write_data(v),
            0xA0 => self.slave.write_cmd(v),
            0xA1 => self.slave.write_data(v),
            0x4D0 => self.elcr[0] = v,
            0x4D1 => self.elcr[1] = v,
            _ => {}
        }
    }

    pub fn debug_summary(&self) -> alloc::string::String {
        alloc::format!(
            "master irr={:#04x} isr={:#04x} imr={:#04x} base={:#x} init={} | slave irr={:#04x} isr={:#04x} imr={:#04x} base={:#x} | delivered={}",
            self.master.irr,
            self.master.isr,
            self.master.imr,
            self.master.vector_base,
            self.master.initialised,
            self.slave.irr,
            self.slave.isr,
            self.slave.imr,
            self.slave.vector_base,
            self.delivered
        )
    }
}
