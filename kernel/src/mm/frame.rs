//! Physical frame allocator.
//!
//! A bitmap covers every 4 KiB frame of RAM (1 bit per frame: 256 KiB of
//! metadata for 8 GiB).  Single-frame allocation goes through a small LIFO
//! cache of recently freed frames so the hot path is O(1); the bitmap is the
//! fallback and also serves contiguous allocations (which are rare and mostly
//! happen at boot).

use crate::mm::{zero_frame, PAGE_SIZE};
use crate::sync::SpinLock;
use crate::uefi::BootInfo;

const CACHE_SIZE: usize = 2048;

pub struct FrameAllocator {
    /// One bit per frame; 1 = used.
    bitmap: &'static mut [u64],
    /// Number of frames covered by the bitmap.
    nframes: usize,
    free_count: usize,
    total_usable: usize,
    /// Word index to start scanning from.
    hint: usize,
    cache: [u32; CACHE_SIZE],
    cache_len: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FrameStats {
    pub total: usize,
    pub free: usize,
}

static ALLOC: SpinLock<Option<FrameAllocator>> = SpinLock::new(None);

impl FrameAllocator {
    #[inline]
    fn is_used(&self, f: usize) -> bool {
        self.bitmap[f / 64] & (1u64 << (f % 64)) != 0
    }
    #[inline]
    fn set_used(&mut self, f: usize) {
        self.bitmap[f / 64] |= 1u64 << (f % 64);
    }
    #[inline]
    fn set_free(&mut self, f: usize) {
        self.bitmap[f / 64] &= !(1u64 << (f % 64));
    }

    fn alloc_one(&mut self) -> Option<u64> {
        if self.cache_len > 0 {
            self.cache_len -= 1;
            let f = self.cache[self.cache_len] as usize;
            self.free_count -= 1;
            return Some((f * PAGE_SIZE) as u64);
        }
        let words = self.bitmap.len();
        let mut w = self.hint;
        for _ in 0..words {
            if w >= words {
                w = 0;
            }
            let word = self.bitmap[w];
            if word != u64::MAX {
                let bit = (!word).trailing_zeros() as usize;
                let f = w * 64 + bit;
                if f < self.nframes {
                    self.set_used(f);
                    self.hint = w;
                    self.free_count -= 1;
                    return Some((f * PAGE_SIZE) as u64);
                }
            }
            w += 1;
        }
        None
    }

    fn free_one(&mut self, pa: u64) {
        let f = (pa as usize) / PAGE_SIZE;
        debug_assert!(f < self.nframes, "free of frame outside RAM: {:#x}", pa);
        debug_assert!(self.is_used(f), "double free of frame {:#x}", pa);
        self.free_count += 1;
        if self.cache_len < CACHE_SIZE {
            self.cache[self.cache_len] = f as u32;
            self.cache_len += 1;
        } else {
            self.set_free(f);
            if f / 64 < self.hint {
                self.hint = f / 64;
            }
        }
    }

    fn alloc_contiguous(&mut self, n: usize, align_frames: usize) -> Option<u64> {
        let align = align_frames.max(1);
        let mut f = 0usize;
        while f + n <= self.nframes {
            // Skip whole used words quickly.
            if self.bitmap[f / 64] == u64::MAX {
                f = (f / 64 + 1) * 64;
                f = (f + align - 1) / align * align;
                continue;
            }
            let mut ok = true;
            for i in 0..n {
                if self.is_used(f + i) {
                    ok = false;
                    f = (f + i + 1 + align - 1) / align * align;
                    break;
                }
            }
            if ok {
                for i in 0..n {
                    self.set_used(f + i);
                }
                self.free_count -= n;
                return Some((f * PAGE_SIZE) as u64);
            }
        }
        None
    }

    fn free_contiguous(&mut self, pa: u64, n: usize) {
        let f0 = (pa as usize) / PAGE_SIZE;
        for f in f0..f0 + n {
            debug_assert!(self.is_used(f));
            self.set_free(f);
        }
        self.free_count += n;
        if f0 / 64 < self.hint {
            self.hint = f0 / 64;
        }
    }
}

/// Build the allocator from the firmware memory map.  Everything is marked
/// used, then regions that are free after boot are released, then the early
/// bump arena's consumed prefix is re-reserved.
pub fn init(info: &BootInfo) {
    let mut max_ram = 0u64;
    for d in info.descriptors() {
        if d.is_ram() {
            max_ram = max_ram.max(d.end());
        }
    }
    let nframes = (max_ram as usize) / PAGE_SIZE;
    let words = (nframes + 63) / 64;
    let bitmap_pages = (words * 8 + PAGE_SIZE - 1) / PAGE_SIZE;
    let bitmap_pa = crate::mm::early::alloc_pages(bitmap_pages);
    let bitmap: &'static mut [u64] =
        unsafe { core::slice::from_raw_parts_mut(bitmap_pa as *mut u64, words) };
    bitmap.fill(u64::MAX);

    let mut a = FrameAllocator {
        bitmap,
        nframes,
        free_count: 0,
        total_usable: 0,
        hint: 0,
        cache: [0; CACHE_SIZE],
        cache_len: 0,
    };

    for d in info.descriptors() {
        if !d.is_free_after_boot() {
            continue;
        }
        let start = (d.phys_start.max(1 << 20) as usize) / PAGE_SIZE; // never hand out the first MiB
        let end = (d.end() as usize) / PAGE_SIZE;
        for f in start..end.min(nframes) {
            if a.is_used(f) {
                a.set_free(f);
                a.free_count += 1;
                a.total_usable += 1;
            }
        }
    }

    // Reserve what the early allocator handed out (page tables, stacks, this
    // very bitmap).
    let early = crate::mm::early::region();
    let s = (early.start as usize) / PAGE_SIZE;
    let e = (early.next as usize + PAGE_SIZE - 1) / PAGE_SIZE;
    for f in s..e {
        if !a.is_used(f) {
            a.set_used(f);
            a.free_count -= 1;
        }
    }

    *ALLOC.lock() = Some(a);
}

#[inline]
fn with<R>(f: impl FnOnce(&mut FrameAllocator) -> R) -> R {
    let mut g = ALLOC.lock();
    f(g.as_mut().expect("frame allocator not initialised"))
}

/// Allocate one 4 KiB frame (contents undefined).
pub fn alloc() -> Option<u64> {
    with(|a| a.alloc_one())
}

/// Allocate one zeroed 4 KiB frame.
pub fn alloc_zeroed() -> Option<u64> {
    let f = alloc()?;
    zero_frame(f);
    Some(f)
}

pub fn free(pa: u64) {
    with(|a| a.free_one(pa))
}

/// Allocate `n` physically contiguous frames aligned to `align_frames` frames.
pub fn alloc_contiguous(n: usize, align_frames: usize) -> Option<u64> {
    if n == 1 && align_frames <= 1 {
        return alloc();
    }
    with(|a| a.alloc_contiguous(n, align_frames))
}

pub fn alloc_contiguous_zeroed(n: usize, align_frames: usize) -> Option<u64> {
    let p = alloc_contiguous(n, align_frames)?;
    unsafe { core::ptr::write_bytes(p as *mut u8, 0, n * PAGE_SIZE) };
    Some(p)
}

pub fn free_contiguous(pa: u64, n: usize) {
    if n == 1 {
        return free(pa);
    }
    with(|a| a.free_contiguous(pa, n))
}

pub fn stats() -> FrameStats {
    with(|a| FrameStats { total: a.total_usable, free: a.free_count })
}
