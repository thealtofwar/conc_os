//! x86_64 architecture support.

pub mod apic;
pub mod cpu;
pub mod gdt;
pub mod idt;
pub mod ioapic;
pub mod paging;

use core::arch::asm;

/// Switch to a new address space and stack, then call `entry(arg)`.  Never
/// returns.  The new page tables must map the currently executing code.
pub unsafe fn switch_stack_and_call(cr3: u64, stack_top: u64, entry: extern "sysv64" fn(u64) -> !, arg: u64) -> ! {
    asm!(
        "mov cr3, rcx",
        "mov rsp, rdx",
        "xor ebp, ebp",
        "call rax",
        in("rcx") cr3,
        in("rdx") stack_top,
        in("rax") entry as usize,
        in("rdi") arg,
        options(noreturn)
    )
}

/// Enable the CPU features the kernel relies on: write-protect for kernel
/// pages, SSE state save/restore (guests may use it), no-execute pages, and
/// global pages.
pub fn enable_cpu_features() {
    unsafe {
        let mut c0 = cpu::read_cr0();
        c0 |= cpu::cr0::WP | cpu::cr0::NE | cpu::cr0::MP;
        c0 &= !(cpu::cr0::EM | cpu::cr0::TS | cpu::cr0::CD | cpu::cr0::NW);
        cpu::write_cr0(c0);
        let mut c4 = cpu::read_cr4();
        c4 |= cpu::cr4::OSFXSR | cpu::cr4::OSXMMEXCPT | cpu::cr4::PGE | cpu::cr4::PAE;
        cpu::write_cr4(c4);
        let e = cpu::rdmsr(cpu::msr::IA32_EFER);
        cpu::wrmsr(cpu::msr::IA32_EFER, e | cpu::efer::NXE);
    }
}
