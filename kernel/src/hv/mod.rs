//! The hypervisor.
//!
//! conc_os runs guests with AMD SVM (AMD-V) and nested paging.  Each VM is an
//! async task that owns its VMCB and memory; the manager routes requests to
//! VMs and applies scale-to-zero policies.  See the submodules:
//!
//! * `svm`     – VMCB layout, VMRUN trampoline, feature detection
//! * `npt`     – nested page tables
//! * `image`   – guest ELF loader and copy-on-write templates
//! * `memory`  – per-VM lazily populated memory with disk swap
//! * `vcpu`    – exit handling and hypercalls
//! * `vm`      – the shared VM handle (state, requests, stats)
//! * `manager` – registry, services, autoscaler, UDP front door

#![allow(dead_code)]

pub mod devices;
pub mod image;
pub mod linux_boot;
pub mod manager;
pub mod memory;
pub mod npt;
pub mod shell;
pub mod svm;
pub mod tests;
pub mod vcpu;
pub mod vm;
pub mod webtest;
pub mod x86;

use alloc::vec::Vec;

use crate::mm::frame;
use crate::selftest::TestFn;
use crate::sync::{OnceCell, SpinLock};
use svm::SvmFeatures;

struct AsidPool {
    /// VM id owning each ASID (index 0 unused, 0 = free).
    owners: Vec<u32>,
    next: usize,
}

pub struct Host {
    pub enabled: bool,
    pub host_vmcb: u64,
    pub hsave: u64,
    pub iopm: u64,
    pub msrpm: u64,
    pub features: SvmFeatures,
    asids: SpinLock<AsidPool>,
}

static HOST: OnceCell<Host> = OnceCell::new();

pub fn host() -> &'static Host {
    HOST.expect("hypervisor host state")
}

pub fn is_enabled() -> bool {
    HOST.get().map(|h| h.enabled).unwrap_or(false)
}

impl Host {
    /// Get an ASID for `vm_id`.  Returns (asid, flush_needed).  ASIDs are
    /// recycled round-robin when there are more VMs than ASIDs.
    pub fn acquire_asid(&self, vm_id: u32, current: u32) -> (u32, bool) {
        let mut p = self.asids.lock();
        let n = p.owners.len();
        if current != 0 && (current as usize) < n && p.owners[current as usize] == vm_id {
            return (current, false);
        }
        // Prefer a free slot.
        for i in 0..n {
            let idx = (p.next + i) % n;
            if idx == 0 {
                continue;
            }
            if p.owners[idx] == 0 {
                p.owners[idx] = vm_id;
                p.next = idx + 1;
                return (idx as u32, true);
            }
        }
        // Steal one.
        let mut idx = p.next % n;
        if idx == 0 {
            idx = 1;
        }
        p.owners[idx] = vm_id;
        p.next = idx + 1;
        (idx as u32, true)
    }

    pub fn release_asid(&self, vm_id: u32, asid: u32) {
        let mut p = self.asids.lock();
        if asid != 0 && (asid as usize) < p.owners.len() && p.owners[asid as usize] == vm_id {
            p.owners[asid as usize] = 0;
        }
    }

    pub fn asids_in_use(&self) -> usize {
        self.asids.lock().owners.iter().filter(|&&o| o != 0).count()
    }
}

/// Enable SVM and set up shared host structures.
/// Clear the read and write intercept bits of one MSR in the MSR permission
/// map (three 2 KiB ranges, two bits per MSR).
fn msrpm_passthrough(msrpm: u64, msr: u32) {
    let (base, start) = match msr {
        0..=0x1FFF => (0usize, 0u32),
        0xC000_0000..=0xC000_1FFF => (0x800, 0xC000_0000),
        0xC001_0000..=0xC001_1FFF => (0x1000, 0xC001_0000),
        _ => return,
    };
    let idx = (msr - start) as usize;
    let byte = base + idx / 4;
    let bit = (idx % 4) * 2;
    unsafe {
        let p = (msrpm as *mut u8).add(byte);
        *p &= !(0b11 << bit);
    }
}

pub fn init() {
    let hsave = frame::alloc_zeroed().expect("hsave");
    let host_vmcb = frame::alloc_zeroed().expect("host vmcb");
    // Intercept every I/O port (12 KiB bitmap) and every MSR (8 KiB bitmap).
    let iopm = frame::alloc_contiguous(3, 1).expect("iopm");
    let msrpm = frame::alloc_contiguous(2, 1).expect("msrpm");
    unsafe {
        core::ptr::write_bytes(iopm as *mut u8, 0xFF, 3 * 4096);
        core::ptr::write_bytes(msrpm as *mut u8, 0xFF, 2 * 4096);
        // MSRs that VMLOAD/VMSAVE carry in the VMCB need no intercept: the
        // guest reads and writes them directly (Linux touches FS/GS/KERNEL_GS
        // base on every context switch).
        for msr in [
            0xC000_0081u32, // STAR
            0xC000_0082,    // LSTAR
            0xC000_0083,    // CSTAR
            0xC000_0084,    // SFMASK
            0xC000_0100,    // FS_BASE
            0xC000_0101,    // GS_BASE
            0xC000_0102,    // KERNEL_GS_BASE
            0x174,          // SYSENTER_CS
            0x175,          // SYSENTER_ESP
            0x176,          // SYSENTER_EIP
        ] {
            msrpm_passthrough(msrpm, msr);
        }
    }

    let (enabled, features) = match svm::enable(hsave) {
        Ok(f) => {
            log!(
                "hv: svm enabled (rev {}, {} asids, npt={} nrip={} flushbyasid={} decode={})",
                f.revision,
                f.nasids,
                f.npt,
                f.nrip_save,
                f.flush_by_asid,
                f.decode_assists
            );
            (true, f)
        }
        Err(e) => {
            log!("hv: cannot enable svm: {:?} -- VMs unavailable", e);
            (false, svm::features())
        }
    };
    let nasids = (features.nasids.max(2) as usize).min(4096);
    HOST.init(Host {
        enabled,
        host_vmcb,
        hsave,
        iopm,
        msrpm,
        features,
        asids: SpinLock::new(AsidPool { owners: alloc::vec![0u32; nasids], next: 1 }),
    });
    memory::init_zero_frame();
    manager::init();
    if enabled {
        // Host segment bases and syscall MSRs never change after boot:
        // save them once instead of before every VMRUN.
        svm::save_host_state(host_vmcb);
        match manager::manager().template(image::DEFAULT_MEM) {
            Ok(t) => log!(
                "hv: guest image {} bytes, entry {:#x}, template {} pages for {} VMs",
                image::GUEST_ELF.len(),
                t.boot.rip,
                t.pages.len(),
                crate::mm::Bytes(t.mem_size)
            ),
            Err(e) => log!("hv: failed to build guest template: {}", e),
        }
    }
}

pub fn help() {
    shell::help();
}

pub async fn dispatch(cmd: &str, args: &[&str]) -> bool {
    shell::dispatch(cmd, args).await
}

pub fn test_suite() -> &'static [(&'static str, TestFn)] {
    tests::tests()
}
