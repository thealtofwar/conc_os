//! Memory management: early bump allocator, physical frame allocator and the
//! kernel heap.
//!
//! The kernel identity-maps all physical memory, so physical and virtual
//! addresses coincide for kernel data.  The helpers below make that
//! assumption explicit so it can be changed in one place later.

#![allow(dead_code)]

pub mod early;
pub mod frame;
pub mod heap;

pub const PAGE_SIZE: usize = 4096;
pub const PAGE_SHIFT: usize = 12;
pub const HUGE_PAGE_SIZE: usize = 2 * 1024 * 1024;

#[inline]
pub fn phys_to_virt(p: u64) -> *mut u8 {
    p as usize as *mut u8
}

#[inline]
pub fn virt_to_phys<T>(v: *const T) -> u64 {
    v as usize as u64
}

#[inline]
pub const fn align_up(x: u64, a: u64) -> u64 {
    (x + a - 1) & !(a - 1)
}

#[inline]
pub const fn align_down(x: u64, a: u64) -> u64 {
    x & !(a - 1)
}

/// Zero a physical frame.
#[inline]
pub fn zero_frame(pa: u64) {
    unsafe { core::ptr::write_bytes(phys_to_virt(pa), 0, PAGE_SIZE) }
}

/// Copy a physical frame.
#[inline]
pub fn copy_frame(dst: u64, src: u64) {
    unsafe { core::ptr::copy_nonoverlapping(phys_to_virt(src), phys_to_virt(dst), PAGE_SIZE) }
}

/// View a physical frame as a byte slice.
#[inline]
pub fn frame_slice<'a>(pa: u64) -> &'a mut [u8] {
    unsafe { core::slice::from_raw_parts_mut(phys_to_virt(pa), PAGE_SIZE) }
}

/// Pretty-print a byte count.
pub struct Bytes(pub u64);

impl core::fmt::Display for Bytes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let b = self.0;
        if b >= 1 << 30 {
            write!(f, "{}.{} GiB", b >> 30, ((b & ((1 << 30) - 1)) * 10) >> 30)
        } else if b >= 1 << 20 {
            write!(f, "{}.{} MiB", b >> 20, ((b & ((1 << 20) - 1)) * 10) >> 20)
        } else if b >= 1 << 10 {
            write!(f, "{} KiB", b >> 10)
        } else {
            write!(f, "{} B", b)
        }
    }
}
