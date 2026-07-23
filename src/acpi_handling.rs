use core::ptr::NonNull;

use acpi::{
    AcpiTables, Handler, PhysicalMapping,
    rsdp::Rsdp,
    sdt::madt::{self, MadtEntry},
};
use alloc::vec::Vec;
use spin::Once;
use x86_64::instructions::port::Port;

use crate::memory::get_offset;

#[derive(Clone)]
struct AcpiHandler;

impl Handler for AcpiHandler {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> acpi::PhysicalMapping<Self, T> {
        PhysicalMapping {
            physical_start: physical_address,
            virtual_start: NonNull::new((get_offset() + physical_address as u64) as *mut T)
                .unwrap(),
            region_length: size,
            mapped_length: size,
            handler: AcpiHandler,
        }
    }

    fn unmap_physical_region<T>(_region: &acpi::PhysicalMapping<Self, T>) {
        // Since memory is directly mapped via a global offset,
        // there is nothing to explicitly unmap here.
    }

    fn read_u8(&self, address: usize) -> u8 {
        unsafe { ((get_offset() + address as u64) as *const u8).read_volatile() }
    }

    fn read_u16(&self, address: usize) -> u16 {
        unsafe { ((get_offset() + address as u64) as *const u16).read_volatile() }
    }

    fn read_u32(&self, address: usize) -> u32 {
        unsafe { ((get_offset() + address as u64) as *const u32).read_volatile() }
    }

    fn read_u64(&self, address: usize) -> u64 {
        unsafe { ((get_offset() + address as u64) as *const u64).read_volatile() }
    }

    fn write_u8(&self, address: usize, value: u8) {
        unsafe { ((get_offset() + address as u64) as *mut u8).write_volatile(value) }
    }

    fn write_u16(&self, address: usize, value: u16) {
        unsafe { ((get_offset() + address as u64) as *mut u16).write_volatile(value) }
    }

    fn write_u32(&self, address: usize, value: u32) {
        unsafe { ((get_offset() + address as u64) as *mut u32).write_volatile(value) }
    }

    fn write_u64(&self, address: usize, value: u64) {
        unsafe { ((get_offset() + address as u64) as *mut u64).write_volatile(value) }
    }

    fn read_io_u8(&self, port: u16) -> u8 {
        let mut p = Port::<u8>::new(port);
        unsafe { p.read() }
    }

    fn read_io_u16(&self, port: u16) -> u16 {
        let mut p = Port::<u16>::new(port);
        unsafe { p.read() }
    }

    fn read_io_u32(&self, port: u16) -> u32 {
        let mut p = Port::<u32>::new(port);
        unsafe { p.read() }
    }

    fn write_io_u8(&self, port: u16, value: u8) {
        let mut p = Port::<u8>::new(port);
        unsafe { p.write(value) }
    }

    fn write_io_u16(&self, port: u16, value: u16) {
        let mut p = Port::<u16>::new(port);
        unsafe { p.write(value) }
    }

    fn write_io_u32(&self, port: u16, value: u32) {
        let mut p = Port::<u32>::new(port);
        unsafe { p.write(value) }
    }

    fn read_pci_u8(&self, address: acpi::PciAddress, offset: u16) -> u8 {
        assert_eq!(
            address.segment(),
            0,
            "Only segment 0 is supported via legacy PCI"
        );
        let pci_addr = pci_address_to_u32(&address, offset);
        self.write_io_u32(0xCF8, pci_addr);
        self.read_io_u8(0xCFC + (offset % 4))
    }

    fn read_pci_u16(&self, address: acpi::PciAddress, offset: u16) -> u16 {
        assert_eq!(
            address.segment(),
            0,
            "Only segment 0 is supported via legacy PCI"
        );
        let pci_addr = pci_address_to_u32(&address, offset);
        self.write_io_u32(0xCF8, pci_addr);
        self.read_io_u16(0xCFC + (offset % 4))
    }

    fn read_pci_u32(&self, address: acpi::PciAddress, offset: u16) -> u32 {
        assert_eq!(
            address.segment(),
            0,
            "Only segment 0 is supported via legacy PCI"
        );
        let pci_addr = pci_address_to_u32(&address, offset);
        self.write_io_u32(0xCF8, pci_addr);
        self.read_io_u32(0xCFC + (offset % 4))
    }

    fn write_pci_u8(&self, address: acpi::PciAddress, offset: u16, value: u8) {
        assert_eq!(
            address.segment(),
            0,
            "Only segment 0 is supported via legacy PCI"
        );
        let pci_addr = pci_address_to_u32(&address, offset);
        self.write_io_u32(0xCF8, pci_addr);
        self.write_io_u8(0xCFC + (offset % 4), value);
    }

    fn write_pci_u16(&self, address: acpi::PciAddress, offset: u16, value: u16) {
        assert_eq!(
            address.segment(),
            0,
            "Only segment 0 is supported via legacy PCI"
        );
        let pci_addr = pci_address_to_u32(&address, offset);
        self.write_io_u32(0xCF8, pci_addr);
        self.write_io_u16(0xCFC + (offset % 4), value);
    }

    fn write_pci_u32(&self, address: acpi::PciAddress, offset: u16, value: u32) {
        assert_eq!(
            address.segment(),
            0,
            "Only segment 0 is supported via legacy PCI"
        );
        let pci_addr = pci_address_to_u32(&address, offset);
        self.write_io_u32(0xCF8, pci_addr);
        self.write_io_u32(0xCFC + (offset % 4), value);
    }

    fn nanos_since_boot(&self) -> u64 {
        unimplemented!("Kernel timer not yet wired to ACPI handler")
    }

    fn stall(&self, _microseconds: u64) {
        unimplemented!("Kernel stall not yet wired to ACPI handler")
    }

    fn sleep(&self, _milliseconds: u64) {
        unimplemented!("Kernel sleep not yet wired to ACPI handler")
    }

    fn create_mutex(&self) -> acpi::Handle {
        unimplemented!("Kernel mutexes not yet wired to ACPI handler")
    }

    fn acquire(&self, _mutex: acpi::Handle, _timeout: u16) -> Result<(), acpi::aml::AmlError> {
        unimplemented!("Kernel mutexes not yet wired to ACPI handler")
    }

    fn release(&self, _mutex: acpi::Handle) {
        unimplemented!("Kernel mutexes not yet wired to ACPI handler")
    }
}

/// Helper function to format an x86 PCI legacy address (Ports 0xCF8/0xCFC)
fn pci_address_to_u32(address: &acpi::PciAddress, offset: u16) -> u32 {
    0x8000_0000
        | ((address.bus() as u32) << 16)
        | ((address.device() as u32) << 11)
        | ((address.function() as u32) << 8)
        | (offset as u32 & 0xFC)
}

pub struct IoApicInfo {
    pub id: u8,
    pub phys_addr: u32,
    pub gsi_base: u32,
}

static IO_APICS: Once<Vec<IoApicInfo>> = Once::new();

pub fn get_io_apics() -> &'static Vec<IoApicInfo> {
    IO_APICS.r#try().expect("must init_acpi")
}

pub struct Iso {
    pub isa_irq: u8,
    pub gsi: u32,
    pub flags: u16,
}

static ISOS: Once<Vec<Iso>> = Once::new();

pub fn get_isos() -> &'static Vec<Iso> {
    ISOS.r#try().expect("must init_acpi")
}

pub fn isa_irq_to_gsi(irq: u8) -> u32 {
    get_isos()
        .iter()
        .find(|iso| iso.isa_irq == irq)
        .map(|iso| iso.gsi)
        .unwrap_or(irq as u32)
}

pub fn init_acpi() {
    let tables = unsafe { Rsdp::search_for_on_bios(AcpiHandler).expect("no madt tables") };
    let acpi_tables = unsafe {
        AcpiTables::from_rsdp(AcpiHandler, tables.physical_start)
            .expect("AcpiTables failed to init")
    };

    let madt = acpi_tables.find_table::<madt::Madt>().expect("madt tables");

    let mut isos: Vec<Iso> = Vec::new();
    let mut apics: Vec<IoApicInfo> = Vec::with_capacity(1);

    for entry in madt.get().entries() {
        match entry {
            MadtEntry::IoApic(ioapic_entry) => {
                let entry = IoApicInfo {
                    id: ioapic_entry.io_apic_id,
                    phys_addr: ioapic_entry.io_apic_address,
                    gsi_base: ioapic_entry.global_system_interrupt_base,
                };
                apics.push(entry);
            }
            MadtEntry::InterruptSourceOverride(iso) => {
                isos.push(Iso {
                    isa_irq: iso.irq,
                    gsi: iso.global_system_interrupt,
                    flags: iso.flags,
                });
            }
            _ => (),
        };
    }

    IO_APICS.call_once(|| apics);
    ISOS.call_once(|| isos);
}
