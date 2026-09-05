//! Virtio over PCI, legacy (0.9.5 / "transitional") transport.
//!
//! The legacy transport keeps all common registers in an I/O port BAR, which
//! keeps the driver small.  Interrupts use MSI-X when the device exposes it
//! (one vector per queue), and fall back to the shared INTx line otherwise.

#![allow(dead_code)]

pub mod blk;
pub mod net;
pub mod queue;

use alloc::vec::Vec;

use crate::arch::cpu::{inb, inl, inw, outb, outl, outw};
use crate::arch::{idt, ioapic};
use crate::mm::frame;
use crate::pci::{Bar, MsixTable, PciDevice};
use queue::Virtqueue;

pub const VENDOR: u16 = 0x1AF4;
pub const DEV_NET: u16 = 0x1000;
pub const DEV_BLK: u16 = 0x1001;

const REG_HOST_FEATURES: u16 = 0x00;
const REG_GUEST_FEATURES: u16 = 0x04;
const REG_QUEUE_PFN: u16 = 0x08;
const REG_QUEUE_NUM: u16 = 0x0C;
const REG_QUEUE_SEL: u16 = 0x0E;
const REG_QUEUE_NOTIFY: u16 = 0x10;
const REG_STATUS: u16 = 0x12;
const REG_ISR: u16 = 0x13;
const REG_MSIX_CONFIG: u16 = 0x14;
const REG_MSIX_QUEUE: u16 = 0x16;

pub const STATUS_ACKNOWLEDGE: u8 = 1;
pub const STATUS_DRIVER: u8 = 2;
pub const STATUS_DRIVER_OK: u8 = 4;
pub const STATUS_FAILED: u8 = 0x80;

pub const NO_VECTOR: u16 = 0xFFFF;

pub struct VirtioDevice {
    pub pci: PciDevice,
    io: u16,
    msix: Option<MsixTable>,
    pub features: u32,
}

impl VirtioDevice {
    /// Reset the device and move it to the DRIVER state.
    pub fn new(pci: &PciDevice) -> Option<Self> {
        let io = match pci.bar(0) {
            Bar::Io(p) => p,
            _ => return None,
        };
        pci.enable();
        let msix = pci.enable_msix();
        let d = VirtioDevice { pci: pci.clone(), io, msix, features: 0 };
        d.write_status(0);
        d.write_status(STATUS_ACKNOWLEDGE);
        d.write_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER);
        Some(d)
    }

    fn write_status(&self, s: u8) {
        outb(self.io + REG_STATUS, s);
    }

    pub fn status(&self) -> u8 {
        inb(self.io + REG_STATUS)
    }

    pub fn host_features(&self) -> u32 {
        inl(self.io + REG_HOST_FEATURES)
    }

    /// Accept the intersection of `wanted` and the device's features.
    pub fn negotiate(&mut self, wanted: u32) -> u32 {
        let f = self.host_features() & wanted;
        outl(self.io + REG_GUEST_FEATURES, f);
        self.features = f;
        f
    }

    pub fn has_msix(&self) -> bool {
        self.msix.is_some()
    }

    fn config_base(&self) -> u16 {
        self.io + if self.msix.is_some() { 0x18 } else { 0x14 }
    }

    pub fn config_read8(&self, off: u16) -> u8 {
        inb(self.config_base() + off)
    }
    pub fn config_read16(&self, off: u16) -> u16 {
        inw(self.config_base() + off)
    }
    pub fn config_read32(&self, off: u16) -> u32 {
        inl(self.config_base() + off)
    }
    pub fn config_read64(&self, off: u16) -> u64 {
        self.config_read32(off) as u64 | (self.config_read32(off + 4) as u64) << 32
    }

    /// Allocate and register virtqueue `idx`.
    pub fn setup_queue(&mut self, idx: u16) -> Option<Virtqueue> {
        outw(self.io + REG_QUEUE_SEL, idx);
        let size = inw(self.io + REG_QUEUE_NUM);
        if size == 0 {
            return None;
        }
        let pages = Virtqueue::pages_for(size);
        let mem = frame::alloc_contiguous_zeroed(pages, 1)?;
        outl(self.io + REG_QUEUE_PFN, (mem >> 12) as u32);
        Some(Virtqueue::new(size, mem, pages, self.io + REG_QUEUE_NOTIFY, idx))
    }

    /// Assign interrupt vectors for queues `0..nqueues`.  With MSI-X each
    /// queue gets its own vector; otherwise all share the INTx vector.  The
    /// caller registers handlers for the returned vectors.
    pub fn setup_interrupts(&mut self, nqueues: u16) -> Vec<u8> {
        let mut vectors = Vec::new();
        match &self.msix {
            Some(table) => {
                outw(self.io + REG_MSIX_CONFIG, NO_VECTOR);
                for q in 0..nqueues {
                    let vec = idt::alloc_vector();
                    table.set(q, vec);
                    outw(self.io + REG_QUEUE_SEL, q);
                    outw(self.io + REG_MSIX_QUEUE, q);
                    let rb = inw(self.io + REG_MSIX_QUEUE);
                    if rb != q {
                        log!("virtio {}: msix vector for queue {} rejected", self.pci.addr, q);
                    }
                    vectors.push(vec);
                }
            }
            None => {
                let vec = idt::alloc_vector();
                let gsi = self.pci.irq_line as u32;
                ioapic::route(gsi, vec, true, true);
                log!("virtio {}: using INTx gsi {} -> vector {}", self.pci.addr, gsi, vec);
                for _ in 0..nqueues {
                    vectors.push(vec);
                }
            }
        }
        vectors
    }

    pub fn driver_ok(&self) {
        self.write_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK);
    }

    /// Read (and thereby acknowledge) the legacy ISR status byte.
    pub fn isr(&self) -> u8 {
        inb(self.io + REG_ISR)
    }
}

/// Probe every virtio device on the PCI bus.
pub fn init() {
    for d in crate::pci::devices() {
        if d.vendor != VENDOR {
            continue;
        }
        match d.device {
            DEV_NET => net::probe(d),
            DEV_BLK => blk::probe(d),
            other => log!("virtio {}: unsupported device id {:#x}", d.addr, other),
        }
    }
}
