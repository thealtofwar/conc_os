use crate::{
    acpi_handling::{get_io_apics, isa_irq_to_gsi},
    memory::get_offset,
    println,
};
use pic8259::ChainedPics;
use spin::{Mutex, Once};
use x2apic::ioapic::{IoApic, IrqFlags, RedirectionTableEntry};
use x86_64::registers::model_specific::Msr;

const IA32_APIC_BASE_MSR: u32 = 0x1B;
// const X2APIC_ENABLE_BIT: u64 = 1 << 10;
const GLOBAL_APIC_ENABLE_BIT: u64 = 1 << 11;

// The standard PIC offsets
const PIC_1_OFFSET: u8 = 32;
const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;
pub static LAPIC: Once<Mutex<PureXapic>> = Once::new();

// A thread-safe wrapper for pure MMIO xAPIC operations
pub struct PureXapic {
    base_addr: u64,
}

impl PureXapic {
    pub const fn new(base_addr: u64) -> Self {
        Self { base_addr }
    }

    pub unsafe fn enable(&self) {
        // Spurious Interrupt Vector Register is at offset 0x0F0
        let sivr_ptr = (self.base_addr + 0x0F0) as *mut u32;

        // Bit 8 (0x100) is the Software Enable bit.
        // We OR it with our chosen spurious vector (e.g., 255 or 0xFF)
        unsafe { core::ptr::write_volatile(sivr_ptr, 0x100 | 0xFF) };
    }

    pub unsafe fn end_of_interrupt(&self) {
        // End of Interrupt (EOI) Register is at offset 0x0B0
        let eoi_ptr = (self.base_addr + 0x0B0) as *mut u32;

        // Writing 0 signals the end of the interrupt
        unsafe { core::ptr::write_volatile(eoi_ptr, 0) };
    }

    pub unsafe fn lapic_id(&self) -> u8 {
        unsafe {
            let id_reg = (self.base_addr + 0x20) as *const u32;
            ((*id_reg >> 24) & 0xff) as u8
        }
    }
}

// It's safe to send across threads if we wrap it in a Mutex globally
unsafe impl Send for PureXapic {}

pub fn init_apic() {
    let mut apic_msr = Msr::new(IA32_APIC_BASE_MSR);
    let mut reg = unsafe { apic_msr.read() };
    println!("{}", reg);
    // reg |= X2APIC_ENABLE_BIT;
    reg |= GLOBAL_APIC_ENABLE_BIT;
    unsafe { apic_msr.write(reg) };

    let mut legacy_apic = unsafe { ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) };
    unsafe {
        legacy_apic.disable(); // Masks all legacy interrupts
    }

    let lapic_addr = 0xFEE0_0000 + get_offset();
    let lapic = PureXapic::new(lapic_addr);

    unsafe {
        lapic.enable();
        println!("{}", lapic.lapic_id())
    } // Direct MMIO write, no MSRs!

    LAPIC.call_once(|| Mutex::new(lapic));

    let io_apic_info = get_io_apics().first().expect("must have an I/O apic");

    let ioapic_addr = io_apic_info.phys_addr as u64 + get_offset();

    unsafe {
        let mut ioapic = IoApic::new(ioapic_addr);
        ioapic.init(32);
    }

    let gsi = isa_irq_to_gsi(4) as u8;
    route_interrupt(gsi, 36);
}

pub fn route_pci_interrupt(gsi: u8, vector: u8) {
    let io_apic_info = get_io_apics().first().expect("must have an I/O apic");

    let ioapic_addr = io_apic_info.phys_addr as u64 + get_offset();

    unsafe {
        let mut ioapic = IoApic::new(ioapic_addr);

        let mut rte = RedirectionTableEntry::default();

        rte.set_vector(vector);
        rte.set_dest(0);
        rte.set_flags(IrqFlags::LEVEL_TRIGGERED | IrqFlags::LOW_ACTIVE);

        let index = gsi - io_apic_info.gsi_base as u8;
        // Route IRQ 4 to vector 36, targeting CPU core 0
        println!("{:?}", rte);
        ioapic.set_table_entry(index, rte);
        ioapic.enable_irq(index);
    }
}

pub fn route_interrupt(gsi: u8, vector: u8) {
    let io_apic_info = get_io_apics().first().expect("must have an I/O apic");

    let ioapic_addr = io_apic_info.phys_addr as u64 + get_offset();

    unsafe {
        let mut ioapic = IoApic::new(ioapic_addr);

        let mut rte = RedirectionTableEntry::default();

        rte.set_vector(vector);
        rte.set_dest(0);

        let index = gsi - io_apic_info.gsi_base as u8;
        // Route IRQ 4 to vector 36, targeting CPU core 0
        ioapic.set_table_entry(index, rte);
        ioapic.enable_irq(index);
    }
}
