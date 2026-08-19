use crate::{
    acpi_handling::{get_io_apics, isa_irq_to_gsi},
    memory::get_offset,
    println,
};
use pic8259::ChainedPics;
use spin::Once;
use x2apic::ioapic::{IoApic, IrqFlags, RedirectionTableEntry};
use x86_64::registers::model_specific::Msr;

const IA32_APIC_BASE_MSR: u32 = 0x1B;
// const X2APIC_ENABLE_BIT: u64 = 1 << 10;
const GLOBAL_APIC_ENABLE_BIT: u64 = 1 << 11;

/// Physical address of the local APIC register block.
///
/// Fixed by the architecture and the same on every core, because each core
/// sees *its own* registers there. The address therefore never identifies a
/// particular APIC, only "whichever one belongs to whoever is asking".
const LAPIC_PHYS_ADDR: u64 = 0xFEE0_0000;

// The standard PIC offsets
const PIC_1_OFFSET: u8 = 32;
const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;

// Register offsets within the block, SDM Vol. 3 Table 11-1.
const REG_LAPIC_ID: u64 = 0x020;
const REG_EOI: u64 = 0x0B0;
const REG_SPURIOUS: u64 = 0x0F0;

/// Virtual address the local APIC registers are mapped at, set once by
/// [`init_apic`].
///
/// Deliberately not behind a lock. There is nothing here to serialize: every
/// access below is a single volatile load or store, none of them a
/// read-modify-write, and they land in a register block private to the calling
/// core. A lock would only introduce the possibility of an interrupt handler
/// spinning on a lock held by the code it interrupted.
static LAPIC_BASE: Once<u64> = Once::new();

fn base() -> u64 {
    *LAPIC_BASE.r#try().expect("APIC must be initialized")
}

/// Reads the local APIC register at `offset`, **on the calling CPU**.
///
/// # Safety
///
/// `offset` must be the offset of a readable local APIC register. Some
/// registers in the block are write-only, and reading those is undefined.
pub unsafe fn read_register(offset: u64) -> u32 {
    unsafe { ((base() + offset) as *const u32).read_volatile() }
}

/// Writes `value` to the local APIC register at `offset`, **on the calling CPU**.
///
/// # Safety
///
/// `offset` must be the offset of a writable local APIC register, and `value`
/// must be meaningful for that register. These registers drive interrupt
/// delivery, so a wrong store misroutes or silently masks interrupts rather
/// than failing in any visible way.
pub unsafe fn write_register(offset: u64, value: u32) {
    unsafe { ((base() + offset) as *mut u32).write_volatile(value) };
}

/// Software-enables the local APIC, parking spurious interrupts on vector 0xFF.
fn enable() {
    // Bit 8 is the software enable bit; the low byte is the spurious vector.
    unsafe { write_register(REG_SPURIOUS, 0x100 | 0xFF) };
}

/// Signals end-of-interrupt to the calling CPU's local APIC.
///
/// Every handler for an interrupt delivered through the APIC must call this
/// exactly once before returning. Skipping it leaves the in-service bit set and
/// that core stops accepting interrupts at or below the serviced priority.
pub fn end_of_interrupt() {
    // The entire protocol is a store of zero; the value written is ignored.
    unsafe { write_register(REG_EOI, 0) };
}

/// The calling CPU's local APIC ID.
///
/// Reports whichever core executes it. It is not a property of anything
/// recorded earlier, so a value read on one core says nothing about another.
pub fn lapic_id() -> u8 {
    (unsafe { read_register(REG_LAPIC_ID) } >> 24) as u8
}

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

    LAPIC_BASE.call_once(|| LAPIC_PHYS_ADDR + get_offset());

    enable();
    println!("{}", lapic_id());

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
