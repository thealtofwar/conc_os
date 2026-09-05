//! The virtual CPU: VMCB setup, the run loop, #VMEXIT handling, the
//! hypercall interface for unikernel guests and the device model / interrupt
//! injection path for Linux guests.  One async task per VM.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use super::devices::DeviceModel;
use super::image::{ResumeState, Template, TemplateKind};
use super::memory::GuestMemory;
use super::svm::{self, exit, intercept3, intercept4, seg_attr, vmcb, FxState, GuestRegs, Vmcb};
use super::vm::{Command, Request, VmHandle, VmKind, VmState};
use super::x86;
use crate::arch::cpu::{self, efer};
use crate::mm::frame;
use crate::task::{self, timer};
use crate::time;

/// Maximum time a VM keeps the CPU before yielding to other tasks.
pub const QUANTUM_US: u64 = 2_000;

/// Hypercall numbers (shared with `guest/src/main.rs`).
pub mod hc {
    pub const LOG: u64 = 0;
    pub const WAIT_REQUEST: u64 = 1;
    pub const RESPOND: u64 = 2;
    pub const EXIT: u64 = 3;
    pub const UPTIME_US: u64 = 4;
    pub const YIELD: u64 = 5;
    pub const SLEEP_MS: u64 = 6;
    /// Linux guests: returns the VM id (used by the guest web server).
    pub const GET_VM_ID: u64 = 7;
}

pub enum Action {
    Continue,
    Yield,
    /// Unikernel: blocked until a request arrives.
    Block,
    /// Linux: halted until `host deadline` (TSC) or an external event.
    WaitEvent(Option<u64>),
    Sleep(u64),
    Reset(&'static str),
    Exit(u64),
    Crash(String),
}

pub struct VmCore {
    pub handle: Arc<VmHandle>,
    template: Arc<Template>,
    vmcb: Vmcb,
    regs: GuestRegs,
    fx: Box<FxState>,
    pub mem: GuestMemory,
    dev: Option<DeviceModel>,
    asid: u32,
    current: Option<Request>,
    booted: bool,
    serial_line: String,
    /// Guest TSC = host TSC + offset.  Linux guests do not see time spent in
    /// the hypervisor, so their PIT/TSC calibration is not skewed by exits.
    tsc_offset: i64,
    tsc_aux: u64,
    /// Event interrupted by an exit; must be re-injected.
    exit_intinfo: Option<(u32, u32)>,
    vintr_window: bool,
    /// Host TSC when the run loop was left while the guest was runnable;
    /// that descheduled time is hidden from the guest as well.
    left_at: Option<u64>,
    /// Profile: host TSC spent in exit handling and exit counts, by class
    /// (npf, mmio, io, msr, cpuid, hlt, intr, other).
    prof_host: [u64; 8],
    prof_count: [u64; 8],
    /// Host TSC spent runnable but descheduled.
    wait_tsc: u64,
    /// Decoded MOV instructions by guest RIP for MMIO emulation.
    mmio_cache: alloc::collections::BTreeMap<u64, x86::MovInsn>,
    /// Pages copied ahead of time from the learned write set.
    eager_pages: u64,
}

const EXCEPTION_NAMES: [&str; 32] = [
    "#DE", "#DB", "NMI", "#BP", "#OF", "#BR", "#UD", "#NM", "#DF", "#CSO", "#TS", "#NP", "#SS", "#GP", "#PF", "?",
    "#MF", "#AC", "#MC", "#XM", "#VE", "#CP", "?", "?", "?", "?", "?", "?", "#HV", "#VC", "#SX", "?",
];

impl VmCore {
    /// Build a VM from a template.  Snapshot templates are resumed rather
    /// than booted; `start_frozen` creates the VM scaled to zero (no nested
    /// page tables, nothing resident) so the first request thaws it.
    pub fn new(handle: Arc<VmHandle>, template: Arc<Template>, start_frozen: bool) -> Result<VmCore, &'static str> {
        let mem = GuestMemory::new(template.clone()).ok_or("out of memory")?;
        let vmcb_frame = frame::alloc_zeroed().ok_or("out of memory")?;
        let is_linux = template.kind == TemplateKind::Linux;
        let mut core = VmCore {
            handle,
            template: template.clone(),
            vmcb: Vmcb { pa: vmcb_frame },
            regs: GuestRegs::default(),
            fx: Box::new(FxState::new()),
            mem,
            dev: None,
            asid: 0,
            current: None,
            booted: false,
            serial_line: String::new(),
            tsc_offset: 0,
            tsc_aux: 0,
            exit_intinfo: None,
            vintr_window: false,
            left_at: None,
            prof_host: [0; 8],
            prof_count: [0; 8],
            wait_tsc: 0,
            mmio_cache: alloc::collections::BTreeMap::new(),
            eager_pages: 0,
        };
        if is_linux {
            core.dev = Some(DeviceModel::new(time::tsc_per_ms() * 1000, time::now(), core.handle.link()));
        }
        core.init_vmcb();
        if let Some(r) = template.resume.as_ref() {
            core.restore(r);
        }
        if start_frozen {
            core.mem.start_frozen();
        } else if is_linux {
            core.prepare_pages();
        }
        Ok(core)
    }

    /// Before a Linux VM's first VMRUN after a thaw or clone: map the shared
    /// template read-only in one pass and give the VM private copies of the
    /// pages clones of this snapshot are known to write.  Both replace exits
    /// (each one a full round trip through every hypervisor level above us)
    /// with plain host-side work.
    fn prepare_pages(&mut self) {
        let m = super::manager::manager();
        if m.prefault.load(Ordering::Relaxed) {
            self.mem.prefault();
        }
        if m.eager_cow.load(Ordering::Relaxed) && self.template.is_snapshot() {
            let learned = self.template.learned_pages();
            if !learned.is_empty() {
                let n = self.mem.eager_cow(&learned);
                self.eager_pages += n as u64;
                self.handle.update_stats(|s| s.eager_pages += n as u64);
            }
        }
    }

    /// Continue from a snapshot: overwrite the freshly initialised VMCB and
    /// registers with the captured ones, keeping this VM's own control
    /// fields (nested page tables, ASID, permission maps, TSC offset).
    fn restore(&mut self, r: &ResumeState) {
        let h = super::host();
        let v = self.vmcb;
        crate::mm::frame_slice(v.pa).copy_from_slice(&r.vmcb[..]);
        v.write64(vmcb::N_CR3, self.mem.npt_root());
        v.write64(vmcb::IOPM_BASE_PA, h.iopm);
        v.write64(vmcb::MSRPM_BASE_PA, h.msrpm);
        v.write32(vmcb::GUEST_ASID, 1);
        v.write8(vmcb::TLB_CONTROL, 1);
        v.write32(vmcb::CLEAN_BITS, 0);
        v.write64(vmcb::EVENTINJ, 0);
        // Keep the guest's TPR, drop any pending virtual-interrupt request.
        let vintr = v.read64(vmcb::VINTR);
        v.write64(vmcb::VINTR, (vintr & 0xF) | (1 << 24));
        v.write64(vmcb::INTERRUPT_SHADOW, 0);
        v.write64(vmcb::TSC_OFFSET, 0);
        let i3 = v.read32(vmcb::INTERCEPT_VEC3);
        v.write32(vmcb::INTERCEPT_VEC3, i3 & !intercept3::VINTR);
        self.regs = r.regs;
        *self.fx = r.fx.as_ref().clone();
        self.tsc_aux = r.tsc_aux;
        let mut dev = r.dev.clone();
        if let Some(n) = dev.vnet.as_mut() {
            if let Some(link) = self.handle.link() {
                n.set_link(link);
            }
        }
        dev.reset_request = None;
        // Statistics start fresh for the clone.
        dev.io_class = [0; 4];
        dev.mmio_class = [0; 3];
        dev.inj = [0; 4];
        dev.io_count = 0;
        self.dev = Some(dev);
        // The guest's clock continues from the snapshot.
        self.tsc_offset = (r.guest_tsc as i64).wrapping_sub(time::now() as i64);
        self.booted = r.booted;
        self.exit_intinfo = None;
        self.vintr_window = false;
        self.left_at = None;
        self.mmio_cache.clear();
    }

    /// Capture this VM's memory and CPU/device state into a new template.
    /// The VM must be halted (Idle or Frozen); it continues afterwards on top
    /// of the snapshot, sharing every page with it copy-on-write.
    pub async fn snapshot(&mut self, name: &str) -> Result<Arc<Template>, String> {
        if !self.is_linux() {
            return Err(String::from("only linux vms can be snapshotted"));
        }
        let t0 = time::now();
        if self.mem.is_frozen() && !self.thaw().await {
            return Err(String::from("out of memory while thawing"));
        }
        let loaded = self.mem.load_all(false).await.map_err(String::from)?;
        let parent = self.template.clone();
        let mut t = Template::new_empty(TemplateKind::Linux, name, parent.mem_size, parent.boot);
        t.text_pages = parent.text_pages;
        t.image_bytes = parent.image_bytes;
        t.origin = Some(self.handle.name.clone());
        // Clones of this snapshot run the same kernel image the VM booted.
        t.image = parent.image.clone();
        let (pages, moved) = self.mem.snapshot_pages().map_err(String::from)?;
        t.pages = pages;
        super::image::add_template_frames(moved);
        let mut vmcb_copy = Box::new([0u8; 4096]);
        vmcb_copy.copy_from_slice(crate::mm::frame_slice(self.vmcb.pa));
        t.resume = Some(ResumeState {
            vmcb: vmcb_copy,
            regs: self.regs,
            fx: Box::new(self.fx.as_ref().clone()),
            tsc_aux: self.tsc_aux,
            guest_tsc: self.guest_now(),
            dev: self.dev.clone().ok_or("no device model")?,
            booted: self.booted,
        });
        let t = t.finish();
        self.template = t.clone();
        self.mem.rebase(t.clone()).map_err(String::from)?;
        self.vmcb.write64(vmcb::N_CR3, self.mem.npt_root());
        self.sync_stats();
        let us = time::tsc_to_us(time::now() - t0);
        log!(
            "vm {} {}: snapshot '{}' in {} ms: {} pages ({}); {} moved from the vm ({} loaded from disk first), {} shared with its parent template",
            self.handle.id,
            self.handle.name,
            name,
            us / 1000,
            t.pages.len(),
            crate::mm::Bytes(t.bytes()),
            moved,
            loaded,
            t.pages.len() - moved
        );
        Ok(t)
    }

    fn is_linux(&self) -> bool {
        self.dev.is_some()
    }

    fn init_vmcb(&mut self) {
        let h = super::host();
        let v = self.vmcb;
        let t = self.template.clone();
        let b = &t.boot;
        // --- control area ---
        // Unikernels: every guest exception is fatal and reported.  Linux
        // handles its own exceptions (page faults are normal).
        v.write32(vmcb::INTERCEPT_EXCEPTIONS, if self.is_linux() { 0 } else { 0xFFFF_FFFF });
        v.write32(
            vmcb::INTERCEPT_VEC3,
            intercept3::INTR
                | intercept3::NMI
                | intercept3::SMI
                | intercept3::INIT
                | intercept3::CPUID
                | intercept3::INVD
                | intercept3::HLT
                | intercept3::IOIO_PROT
                | intercept3::MSR_PROT
                | intercept3::SHUTDOWN
                | intercept3::RDPMC
                | intercept3::TASK_SWITCH
                | intercept3::FERR_FREEZE,
        );
        v.write32(
            vmcb::INTERCEPT_VEC4,
            intercept4::VMRUN
                | intercept4::VMMCALL
                | intercept4::VMLOAD
                | intercept4::VMSAVE
                | intercept4::STGI
                | intercept4::CLGI
                | intercept4::SKINIT
                | intercept4::ICEBP
                | intercept4::WBINVD
                | intercept4::MONITOR
                | intercept4::MWAIT
                | intercept4::MWAIT_COND
                | intercept4::XSETBV
                | intercept4::RDPRU,
        );
        v.write64(vmcb::IOPM_BASE_PA, h.iopm);
        v.write64(vmcb::MSRPM_BASE_PA, h.msrpm);
        v.write64(vmcb::TSC_OFFSET, 0);
        v.write32(vmcb::GUEST_ASID, 1);
        v.write8(vmcb::TLB_CONTROL, 1);
        v.write64(vmcb::VINTR, 1 << 24); // V_INTR_MASKING
        v.write64(vmcb::INTERRUPT_SHADOW, 0);
        v.write64(vmcb::EVENTINJ, 0);
        v.write64(vmcb::NP_ENABLE, 1);
        v.write64(vmcb::N_CR3, self.mem.npt_root());

        // --- guest state from the template's boot block ---
        v.set_segment(vmcb::CS, b.cs, seg_attr::CODE64, 0xFFFF_FFFF, 0);
        for seg in [vmcb::ES, vmcb::SS, vmcb::DS, vmcb::FS, vmcb::GS] {
            v.set_segment(seg, b.ds, seg_attr::DATA, 0xFFFF_FFFF, 0);
        }
        v.set_segment(vmcb::GDTR, 0, 0, b.gdtr_limit, b.gdtr_base);
        v.set_segment(vmcb::IDTR, 0, 0, b.idtr_limit, b.idtr_base);
        v.set_segment(vmcb::LDTR, 0, seg_attr::UNUSABLE, 0, 0);
        v.set_segment(vmcb::TR, b.tr, b.tr_attr, b.tr_limit, 0);
        v.write8(vmcb::CPL, 0);
        v.write64(vmcb::EFER, b.efer | efer::SVME);
        v.write64(vmcb::CR0, b.cr0);
        v.write64(vmcb::CR3, b.cr3);
        v.write64(vmcb::CR4, b.cr4);
        v.write64(vmcb::CR2, 0);
        v.write64(vmcb::DR6, 0xFFFF_0FF0);
        v.write64(vmcb::DR7, 0x400);
        v.write64(vmcb::RFLAGS, 0x2);
        v.write64(vmcb::RIP, b.rip);
        v.write64(vmcb::RSP, b.rsp);
        v.write64(vmcb::RAX, 0);
        v.write64(vmcb::G_PAT, 0x0007_0406_0007_0406);
        for off in [vmcb::STAR, vmcb::LSTAR, vmcb::CSTAR, vmcb::SFMASK, vmcb::KERNEL_GS_BASE, vmcb::SYSENTER_CS, vmcb::SYSENTER_ESP, vmcb::SYSENTER_EIP] {
            v.write64(off, 0);
        }

        self.regs = GuestRegs::default();
        self.regs.rdi = match self.handle.kind {
            VmKind::Unikernel(k) => k,
            VmKind::Linux => b.rdi,
        };
        self.regs.rsi = b.rsi;
        self.regs.rdx = b.rdx;
        self.regs.rbp = if self.is_linux() { b.rsp } else { 0 };
    }

    fn rip(&self) -> u64 {
        self.vmcb.rip()
    }

    /// Advance RIP past the intercepted instruction.
    fn advance(&self, len: u64) {
        if super::host().features.nrip_save {
            let n = self.vmcb.nrip();
            if n != 0 {
                self.vmcb.set_rip(n);
                return;
            }
        }
        self.vmcb.set_rip(self.vmcb.rip() + len);
    }

    /// Guest-visible TSC value right now.
    fn guest_now(&self) -> u64 {
        (time::now() as i64).wrapping_add(self.tsc_offset) as u64
    }

    fn host_tsc_for(&self, guest_tsc: u64) -> u64 {
        (guest_tsc as i64).wrapping_sub(self.tsc_offset) as u64
    }

    fn sync_stats(&self) {
        let mem = &self.mem;
        let mut host_us = [0u64; 8];
        for (i, t) in self.prof_host.iter().enumerate() {
            host_us[i] = time::tsc_to_us(*t);
        }
        let counts = self.prof_count;
        let wait_us = time::tsc_to_us(self.wait_tsc);
        let (io_class, mmio_class, inj) = match &self.dev {
            Some(d) => (d.io_class, d.mmio_class, d.inj),
            None => ([0; 4], [0; 3], [0; 4]),
        };
        let (to_guest, from_guest) = match self.handle.link() {
            Some(l) => (l.sent_to_guest.load(Ordering::Relaxed), l.received_from_guest.load(Ordering::Relaxed)),
            None => (0, 0),
        };
        self.handle.update_stats(|s| {
            s.exit_host_us = host_us;
            s.exit_count = counts;
            s.wait_us = wait_us;
            s.npf_zero = mem.zero_allocs;
            s.npf_ro = mem.ro_maps;
            s.npf_dirty = mem.dirty_faults;
            s.io_class = io_class;
            s.mmio_class = mmio_class;
            s.inj = inj;
            s.frames_to_guest = to_guest;
            s.frames_from_guest = from_guest;
            s.resident_pages = mem.resident;
            s.swapped_pages = mem.swapped;
            s.npt_pages = mem.npt_frames();
            s.cow = mem.cow_copies;
            s.pages_written = mem.pages_written;
            s.pages_loaded = mem.pages_loaded;
            s.private_pages = mem.overlay_len();
            s.text_private_pages = match mem.template.text_pages {
                Some((a, b)) => mem.overlay_in(a, b),
                None => 0,
            };
        });
    }

    // ----------------------------------------------------- interrupt window --

    fn open_window(&mut self) {
        if self.vintr_window {
            return;
        }
        let ctl = self.vmcb.read32(vmcb::VINTR);
        self.vmcb.write32(vmcb::VINTR, ctl | (1 << 8) | (0xF << 16));
        self.vmcb.write8(vmcb::VINTR + 4, 0);
        let i3 = self.vmcb.read32(vmcb::INTERCEPT_VEC3);
        self.vmcb.write32(vmcb::INTERCEPT_VEC3, i3 | intercept3::VINTR);
        self.vintr_window = true;
    }

    fn close_window(&mut self) {
        if !self.vintr_window {
            return;
        }
        let ctl = self.vmcb.read32(vmcb::VINTR);
        self.vmcb.write32(vmcb::VINTR, ctl & !((1 << 8) | (0xF << 16) | (1 << 20)));
        let i3 = self.vmcb.read32(vmcb::INTERCEPT_VEC3);
        self.vmcb.write32(vmcb::INTERCEPT_VEC3, i3 & !intercept3::VINTR);
        self.vintr_window = false;
    }

    /// Feed console input to the UART and collect its output.
    fn service_console(&mut self) {
        let dev = match self.dev.as_mut() {
            Some(d) => d,
            None => return,
        };
        let input = self.handle.take_console_input();
        for b in input {
            dev.push_serial_input(b);
        }
        let out = dev.take_serial_output();
        if !out.is_empty() {
            if !self.booted {
                self.booted = true;
                let created = self.handle.stats().created_tsc;
                let us = time::tsc_to_us(time::now() - created);
                self.handle.update_stats(|s| s.boot_us = us);
            }
            self.handle.console_output(&out);
            self.handle.touch_from(0);
        }
    }

    /// Decide what to inject on the next VMRUN (Linux only).
    fn prepare_injection(&mut self, gnow: u64) {
        self.vmcb.write32(vmcb::EVENTINJ, 0);
        self.vmcb.write32(vmcb::EVENTINJ + 4, 0);
        if let Some((info, err)) = self.exit_intinfo.take() {
            self.vmcb.write32(vmcb::EVENTINJ, info);
            self.vmcb.write32(vmcb::EVENTINJ + 4, err);
            return;
        }
        let pending = match self.dev.as_mut() {
            Some(d) => {
                d.poll(gnow);
                d.pending()
            }
            None => None,
        };
        let p = match pending {
            Some(p) => p,
            None => {
                self.close_window();
                return;
            }
        };
        let if_set = self.vmcb.rflags() & 0x200 != 0;
        let shadow = self.vmcb.read64(vmcb::INTERRUPT_SHADOW) & 1 != 0;
        if if_set && !shadow {
            if let Some(d) = self.dev.as_mut() {
                d.ack(p);
            }
            self.vmcb.write32(vmcb::EVENTINJ, 0x8000_0000 | p.vector() as u32);
            self.handle.update_stats(|s| s.injected += 1);
            self.close_window();
        } else {
            self.open_window();
        }
    }

    /// Run the guest until it blocks, yields, exits, crashes, or uses up its
    /// time slice.
    pub async fn run_slice(&mut self) -> Action {
        let h = super::host();
        let start = time::now();
        if let Some(t) = self.left_at.take() {
            self.wait_tsc += start.saturating_sub(t);
            if self.is_linux() {
                // Descheduled while runnable: that time never happened for the guest.
                self.tsc_offset -= start.saturating_sub(t) as i64;
            }
        }
        let (asid, stolen) = h.acquire_asid(self.handle.id, self.asid);
        self.asid = asid;
        self.vmcb.write32(vmcb::GUEST_ASID, asid);
        if stolen {
            self.mem.needs_flush = true;
        }
        self.vmcb.write64(vmcb::N_CR3, self.mem.npt_root());

        let mut exits_this_slice = 0u64;
        let mut guest_tsc = 0u64;
        let mut exit_start = time::now();
        loop {
            // Re-acquire the ASID if another VM took it while we awaited I/O.
            let (asid, stolen) = h.acquire_asid(self.handle.id, self.asid);
            if stolen || asid != self.asid {
                self.asid = asid;
                self.vmcb.write32(vmcb::GUEST_ASID, asid);
                self.mem.needs_flush = true;
            }
            if self.mem.needs_flush {
                self.vmcb.write8(vmcb::TLB_CONTROL, 1);
                self.mem.needs_flush = false;
            }
            if self.is_linux() {
                self.service_console();
                if self.handle.take_extra_work() {
                    if let Err(e) = self.deliver_net().await {
                        return Action::Crash(format!("virtio-net receive: {}", e));
                    }
                }
                // Hide the time we spent handling the last exit.
                let now = time::now();
                if exits_this_slice > 0 {
                    self.tsc_offset -= (now - exit_start) as i64;
                }
                let gnow = (now as i64).wrapping_add(self.tsc_offset) as u64;
                self.prepare_injection(gnow);
                self.vmcb.write64(vmcb::TSC_OFFSET, self.tsc_offset as u64);
            }
            svm::fxrstor(&self.fx);
            let t0 = time::now();
            svm::run(self.vmcb.pa, h.host_vmcb, &mut self.regs);
            let t1 = time::now();
            exit_start = t1;
            svm::fxsave(&mut self.fx);
            self.vmcb.write8(vmcb::TLB_CONTROL, 0);
            exits_this_slice += 1;
            guest_tsc += t1 - t0;

            if self.is_linux() {
                let ii = self.vmcb.read32(vmcb::EXITINTINFO);
                if ii & (1 << 31) != 0 {
                    self.exit_intinfo = Some((ii, self.vmcb.read32(vmcb::EXITINTINFO + 4)));
                }
                if self.handle.trace.load(Ordering::Relaxed) {
                    let code = self.vmcb.exit_code();
                    log!(
                        "vm {} exit {:#x} {} rip={:#x} info1={:#x} info2={:#x} rax={:#x}",
                        self.handle.id,
                        code,
                        exit::name(code),
                        self.rip(),
                        self.vmcb.exit_info1(),
                        self.vmcb.exit_info2(),
                        self.vmcb.rax()
                    );
                }
            }

            let action = self.handle_exit().await;
            if self.is_linux() {
                self.service_console();
            }
            match action {
                Action::Continue => {
                    if time::now() - start > time::us_to_tsc(QUANTUM_US) {
                        self.finish_slice(exits_this_slice, guest_tsc);
                        self.account_exit(exit_start, true);
                        return Action::Yield;
                    }
                }
                other => {
                    let runnable = matches!(other, Action::Yield | Action::Continue);
                    self.finish_slice(exits_this_slice, guest_tsc);
                    self.account_exit(exit_start, runnable);
                    return other;
                }
            }
        }
    }

    /// Hide the tail of an exit when leaving the run loop; if the guest is
    /// still runnable, also hide the time until it is scheduled again.
    fn account_exit(&mut self, exit_start: u64, runnable: bool) {
        if self.is_linux() {
            let now = time::now();
            self.tsc_offset -= now.saturating_sub(exit_start) as i64;
            self.left_at = if runnable { Some(now) } else { None };
        }
    }

    fn finish_slice(&self, exits: u64, guest_tsc: u64) {
        self.handle.update_stats(|s| {
            s.runs += 1;
            s.exits += exits;
            s.guest_tsc += guest_tsc;
        });
        self.sync_stats();
    }

    /// Dispatch an exit and attribute the host time it took to its class.
    async fn handle_exit(&mut self) -> Action {
        let t0 = time::now();
        let code = self.vmcb.exit_code();
        let class = match code {
            exit::NPF => {
                let gpa = self.vmcb.exit_info2();
                if self.dev.as_ref().map(|d| d.is_mmio(gpa)).unwrap_or(false) {
                    1
                } else {
                    0
                }
            }
            exit::IOIO => 2,
            exit::MSR => 3,
            exit::CPUID => 4,
            exit::HLT => 5,
            exit::INTR | exit::VINTR => 6,
            _ => 7,
        };
        let a = self.handle_exit_inner().await;
        self.prof_count[class] += 1;
        self.prof_host[class] += time::now().wrapping_sub(t0);
        a
    }

    async fn handle_exit_inner(&mut self) -> Action {
        let code = self.vmcb.exit_code();
        let info1 = self.vmcb.exit_info1();
        let info2 = self.vmcb.exit_info2();
        match code {
            exit::INTR => {
                self.handle.update_stats(|s| s.intr += 1);
                Action::Yield
            }
            exit::NMI | exit::SMI | exit::INIT => Action::Continue,
            exit::VINTR => {
                // The guest opened its interrupt window: inject on re-entry.
                self.close_window();
                Action::Continue
            }
            exit::VMMCALL => {
                if self.is_linux() {
                    // Linux guests get a tiny hypercall set; everything else
                    // answers -1 so probing code fails gracefully.
                    let n = self.vmcb.rax();
                    self.advance(3);
                    let r = match n {
                        hc::GET_VM_ID => self.handle.id as u64,
                        hc::UPTIME_US => time::uptime_us(),
                        _ => u64::MAX,
                    };
                    self.vmcb.set_rax(r);
                    return Action::Continue;
                }
                self.hypercall().await
            }
            exit::CPUID => {
                if self.is_linux() {
                    self.cpuid_linux();
                } else {
                    self.cpuid();
                }
                self.advance(2);
                Action::Continue
            }
            exit::HLT => {
                self.advance(1);
                if !self.is_linux() {
                    self.flush_serial();
                    return Action::Exit(0);
                }
                self.handle.update_stats(|s| s.halts += 1);
                if self.vmcb.rflags() & 0x200 == 0 {
                    // stop_this_cpu(): halted with interrupts disabled.
                    self.handle.push_log(String::from("(guest halted with interrupts disabled)"));
                    return Action::Exit(0);
                }
                let gnow = self.guest_now();
                let dev = self.dev.as_mut().unwrap();
                dev.poll(gnow);
                if dev.pending().is_some() || self.handle.has_console_input() {
                    return Action::Continue;
                }
                let deadline = dev.next_deadline(gnow).map(|d| self.host_tsc_for(d));
                Action::WaitEvent(deadline)
            }
            exit::IOIO => {
                let r = self.io(info1);
                self.vmcb.set_rip(info2);
                r
            }
            exit::MSR => {
                self.msr(info1);
                self.advance(2);
                Action::Continue
            }
            exit::NPF => {
                let write = info1 & 2 != 0;
                self.handle.update_stats(|s| s.npf += 1);
                if self.dev.as_ref().map(|d| d.is_mmio(info2)).unwrap_or(false) {
                    return self.mmio(info2, write).await;
                }
                match self.mem.handle_npf(info2, write).await {
                    Ok(()) => Action::Continue,
                    Err(e) => Action::Crash(format!(
                        "nested page fault: {} of gpa {:#x} at rip {:#x}: {}",
                        if write { "write" } else { "read" },
                        info2,
                        self.rip(),
                        e
                    )),
                }
            }
            exit::EXCP_BASE..=0x5F => {
                let vec = (code - exit::EXCP_BASE) as usize;
                let mut msg = format!(
                    "guest exception {} (vector {}) at rip {:#x} rsp {:#x}",
                    EXCEPTION_NAMES[vec],
                    vec,
                    self.rip(),
                    self.vmcb.rsp()
                );
                if vec == 14 {
                    msg.push_str(&format!(" error={:#x} address={:#x}", info1, info2));
                } else if matches!(vec, 8 | 10..=13 | 17 | 21 | 29 | 30) {
                    msg.push_str(&format!(" error={:#x}", info1));
                }
                Action::Crash(msg)
            }
            exit::SHUTDOWN => {
                if self.is_linux() {
                    Action::Reset("triple fault")
                } else {
                    Action::Crash(format!("shutdown (triple fault) at rip {:#x}", self.rip()))
                }
            }
            exit::INVALID => Action::Crash(format!(
                "invalid guest state: rip {:#x} cr0 {:#x} cr3 {:#x} cr4 {:#x} efer {:#x}",
                self.rip(),
                self.vmcb.read64(vmcb::CR0),
                self.vmcb.read64(vmcb::CR3),
                self.vmcb.read64(vmcb::CR4),
                self.vmcb.read64(vmcb::EFER)
            )),
            exit::VMRUN | exit::VMLOAD | exit::VMSAVE | exit::STGI | exit::CLGI | exit::SKINIT => {
                Action::Crash(format!("guest executed privileged SVM instruction ({}) at rip {:#x}", exit::name(code), self.rip()))
            }
            exit::INVD | exit::WBINVD | exit::RDPMC => {
                if code == exit::RDPMC {
                    self.vmcb.set_rax(0);
                    self.regs.rdx = 0;
                }
                self.advance(2);
                Action::Continue
            }
            exit::MONITOR | exit::MWAIT | exit::XSETBV | exit::RDTSCP => {
                if code == exit::RDTSCP {
                    let t = self.guest_now();
                    self.vmcb.set_rax(t & 0xFFFF_FFFF);
                    self.regs.rdx = t >> 32;
                    self.regs.rcx = self.tsc_aux;
                }
                self.advance(3);
                Action::Continue
            }
            exit::PAUSE => {
                self.advance(2);
                Action::Yield
            }
            exit::ICEBP => {
                self.advance(1);
                Action::Continue
            }
            _ => Action::Crash(format!(
                "unhandled #VMEXIT {:#x} ({}) at rip {:#x} info1={:#x} info2={:#x}",
                code,
                exit::name(code),
                self.rip(),
                info1,
                info2
            )),
        }
    }

    // ------------------------------------------------------------- MMIO ---

    async fn mmio(&mut self, gpa: u64, write: bool) -> Action {
        let rip = self.rip();
        // The handful of kernel instructions that touch device registers
        // repeat endlessly; decoding one means walking the guest's page
        // tables, so remember the result (kernel text does not move).
        let insn = match self.mmio_cache.get(&rip) {
            Some(i) => *i,
            None => {
                let cr3 = self.vmcb.read64(vmcb::CR3);
                let (bytes, n) = match x86::fetch(&mut self.mem, cr3, rip).await {
                    Ok(v) => v,
                    Err(e) => return Action::Crash(format!("mmio: cannot fetch instruction at rip {:#x}: {}", rip, e)),
                };
                let insn = match x86::decode_mov(&bytes[..n]) {
                    Ok(i) => i,
                    Err(e) => {
                        return Action::Crash(format!(
                            "mmio: cannot emulate instruction at rip {:#x} ({:02x?}) for gpa {:#x}: {}",
                            rip,
                            &bytes[..n.min(10)],
                            gpa,
                            e
                        ))
                    }
                };
                if rip >= 0xFFFF_8000_0000_0000 {
                    if self.mmio_cache.len() >= 256 {
                        self.mmio_cache.clear();
                    }
                    self.mmio_cache.insert(rip, insn);
                }
                insn
            }
        };
        let mut rax = self.vmcb.rax();
        let mut rsp = self.vmcb.rsp();
        let gnow = self.guest_now();
        let dev = self.dev.as_mut().unwrap();
        match insn.access {
            x86::Access::Store { .. } | x86::Access::StoreImm { .. } => {
                if !write {
                    // Fault said read but instruction stores: trust the instruction.
                }
                let v = x86::store_value(&insn, &self.regs, rax, rsp);
                dev.mmio_write(gpa, insn.size, v, gnow);
            }
            x86::Access::Load { reg } => {
                let v = dev.mmio_read(gpa, insn.size, gnow);
                x86::set_gpr(reg, v, insn.size, &mut self.regs, &mut rax, &mut rsp);
            }
            x86::Access::LoadZx { reg, from_size } => {
                let v = dev.mmio_read(gpa, from_size, gnow);
                x86::set_gpr(reg, v, 8, &mut self.regs, &mut rax, &mut rsp);
            }
        }
        self.vmcb.set_rax(rax);
        self.vmcb.write64(vmcb::RSP, rsp);
        self.vmcb.set_rip(rip + insn.len as u64);
        self.handle.update_stats(|s| s.mmio += 1);
        if self.dev.as_ref().map(|d| d.vnet_kicked()).unwrap_or(false) {
            if let Err(e) = self.service_net().await {
                return Action::Crash(format!("virtio-net queue: {}", e));
            }
        }
        Action::Continue
    }

    /// Deliver frames queued on the link into the guest's receive queue.
    async fn deliver_net(&mut self) -> Result<(), &'static str> {
        if let Some(d) = self.dev.as_mut() {
            if let Some(v) = d.vnet.as_mut() {
                let r = v.deliver_rx(&mut self.mem).await;
                d.sync_vnet_irq();
                r?;
            }
        }
        Ok(())
    }

    /// Handle a virtqueue notification (transmit, or new receive buffers).
    async fn service_net(&mut self) -> Result<(), &'static str> {
        if let Some(d) = self.dev.as_mut() {
            if let Some(v) = d.vnet.as_mut() {
                let r = v.service_kick(&mut self.mem).await;
                d.sync_vnet_irq();
                r?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------- hypercalls ---

    async fn hypercall(&mut self) -> Action {
        let n = self.vmcb.rax();
        let a = self.regs.rdi;
        let b = self.regs.rsi;
        if !self.booted {
            self.booted = true;
            let created = self.handle.stats().created_tsc;
            let us = time::tsc_to_us(time::now() - created);
            self.handle.update_stats(|s| s.boot_us = us);
        }
        self.handle.update_stats(|s| s.hcalls += 1);
        match n {
            hc::LOG => {
                match self.mem.read_string(a, b).await {
                    Ok(s) => {
                        println!("[vm {} {}] {}", self.handle.id, self.handle.name, s);
                        self.handle.push_log(s);
                    }
                    Err(e) => return Action::Crash(format!("bad log buffer {:#x}+{}: {}", a, b, e)),
                }
                self.vmcb.set_rax(0);
                self.advance(3);
                Action::Continue
            }
            hc::WAIT_REQUEST => {
                if let Some(prev) = self.current.take() {
                    if let Some(r) = prev.reply {
                        r.set(Vec::new());
                    }
                }
                match self.handle.pop_request() {
                    Some(req) => {
                        let len = req.data.len().min(b as usize);
                        if let Err(e) = self.mem.write(a, &req.data[..len]).await {
                            if let Some(r) = req.reply {
                                r.set(b"error: vm crashed".to_vec());
                            }
                            return Action::Crash(format!("bad request buffer {:#x}+{}: {}", a, b, e));
                        }
                        let wake_us = time::tsc_to_us(time::now().saturating_sub(req.enqueued));
                        self.handle.update_stats(|s| {
                            s.requests += 1;
                            s.last_wake_us = wake_us;
                            s.wake_us_total += wake_us;
                            s.wake_us_max = s.wake_us_max.max(wake_us);
                            s.wake_samples += 1;
                        });
                        self.handle.touch();
                        self.current = Some(req);
                        self.vmcb.set_rax(len as u64);
                        self.advance(3);
                        Action::Continue
                    }
                    // RIP is not advanced: the guest re-executes VMMCALL when
                    // it is resumed, and finds its request then.
                    None => Action::Block,
                }
            }
            hc::RESPOND => {
                let len = b.min(65536) as usize;
                let mut buf = alloc::vec![0u8; len];
                if let Err(e) = self.mem.read(a, &mut buf).await {
                    return Action::Crash(format!("bad response buffer {:#x}+{}: {}", a, b, e));
                }
                if let Some(req) = self.current.take() {
                    if let Some(r) = req.reply {
                        r.set(buf);
                    }
                }
                self.handle.touch();
                self.vmcb.set_rax(0);
                self.advance(3);
                Action::Continue
            }
            hc::EXIT => {
                self.advance(3);
                self.flush_serial();
                Action::Exit(a)
            }
            hc::UPTIME_US => {
                self.vmcb.set_rax(time::uptime_us());
                self.advance(3);
                Action::Continue
            }
            hc::YIELD => {
                self.vmcb.set_rax(0);
                self.advance(3);
                Action::Yield
            }
            hc::SLEEP_MS => {
                self.vmcb.set_rax(0);
                self.advance(3);
                Action::Sleep(a.min(600_000))
            }
            _ => {
                self.vmcb.set_rax(u64::MAX);
                self.advance(3);
                Action::Continue
            }
        }
    }

    // ------------------------------------------------------------ CPUID ---

    fn cpuid(&mut self) {
        let leaf = self.vmcb.rax() as u32;
        let sub = self.regs.rcx as u32;
        let mut r = cpu::cpuid(leaf, sub);
        match leaf {
            1 => {
                r.ecx |= 1 << 31; // running under a hypervisor
                r.ecx &= !(1 << 21); // no x2APIC
                r.ebx &= 0x0000_FFFF; // APIC id 0, one logical CPU
            }
            0x4000_0000 => {
                r.eax = 0x4000_0001;
                r.ebx = u32::from_le_bytes(*b"conc");
                r.ecx = u32::from_le_bytes(*b"_os ");
                r.edx = u32::from_le_bytes(*b"svm ");
            }
            0x4000_0001 => {
                r.eax = self.handle.id;
                r.ebx = self.handle.kind.service_kind() as u32;
                r.ecx = (self.mem.mem_size() >> 12) as u32;
                r.edx = 0;
            }
            0x8000_0001 => {
                r.ecx &= !(1 << 2); // hide SVM
            }
            0x8000_000A => {
                r = cpu::CpuidResult::default();
            }
            _ => {}
        }
        self.vmcb.set_rax(r.eax as u64);
        self.regs.rbx = r.ebx as u64;
        self.regs.rcx = r.ecx as u64;
        self.regs.rdx = r.edx as u64;
        self.handle.update_stats(|s| s.cpuid += 1);
    }

    /// CPUID as seen by Linux: a plain single-core CPU without the features
    /// whose MSRs and state we do not model.
    fn cpuid_linux(&mut self) {
        let leaf = self.vmcb.rax() as u32;
        let sub = self.regs.rcx as u32;
        let mut r = cpu::cpuid(leaf, sub);
        match leaf {
            0 => {
                r.eax = r.eax.min(0xD);
            }
            1 => {
                // ECX: keep SSE3/PCLMUL/SSSE3/CX16/SSE4.1/SSE4.2/MOVBE/POPCNT/AES/RDRAND.
                r.ecx &= (1 << 0) | (1 << 1) | (1 << 9) | (1 << 13) | (1 << 19) | (1 << 20) | (1 << 22) | (1 << 23) | (1 << 25) | (1 << 30);
                r.ecx |= 1 << 24; // TSC-deadline timer: one MSR write per tick instead of three PIT ports
                r.ecx |= 1 << 31; // hypervisor
                // EDX: FPU VME DE PSE TSC MSR PAE CX8 APIC SEP PGE CMOV PAT PSE36 CLFLUSH MMX FXSR SSE SSE2.
                r.edx &= (1 << 0) | (1 << 1) | (1 << 2) | (1 << 3) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 8) | (1 << 9) | (1 << 11)
                    | (1 << 13) | (1 << 15) | (1 << 16) | (1 << 17) | (1 << 19) | (1 << 23) | (1 << 24) | (1 << 25) | (1 << 26);
                r.edx |= (1 << 4) | (1 << 9); // TSC and APIC are mandatory
                // EBX: APIC id 0, 1 logical processor, CLFLUSH line 8 qwords.
                r.ebx = (r.ebx & 0xFF) | (8 << 8) | (1 << 16);
            }
            2 | 3 | 5 | 9 | 0xA | 0xC | 0xF | 0x10 | 0x12 | 0x14 | 0x15 | 0x16 | 0x17 | 0x18 | 0x19 | 0x1A | 0x1F => {
                r = cpu::CpuidResult::default();
            }
            6 => {
                r = cpu::CpuidResult::default();
                r.eax = 1 << 2; // ARAT
            }
            7 => {
                if sub == 0 {
                    // BMI1 SMEP BMI2 ERMS RDSEED ADX SMAP CLFLUSHOPT SHA.
                    r.ebx &= (1 << 3) | (1 << 7) | (1 << 8) | (1 << 9) | (1 << 18) | (1 << 19) | (1 << 20) | (1 << 23) | (1 << 29);
                    r.ecx = 0;
                    r.edx = 0;
                    r.eax = 0;
                } else {
                    r = cpu::CpuidResult::default();
                }
            }
            0xB | 0xD => {
                r = cpu::CpuidResult::default();
            }
            0x4000_0000 => {
                r.eax = 0x4000_0001;
                r.ebx = u32::from_le_bytes(*b"conc");
                r.ecx = u32::from_le_bytes(*b"_os ");
                r.edx = u32::from_le_bytes(*b"svm ");
            }
            0x4000_0001 => {
                r = cpu::CpuidResult::default();
            }
            0x8000_0000 => {
                r.eax = r.eax.min(0x8000_0008);
            }
            0x8000_0001 => {
                // ECX: LAHF ABM SSE4A MISALIGNSSE 3DNOWPREFETCH.
                r.ecx &= (1 << 0) | (1 << 5) | (1 << 6) | (1 << 7) | (1 << 8);
                // EDX: legacy bits, SYSCALL, NX, PDPE1GB, RDTSCP, LM (no FFXSR, no 3DNow).
                r.edx &= 0x0183_FFFF | (1 << 20) | (1 << 26) | (1 << 27) | (1 << 29);
                r.edx &= !(1 << 12); // MTRR
                r.edx &= !(1 << 7); // MCE
                r.edx &= !(1 << 14); // MCA
            }
            0x8000_0007 => {
                r.eax = 0;
                r.ebx = 0;
                r.ecx = 0;
                r.edx = 1 << 8; // invariant TSC
            }
            0x8000_0008 => {
                r.ebx = 0; // no IBPB/IBRS/STIBP/SSBD/... to manage
                r.ecx = 0; // one core
                r.edx = 0;
            }
            0x8000_0002..=0x8000_0006 => {}
            l if l > 0x8000_0008 => {
                r = cpu::CpuidResult::default();
            }
            _ => {}
        }
        // Never advertise XSAVE/AVX/MONITOR/PCID/x2APIC/TSC-deadline (leaf 1 mask above).
        self.vmcb.set_rax(r.eax as u64);
        self.regs.rbx = r.ebx as u64;
        self.regs.rcx = r.ecx as u64;
        self.regs.rdx = r.edx as u64;
        self.handle.update_stats(|s| s.cpuid += 1);
    }

    // ------------------------------------------------------ port I/O ---

    fn flush_serial(&mut self) {
        if !self.serial_line.is_empty() {
            let line = core::mem::take(&mut self.serial_line);
            println!("[vm {} {} serial] {}", self.handle.id, self.handle.name, line);
            self.handle.push_log(line);
        }
    }

    fn io(&mut self, info1: u64) -> Action {
        let is_in = info1 & 1 != 0;
        let port = (info1 >> 16) as u16;
        let size: u8 = if info1 & 0x10 != 0 {
            1
        } else if info1 & 0x20 != 0 {
            2
        } else {
            4
        };
        self.handle.update_stats(|s| s.io += 1);
        if info1 & 0x4 != 0 {
            return Action::Crash(format!("string I/O (port {:#x}) is not supported, rip {:#x}", port, self.rip()));
        }
        let rax = self.vmcb.rax();
        if let Some(dev) = self.dev.as_mut() {
            let gnow = (time::now() as i64).wrapping_add(self.tsc_offset) as u64;
            if is_in {
                let v = dev.io_read(port, size, gnow) as u64;
                let nv = match size {
                    1 => (rax & !0xFF) | (v & 0xFF),
                    2 => (rax & !0xFFFF) | (v & 0xFFFF),
                    _ => v & 0xFFFF_FFFF,
                };
                self.vmcb.set_rax(nv);
            } else {
                dev.io_write(port, size, rax as u32, gnow);
                if let Some(why) = dev.reset_request.take() {
                    return Action::Reset(why);
                }
            }
            return Action::Continue;
        }
        if is_in {
            // No devices: reads return all ones.
            let v = match size {
                1 => (rax & !0xFF) | 0xFF,
                2 => (rax & !0xFFFF) | 0xFFFF,
                _ => 0xFFFF_FFFF,
            };
            self.vmcb.set_rax(v);
        } else if port == 0x3F8 && size == 1 {
            let ch = rax as u8;
            if ch == b'\n' {
                self.flush_serial();
            } else if ch != b'\r' && self.serial_line.len() < 256 {
                self.serial_line.push(ch as char);
            }
        }
        Action::Continue
    }

    // --------------------------------------------------------------- MSRs ---

    fn msr(&mut self, info1: u64) {
        let msr = self.regs.rcx as u32;
        self.handle.update_stats(|s| s.msr += 1);
        let gnow = (time::now() as i64).wrapping_add(self.tsc_offset) as u64;
        if info1 == 0 {
            let dev_val = self.dev.as_mut().and_then(|d| d.msr_read(msr, gnow));
            let v: u64 = match dev_val {
                Some(v) => v,
                None => match msr {
                    cpu::msr::IA32_EFER => self.vmcb.read64(vmcb::EFER) & !efer::SVME,
                    cpu::msr::IA32_APIC_BASE => 0xFEE0_0900,
                    cpu::msr::IA32_PAT => self.vmcb.read64(vmcb::G_PAT),
                    cpu::msr::IA32_STAR => self.vmcb.read64(vmcb::STAR),
                    cpu::msr::IA32_LSTAR => self.vmcb.read64(vmcb::LSTAR),
                    0xC000_0083 => self.vmcb.read64(vmcb::CSTAR),
                    cpu::msr::IA32_FMASK => self.vmcb.read64(vmcb::SFMASK),
                    cpu::msr::IA32_FS_BASE => self.vmcb.read64(vmcb::FS + 8),
                    cpu::msr::IA32_GS_BASE => self.vmcb.read64(vmcb::GS + 8),
                    cpu::msr::IA32_KERNEL_GS_BASE => self.vmcb.read64(vmcb::KERNEL_GS_BASE),
                    0x174 => self.vmcb.read64(vmcb::SYSENTER_CS),
                    0x175 => self.vmcb.read64(vmcb::SYSENTER_ESP),
                    0x176 => self.vmcb.read64(vmcb::SYSENTER_EIP),
                    0xC000_0103 => self.tsc_aux,
                    0x10 => gnow, // TSC
                    _ => 0,
                },
            };
            self.vmcb.set_rax(v & 0xFFFF_FFFF);
            self.regs.rdx = v >> 32;
        } else {
            let v = (self.vmcb.rax() & 0xFFFF_FFFF) | (self.regs.rdx << 32);
            if let Some(d) = self.dev.as_mut() {
                if d.msr_write(msr, v, gnow) {
                    if msr == cpu::msr::IA32_PAT {
                        self.vmcb.write64(vmcb::G_PAT, v);
                    }
                    return;
                }
            }
            match msr {
                cpu::msr::IA32_EFER => {
                    // Guest may toggle SCE/LME/NXE/FFXSR; SVME stays set.
                    let allowed = v & (efer::SCE | efer::LME | efer::LMA | efer::NXE | (1 << 14));
                    self.vmcb.write64(vmcb::EFER, allowed | efer::SVME);
                }
                cpu::msr::IA32_PAT => self.vmcb.write64(vmcb::G_PAT, v),
                cpu::msr::IA32_STAR => self.vmcb.write64(vmcb::STAR, v),
                cpu::msr::IA32_LSTAR => self.vmcb.write64(vmcb::LSTAR, v),
                0xC000_0083 => self.vmcb.write64(vmcb::CSTAR, v),
                cpu::msr::IA32_FMASK => self.vmcb.write64(vmcb::SFMASK, v),
                cpu::msr::IA32_FS_BASE => self.vmcb.write64(vmcb::FS + 8, v),
                cpu::msr::IA32_GS_BASE => self.vmcb.write64(vmcb::GS + 8, v),
                cpu::msr::IA32_KERNEL_GS_BASE => self.vmcb.write64(vmcb::KERNEL_GS_BASE, v),
                0x174 => self.vmcb.write64(vmcb::SYSENTER_CS, v),
                0x175 => self.vmcb.write64(vmcb::SYSENTER_ESP, v),
                0x176 => self.vmcb.write64(vmcb::SYSENTER_EIP, v),
                0xC000_0103 => self.tsc_aux = v,
                _ => {} // silently ignore other writes
            }
        }
    }

    // ------------------------------------------------ lifecycle helpers ---

    /// Evict memory to disk.  Only meaningful while the VM is blocked.
    pub async fn freeze(&mut self) {
        if self.mem.is_frozen() {
            return;
        }
        // What this clone wrote is what the next clone will write.
        if self.template.is_snapshot() {
            let pages = self.mem.overlay_pages();
            self.template.learn(&pages);
        }
        let t0 = time::now();
        match self.mem.freeze().await {
            Ok(rep) => {
                let us = time::tsc_to_us(time::now() - t0);
                self.handle.update_stats(|s| {
                    s.freezes += 1;
                    s.last_freeze_us = us;
                });
                self.handle.set_state(VmState::Frozen);
                if rep.kept_resident > 0 {
                    log!("vm {} {}: frozen but {} pages kept resident (no page store)", self.handle.id, self.handle.name, rep.kept_resident);
                }
            }
            Err(e) => log!("vm {} {}: freeze failed: {}", self.handle.id, self.handle.name, e),
        }
        self.sync_stats();
    }

    /// Re-create the nested page tables and prefetch what the VM owns on
    /// disk.  Returns false if that failed, in which case the VM must not run.
    pub async fn thaw(&mut self) -> bool {
        if !self.mem.is_frozen() {
            return true;
        }
        let t0 = time::now();
        if let Err(e) = self.mem.thaw() {
            log!("vm {} {}: thaw failed: {}", self.handle.id, self.handle.name, e);
            return false;
        }
        if self.is_linux() {
            if super::manager::manager().prefetch.load(Ordering::Relaxed) {
                // One batch of reads for everything the VM owns on disk,
                // mapped writable straight away: it dirtied them last time.
                if let Err(e) = self.mem.load_all(true).await {
                    log!("vm {} {}: prefetch failed: {}", self.handle.id, self.handle.name, e);
                }
            }
            self.prepare_pages();
        }
        let us = time::tsc_to_us(time::now() - t0);
        self.handle.update_stats(|s| {
            s.thaws += 1;
            s.last_thaw_us = us;
        });
        self.sync_stats();
        true
    }

    /// Reboot from the template.
    fn reset(&mut self, why: &str) -> Result<(), &'static str> {
        log!("vm {} {}: reset ({})", self.handle.id, self.handle.name, why);
        self.handle.push_log(format!("(reset: {})", why));
        self.mem.release();
        self.mem = GuestMemory::new(self.template.clone()).ok_or("out of memory")?;
        crate::mm::zero_frame(self.vmcb.pa);
        *self.fx = FxState::new();
        self.exit_intinfo = None;
        self.vintr_window = false;
        self.tsc_offset = 0;
        if self.dev.is_some() {
            self.dev = Some(DeviceModel::new(time::tsc_per_ms() * 1000, time::now(), self.handle.link()));
        }
        self.init_vmcb();
        let t = self.template.clone();
        if let Some(r) = t.resume.as_ref() {
            self.restore(r);
        }
        self.handle.update_stats(|s| s.resets += 1);
        Ok(())
    }

    fn dump_devices(&self) {
        if let Some(d) = &self.dev {
            let gnow = self.guest_now();
            println!(
                "vm {} {}: rip={:#x} rsp={:#x} rflags={:#x} cr0={:#x} cr3={:#x} cr4={:#x} efer={:#x} tsc_offset={}",
                self.handle.id,
                self.handle.name,
                self.rip(),
                self.vmcb.rsp(),
                self.vmcb.rflags(),
                self.vmcb.read64(vmcb::CR0),
                self.vmcb.read64(vmcb::CR3),
                self.vmcb.read64(vmcb::CR4),
                self.vmcb.read64(vmcb::EFER),
                self.tsc_offset
            );
            println!("  {}", d.debug_summary(gnow));
        } else {
            println!("vm {} {}: rip={:#x} rsp={:#x} rflags={:#x} (no device model)", self.handle.id, self.handle.name, self.rip(), self.vmcb.rsp(), self.vmcb.rflags());
        }
    }

    fn release(&mut self) {
        if let Some(cur) = self.current.take() {
            if let Some(r) = cur.reply {
                r.set(b"error: vm terminated".to_vec());
            }
        }
        self.handle.drain_requests("vm terminated");
        self.mem.release();
        super::host().release_asid(self.handle.id, self.asid);
        frame::free(self.vmcb.pa);
        self.sync_stats();
    }
}

/// Sleep for `ms`, returning early only if a host command arrives.
async fn interruptible_sleep(handle: &VmHandle, ms: u64) {
    let deadline = time::now() + time::us_to_tsc(ms * 1000);
    loop {
        let now = time::now();
        if now >= deadline {
            return;
        }
        let _ = timer::timeout_until(deadline, handle.notify.notified()).await;
        if handle.has_command() {
            return;
        }
    }
}

/// Wait until `deadline` (host TSC), or until the handle is notified.
async fn wait_event(handle: &VmHandle, deadline: Option<u64>) {
    match deadline {
        Some(d) => {
            let _ = timer::timeout_until(d, handle.notify.notified()).await;
        }
        None => handle.notify.notified().await,
    }
}

fn finish(core: &mut VmCore, state: VmState) {
    core.release();
    core.handle.set_state(state);
    core.handle.set_attached(false);
    super::manager::manager().vm_finished(&core.handle);
}

/// The task body that drives one VM for its whole life.
pub async fn vcpu_task(mut core: VmCore) {
    let handle = core.handle.clone();
    // Clones can be born frozen: nothing runs until something arrives.
    handle.set_state(if core.mem.is_frozen() { VmState::Frozen } else { VmState::Running });
    if handle.trace.load(Ordering::Relaxed) {
        log!(
            "vm {} {}: vcpu task started, rip={:#x} cr3={:#x} n_cr3={:#x} efer={:#x}",
            handle.id,
            handle.name,
            core.rip(),
            core.vmcb.read64(vmcb::CR3),
            core.mem.npt_root(),
            core.vmcb.read64(vmcb::EFER)
        );
    }
    loop {
        // Host commands take priority.
        let mut deferred: Vec<Command> = Vec::new();
        while let Some(cmd) = handle.pop_command() {
            match cmd {
                Command::Kill => {
                    finish(&mut core, VmState::Killed);
                    return;
                }
                Command::Snapshot(name) => {
                    // Only a halted guest is at a clean point; otherwise try
                    // again after the next slice.
                    let st = handle.state();
                    if st == VmState::Idle || st == VmState::Frozen {
                        let r = core.snapshot(&name).await;
                        if handle.state() == VmState::Frozen && !core.mem.is_frozen() {
                            handle.set_state(VmState::Idle);
                        }
                        handle.set_snapshot_result(r);
                    } else {
                        deferred.push(Command::Snapshot(name));
                    }
                }
                Command::Freeze => {
                    if matches!(handle.state(), VmState::Idle) {
                        core.freeze().await;
                    }
                }
                Command::Thaw => {
                    if core.thaw().await {
                        if handle.state() == VmState::Frozen {
                            handle.set_state(VmState::Idle);
                        }
                    } else {
                        handle.set_crashed(String::from("out of memory while thawing"));
                        finish(&mut core, VmState::Crashed);
                        return;
                    }
                }
                Command::Reset => {
                    if !core.thaw().await {
                        handle.set_crashed(String::from("out of memory while thawing"));
                        finish(&mut core, VmState::Crashed);
                        return;
                    }
                    if let Err(e) = core.reset("host command") {
                        handle.set_crashed(String::from(e));
                        finish(&mut core, VmState::Crashed);
                        return;
                    }
                    handle.set_state(VmState::Running);
                }
                Command::Dump => core.dump_devices(),
            }
        }
        for c in deferred {
            handle.requeue_command(c);
        }

        let st = handle.state();
        if st == VmState::Frozen {
            if !handle.has_work() {
                // Scaled to zero: nothing to do until input, a request or a command.
                handle.notify.notified().await;
                continue;
            }
            if !core.thaw().await {
                handle.set_crashed(String::from("out of memory while thawing"));
                finish(&mut core, VmState::Crashed);
                return;
            }
            handle.set_state(VmState::Running);
        } else if st == VmState::Idle {
            if !handle.has_work() {
                handle.notify.notified().await;
                continue;
            }
            handle.set_state(VmState::Running);
        }

        match core.run_slice().await {
            Action::Continue | Action::Yield => task::yield_now().await,
            Action::Block => {
                handle.set_state(VmState::Idle);
                handle.touch();
            }
            Action::WaitEvent(deadline) => {
                // Halted Linux guest: idle until the next device deadline or
                // external input.  Timer wakeups do not count as activity, so
                // a quiet guest can be frozen by the idle policy.
                handle.set_state(VmState::Idle);
                if !handle.has_work() {
                    wait_event(&handle, deadline).await;
                }
                if handle.state() == VmState::Idle && !handle.has_command() {
                    handle.set_state(VmState::Running);
                }
            }
            Action::Sleep(ms) => {
                handle.set_state(VmState::Sleeping);
                interruptible_sleep(&handle, ms).await;
                if handle.state() == VmState::Sleeping {
                    handle.set_state(VmState::Running);
                }
            }
            Action::Reset(why) => {
                if handle.stats().resets >= 3 {
                    let reason = format!("reset loop ({}); rip {:#x} rsp {:#x} cr3 {:#x}", why, core.rip(), core.vmcb.rsp(), core.vmcb.read64(vmcb::CR3));
                    println!("[vm {} {}] CRASHED: {}", handle.id, handle.name, reason);
                    core.release();
                    handle.set_crashed(reason);
                    handle.set_attached(false);
                    super::manager::manager().vm_finished(&handle);
                    return;
                }
                if let Err(e) = core.reset(why) {
                    handle.set_crashed(String::from(e));
                    finish(&mut core, VmState::Crashed);
                    return;
                }
                task::yield_now().await;
            }
            Action::Exit(code) => {
                finish(&mut core, VmState::Exited(code));
                return;
            }
            Action::Crash(reason) => {
                println!("[vm {} {}] CRASHED: {}", handle.id, handle.name, reason);
                core.release();
                handle.set_crashed(reason);
                handle.set_attached(false);
                super::manager::manager().vm_finished(&handle);
                return;
            }
        }
    }
}
