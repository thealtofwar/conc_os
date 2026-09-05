//! Kernel heap: a size-class slab allocator for small objects with large
//! allocations served directly from contiguous physical frames.
//!
//! Small sizes (<= 4 KiB) map to one of a fixed set of classes.  Each class
//! keeps an intrusive free list; when it runs dry a fresh frame is carved into
//! objects.  Because Rust hands us the `Layout` on deallocation we need no
//! headers, so `dealloc` is O(1) and small objects are packed tightly.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;

use crate::mm::{frame, PAGE_SIZE};
use crate::sync::SpinLock;

const CLASSES: [usize; 15] = [16, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536, 2048, 4096];

struct Class {
    free: *mut u8,
    free_objects: usize,
    total_objects: usize,
}

struct Heap {
    classes: [Class; CLASSES.len()],
    slab_bytes: usize,
    large_bytes: usize,
}

unsafe impl Send for Heap {}

#[derive(Clone, Copy, Debug, Default)]
pub struct HeapStats {
    /// Bytes of frames owned by the slab classes.
    pub slab_bytes: usize,
    /// Bytes currently free inside slab classes.
    pub slab_free_bytes: usize,
    /// Bytes currently out in large (page-multiple) allocations.
    pub large_bytes: usize,
}

const fn new_class() -> Class {
    Class { free: null_mut(), free_objects: 0, total_objects: 0 }
}

static HEAP: SpinLock<Heap> = SpinLock::new(Heap {
    classes: [
        new_class(), new_class(), new_class(), new_class(), new_class(),
        new_class(), new_class(), new_class(), new_class(), new_class(),
        new_class(), new_class(), new_class(), new_class(), new_class(),
    ],
    slab_bytes: 0,
    large_bytes: 0,
});

/// Pick the class index for a layout, or `None` for a large allocation.
#[inline]
fn class_for(layout: Layout) -> Option<usize> {
    let size = layout.size().max(1);
    let align = layout.align();
    if size > 4096 || align > 4096 {
        return None;
    }
    // Objects of class `c` start at multiples of `c` within a frame, so an
    // object is aligned to the largest power of two dividing `c`.
    for (i, &c) in CLASSES.iter().enumerate() {
        if c >= size && c % align == 0 {
            return Some(i);
        }
    }
    None
}

impl Heap {
    unsafe fn refill(&mut self, idx: usize) -> bool {
        let size = CLASSES[idx];
        let frame = match frame::alloc() {
            Some(f) => f,
            None => return false,
        };
        self.slab_bytes += PAGE_SIZE;
        let base = frame as usize;
        let count = PAGE_SIZE / size;
        let class = &mut self.classes[idx];
        // Push objects in reverse so allocation order is ascending.
        for i in (0..count).rev() {
            let obj = (base + i * size) as *mut u8;
            *(obj as *mut *mut u8) = class.free;
            class.free = obj;
        }
        class.free_objects += count;
        class.total_objects += count;
        true
    }

    unsafe fn alloc_small(&mut self, idx: usize) -> *mut u8 {
        if self.classes[idx].free.is_null() && !self.refill(idx) {
            return null_mut();
        }
        let class = &mut self.classes[idx];
        let obj = class.free;
        class.free = *(obj as *mut *mut u8);
        class.free_objects -= 1;
        obj
    }

    unsafe fn free_small(&mut self, idx: usize, ptr: *mut u8) {
        let class = &mut self.classes[idx];
        *(ptr as *mut *mut u8) = class.free;
        class.free = ptr;
        class.free_objects += 1;
    }
}

#[inline]
fn large_pages(layout: Layout) -> (usize, usize) {
    let pages = (layout.size() + PAGE_SIZE - 1) / PAGE_SIZE;
    let align_frames = (layout.align() + PAGE_SIZE - 1) / PAGE_SIZE;
    (pages.max(1), align_frames.max(1))
}

pub struct KernelAllocator;

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match class_for(layout) {
            Some(idx) => HEAP.lock().alloc_small(idx),
            None => {
                let (pages, align) = large_pages(layout);
                match frame::alloc_contiguous(pages, align) {
                    Some(pa) => {
                        HEAP.lock().large_bytes += pages * PAGE_SIZE;
                        pa as usize as *mut u8
                    }
                    None => null_mut(),
                }
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        match class_for(layout) {
            Some(idx) => HEAP.lock().free_small(idx, ptr),
            None => {
                let (pages, _) = large_pages(layout);
                frame::free_contiguous(ptr as usize as u64, pages);
                HEAP.lock().large_bytes -= pages * PAGE_SIZE;
            }
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_layout = Layout::from_size_align_unchecked(new_size, layout.align());
        match (class_for(layout), class_for(new_layout)) {
            (Some(a), Some(b)) if a == b => return ptr,
            (None, None) if large_pages(layout).0 == large_pages(new_layout).0 => return ptr,
            _ => {}
        }
        let new_ptr = self.alloc(new_layout);
        if !new_ptr.is_null() {
            core::ptr::copy_nonoverlapping(ptr, new_ptr, layout.size().min(new_size));
            self.dealloc(ptr, layout);
        }
        new_ptr
    }
}

#[global_allocator]
static GLOBAL: KernelAllocator = KernelAllocator;

/// Pre-populate the most common classes so the first allocations after boot
/// do not all take the refill path.
pub fn init() {
    let mut h = HEAP.lock();
    for idx in 0..CLASSES.len() {
        unsafe {
            h.refill(idx);
        }
    }
}

pub fn stats() -> HeapStats {
    let h = HEAP.lock();
    let mut free = 0;
    for (i, c) in h.classes.iter().enumerate() {
        free += c.free_objects * CLASSES[i];
    }
    HeapStats { slab_bytes: h.slab_bytes, slab_free_bytes: free, large_bytes: h.large_bytes }
}
