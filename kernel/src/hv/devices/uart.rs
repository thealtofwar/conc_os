//! 16550A UART model at 0x3F8 (COM1, IRQ 4).
//!
//! Transmission is instantaneous: every byte written to THR goes straight to
//! the host side, so THRE/TEMT are always set.  The transmit-holding
//! interrupt uses the same latch semantics as QEMU's model, which is what the
//! Linux 8250 driver's start-up test expects.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

const IER_RDI: u8 = 0x01;
const IER_THRI: u8 = 0x02;
const IER_RLSI: u8 = 0x04;
const IER_MSI: u8 = 0x08;

const LCR_DLAB: u8 = 0x80;
const MCR_LOOP: u8 = 0x10;

#[derive(Clone)]
pub struct Uart {
    rx: VecDeque<u8>,
    tx: Vec<u8>,
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
    fcr: u8,
    /// Transmit-holding-register-empty interrupt pending.
    thr_ipending: bool,
    /// Overrun error to report in LSR.
    overrun: bool,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
}

impl Default for Uart {
    fn default() -> Self {
        Self::new()
    }
}

impl Uart {
    pub fn new() -> Self {
        Uart {
            rx: VecDeque::new(),
            tx: Vec::new(),
            ier: 0,
            lcr: 0,
            mcr: 0x08,
            scr: 0,
            dll: 0x0C,
            dlm: 0,
            fcr: 0,
            thr_ipending: false,
            overrun: false,
            tx_bytes: 0,
            rx_bytes: 0,
        }
    }

    /// Host -> guest byte.
    pub fn push_input(&mut self, b: u8) {
        if self.rx.len() >= 1024 {
            self.overrun = true;
            return;
        }
        self.rx.push_back(b);
    }

    pub fn has_input(&self) -> bool {
        !self.rx.is_empty()
    }

    /// Bytes the guest transmitted since the last call.
    pub fn take_output(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.tx)
    }

    fn lsr(&self) -> u8 {
        let mut v = 0x60u8; // THRE | TEMT
        if !self.rx.is_empty() {
            v |= 0x01;
        }
        if self.overrun {
            v |= 0x02;
        }
        v
    }

    fn msr(&self) -> u8 {
        if self.mcr & MCR_LOOP != 0 {
            ((self.mcr & 0x01) << 5) | ((self.mcr & 0x02) << 3) | ((self.mcr & 0x04) << 4) | ((self.mcr & 0x08) << 4)
        } else {
            0xB0 // DCD | DSR | CTS
        }
    }

    /// Interrupt identification, highest priority first.
    fn iir(&self) -> u8 {
        let fifo = if self.fcr & 1 != 0 { 0xC0 } else { 0x00 };
        let id = if self.ier & IER_RLSI != 0 && self.overrun {
            0x06
        } else if self.ier & IER_RDI != 0 && !self.rx.is_empty() {
            0x04
        } else if self.ier & IER_THRI != 0 && self.thr_ipending {
            0x02
        } else if self.ier & IER_MSI != 0 && false {
            0x00
        } else {
            0x01
        };
        fifo | id
    }

    /// Level of the IRQ output.
    pub fn irq_asserted(&self) -> bool {
        self.iir() & 0x01 == 0
    }

    pub fn io_read(&mut self, off: u16) -> u8 {
        match off {
            0 => {
                if self.lcr & LCR_DLAB != 0 {
                    self.dll
                } else {
                    let b = self.rx.pop_front().unwrap_or(0);
                    if !self.rx.is_empty() || true {
                        self.rx_bytes += 1;
                    }
                    b
                }
            }
            1 => {
                if self.lcr & LCR_DLAB != 0 {
                    self.dlm
                } else {
                    self.ier
                }
            }
            2 => {
                let v = self.iir();
                if v & 0x0E == 0x02 {
                    self.thr_ipending = false;
                }
                v
            }
            3 => self.lcr,
            4 => self.mcr,
            5 => {
                let v = self.lsr();
                self.overrun = false;
                v
            }
            6 => self.msr(),
            7 => self.scr,
            _ => 0xFF,
        }
    }

    pub fn io_write(&mut self, off: u16, v: u8) {
        match off {
            0 => {
                if self.lcr & LCR_DLAB != 0 {
                    self.dll = v;
                } else {
                    self.thr_ipending = false;
                    if self.mcr & MCR_LOOP != 0 {
                        self.rx.push_back(v);
                    } else {
                        self.tx.push(v);
                    }
                    self.tx_bytes += 1;
                    // Byte "transmitted": THR empty again.
                    self.thr_ipending = true;
                }
            }
            1 => {
                if self.lcr & LCR_DLAB != 0 {
                    self.dlm = v;
                } else {
                    let was = self.ier & IER_THRI != 0;
                    self.ier = v & 0x0F;
                    let now = self.ier & IER_THRI != 0;
                    if now && !was {
                        self.thr_ipending = true;
                    } else if !now {
                        self.thr_ipending = false;
                    }
                }
            }
            2 => {
                let was_fifo = self.fcr & 1;
                self.fcr = v & 0xC9;
                if v & 0x02 != 0 || (v & 1) != was_fifo {
                    self.rx.clear();
                }
                if v & 0x04 != 0 || (v & 1) != was_fifo {
                    if self.ier & IER_THRI != 0 {
                        self.thr_ipending = true;
                    }
                }
            }
            3 => self.lcr = v,
            4 => self.mcr = v & 0x1F,
            7 => self.scr = v,
            _ => {}
        }
    }

    pub fn debug_summary(&self) -> alloc::string::String {
        alloc::format!(
            "ier={:#x} iir={:#x} lcr={:#x} mcr={:#x} fcr={:#x} rx_pending={} thr_ipending={} tx={} rx={}",
            self.ier,
            self.iir(),
            self.lcr,
            self.mcr,
            self.fcr,
            self.rx.len(),
            self.thr_ipending,
            self.tx_bytes,
            self.rx_bytes
        )
    }
}
