//! PCI configuration space access and device enumeration.

#![allow(dead_code)]

use alloc::vec::Vec;

use crate::arch::cpu::{inl, outl};
use crate::sync::{OnceCell, SpinLock};

static CONFIG_LOCK: SpinLock<()> = SpinLock::new(());
static DEVICES: OnceCell<Vec<PciDevice>> = OnceCell::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciAddr {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
}

impl core::fmt::Display for PciAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:02x}:{:02x}.{}", self.bus, self.dev, self.func)
    }
}

impl PciAddr {
    #[inline]
    fn cfg(&self, off: u8) -> u32 {
        0x8000_0000 | (self.bus as u32) << 16 | (self.dev as u32) << 11 | (self.func as u32) << 8 | (off as u32 & 0xFC)
    }
    pub fn read32(&self, off: u8) -> u32 {
        let _g = CONFIG_LOCK.lock();
        outl(0xCF8, self.cfg(off));
        inl(0xCFC)
    }
    pub fn write32(&self, off: u8, v: u32) {
        let _g = CONFIG_LOCK.lock();
        outl(0xCF8, self.cfg(off));
        outl(0xCFC, v);
    }
    pub fn read16(&self, off: u8) -> u16 {
        (self.read32(off) >> ((off as u32 & 2) * 8)) as u16
    }
    pub fn read8(&self, off: u8) -> u8 {
        (self.read32(off) >> ((off as u32 & 3) * 8)) as u8
    }
    pub fn write16(&self, off: u8, v: u16) {
        let shift = (off as u32 & 2) * 8;
        let old = self.read32(off);
        self.write32(off, (old & !(0xFFFF << shift)) | ((v as u32) << shift));
    }
    pub fn write8(&self, off: u8, v: u8) {
        let shift = (off as u32 & 3) * 8;
        let old = self.read32(off);
        self.write32(off, (old & !(0xFF << shift)) | ((v as u32) << shift));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bar {
    None,
    Io(u16),
    Mem { base: u64, is64: bool, prefetch: bool },
}

#[derive(Clone, Debug)]
pub struct PciDevice {
    pub addr: PciAddr,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub revision: u8,
    pub header_type: u8,
    pub subsystem_vendor: u16,
    pub subsystem_id: u16,
    pub irq_line: u8,
    pub irq_pin: u8,
}

/// An MSI-X table mapped in MMIO.
#[derive(Clone, Copy, Debug)]
pub struct MsixTable {
    base: u64,
    pub size: u16,
}

impl MsixTable {
    /// Point table entry `idx` at local `vector` on the boot CPU and unmask it.
    pub fn set(&self, idx: u16, vector: u8) {
        assert!(idx < self.size);
        let e = (self.base + 16 * idx as u64) as *mut u32;
        let apic_id = crate::arch::apic::id();
        unsafe {
            core::ptr::write_volatile(e.add(3), 1); // masked while we program it
            core::ptr::write_volatile(e, 0xFEE0_0000 | (apic_id << 12));
            core::ptr::write_volatile(e.add(1), 0);
            core::ptr::write_volatile(e.add(2), vector as u32);
            core::ptr::write_volatile(e.add(3), 0);
        }
    }
    pub fn mask(&self, idx: u16) {
        let e = (self.base + 16 * idx as u64) as *mut u32;
        unsafe { core::ptr::write_volatile(e.add(3), 1) };
    }
}

impl PciDevice {
    fn probe(addr: PciAddr) -> Option<PciDevice> {
        let id = addr.read32(0);
        if id & 0xFFFF == 0xFFFF {
            return None;
        }
        let class = addr.read32(8);
        let misc = addr.read32(0x0C);
        let irq = addr.read32(0x3C);
        Some(PciDevice {
            addr,
            vendor: id as u16,
            device: (id >> 16) as u16,
            class: (class >> 24) as u8,
            subclass: (class >> 16) as u8,
            prog_if: (class >> 8) as u8,
            revision: class as u8,
            header_type: (misc >> 16) as u8,
            subsystem_vendor: addr.read16(0x2C),
            subsystem_id: addr.read16(0x2E),
            irq_line: irq as u8,
            irq_pin: (irq >> 8) as u8,
        })
    }

    pub fn bar(&self, n: u8) -> Bar {
        let off = 0x10 + 4 * n;
        let v = self.addr.read32(off);
        if v == 0 {
            return Bar::None;
        }
        if v & 1 != 0 {
            return Bar::Io((v & !3) as u16);
        }
        let ty = (v >> 1) & 3;
        let mut base = (v & !0xF) as u64;
        let is64 = ty == 2;
        if is64 {
            base |= (self.addr.read32(off + 4) as u64) << 32;
        }
        Bar::Mem { base, is64, prefetch: v & 8 != 0 }
    }

    /// Enable I/O space, memory space and bus mastering.
    pub fn enable(&self) {
        let cmd = self.addr.read16(4);
        self.addr.write16(4, cmd | 0x7);
    }

    /// Walk the capability list: (id, offset) pairs.
    pub fn capabilities(&self) -> Vec<(u8, u8)> {
        let mut caps = Vec::new();
        if self.addr.read16(6) & 0x10 == 0 {
            return caps;
        }
        let mut ptr = self.addr.read8(0x34) & 0xFC;
        let mut guard = 0;
        while ptr != 0 && guard < 48 {
            let id = self.addr.read8(ptr);
            caps.push((id, ptr));
            ptr = self.addr.read8(ptr + 1) & 0xFC;
            guard += 1;
        }
        caps
    }

    pub fn find_capability(&self, id: u8) -> Option<u8> {
        self.capabilities().into_iter().find(|c| c.0 == id).map(|c| c.1)
    }

    /// Enable MSI-X (if the device supports it) and return its table.
    pub fn enable_msix(&self) -> Option<MsixTable> {
        let cap = self.find_capability(0x11)?;
        let ctrl = self.addr.read16(cap + 2);
        let size = (ctrl & 0x7FF) + 1;
        let t = self.addr.read32(cap + 4);
        let bir = (t & 7) as u8;
        let off = (t & !7) as u64;
        let base = match self.bar(bir) {
            Bar::Mem { base, .. } => base,
            _ => return None,
        };
        let table_base = base + off;
        crate::arch::paging::map_mmio(table_base, size as u64 * 16);
        let table = MsixTable { base: table_base, size };
        for i in 0..size {
            table.mask(i);
        }
        self.addr.write16(cap + 2, (ctrl | 0x8000) & !0x4000);
        Some(table)
    }

    pub fn is_bridge(&self) -> bool {
        self.header_type & 0x7F == 1
    }
}

fn scan_bus(bus: u8, out: &mut Vec<PciDevice>, depth: u8) {
    if depth > 8 {
        return;
    }
    for dev in 0..32u8 {
        let a0 = PciAddr { bus, dev, func: 0 };
        let d0 = match PciDevice::probe(a0) {
            Some(d) => d,
            None => continue,
        };
        let multi = d0.header_type & 0x80 != 0;
        let funcs = if multi { 8 } else { 1 };
        for func in 0..funcs {
            let d = if func == 0 {
                d0.clone()
            } else {
                match PciDevice::probe(PciAddr { bus, dev, func }) {
                    Some(d) => d,
                    None => continue,
                }
            };
            if d.is_bridge() {
                let secondary = d.addr.read8(0x19);
                if secondary != bus {
                    scan_bus(secondary, out, depth + 1);
                }
            }
            out.push(d);
        }
    }
}

pub fn class_name(class: u8, subclass: u8) -> &'static str {
    match (class, subclass) {
        (0x01, 0x01) => "IDE controller",
        (0x01, 0x06) => "SATA controller",
        (0x01, _) => "storage controller",
        (0x02, 0x00) => "ethernet controller",
        (0x02, _) => "network controller",
        (0x03, _) => "display controller",
        (0x04, _) => "multimedia",
        (0x06, 0x00) => "host bridge",
        (0x06, 0x01) => "ISA bridge",
        (0x06, 0x04) => "PCI bridge",
        (0x06, _) => "bridge",
        (0x0C, 0x03) => "USB controller",
        (0x0C, 0x05) => "SMBus",
        (0x0C, _) => "serial bus",
        (0xFF, _) => "unassigned",
        _ => "device",
    }
}

pub fn init() {
    let mut devs = Vec::new();
    scan_bus(0, &mut devs, 0);
    for d in &devs {
        log!(
            "pci {}: {:04x}:{:04x} {} (class {:02x}.{:02x}) irq {}",
            d.addr,
            d.vendor,
            d.device,
            class_name(d.class, d.subclass),
            d.class,
            d.subclass,
            d.irq_line
        );
    }
    DEVICES.init(devs);
}

pub fn devices() -> &'static [PciDevice] {
    DEVICES.get().map(|v| v.as_slice()).unwrap_or(&[])
}
