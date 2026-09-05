//! Global descriptor table and task state segment.
//!
//! The kernel never leaves ring 0, so the GDT is tiny: a 64-bit code segment,
//! a data segment and a TSS whose interrupt stacks are used for faults that
//! must not trust the current stack (double fault, NMI, machine check).

#![allow(dead_code)]

use core::arch::asm;
use core::mem::size_of;

pub const KERNEL_CODE: u16 = 0x08;
pub const KERNEL_DATA: u16 = 0x10;
pub const TSS_SEL: u16 = 0x18;

const IST_STACK_PAGES: usize = 8; // 32 KiB per emergency stack

#[repr(C, packed)]
pub struct Tss {
    reserved0: u32,
    pub rsp: [u64; 3],
    reserved1: u64,
    pub ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    pub iomap_base: u16,
}

#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

static mut TSS: Tss = Tss {
    reserved0: 0,
    rsp: [0; 3],
    reserved1: 0,
    ist: [0; 7],
    reserved2: 0,
    reserved3: 0,
    iomap_base: size_of::<Tss>() as u16,
};

static mut GDT: [u64; 5] = [
    0,
    0x00AF_9A00_0000_FFFF, // 64-bit kernel code
    0x00CF_9200_0000_FFFF, // kernel data
    0,                     // TSS low
    0,                     // TSS high
];

fn tss_descriptor(base: u64, limit: u32) -> (u64, u64) {
    let low = (limit as u64 & 0xFFFF)
        | ((base & 0xFF_FFFF) << 16)
        | (0x89u64 << 40) // present, 64-bit available TSS
        | (((limit as u64 >> 16) & 0xF) << 48)
        | (((base >> 24) & 0xFF) << 56);
    let high = base >> 32;
    (low, high)
}

/// Install the GDT and TSS.  Requires the frame allocator (for IST stacks).
pub fn init() {
    unsafe {
        let tss = &mut *core::ptr::addr_of_mut!(TSS);
        for i in 0..3 {
            let stack = crate::mm::frame::alloc_contiguous(IST_STACK_PAGES, 1).expect("IST stack");
            tss.ist[i] = stack + (IST_STACK_PAGES * 4096) as u64;
        }
        let (lo, hi) = tss_descriptor(core::ptr::addr_of!(TSS) as u64, (size_of::<Tss>() - 1) as u32);
        let gdt = &mut *core::ptr::addr_of_mut!(GDT);
        gdt[3] = lo;
        gdt[4] = hi;

        let ptr = DescriptorTablePointer {
            limit: (size_of::<[u64; 5]>() - 1) as u16,
            base: core::ptr::addr_of!(GDT) as u64,
        };
        asm!(
            "lgdt [{ptr}]",
            // Reload CS with a far return.
            "push {code}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ds, {data:x}",
            "mov es, {data:x}",
            "mov ss, {data:x}",
            "xor {tmp:e}, {tmp:e}",
            "mov fs, {tmp:x}",
            "mov gs, {tmp:x}",
            "ltr {tss:x}",
            ptr = in(reg) &ptr,
            code = const KERNEL_CODE as u64,
            data = in(reg) KERNEL_DATA as u64,
            tss = in(reg) TSS_SEL as u64,
            tmp = out(reg) _,
        );
    }
}

/// Stack tops of the interrupt stacks (for diagnostics).
pub fn ist_stacks() -> [u64; 3] {
    unsafe {
        let tss = &*core::ptr::addr_of!(TSS);
        let ist = core::ptr::addr_of!(tss.ist).read_unaligned();
        [ist[0], ist[1], ist[2]]
    }
}
