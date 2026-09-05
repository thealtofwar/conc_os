//! Legacy-layout virtqueue.

use core::sync::atomic::{fence, Ordering};

use crate::arch::cpu::outw;
use crate::mm::{align_up, PAGE_SIZE};

pub const DESC_F_NEXT: u16 = 1;
pub const DESC_F_WRITE: u16 = 2;
pub const AVAIL_F_NO_INTERRUPT: u16 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Desc {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct UsedElem {
    pub id: u32,
    pub len: u32,
}

/// A buffer to hand to the device.  `write` means the device writes into it.
#[derive(Clone, Copy, Debug)]
pub struct Buf {
    pub addr: u64,
    pub len: u32,
    pub write: bool,
}

pub struct Virtqueue {
    size: u16,
    desc: *mut Desc,
    avail: *mut u16,
    used: *mut u16,
    free_head: u16,
    num_free: u16,
    last_used_idx: u16,
    notify_port: u16,
    index: u16,
    pub mem: u64,
    pub pages: usize,
}

unsafe impl Send for Virtqueue {}

impl Virtqueue {
    /// Pages needed for a queue of `size` entries in the legacy layout.
    pub fn pages_for(size: u16) -> usize {
        let s = size as u64;
        let avail_end = 16 * s + 6 + 2 * s;
        let used_start = align_up(avail_end, PAGE_SIZE as u64);
        let used_end = used_start + 6 + 8 * s;
        (align_up(used_end, PAGE_SIZE as u64) / PAGE_SIZE as u64) as usize
    }

    pub fn new(size: u16, mem: u64, pages: usize, notify_port: u16, index: u16) -> Self {
        let s = size as u64;
        let desc = mem as *mut Desc;
        let avail = (mem + 16 * s) as *mut u16;
        let used = align_up(mem + 16 * s + 6 + 2 * s, PAGE_SIZE as u64) as *mut u16;
        for i in 0..size {
            unsafe { (*desc.add(i as usize)).next = i + 1 };
        }
        Virtqueue {
            size,
            desc,
            avail,
            used,
            free_head: 0,
            num_free: size,
            last_used_idx: 0,
            notify_port,
            index,
            mem,
            pages,
        }
    }

    pub fn size(&self) -> u16 {
        self.size
    }

    pub fn num_free(&self) -> u16 {
        self.num_free
    }

    #[inline]
    fn avail_idx_ptr(&self) -> *mut u16 {
        unsafe { self.avail.add(1) }
    }
    #[inline]
    fn avail_ring(&self, i: u16) -> *mut u16 {
        unsafe { self.avail.add(2 + i as usize) }
    }
    #[inline]
    fn used_idx_ptr(&self) -> *const u16 {
        unsafe { self.used.add(1) }
    }
    #[inline]
    fn used_ring(&self, i: u16) -> *const UsedElem {
        unsafe { (self.used.add(2) as *const UsedElem).add(i as usize) }
    }

    /// Suppress (or re-enable) used-buffer interrupts for this queue.
    pub fn set_no_interrupt(&mut self, on: bool) {
        unsafe { core::ptr::write_volatile(self.avail, if on { AVAIL_F_NO_INTERRUPT } else { 0 }) };
    }

    /// Chain `bufs` into descriptors and publish them.  Returns the head
    /// descriptor index (the token that comes back in the used ring).
    pub fn add(&mut self, bufs: &[Buf]) -> Option<u16> {
        let n = bufs.len();
        if n == 0 || n > self.num_free as usize {
            return None;
        }
        let head = self.free_head;
        let mut i = head;
        for (k, b) in bufs.iter().enumerate() {
            let d = unsafe { &mut *self.desc.add(i as usize) };
            d.addr = b.addr;
            d.len = b.len;
            d.flags = if b.write { DESC_F_WRITE } else { 0 } | if k + 1 < n { DESC_F_NEXT } else { 0 };
            i = d.next;
        }
        self.free_head = i;
        self.num_free -= n as u16;

        let idx = unsafe { core::ptr::read_volatile(self.avail_idx_ptr()) };
        unsafe { core::ptr::write_volatile(self.avail_ring(idx % self.size), head) };
        fence(Ordering::SeqCst);
        unsafe { core::ptr::write_volatile(self.avail_idx_ptr(), idx.wrapping_add(1)) };
        fence(Ordering::SeqCst);
        Some(head)
    }

    /// Tell the device there is work.
    pub fn kick(&self) {
        fence(Ordering::SeqCst);
        outw(self.notify_port, self.index);
    }

    pub fn has_used(&self) -> bool {
        fence(Ordering::SeqCst);
        unsafe { core::ptr::read_volatile(self.used_idx_ptr()) != self.last_used_idx }
    }

    /// Take the next completed chain: (head descriptor, bytes written).
    pub fn pop_used(&mut self) -> Option<(u16, u32)> {
        fence(Ordering::SeqCst);
        let used_idx = unsafe { core::ptr::read_volatile(self.used_idx_ptr()) };
        if used_idx == self.last_used_idx {
            return None;
        }
        let e = unsafe { core::ptr::read_volatile(self.used_ring(self.last_used_idx % self.size)) };
        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        let head = e.id as u16;
        // Return the chain to the free list.
        let mut i = head;
        let mut count = 1u16;
        loop {
            let d = unsafe { &mut *self.desc.add(i as usize) };
            if d.flags & DESC_F_NEXT == 0 {
                d.next = self.free_head;
                break;
            }
            i = d.next;
            count += 1;
        }
        self.free_head = head;
        self.num_free += count;
        Some((head, e.len))
    }
}
