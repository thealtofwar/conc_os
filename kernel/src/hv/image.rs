//! Guest images and templates.
//!
//! A *template* is the fully prepared initial memory of a VM as a sparse set
//! of frames, plus the register state the guest starts with.  Every VM
//! created from a template shares those frames read-only and copies a page
//! only when it first writes to it, so a thousand idle VMs cost a thousand
//! VMCBs, not a thousand copies of the image.
//!
//! Two kinds of template exist: the bundled unikernel (`Template::build`) and
//! Linux kernels loaded from the disk image area (`hv::linux_boot`).

#![allow(dead_code)]

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use super::devices::DeviceModel;
use super::svm::seg_attr;
use super::svm::{FxState, GuestRegs};
use crate::arch::cpu::{cr0, cr4, efer};
use crate::mm::{frame, frame_slice, PAGE_SIZE};

/// The guest program, built by `cargo xtask` from `guest/`.
pub static GUEST_ELF: &[u8] = include_bytes!(env!("GUEST_ELF"));

pub const GUEST_PML4: u64 = 0x1000;
pub const GUEST_PDPT: u64 = 0x2000;
pub const GUEST_PD: u64 = 0x3000;
pub const GUEST_GDT: u64 = 0x4000;
pub const GUEST_IDT: u64 = 0x5000;
pub const GUEST_PARAMS: u64 = 0x6000;
pub const GUEST_IMAGE_BASE: u64 = 0x10000;
pub const MIN_MEM: u64 = 256 * 1024;
pub const MAX_MEM: u64 = 1 << 30;
pub const DEFAULT_MEM: u64 = 2 * 1024 * 1024;

pub const GDT_CODE: u16 = 0x08;
pub const GDT_DATA: u16 = 0x10;
pub const GDT_TSS: u16 = 0x18;

static TEMPLATE_FRAMES: AtomicUsize = AtomicUsize::new(0);

pub fn template_frames() -> usize {
    TEMPLATE_FRAMES.load(Ordering::Relaxed)
}

pub fn add_template_frames(n: usize) {
    TEMPLATE_FRAMES.fetch_add(n, Ordering::Relaxed);
}

/// Everything a VM needs to continue from the moment a snapshot was taken
/// instead of booting: the VMCB, registers, FPU state, the guest's TSC and
/// the whole device model.
pub struct ResumeState {
    /// Raw copy of the VMCB page (per-VM control fields are patched on use).
    pub vmcb: Box<[u8; 4096]>,
    pub regs: GuestRegs,
    pub fx: Box<FxState>,
    pub tsc_aux: u64,
    /// Guest-visible TSC at the snapshot; clones continue from here.
    pub guest_tsc: u64,
    pub dev: DeviceModel,
    pub booted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TemplateKind {
    /// The bundled request/response unikernel.
    Unikernel,
    /// A Linux kernel with a device model.
    Linux,
}

/// Register state a guest starts with.
#[derive(Clone, Copy, Debug)]
pub struct BootState {
    pub rip: u64,
    pub rsp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub cr0: u64,
    pub cr3: u64,
    pub cr4: u64,
    /// EFER without SVME (the vCPU adds it).
    pub efer: u64,
    pub gdtr_base: u64,
    pub gdtr_limit: u32,
    pub idtr_base: u64,
    pub idtr_limit: u32,
    pub cs: u16,
    pub ds: u16,
    pub tr: u16,
    pub tr_attr: u16,
    pub tr_limit: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Segment {
    pub vaddr: u64,
    pub offset: u64,
    pub filesz: u64,
    pub memsz: u64,
}

pub struct Elf {
    pub entry: u64,
    pub segments: alloc::vec::Vec<Segment>,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

/// Parse a static ELF64 executable's loadable segments.
pub fn parse_elf(b: &[u8]) -> Result<Elf, &'static str> {
    if b.len() < 64 || &b[0..4] != b"\x7fELF" {
        return Err("not an ELF file");
    }
    if b[4] != 2 || b[5] != 1 {
        return Err("not a little-endian ELF64");
    }
    if u16_at(b, 18) != 0x3E {
        return Err("not an x86_64 ELF");
    }
    let entry = u64_at(b, 24);
    let phoff = u64_at(b, 32) as usize;
    let phentsize = u16_at(b, 54) as usize;
    let phnum = u16_at(b, 56) as usize;
    if phentsize < 56 || phoff + phnum * phentsize > b.len() {
        return Err("bad program headers");
    }
    let mut segments = alloc::vec::Vec::new();
    for i in 0..phnum {
        let p = phoff + i * phentsize;
        if u32_at(b, p) != 1 {
            continue; // PT_LOAD only
        }
        let seg = Segment {
            offset: u64_at(b, p + 8),
            vaddr: u64_at(b, p + 16),
            filesz: u64_at(b, p + 32),
            memsz: u64_at(b, p + 40),
        };
        if seg.offset + seg.filesz > b.len() as u64 {
            return Err("segment outside file");
        }
        segments.push(seg);
    }
    Ok(Elf { entry, segments })
}

/// Prepared initial guest memory plus boot register state.
pub struct Template {
    pub kind: TemplateKind,
    pub name: String,
    pub mem_size: u64,
    pub npages: usize,
    pub boot: BootState,
    /// Guest page index → frame with initial contents.
    pub pages: BTreeMap<u32, u64>,
    pub image_bytes: usize,
    /// Guest pages `[start, end)` holding the kernel's text/rodata, for
    /// sharing statistics.
    pub text_pages: Option<(u32, u32)>,
    /// Present on snapshot templates: VMs resume here instead of booting.
    pub resume: Option<ResumeState>,
    /// Name of the VM this snapshot was taken from.
    pub origin: Option<String>,
    /// Installed image set this template (or the snapshot's ancestor) booted.
    pub image: Option<String>,
    /// Pages that earlier clones wrote after resuming, with how many clone
    /// freezes reported each (capped).  New clones get private copies of the
    /// pages seen at least `LEARNED_MIN_COUNT` times before their first
    /// VMRUN so those writes never fault; pages one clone happened to touch
    /// are not copied for everyone.
    pub learned: crate::sync::SpinLock<alloc::collections::BTreeMap<u32, u32>>,
}

/// Cap on the learned write set (pages).
pub const LEARNED_MAX: usize = 4096;
/// A page must have been written by this many frozen clones to be copied
/// eagerly.
pub const LEARNED_MIN_COUNT: u32 = 2;

impl Template {
    /// An empty template; fill it with `write`/`page_mut`, then `finish`.
    pub fn new_empty(kind: TemplateKind, name: &str, mem_size: u64, boot: BootState) -> Template {
        Template {
            kind,
            name: String::from(name),
            mem_size,
            npages: (mem_size / PAGE_SIZE as u64) as usize,
            boot,
            pages: BTreeMap::new(),
            image_bytes: 0,
            text_pages: None,
            resume: None,
            origin: None,
            image: None,
            learned: crate::sync::SpinLock::new(alloc::collections::BTreeMap::new()),
        }
    }

    pub fn is_snapshot(&self) -> bool {
        self.resume.is_some()
    }

    /// Merge pages a clone wrote into the learned set.
    pub fn learn(&self, pages: &[u32]) {
        let mut s = self.learned.lock();
        for &p in pages {
            match s.get_mut(&p) {
                Some(c) => *c = c.saturating_add(1),
                None => {
                    if s.len() >= LEARNED_MAX {
                        continue;
                    }
                    s.insert(p, 1);
                }
            }
        }
    }

    /// Pages worth copying ahead of time.
    pub fn learned_pages(&self) -> alloc::vec::Vec<u32> {
        self.learned.lock().iter().filter(|(_, &c)| c >= LEARNED_MIN_COUNT).map(|(&p, _)| p).collect()
    }

    /// Bytes of host memory the template's frames occupy.
    pub fn bytes(&self) -> u64 {
        (self.pages.len() * PAGE_SIZE) as u64
    }

    /// Frame for guest page `idx`, allocating a zeroed one on first use.
    pub fn page_mut(&mut self, idx: u32) -> Result<u64, &'static str> {
        if idx as usize >= self.npages {
            return Err("template page outside guest memory");
        }
        if let Some(&f) = self.pages.get(&idx) {
            return Ok(f);
        }
        let f = frame::alloc_zeroed().ok_or("out of memory building template")?;
        self.pages.insert(idx, f);
        TEMPLATE_FRAMES.fetch_add(1, Ordering::Relaxed);
        Ok(f)
    }

    /// Copy `data` into guest memory at `gpa`.
    pub fn write(&mut self, gpa: u64, data: &[u8]) -> Result<(), &'static str> {
        if gpa + data.len() as u64 > self.mem_size {
            return Err("template write outside guest memory");
        }
        let mut done = 0usize;
        while done < data.len() {
            let addr = gpa + done as u64;
            let page = (addr / PAGE_SIZE as u64) as u32;
            let off = (addr % PAGE_SIZE as u64) as usize;
            let n = (PAGE_SIZE - off).min(data.len() - done);
            let f = self.page_mut(page)?;
            frame_slice(f)[off..off + n].copy_from_slice(&data[done..done + n]);
            done += n;
        }
        Ok(())
    }

    pub fn write_u64(&mut self, gpa: u64, v: u64) -> Result<(), &'static str> {
        self.write(gpa, &v.to_le_bytes())
    }

    pub fn finish(self) -> Arc<Template> {
        Arc::new(self)
    }

    pub fn frame_for(&self, page: u32) -> Option<u64> {
        self.pages.get(&page).copied()
    }

    /// Boot state for the unikernel layout.
    fn unikernel_boot(entry: u64, mem_size: u64) -> BootState {
        BootState {
            rip: entry,
            rsp: mem_size - 64,
            rdi: 0,
            rsi: GUEST_PARAMS,
            rdx: mem_size,
            cr0: cr0::PE | cr0::MP | cr0::ET | cr0::NE | cr0::WP | cr0::PG,
            cr3: GUEST_PML4,
            cr4: cr4::PAE | cr4::PGE | cr4::OSFXSR | cr4::OSXMMEXCPT,
            efer: efer::LME | efer::LMA | efer::NXE,
            gdtr_base: GUEST_GDT,
            gdtr_limit: 0x1F,
            idtr_base: GUEST_IDT,
            idtr_limit: 0xFFF,
            cs: GDT_CODE,
            ds: GDT_DATA,
            tr: GDT_TSS,
            tr_attr: seg_attr::TSS64,
            tr_limit: 0x67,
        }
    }

    /// Build the template for a unikernel VM with `mem_size` bytes of memory.
    pub fn build(mem_size: u64) -> Result<Arc<Template>, &'static str> {
        if mem_size < MIN_MEM || mem_size > MAX_MEM || mem_size % PAGE_SIZE as u64 != 0 {
            return Err("invalid memory size (256 KiB .. 1 GiB, page multiple)");
        }
        let elf = parse_elf(GUEST_ELF)?;
        let mut t = Template::new_empty(TemplateKind::Unikernel, "unikernel", mem_size, Self::unikernel_boot(elf.entry, mem_size));

        // Guest page tables: identity map [0, mem_size) with 2 MiB pages.
        // Accessed/Dirty are pre-set so the CPU never needs to write to the
        // (shared) table pages.
        const AD: u64 = 0x60;
        t.write_u64(GUEST_PML4, GUEST_PDPT | 0x3 | AD)?;
        t.write_u64(GUEST_PDPT, GUEST_PD | 0x3 | AD)?;
        let pd_entries = ((mem_size + (2 << 20) - 1) >> 21).max(1).min(512);
        for i in 0..pd_entries {
            t.write_u64(GUEST_PD + i * 8, (i << 21) | 0x83 | AD)?;
        }

        // GDT: null, code, data, TSS descriptor placeholder.
        t.write_u64(GUEST_GDT + 8, 0x00AF_9A00_0000_FFFF)?;
        t.write_u64(GUEST_GDT + 16, 0x00CF_9200_0000_FFFF)?;
        t.write_u64(GUEST_GDT + 24, 0x0000_8900_0000_0067)?;

        // Boot parameters page (informational; registers carry the ABI).
        t.write(GUEST_PARAMS, b"CONCOSVM")?;
        t.write_u64(GUEST_PARAMS + 8, mem_size)?;
        t.write_u64(GUEST_PARAMS + 16, elf.entry)?;

        // ELF segments.
        let mut image_bytes = 0usize;
        for seg in &elf.segments {
            if seg.vaddr < GUEST_IMAGE_BASE || seg.vaddr + seg.memsz > mem_size - 64 * 1024 {
                return Err("guest image does not fit in VM memory");
            }
            image_bytes += seg.memsz as usize;
            let src = &GUEST_ELF[seg.offset as usize..(seg.offset + seg.filesz) as usize];
            t.write(seg.vaddr, src)?;
            // Bytes between filesz and memsz are zero: pages that are fully
            // beyond filesz stay absent (the VM sees them as zero pages).
        }
        t.image_bytes = image_bytes;
        Ok(t.finish())
    }
}
