//! Low-level CPU primitives: port I/O, MSRs, control registers, CPUID, TSC.

#![allow(dead_code)]

use core::arch::asm;

#[inline]
pub fn outb(port: u16, v: u8) {
    unsafe { asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack, preserves_flags)) }
}
#[inline]
pub fn inb(port: u16) -> u8 {
    let v: u8;
    unsafe { asm!("in al, dx", in("dx") port, out("al") v, options(nomem, nostack, preserves_flags)) }
    v
}
#[inline]
pub fn outw(port: u16, v: u16) {
    unsafe { asm!("out dx, ax", in("dx") port, in("ax") v, options(nomem, nostack, preserves_flags)) }
}
#[inline]
pub fn inw(port: u16) -> u16 {
    let v: u16;
    unsafe { asm!("in ax, dx", in("dx") port, out("ax") v, options(nomem, nostack, preserves_flags)) }
    v
}
#[inline]
pub fn outl(port: u16, v: u32) {
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") v, options(nomem, nostack, preserves_flags)) }
}
#[inline]
pub fn inl(port: u16) -> u32 {
    let v: u32;
    unsafe { asm!("in eax, dx", in("dx") port, out("eax") v, options(nomem, nostack, preserves_flags)) }
    v
}

#[inline]
pub fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    unsafe { asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) }
    ((hi as u64) << 32) | lo as u64
}
#[inline]
pub unsafe fn wrmsr(msr: u32, v: u64) {
    let lo = v as u32;
    let hi = (v >> 32) as u32;
    asm!("wrmsr", in("ecx") msr, in("eax") lo, in("edx") hi, options(nomem, nostack, preserves_flags))
}

pub mod msr {
    pub const IA32_APIC_BASE: u32 = 0x1B;
    pub const IA32_PAT: u32 = 0x277;
    pub const IA32_EFER: u32 = 0xC000_0080;
    pub const IA32_STAR: u32 = 0xC000_0081;
    pub const IA32_LSTAR: u32 = 0xC000_0082;
    pub const IA32_FMASK: u32 = 0xC000_0084;
    pub const IA32_FS_BASE: u32 = 0xC000_0100;
    pub const IA32_GS_BASE: u32 = 0xC000_0101;
    pub const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;
    pub const VM_CR: u32 = 0xC001_0114;
    pub const VM_HSAVE_PA: u32 = 0xC001_0117;
    pub const X2APIC_BASE: u32 = 0x800;
}

#[inline]
pub fn read_cr0() -> u64 {
    let v: u64;
    unsafe { asm!("mov {}, cr0", out(reg) v, options(nomem, nostack, preserves_flags)) }
    v
}
#[inline]
pub unsafe fn write_cr0(v: u64) {
    asm!("mov cr0, {}", in(reg) v, options(nomem, nostack, preserves_flags))
}
#[inline]
pub fn read_cr2() -> u64 {
    let v: u64;
    unsafe { asm!("mov {}, cr2", out(reg) v, options(nomem, nostack, preserves_flags)) }
    v
}
#[inline]
pub fn read_cr3() -> u64 {
    let v: u64;
    unsafe { asm!("mov {}, cr3", out(reg) v, options(nomem, nostack, preserves_flags)) }
    v
}
#[inline]
pub unsafe fn write_cr3(v: u64) {
    asm!("mov cr3, {}", in(reg) v, options(nostack, preserves_flags))
}
#[inline]
pub fn read_cr4() -> u64 {
    let v: u64;
    unsafe { asm!("mov {}, cr4", out(reg) v, options(nomem, nostack, preserves_flags)) }
    v
}
#[inline]
pub unsafe fn write_cr4(v: u64) {
    asm!("mov cr4, {}", in(reg) v, options(nomem, nostack, preserves_flags))
}

pub mod cr0 {
    pub const PE: u64 = 1 << 0;
    pub const MP: u64 = 1 << 1;
    pub const EM: u64 = 1 << 2;
    pub const TS: u64 = 1 << 3;
    pub const ET: u64 = 1 << 4;
    pub const NE: u64 = 1 << 5;
    pub const WP: u64 = 1 << 16;
    pub const NW: u64 = 1 << 29;
    pub const CD: u64 = 1 << 30;
    pub const PG: u64 = 1 << 31;
}
pub mod cr4 {
    pub const PAE: u64 = 1 << 5;
    pub const PGE: u64 = 1 << 7;
    pub const OSFXSR: u64 = 1 << 9;
    pub const OSXMMEXCPT: u64 = 1 << 10;
    pub const OSXSAVE: u64 = 1 << 18;
}
pub mod efer {
    pub const SCE: u64 = 1 << 0;
    pub const LME: u64 = 1 << 8;
    pub const LMA: u64 = 1 << 10;
    pub const NXE: u64 = 1 << 11;
    pub const SVME: u64 = 1 << 12;
}

#[inline]
pub fn rflags() -> u64 {
    let v: u64;
    unsafe { asm!("pushfq", "pop {}", out(reg) v, options(nomem, preserves_flags)) }
    v
}
#[inline]
pub fn interrupts_enabled() -> bool {
    rflags() & (1 << 9) != 0
}
#[inline]
pub fn cli() {
    unsafe { asm!("cli", options(nomem, nostack)) }
}
#[inline]
pub fn sti() {
    unsafe { asm!("sti", options(nomem, nostack)) }
}
/// Disable interrupts, returning whether they were enabled before.
#[inline]
pub fn save_and_disable_interrupts() -> bool {
    let was = interrupts_enabled();
    if was {
        cli();
    }
    was
}
#[inline]
pub fn restore_interrupts(enable: bool) {
    if enable {
        sti();
    }
}
#[inline]
pub fn hlt() {
    unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) }
}
/// Atomically enable interrupts and halt: an interrupt arriving right after
/// `sti` is delivered after the `hlt` completes, so no wakeup is lost.
#[inline]
pub fn sti_hlt() {
    unsafe { asm!("sti", "hlt", options(nomem, nostack)) }
}
#[inline]
pub fn pause() {
    core::hint::spin_loop();
}
#[inline]
pub fn rdtsc() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() }
}
#[inline]
pub fn invlpg(addr: u64) {
    unsafe { asm!("invlpg [{}]", in(reg) addr, options(nostack, preserves_flags)) }
}
#[inline]
pub fn flush_tlb() {
    unsafe { write_cr3(read_cr3()) }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CpuidResult {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

#[inline]
pub fn cpuid(leaf: u32, subleaf: u32) -> CpuidResult {
    let r = unsafe { core::arch::x86_64::__cpuid_count(leaf, subleaf) };
    CpuidResult { eax: r.eax, ebx: r.ebx, ecx: r.ecx, edx: r.edx }
}

/// Maximum basic and extended CPUID leaves.
pub fn cpuid_max() -> (u32, u32) {
    (cpuid(0, 0).eax, cpuid(0x8000_0000, 0).eax)
}

/// The 12-byte CPU vendor string.
pub fn vendor() -> [u8; 12] {
    let r = cpuid(0, 0);
    let mut v = [0u8; 12];
    v[0..4].copy_from_slice(&r.ebx.to_le_bytes());
    v[4..8].copy_from_slice(&r.edx.to_le_bytes());
    v[8..12].copy_from_slice(&r.ecx.to_le_bytes());
    v
}

/// The processor brand string (up to 48 bytes, NUL padded).
pub fn brand() -> [u8; 48] {
    let mut b = [0u8; 48];
    let (_, max_ext) = cpuid_max();
    if max_ext < 0x8000_0004 {
        return b;
    }
    for i in 0..3 {
        let r = cpuid(0x8000_0002 + i as u32, 0);
        let o = i * 16;
        b[o..o + 4].copy_from_slice(&r.eax.to_le_bytes());
        b[o + 4..o + 8].copy_from_slice(&r.ebx.to_le_bytes());
        b[o + 8..o + 12].copy_from_slice(&r.ecx.to_le_bytes());
        b[o + 12..o + 16].copy_from_slice(&r.edx.to_le_bytes());
    }
    b
}

pub fn has_svm() -> bool {
    let (_, max_ext) = cpuid_max();
    max_ext >= 0x8000_0001 && cpuid(0x8000_0001, 0).ecx & (1 << 2) != 0
}

pub fn has_vmx() -> bool {
    cpuid(1, 0).ecx & (1 << 5) != 0
}

pub fn has_x2apic() -> bool {
    cpuid(1, 0).ecx & (1 << 21) != 0
}

/// Physical address width supported by the CPU.
pub fn phys_addr_bits() -> u32 {
    let (_, max_ext) = cpuid_max();
    if max_ext >= 0x8000_0008 {
        cpuid(0x8000_0008, 0).eax & 0xFF
    } else {
        36
    }
}

/// Exit QEMU through the `isa-debug-exit` device (if present).  QEMU exits
/// with status `(code << 1) | 1`.  On real hardware this is a harmless write
/// to an unused port.
pub fn qemu_exit(code: u32) -> ! {
    outl(0xf4, code);
    loop {
        cli();
        hlt();
    }
}

/// Ask QEMU (or an ACPI-less machine) to power off via the q35/i440fx PM
/// ports, falling back to a halt loop.
pub fn power_off() -> ! {
    outw(0x604, 0x2000); // q35
    outw(0xB004, 0x2000); // i440fx
    loop {
        cli();
        hlt();
    }
}
