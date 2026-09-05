//! Four-level x86_64 page tables.
//!
//! The same structure is used for the host address space and for SVM nested
//! page tables (which use the ordinary long-mode format).  Tables are
//! addressed physically; the kernel's identity map makes them directly
//! accessible.

#![allow(dead_code)]

use crate::mm::phys_to_virt;
use crate::uefi::BootInfo;

pub const PRESENT: u64 = 1 << 0;
pub const WRITABLE: u64 = 1 << 1;
pub const USER: u64 = 1 << 2;
pub const PWT: u64 = 1 << 3;
pub const PCD: u64 = 1 << 4;
pub const ACCESSED: u64 = 1 << 5;
pub const DIRTY: u64 = 1 << 6;
pub const HUGE: u64 = 1 << 7;
pub const GLOBAL: u64 = 1 << 8;
pub const NX: u64 = 1 << 63;

pub const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
pub const FLAGS_MASK: u64 = !ADDR_MASK;

/// Flags used on intermediate (non-leaf) entries.
const INTERMEDIATE: u64 = PRESENT | WRITABLE | USER;

#[inline]
fn index(va: u64, level: u8) -> usize {
    ((va >> (12 + 9 * level as u64)) & 0x1FF) as usize
}

#[inline]
unsafe fn table<'a>(pa: u64) -> &'a mut [u64; 512] {
    &mut *(phys_to_virt(pa) as *mut [u64; 512])
}

/// A page-table hierarchy rooted at a PML4.
pub struct Mapper {
    root: u64,
}

impl Mapper {
    /// Wrap an existing root table.
    pub const fn new(root: u64) -> Self {
        Mapper { root }
    }

    /// Create an empty hierarchy using `alloc` for the root table.
    pub fn create(alloc: &mut dyn FnMut() -> u64) -> Option<Self> {
        let root = alloc();
        if root == 0 {
            None
        } else {
            Some(Mapper { root })
        }
    }

    pub fn root(&self) -> u64 {
        self.root
    }

    /// Walk down to the table holding entries of `target_level`
    /// (0 = PT, 1 = PD, 2 = PDPT), creating tables on the way.
    fn walk_create(&mut self, va: u64, target_level: u8, alloc: &mut dyn FnMut() -> u64) -> Option<u64> {
        let mut tbl = self.root;
        let mut level = 3u8;
        while level > target_level {
            let t = unsafe { table(tbl) };
            let e = t[index(va, level)];
            if e & PRESENT == 0 {
                let new = alloc();
                if new == 0 {
                    return None;
                }
                t[index(va, level)] = new | INTERMEDIATE;
                tbl = new;
            } else if e & HUGE != 0 {
                return None;
            } else {
                tbl = e & ADDR_MASK;
            }
            level -= 1;
        }
        Some(tbl)
    }

    /// Walk without creating.  Returns the table and level that hold the
    /// final entry for `va` (either a leaf, or a non-present entry).
    fn walk(&self, va: u64) -> (u64, u8) {
        let mut tbl = self.root;
        let mut level = 3u8;
        loop {
            let t = unsafe { table(tbl) };
            let e = t[index(va, level)];
            if level == 0 || e & PRESENT == 0 || e & HUGE != 0 {
                return (tbl, level);
            }
            tbl = e & ADDR_MASK;
            level -= 1;
        }
    }

    pub fn map_4k(&mut self, va: u64, pa: u64, flags: u64, alloc: &mut dyn FnMut() -> u64) -> bool {
        match self.walk_create(va, 0, alloc) {
            Some(t) => {
                unsafe { table(t)[index(va, 0)] = (pa & ADDR_MASK) | (flags & FLAGS_MASK) | PRESENT };
                true
            }
            None => false,
        }
    }

    pub fn map_2m(&mut self, va: u64, pa: u64, flags: u64, alloc: &mut dyn FnMut() -> u64) -> bool {
        match self.walk_create(va, 1, alloc) {
            Some(t) => {
                unsafe { table(t)[index(va, 1)] = (pa & ADDR_MASK) | (flags & FLAGS_MASK) | PRESENT | HUGE };
                true
            }
            None => false,
        }
    }

    /// Pointer to the leaf (or would-be leaf) entry for `va`, plus its level.
    pub fn entry(&self, va: u64) -> (*mut u64, u8) {
        let (t, level) = self.walk(va);
        let tbl = unsafe { table(t) };
        (&mut tbl[index(va, level)] as *mut u64, level)
    }

    /// Translate a virtual address, returning (physical, flags).
    pub fn translate(&self, va: u64) -> Option<(u64, u64)> {
        let (t, level) = self.walk(va);
        let e = unsafe { table(t)[index(va, level)] };
        if e & PRESENT == 0 {
            return None;
        }
        let page_bits = 12 + 9 * level as u64;
        let mask = (1u64 << page_bits) - 1;
        Some((((e & ADDR_MASK) & !mask) | (va & mask), e & FLAGS_MASK))
    }

    /// Remove a 4 KiB mapping, returning the physical address it pointed to.
    pub fn unmap_4k(&mut self, va: u64) -> Option<u64> {
        let (t, level) = self.walk(va);
        if level != 0 {
            return None;
        }
        let tbl = unsafe { table(t) };
        let e = tbl[index(va, 0)];
        if e & PRESENT == 0 {
            return None;
        }
        tbl[index(va, 0)] = 0;
        Some(e & ADDR_MASK)
    }

    /// Visit every present leaf entry below `va_start .. va_end`.
    /// The callback receives (va, entry_ptr, level).
    pub fn for_each_leaf(&self, va_start: u64, va_end: u64, f: &mut dyn FnMut(u64, *mut u64, u8)) {
        fn rec(tbl: u64, level: u8, base: u64, va_start: u64, va_end: u64, f: &mut dyn FnMut(u64, *mut u64, u8)) {
            let t = unsafe { table(tbl) };
            let span = 1u64 << (12 + 9 * level as u64);
            for i in 0..512 {
                let va = base + i as u64 * span;
                if va >= va_end || va + span <= va_start {
                    continue;
                }
                let e = t[i];
                if e & PRESENT == 0 {
                    continue;
                }
                if level == 0 || e & HUGE != 0 {
                    f(va, &mut t[i] as *mut u64, level);
                } else {
                    rec(e & ADDR_MASK, level - 1, va, va_start, va_end, f);
                }
            }
        }
        rec(self.root, 3, 0, va_start, va_end, f);
    }

    /// Free every intermediate table (including the root) via `dealloc`.
    /// Leaf frames are *not* freed; the caller owns those.
    pub fn free_tables(self, dealloc: &mut dyn FnMut(u64)) {
        fn rec(tbl: u64, level: u8, dealloc: &mut dyn FnMut(u64)) {
            if level > 0 {
                let t = unsafe { table(tbl) };
                for i in 0..512 {
                    let e = t[i];
                    if e & PRESENT != 0 && e & HUGE == 0 {
                        rec(e & ADDR_MASK, level - 1, dealloc);
                    }
                }
            }
            dealloc(tbl);
        }
        rec(self.root, 3, dealloc);
    }
}

/// Make sure `[pa, pa + len)` is identity-mapped uncacheable in the running
/// kernel address space (for device BARs outside the initial map).
pub fn map_mmio(pa: u64, len: u64) {
    let mut m = Mapper::new(crate::arch::cpu::read_cr3() & ADDR_MASK);
    let mut alloc = || crate::mm::frame::alloc_zeroed().unwrap_or(0);
    let start = crate::mm::align_down(pa, 1 << 21);
    let end = crate::mm::align_up(pa + len, 1 << 21);
    let mut va = start;
    while va < end {
        if m.translate(va).is_none() {
            m.map_2m(va, va, WRITABLE | PCD | PWT, &mut alloc);
        }
        va += 1 << 21;
    }
}

/// Build the kernel address space: an identity map of physical memory using
/// 2 MiB pages.  RAM is mapped write-back; everything else (MMIO holes, the
/// APIC, PCI BARs) is mapped uncacheable.
pub fn build_kernel_tables(info: &BootInfo) -> u64 {
    let mut max_ram = 0u64;
    for d in info.descriptors() {
        if d.is_ram() {
            max_ram = max_ram.max(d.end());
        }
    }
    let mut top = max_ram.max(1 << 32);
    if let Some(fb) = &info.framebuffer {
        top = top.max(fb.base + fb.size as u64);
    }
    top = crate::mm::align_up(top, 1 << 30);

    let mut alloc = || crate::mm::early::alloc_pages(1);
    let mut m = Mapper::create(&mut alloc).unwrap();

    let ram_covers = |va: u64| -> bool {
        for d in info.descriptors() {
            if d.is_ram() && d.phys_start < va + (1 << 21) && d.end() > va {
                return true;
            }
        }
        false
    };

    let mut va = 0u64;
    while va < top {
        let flags = if ram_covers(va) { WRITABLE } else { WRITABLE | PCD | PWT };
        assert!(m.map_2m(va, va, flags, &mut alloc), "failed to map {:#x}", va);
        va += 1 << 21;
    }
    m.root()
}
