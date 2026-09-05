//! Nested page tables (guest-physical → host-physical) for one VM.
//!
//! SVM nested tables use the ordinary long-mode format, so this is a thin
//! wrapper over `arch::paging::Mapper` that always sets the User bit (guest
//! accesses are treated as user accesses by the nested walk) and keeps track
//! of how many table frames the VM owns.

#![allow(dead_code)]

use crate::arch::paging::{self, Mapper};
use crate::mm::frame;
use crate::sync::SpinLock;

/// Frames that have been nested page-table pages stay nested page-table
/// pages.  When conc_os itself runs under KVM, KVM shadows our tables and
/// keeps write-protecting a frame it once saw as a table; handing such a
/// frame to a guest as data would turn every guest write to it into a fault
/// KVM cannot resolve for us (observed as an endless storm of nested page
/// faults on a correctly mapped page).
static TABLE_POOL: SpinLock<alloc::vec::Vec<u64>> = SpinLock::new(alloc::vec::Vec::new());

fn alloc_table() -> Option<u64> {
    if let Some(f) = TABLE_POOL.lock().pop() {
        crate::mm::zero_frame(f);
        return Some(f);
    }
    frame::alloc_zeroed()
}

fn free_table(pa: u64) {
    TABLE_POOL.lock().push(pa);
}

/// Frames parked in the table pool.
pub fn pooled_tables() -> usize {
    TABLE_POOL.lock().len()
}

pub struct Npt {
    mapper: Mapper,
    tables: usize,
}

impl Npt {
    pub fn new() -> Option<Npt> {
        let root = alloc_table()?;
        Some(Npt { mapper: Mapper::new(root), tables: 1 })
    }

    pub fn root(&self) -> u64 {
        self.mapper.root()
    }

    /// Map one 4 KiB guest page.
    pub fn map(&mut self, gpa: u64, hpa: u64, writable: bool) -> bool {
        let mut created = 0usize;
        let mut alloc = || match alloc_table() {
            Some(f) => {
                created += 1;
                f
            }
            None => 0,
        };
        let flags = paging::USER | if writable { paging::WRITABLE } else { 0 };
        let ok = self.mapper.map_4k(gpa, hpa, flags, &mut alloc);
        self.tables += created;
        ok
    }

    pub fn unmap(&mut self, gpa: u64) -> Option<u64> {
        self.mapper.unmap_4k(gpa)
    }

    /// (host physical, writable)
    pub fn translate(&self, gpa: u64) -> Option<(u64, bool)> {
        self.mapper.translate(gpa).map(|(pa, fl)| (pa, fl & paging::WRITABLE != 0))
    }

    /// Number of page-table frames owned by this NPT.
    pub fn table_frames(&self) -> usize {
        self.tables
    }

    /// Return all table frames to the table pool (guest pages are owned
    /// elsewhere).
    pub fn destroy(self) {
        self.mapper.free_tables(&mut |pa| free_table(pa));
    }
}
