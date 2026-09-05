//! Early boot bump allocator.
//!
//! Between `ExitBootServices` and the frame allocator coming up we need a few
//! pages for page tables and the kernel stack.  We carve them off the start of
//! the largest free region reported by firmware; the frame allocator later
//! marks the consumed prefix as used.

use crate::sync::SpinLock;
use crate::uefi::BootInfo;

#[derive(Clone, Copy, Debug, Default)]
pub struct Bump {
    pub start: u64,
    pub next: u64,
    pub end: u64,
}

static BUMP: SpinLock<Bump> = SpinLock::new(Bump { start: 0, next: 0, end: 0 });

/// Pick the largest conventional region (preferring memory below 4 GiB so it
/// is always reachable by devices later) and make it the early arena.
pub fn init(info: &BootInfo) {
    let mut best: Option<(u64, u64)> = None;
    for d in info.descriptors() {
        if d.ty != crate::uefi::mem_type::CONVENTIONAL {
            continue;
        }
        let start = d.phys_start.max(1 << 20); // skip the first MiB
        let end = d.end().min(1 << 32);
        if end <= start {
            continue;
        }
        let size = end - start;
        if best.map_or(true, |(s, e)| size > e - s) {
            best = Some((start, end));
        }
    }
    let (start, end) = best.expect("no usable conventional memory below 4 GiB");
    let mut b = BUMP.lock();
    b.start = start;
    b.next = start;
    b.end = end;
}

/// Allocate `n` zeroed, physically contiguous pages.  Returns the physical
/// (== virtual) address.
pub fn alloc_pages(n: usize) -> u64 {
    let mut b = BUMP.lock();
    let bytes = (n as u64) * 4096;
    if b.next + bytes > b.end {
        panic!("early allocator exhausted ({} pages requested)", n);
    }
    let p = b.next;
    b.next += bytes;
    drop(b);
    unsafe { core::ptr::write_bytes(p as *mut u8, 0, bytes as usize) };
    p
}

pub fn region() -> Bump {
    *BUMP.lock()
}
