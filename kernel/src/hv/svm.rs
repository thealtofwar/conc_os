//! AMD SVM primitives: VMCB layout, exit codes, feature detection and the
//! `vmrun` trampoline.

#![allow(dead_code)]

use core::arch::global_asm;

use crate::arch::cpu::{self, msr};

// ------------------------------------------------------------ VMCB layout ---

pub mod vmcb {
    // Control area.
    pub const INTERCEPT_CR: usize = 0x000;
    pub const INTERCEPT_DR: usize = 0x004;
    pub const INTERCEPT_EXCEPTIONS: usize = 0x008;
    pub const INTERCEPT_VEC3: usize = 0x00C;
    pub const INTERCEPT_VEC4: usize = 0x010;
    pub const INTERCEPT_VEC5: usize = 0x014;
    pub const PAUSE_FILTER_THRESHOLD: usize = 0x03C;
    pub const PAUSE_FILTER_COUNT: usize = 0x03E;
    pub const IOPM_BASE_PA: usize = 0x040;
    pub const MSRPM_BASE_PA: usize = 0x048;
    pub const TSC_OFFSET: usize = 0x050;
    pub const GUEST_ASID: usize = 0x058;
    pub const TLB_CONTROL: usize = 0x05C;
    pub const VINTR: usize = 0x060;
    pub const INTERRUPT_SHADOW: usize = 0x068;
    pub const EXITCODE: usize = 0x070;
    pub const EXITINFO1: usize = 0x078;
    pub const EXITINFO2: usize = 0x080;
    pub const EXITINTINFO: usize = 0x088;
    pub const NP_ENABLE: usize = 0x090;
    pub const EVENTINJ: usize = 0x0A8;
    pub const N_CR3: usize = 0x0B0;
    pub const LBR_VIRT: usize = 0x0B8;
    pub const CLEAN_BITS: usize = 0x0C0;
    pub const NRIP: usize = 0x0C8;
    pub const INSN_FETCH_COUNT: usize = 0x0D0;
    pub const INSN_BYTES: usize = 0x0D1;

    // State save area.
    pub const ES: usize = 0x400;
    pub const CS: usize = 0x410;
    pub const SS: usize = 0x420;
    pub const DS: usize = 0x430;
    pub const FS: usize = 0x440;
    pub const GS: usize = 0x450;
    pub const GDTR: usize = 0x460;
    pub const LDTR: usize = 0x470;
    pub const IDTR: usize = 0x480;
    pub const TR: usize = 0x490;
    pub const CPL: usize = 0x4CB;
    pub const EFER: usize = 0x4D0;
    pub const CR4: usize = 0x548;
    pub const CR3: usize = 0x550;
    pub const CR0: usize = 0x558;
    pub const DR7: usize = 0x560;
    pub const DR6: usize = 0x568;
    pub const RFLAGS: usize = 0x570;
    pub const RIP: usize = 0x578;
    pub const RSP: usize = 0x5D8;
    pub const RAX: usize = 0x5F8;
    pub const STAR: usize = 0x600;
    pub const LSTAR: usize = 0x608;
    pub const CSTAR: usize = 0x610;
    pub const SFMASK: usize = 0x618;
    pub const KERNEL_GS_BASE: usize = 0x620;
    pub const SYSENTER_CS: usize = 0x628;
    pub const SYSENTER_ESP: usize = 0x630;
    pub const SYSENTER_EIP: usize = 0x638;
    pub const CR2: usize = 0x640;
    pub const G_PAT: usize = 0x668;
    pub const DBGCTL: usize = 0x670;
}

/// Intercept vector 3 bits.
pub mod intercept3 {
    pub const INTR: u32 = 1 << 0;
    pub const NMI: u32 = 1 << 1;
    pub const SMI: u32 = 1 << 2;
    pub const INIT: u32 = 1 << 3;
    pub const VINTR: u32 = 1 << 4;
    pub const CR0_SEL_WRITE: u32 = 1 << 5;
    pub const RDTSC: u32 = 1 << 14;
    pub const RDPMC: u32 = 1 << 15;
    pub const CPUID: u32 = 1 << 18;
    pub const RSM: u32 = 1 << 19;
    pub const INVD: u32 = 1 << 22;
    pub const PAUSE: u32 = 1 << 23;
    pub const HLT: u32 = 1 << 24;
    pub const INVLPG: u32 = 1 << 25;
    pub const INVLPGA: u32 = 1 << 26;
    pub const IOIO_PROT: u32 = 1 << 27;
    pub const MSR_PROT: u32 = 1 << 28;
    pub const TASK_SWITCH: u32 = 1 << 29;
    pub const FERR_FREEZE: u32 = 1 << 30;
    pub const SHUTDOWN: u32 = 1 << 31;
}

/// Intercept vector 4 bits.
pub mod intercept4 {
    pub const VMRUN: u32 = 1 << 0;
    pub const VMMCALL: u32 = 1 << 1;
    pub const VMLOAD: u32 = 1 << 2;
    pub const VMSAVE: u32 = 1 << 3;
    pub const STGI: u32 = 1 << 4;
    pub const CLGI: u32 = 1 << 5;
    pub const SKINIT: u32 = 1 << 6;
    pub const RDTSCP: u32 = 1 << 7;
    pub const ICEBP: u32 = 1 << 8;
    pub const WBINVD: u32 = 1 << 9;
    pub const MONITOR: u32 = 1 << 10;
    pub const MWAIT: u32 = 1 << 11;
    pub const MWAIT_COND: u32 = 1 << 12;
    pub const XSETBV: u32 = 1 << 13;
    pub const RDPRU: u32 = 1 << 14;
}

/// #VMEXIT codes.
pub mod exit {
    pub const CR0_READ: u64 = 0x00;
    pub const CR0_WRITE: u64 = 0x10;
    pub const EXCP_BASE: u64 = 0x40;
    pub const INTR: u64 = 0x60;
    pub const NMI: u64 = 0x61;
    pub const SMI: u64 = 0x62;
    pub const INIT: u64 = 0x63;
    pub const VINTR: u64 = 0x64;
    pub const CR0_SEL_WRITE: u64 = 0x65;
    pub const RDTSC: u64 = 0x6E;
    pub const RDPMC: u64 = 0x6F;
    pub const CPUID: u64 = 0x72;
    pub const INVD: u64 = 0x76;
    pub const PAUSE: u64 = 0x77;
    pub const HLT: u64 = 0x78;
    pub const INVLPG: u64 = 0x79;
    pub const INVLPGA: u64 = 0x7A;
    pub const IOIO: u64 = 0x7B;
    pub const MSR: u64 = 0x7C;
    pub const TASK_SWITCH: u64 = 0x7D;
    pub const FERR_FREEZE: u64 = 0x7E;
    pub const SHUTDOWN: u64 = 0x7F;
    pub const VMRUN: u64 = 0x80;
    pub const VMMCALL: u64 = 0x81;
    pub const VMLOAD: u64 = 0x82;
    pub const VMSAVE: u64 = 0x83;
    pub const STGI: u64 = 0x84;
    pub const CLGI: u64 = 0x85;
    pub const SKINIT: u64 = 0x86;
    pub const RDTSCP: u64 = 0x87;
    pub const ICEBP: u64 = 0x88;
    pub const WBINVD: u64 = 0x89;
    pub const MONITOR: u64 = 0x8A;
    pub const MWAIT: u64 = 0x8B;
    pub const XSETBV: u64 = 0x8D;
    pub const NPF: u64 = 0x400;
    pub const INVALID: u64 = u64::MAX;

    pub fn name(code: u64) -> &'static str {
        match code {
            0x00..=0x0F => "CR read",
            0x10..=0x1F => "CR write",
            0x20..=0x3F => "DR access",
            0x40..=0x5F => "exception",
            INTR => "INTR",
            NMI => "NMI",
            SMI => "SMI",
            INIT => "INIT",
            VINTR => "VINTR",
            CR0_SEL_WRITE => "CR0 write",
            RDTSC => "RDTSC",
            CPUID => "CPUID",
            INVD => "INVD",
            PAUSE => "PAUSE",
            HLT => "HLT",
            INVLPG => "INVLPG",
            IOIO => "IOIO",
            MSR => "MSR",
            SHUTDOWN => "SHUTDOWN",
            VMRUN => "VMRUN",
            VMMCALL => "VMMCALL",
            VMLOAD => "VMLOAD",
            VMSAVE => "VMSAVE",
            STGI => "STGI",
            CLGI => "CLGI",
            SKINIT => "SKINIT",
            RDTSCP => "RDTSCP",
            WBINVD => "WBINVD",
            MONITOR => "MONITOR",
            MWAIT => "MWAIT",
            XSETBV => "XSETBV",
            NPF => "NPF",
            INVALID => "INVALID",
            _ => "?",
        }
    }
}

/// Segment attribute words (descriptor bits 40-47 and 52-55).
pub mod seg_attr {
    /// 64-bit code: type 0xB, S, P, L, G.
    pub const CODE64: u16 = 0x0A9B;
    /// Read/write data: type 3, S, P, D/B, G.
    pub const DATA: u16 = 0x0C93;
    /// 64-bit busy TSS.
    pub const TSS64: u16 = 0x008B;
    /// Not present.
    pub const UNUSABLE: u16 = 0x0000;
}

/// Guest general purpose registers not held in the VMCB.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GuestRegs {
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
}

/// Typed access to a VMCB page.
#[derive(Clone, Copy)]
pub struct Vmcb {
    pub pa: u64,
}

impl Vmcb {
    #[inline]
    pub fn read8(&self, off: usize) -> u8 {
        unsafe { core::ptr::read_volatile((self.pa as usize + off) as *const u8) }
    }
    #[inline]
    pub fn write8(&self, off: usize, v: u8) {
        unsafe { core::ptr::write_volatile((self.pa as usize + off) as *mut u8, v) }
    }
    #[inline]
    pub fn read16(&self, off: usize) -> u16 {
        unsafe { core::ptr::read_volatile((self.pa as usize + off) as *const u16) }
    }
    #[inline]
    pub fn write16(&self, off: usize, v: u16) {
        unsafe { core::ptr::write_volatile((self.pa as usize + off) as *mut u16, v) }
    }
    #[inline]
    pub fn read32(&self, off: usize) -> u32 {
        unsafe { core::ptr::read_volatile((self.pa as usize + off) as *const u32) }
    }
    #[inline]
    pub fn write32(&self, off: usize, v: u32) {
        unsafe { core::ptr::write_volatile((self.pa as usize + off) as *mut u32, v) }
    }
    #[inline]
    pub fn read64(&self, off: usize) -> u64 {
        unsafe { core::ptr::read_volatile((self.pa as usize + off) as *const u64) }
    }
    #[inline]
    pub fn write64(&self, off: usize, v: u64) {
        unsafe { core::ptr::write_volatile((self.pa as usize + off) as *mut u64, v) }
    }

    /// Write a segment register: selector, attributes, limit, base.
    pub fn set_segment(&self, off: usize, selector: u16, attr: u16, limit: u32, base: u64) {
        self.write16(off, selector);
        self.write16(off + 2, attr);
        self.write32(off + 4, limit);
        self.write64(off + 8, base);
    }

    pub fn rip(&self) -> u64 {
        self.read64(vmcb::RIP)
    }
    pub fn set_rip(&self, v: u64) {
        self.write64(vmcb::RIP, v)
    }
    pub fn rax(&self) -> u64 {
        self.read64(vmcb::RAX)
    }
    pub fn set_rax(&self, v: u64) {
        self.write64(vmcb::RAX, v)
    }
    pub fn rsp(&self) -> u64 {
        self.read64(vmcb::RSP)
    }
    pub fn rflags(&self) -> u64 {
        self.read64(vmcb::RFLAGS)
    }
    pub fn exit_code(&self) -> u64 {
        self.read64(vmcb::EXITCODE)
    }
    pub fn exit_info1(&self) -> u64 {
        self.read64(vmcb::EXITINFO1)
    }
    pub fn exit_info2(&self) -> u64 {
        self.read64(vmcb::EXITINFO2)
    }
    pub fn nrip(&self) -> u64 {
        self.read64(vmcb::NRIP)
    }
}

// --------------------------------------------------------------- features ---

#[derive(Clone, Copy, Debug, Default)]
pub struct SvmFeatures {
    pub revision: u8,
    pub nasids: u32,
    pub npt: bool,
    pub nrip_save: bool,
    pub flush_by_asid: bool,
    pub decode_assists: bool,
    pub vmcb_clean: bool,
    pub pause_filter: bool,
    pub tsc_rate_msr: bool,
    pub avic: bool,
    pub vgif: bool,
    pub raw_edx: u32,
}

pub fn features() -> SvmFeatures {
    let r = cpu::cpuid(0x8000_000A, 0);
    SvmFeatures {
        revision: (r.eax & 0xFF) as u8,
        nasids: r.ebx,
        npt: r.edx & (1 << 0) != 0,
        nrip_save: r.edx & (1 << 3) != 0,
        tsc_rate_msr: r.edx & (1 << 4) != 0,
        vmcb_clean: r.edx & (1 << 5) != 0,
        flush_by_asid: r.edx & (1 << 6) != 0,
        decode_assists: r.edx & (1 << 7) != 0,
        pause_filter: r.edx & (1 << 10) != 0,
        avic: r.edx & (1 << 13) != 0,
        vgif: r.edx & (1 << 16) != 0,
        raw_edx: r.edx,
    }
}

/// Why SVM cannot be used on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvmError {
    NotSupported,
    DisabledByFirmware,
    NoNestedPaging,
}

/// Turn SVM on for the boot CPU.  `hsave_pa` is a 4 KiB page for the
/// host state save area.
pub fn enable(hsave_pa: u64) -> Result<SvmFeatures, SvmError> {
    if !cpu::has_svm() {
        return Err(SvmError::NotSupported);
    }
    let f = features();
    if !f.npt {
        return Err(SvmError::NoNestedPaging);
    }
    let vm_cr = cpu::rdmsr(msr::VM_CR);
    if vm_cr & (1 << 4) != 0 {
        return Err(SvmError::DisabledByFirmware);
    }
    unsafe {
        let e = cpu::rdmsr(msr::IA32_EFER);
        cpu::wrmsr(msr::IA32_EFER, e | cpu::efer::SVME);
        cpu::wrmsr(msr::VM_HSAVE_PA, hsave_pa);
    }
    Ok(f)
}

// ---------------------------------------------------------------- vmrun ---

global_asm!(
    ".section .text",
    ".global svm_run",
    // svm_run(rdi = guest vmcb pa, rsi = host vmcb pa, rdx = &mut GuestRegs)
    "svm_run:",
    "    push rbp",
    "    push rbx",
    "    push r12",
    "    push r13",
    "    push r14",
    "    push r15",
    "    push rdx",            // [rsp+16] regs
    "    push rsi",            // [rsp+8]  host vmcb
    "    push rdi",            // [rsp]    guest vmcb
    // Host FS/GS/TR/LDTR/KernelGsBase/STAR... were saved once by
    // `save_host_state` (they never change), so only the guest side is
    // loaded and stored around VMRUN: three VMLOAD/VMSAVEs per entry, not
    // four.  Each one is an intercepted instruction when we ourselves run
    // nested, so this matters.
    // Load guest GPRs (rdx last, it is the base pointer).
    "    mov rbx, [rdx + 0]",
    "    mov rcx, [rdx + 8]",
    "    mov rsi, [rdx + 24]",
    "    mov rdi, [rdx + 32]",
    "    mov rbp, [rdx + 40]",
    "    mov r8,  [rdx + 48]",
    "    mov r9,  [rdx + 56]",
    "    mov r10, [rdx + 64]",
    "    mov r11, [rdx + 72]",
    "    mov r12, [rdx + 80]",
    "    mov r13, [rdx + 88]",
    "    mov r14, [rdx + 96]",
    "    mov r15, [rdx + 104]",
    "    mov rdx, [rdx + 16]",
    "    mov rax, [rsp]",
    "    clgi",
    "    vmload",
    "    vmrun",
    "    vmsave",
    // Store guest GPRs.
    "    mov rax, [rsp + 16]",
    "    mov [rax + 0],   rbx",
    "    mov [rax + 8],   rcx",
    "    mov [rax + 16],  rdx",
    "    mov [rax + 24],  rsi",
    "    mov [rax + 32],  rdi",
    "    mov [rax + 40],  rbp",
    "    mov [rax + 48],  r8",
    "    mov [rax + 56],  r9",
    "    mov [rax + 64],  r10",
    "    mov [rax + 72],  r11",
    "    mov [rax + 80],  r12",
    "    mov [rax + 88],  r13",
    "    mov [rax + 96],  r14",
    "    mov [rax + 104], r15",
    // Restore host state.
    "    pop rdi",
    "    pop rax",
    "    vmload",
    "    stgi",
    "    pop rdx",
    "    pop r15",
    "    pop r14",
    "    pop r13",
    "    pop r12",
    "    pop rbx",
    "    pop rbp",
    "    ret",
    // svm_save_host(rdi = host vmcb pa): VMSAVE the host's segment/MSR state.
    ".global svm_save_host",
    "svm_save_host:",
    "    mov rax, rdi",
    "    vmsave",
    "    ret",
);

extern "sysv64" {
    fn svm_run(guest_vmcb: u64, host_vmcb: u64, regs: *mut GuestRegs);
    fn svm_save_host(host_vmcb: u64);
}

/// Save the host's FS/GS/TR/LDTR bases and syscall/sysenter MSRs into the
/// host VMCB once; `run` restores them after every exit.
pub fn save_host_state(host_vmcb: u64) {
    unsafe { svm_save_host(host_vmcb) }
}

/// Enter the guest once.  Returns after the next #VMEXIT with the guest's
/// registers stored back into `regs` and exit information in the VMCB.
/// Must be called with interrupts enabled so that INTR intercepts fire.
#[inline]
pub fn run(guest_vmcb: u64, host_vmcb: u64, regs: &mut GuestRegs) {
    unsafe { svm_run(guest_vmcb, host_vmcb, regs) }
}

/// 512-byte FXSAVE area.
#[repr(C, align(64))]
#[derive(Clone)]
pub struct FxState(pub [u8; 512]);

impl FxState {
    pub const fn new() -> Self {
        FxState([0; 512])
    }
}

#[inline]
pub fn fxsave(s: &mut FxState) {
    unsafe { core::arch::asm!("fxsave64 [{}]", in(reg) s.0.as_mut_ptr(), options(nostack, preserves_flags)) }
}

#[inline]
pub fn fxrstor(s: &FxState) {
    unsafe { core::arch::asm!("fxrstor64 [{}]", in(reg) s.0.as_ptr(), options(nostack, preserves_flags)) }
}
