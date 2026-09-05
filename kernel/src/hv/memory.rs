//! Per-VM guest memory: copy-on-write over a template, lazily populated
//! nested page tables, and swap-out to the disk page store.
//!
//! Every guest page is in one of four states:
//!
//! * `Zero` — never written; reads map a shared all-zero frame read-only.
//! * `Template(f)` — identical to the template; mapped read-only, shared.
//! * `Private { frame, clean_block }` — the VM's own copy.  If the page was
//!   loaded from disk and not written since, `clean_block` remembers where
//!   the identical copy lives so freezing it again costs no I/O.
//! * `Swapped(block)` — evicted to disk; loaded on the next access.
//!
//! Only pages that differ from the template are stored (in a sparse
//! overlay); the rest are implied by the template.  A VM that has never
//! run, or that is frozen, therefore costs a few hundred bytes of host memory
//! plus one entry per page it has dirtied — which is what makes a thousand
//! clones of one snapshot affordable.
//!
//! Nothing is mapped into the NPT until the guest touches it, so a freshly
//! created (or thawed) VM has a one-frame NPT and no private memory at all.

#![allow(dead_code)]

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::image::Template;
use super::npt::Npt;
use crate::mm::{copy_frame, frame, frame_slice, zero_frame, PAGE_SIZE};
use crate::sync::OnceCell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageState {
    Zero,
    Template(u64),
    Private { frame: u64, clean_block: Option<u32> },
    Swapped(u32),
}

static ZERO_FRAME: OnceCell<u64> = OnceCell::new();

pub fn init_zero_frame() {
    ZERO_FRAME.init(frame::alloc_zeroed().expect("zero frame"));
}

fn zero_frame_pa() -> u64 {
    *ZERO_FRAME.expect("hv zero frame")
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FreezeReport {
    pub written: usize,
    pub dropped_clean: usize,
    pub kept_resident: usize,
    pub npt_frames_freed: usize,
}

pub struct GuestMemory {
    pub template: Arc<Template>,
    npages: usize,
    /// Pages that differ from the template (private or swapped).
    overlay: BTreeMap<u32, PageState>,
    npt: Option<Npt>,
    /// Number of private frames currently resident.
    pub resident: usize,
    pub swapped: usize,
    pub cow_copies: u64,
    pub pages_loaded: u64,
    pub pages_written: u64,
    /// Pages first written as zero pages (fresh frame, no copy).
    pub zero_allocs: u64,
    /// Read faults that only mapped an existing frame read-only.
    pub ro_maps: u64,
    /// Faults on pages that were already mapped adequately.
    pub redundant_npf: u64,
    /// Write faults that only turned a clean private page dirty.
    pub dirty_faults: u64,
    /// Set after NPT changes; the next VMRUN must flush the guest TLB.
    pub needs_flush: bool,
}

impl GuestMemory {
    pub fn new(template: Arc<Template>) -> Option<Self> {
        let npt = Npt::new()?;
        Some(GuestMemory {
            npages: template.npages,
            template,
            overlay: BTreeMap::new(),
            npt: Some(npt),
            resident: 0,
            swapped: 0,
            cow_copies: 0,
            pages_loaded: 0,
            pages_written: 0,
            zero_allocs: 0,
            ro_maps: 0,
            redundant_npf: 0,
            dirty_faults: 0,
            needs_flush: true,
        })
    }

    pub fn mem_size(&self) -> u64 {
        self.template.mem_size
    }

    pub fn npages(&self) -> usize {
        self.npages
    }

    pub fn npt_root(&self) -> u64 {
        self.npt.as_ref().map(|n| n.root()).unwrap_or(0)
    }

    pub fn npt_frames(&self) -> usize {
        self.npt.as_ref().map(|n| n.table_frames()).unwrap_or(0)
    }

    pub fn is_frozen(&self) -> bool {
        self.npt.is_none()
    }

    /// Pages that are not shared with the template (private + swapped).
    pub fn overlay_len(&self) -> usize {
        self.overlay.len()
    }

    /// Overlay pages inside `[start, end)` (used for kernel-text sharing
    /// statistics).
    pub fn overlay_in(&self, start: u32, end: u32) -> usize {
        self.overlay.range(start..end).count()
    }

    pub fn state(&self, page: usize) -> PageState {
        if let Some(&s) = self.overlay.get(&(page as u32)) {
            return s;
        }
        match self.template.pages.get(&(page as u32)) {
            Some(&f) => PageState::Template(f),
            None => PageState::Zero,
        }
    }

    fn set_state(&mut self, page: usize, s: PageState) {
        match s {
            PageState::Private { .. } | PageState::Swapped(_) => {
                self.overlay.insert(page as u32, s);
            }
            _ => {
                self.overlay.remove(&(page as u32));
            }
        }
    }

    fn npt_mut(&mut self) -> Result<&mut Npt, &'static str> {
        self.npt.as_mut().ok_or("memory is frozen")
    }

    fn map(&mut self, page: usize, hpa: u64, writable: bool) -> Result<(), &'static str> {
        let gpa = (page * PAGE_SIZE) as u64;
        if !self.npt_mut()?.map(gpa, hpa, writable) {
            return Err("out of memory for nested page tables");
        }
        self.needs_flush = true;
        Ok(())
    }

    /// Bring a swapped page back into a fresh frame.  The frame is marked
    /// clean (still identical to its disk block).
    async fn swap_in(&mut self, page: usize, block: u32) -> Result<u64, &'static str> {
        let store = crate::disk::store().ok_or("page store unavailable")?;
        let f = frame::alloc().ok_or("out of memory")?;
        if !store.load_frame(block, f).await {
            frame::free(f);
            return Err("disk read failed");
        }
        self.set_state(page, PageState::Private { frame: f, clean_block: Some(block) });
        self.swapped -= 1;
        self.resident += 1;
        self.pages_loaded += 1;
        Ok(f)
    }

    /// Give the page a private, writable frame (copying as needed).
    pub async fn private_frame(&mut self, page: usize) -> Result<u64, &'static str> {
        if page >= self.npages {
            return Err("guest address outside memory");
        }
        match self.state(page) {
            PageState::Private { frame, clean_block } => {
                if let Some(b) = clean_block {
                    // First write since it came from disk: the block is stale.
                    if let Some(store) = crate::disk::store() {
                        store.free_block(b);
                    }
                    self.set_state(page, PageState::Private { frame, clean_block: None });
                }
                Ok(frame)
            }
            PageState::Zero => {
                let f = frame::alloc_zeroed().ok_or("out of memory")?;
                self.set_state(page, PageState::Private { frame: f, clean_block: None });
                self.resident += 1;
                self.zero_allocs += 1;
                Ok(f)
            }
            PageState::Template(t) => {
                let f = frame::alloc().ok_or("out of memory")?;
                copy_frame(f, t);
                self.set_state(page, PageState::Private { frame: f, clean_block: None });
                self.resident += 1;
                self.cow_copies += 1;
                Ok(f)
            }
            PageState::Swapped(b) => {
                let f = self.swap_in(page, b).await?;
                // Caller is about to write: block becomes stale.
                if let Some(store) = crate::disk::store() {
                    store.free_block(b);
                }
                self.set_state(page, PageState::Private { frame: f, clean_block: None });
                Ok(f)
            }
        }
    }

    /// Frame holding the page's current contents, for reading.  `None`
    /// means the page is all zeros.
    pub async fn readable_frame(&mut self, page: usize) -> Result<Option<u64>, &'static str> {
        if page >= self.npages {
            return Err("guest address outside memory");
        }
        Ok(match self.state(page) {
            PageState::Zero => None,
            PageState::Template(t) => Some(t),
            PageState::Private { frame, .. } => Some(frame),
            PageState::Swapped(b) => Some(self.swap_in(page, b).await?),
        })
    }

    /// Resolve a nested page fault at `gpa`.
    pub async fn handle_npf(&mut self, gpa: u64, write: bool) -> Result<(), &'static str> {
        let page = (gpa / PAGE_SIZE as u64) as usize;
        if page >= self.npages {
            return Err("guest accessed memory outside its allocation");
        }
        // A fault on a page that is already mapped with enough permission
        // means the CPU's view of our tables is stale: count it.
        if let Some(npt) = self.npt.as_ref() {
            if let Some((hpa, w)) = npt.translate(gpa & !0xFFF) {
                if !write || w {
                    self.redundant_npf += 1;
                    if self.redundant_npf <= 3 {
                        log!("npf: redundant fault gpa {:#x} write={} mapped to {:#x} (w={}), state {:?}", gpa, write, hpa, w, self.state(page));
                    }
                }
            }
        }
        if write {
            if let PageState::Private { clean_block: Some(_), .. } = self.state(page) {
                self.dirty_faults += 1;
            }
            let f = self.private_frame(page).await?;
            self.map(page, f, true)
        } else {
            self.ro_maps += 1;
            match self.state(page) {
                PageState::Zero => self.map(page, zero_frame_pa(), false),
                PageState::Template(t) => self.map(page, t, false),
                PageState::Private { frame, clean_block } => {
                    // Clean pages stay read-only so a later write is noticed.
                    self.map(page, frame, clean_block.is_none())
                }
                PageState::Swapped(b) => {
                    let f = self.swap_in(page, b).await?;
                    self.map(page, f, false)
                }
            }
        }
    }

    pub async fn read(&mut self, gpa: u64, buf: &mut [u8]) -> Result<(), &'static str> {
        if gpa.checked_add(buf.len() as u64).ok_or("address overflow")? > self.mem_size() {
            return Err("guest buffer outside memory");
        }
        let mut done = 0usize;
        while done < buf.len() {
            let addr = gpa + done as u64;
            let page = (addr / PAGE_SIZE as u64) as usize;
            let off = (addr % PAGE_SIZE as u64) as usize;
            let n = (PAGE_SIZE - off).min(buf.len() - done);
            match self.readable_frame(page).await? {
                Some(f) => buf[done..done + n].copy_from_slice(&frame_slice(f)[off..off + n]),
                None => buf[done..done + n].fill(0),
            }
            done += n;
        }
        Ok(())
    }

    pub async fn write(&mut self, gpa: u64, data: &[u8]) -> Result<(), &'static str> {
        if gpa.checked_add(data.len() as u64).ok_or("address overflow")? > self.mem_size() {
            return Err("guest buffer outside memory");
        }
        let mut done = 0usize;
        while done < data.len() {
            let addr = gpa + done as u64;
            let page = (addr / PAGE_SIZE as u64) as usize;
            let off = (addr % PAGE_SIZE as u64) as usize;
            let n = (PAGE_SIZE - off).min(data.len() - done);
            let f = self.private_frame(page).await?;
            frame_slice(f)[off..off + n].copy_from_slice(&data[done..done + n]);
            // If the NPT currently maps this page read-only (or to a shared
            // frame), refresh it so the guest sees the private copy.
            if let Some(npt) = self.npt.as_mut() {
                let g = (page * PAGE_SIZE) as u64;
                if let Some((hpa, w)) = npt.translate(g) {
                    if hpa != f || !w {
                        npt.map(g, f, true);
                        self.needs_flush = true;
                    }
                }
            }
            done += n;
        }
        Ok(())
    }

    pub async fn read_string(&mut self, gpa: u64, len: u64) -> Result<String, &'static str> {
        let len = len.min(4096) as usize;
        let mut buf = alloc::vec![0u8; len];
        self.read(gpa, &mut buf).await?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    /// Evict every private page to the page store and drop the nested page
    /// tables.  Afterwards the VM holds no guest memory frames at all.
    pub async fn freeze(&mut self) -> Result<FreezeReport, &'static str> {
        let mut rep = FreezeReport::default();
        let store = crate::disk::store();
        // Pages that are still identical to their disk block are dropped for
        // free; dirty pages are written out in concurrent batches.
        let mut dirty: Vec<(usize, u64)> = Vec::new();
        let private: Vec<(u32, u64, Option<u32>)> = self
            .overlay
            .iter()
            .filter_map(|(&p, s)| match *s {
                PageState::Private { frame, clean_block } => Some((p, frame, clean_block)),
                _ => None,
            })
            .collect();
        for (page, frame, clean_block) in private {
            match (clean_block, store) {
                (Some(b), _) => {
                    frame::free(frame);
                    self.set_state(page as usize, PageState::Swapped(b));
                    self.resident -= 1;
                    self.swapped += 1;
                    rep.dropped_clean += 1;
                }
                (None, Some(_)) => dirty.push((page as usize, frame)),
                (None, None) => rep.kept_resident += 1,
            }
        }
        if let Some(st) = store {
            const BATCH: usize = 32;
            for chunk in dirty.chunks(BATCH) {
                let futs: Vec<core::pin::Pin<alloc::boxed::Box<dyn core::future::Future<Output = Option<u32>> + Send + '_>>> =
                    chunk.iter().map(|&(_, frame)| -> core::pin::Pin<alloc::boxed::Box<dyn core::future::Future<Output = Option<u32>> + Send + '_>> {
                        alloc::boxed::Box::pin(st.store_frame(frame))
                    }).collect();
                let results = crate::task::join_all(futs).await;
                for (&(page, frame), blk) in chunk.iter().zip(results) {
                    match blk {
                        Some(b) => {
                            frame::free(frame);
                            self.set_state(page, PageState::Swapped(b));
                            self.resident -= 1;
                            self.swapped += 1;
                            self.pages_written += 1;
                            rep.written += 1;
                        }
                        None => rep.kept_resident += 1,
                    }
                }
            }
        }
        if let Some(npt) = self.npt.take() {
            rep.npt_frames_freed = npt.table_frames();
            npt.destroy();
        }
        Ok(rep)
    }

    /// Drop the nested page tables of a VM that has nothing private yet: a
    /// clone created straight into the frozen state.
    pub fn start_frozen(&mut self) {
        if let Some(npt) = self.npt.take() {
            npt.destroy();
        }
    }

    /// Map every template page the VM has not overridden read-only into the
    /// nested page tables in one go, so a thawed or freshly cloned guest does
    /// not take one #NPF per page it reads.  Returns the number mapped.
    pub fn prefault(&mut self) -> usize {
        let t = self.template.clone();
        let mut n = 0;
        for (&p, &f) in t.pages.iter() {
            if self.overlay.contains_key(&p) {
                continue;
            }
            match self.npt.as_mut() {
                Some(npt) => {
                    if !npt.map(p as u64 * PAGE_SIZE as u64, f, false) {
                        break;
                    }
                    n += 1;
                }
                None => break,
            }
        }
        if n > 0 {
            self.needs_flush = true;
        }
        n
    }

    /// Re-create the (empty) nested page tables; pages come back on demand.
    pub fn thaw(&mut self) -> Result<(), &'static str> {
        if self.npt.is_none() {
            self.npt = Some(Npt::new().ok_or("out of memory")?);
            self.needs_flush = true;
        }
        Ok(())
    }

    /// Bring every swapped page back, issuing the disk reads in concurrent
    /// batches, and map the frames read-only if the NPT exists.  Used before
    /// a snapshot and, as a prefetch, when thawing: the pages a frozen VM
    /// owns are exactly the ones it dirtied last time, so it will want them
    /// again, and one batch of reads beats hundreds of faults each waiting
    /// for its own read.
    pub async fn load_all(&mut self, writable: bool) -> Result<usize, &'static str> {
        let swapped: Vec<(u32, u32)> = self
            .overlay
            .iter()
            .filter_map(|(&p, s)| match *s {
                PageState::Swapped(b) => Some((p, b)),
                _ => None,
            })
            .collect();
        let n = swapped.len();
        if n == 0 {
            return Ok(0);
        }
        let store = crate::disk::store().ok_or("page store unavailable")?;
        // One virtio queue of reads at a time.
        const BATCH: usize = 192;
        for chunk in swapped.chunks(BATCH) {
            let mut frames = Vec::with_capacity(chunk.len());
            for _ in chunk {
                frames.push(frame::alloc().ok_or("out of memory")?);
            }
            let futs: Vec<core::pin::Pin<alloc::boxed::Box<dyn core::future::Future<Output = bool> + Send + '_>>> = chunk
                .iter()
                .zip(frames.iter())
                .map(|(&(_, b), &f)| -> core::pin::Pin<alloc::boxed::Box<dyn core::future::Future<Output = bool> + Send + '_>> {
                    alloc::boxed::Box::pin(store.load_frame(b, f))
                })
                .collect();
            let results = crate::task::join_all(futs).await;
            for ((&(page, b), &f), ok) in chunk.iter().zip(frames.iter()).zip(results) {
                if !ok {
                    frame::free(f);
                    return Err("disk read failed");
                }
                self.swapped -= 1;
                self.resident += 1;
                self.pages_loaded += 1;
                if writable {
                    // The VM dirtied these pages last time and will again:
                    // map them writable now (no fault later) and let the next
                    // freeze write them out afresh.
                    store.free_block(b);
                    self.set_state(page as usize, PageState::Private { frame: f, clean_block: None });
                    if self.npt.is_some() {
                        let _ = self.map(page as usize, f, true);
                    }
                } else {
                    self.set_state(page as usize, PageState::Private { frame: f, clean_block: Some(b) });
                    if self.npt.is_some() {
                        let _ = self.map(page as usize, f, false);
                    }
                }
            }
        }
        Ok(n)
    }

    /// Copy the given template pages into private, writable frames now, so
    /// the guest's first writes to them do not fault.  Pages already in the
    /// overlay are left alone.  Returns the number copied.
    pub fn eager_cow(&mut self, pages: &[u32]) -> usize {
        let t = self.template.clone();
        let mut n = 0;
        for &p in pages {
            if (p as usize) >= self.npages || self.overlay.contains_key(&p) {
                continue;
            }
            let f = match t.pages.get(&p) {
                Some(&src) => match frame::alloc() {
                    Some(f) => {
                        copy_frame(f, src);
                        self.cow_copies += 1;
                        f
                    }
                    None => break,
                },
                None => match frame::alloc_zeroed() {
                    Some(f) => {
                        self.zero_allocs += 1;
                        f
                    }
                    None => break,
                },
            };
            self.set_state(p as usize, PageState::Private { frame: f, clean_block: None });
            self.resident += 1;
            if self.npt.is_some() && self.map(p as usize, f, true).is_err() {
                break;
            }
            n += 1;
        }
        n
    }

    /// Guest pages the VM has written (its overlay), for learning which
    /// pages a clone touches after it resumes.
    pub fn overlay_pages(&self) -> Vec<u32> {
        self.overlay.keys().copied().collect()
    }

    /// Turn this VM's current memory into a new template's page set: the
    /// template's own pages plus every private frame, which the template now
    /// owns.  The VM is left with no private pages (they are shared, copy on
    /// write, from here on).  Returns the page map and how many frames moved.
    /// Requires that nothing is swapped out (call `load_all` first).
    pub fn snapshot_pages(&mut self) -> Result<(BTreeMap<u32, u64>, usize), &'static str> {
        if self.overlay.values().any(|s| matches!(s, PageState::Swapped(_))) {
            return Err("swapped pages remain");
        }
        let mut pages = self.template.pages.clone();
        let mut moved = 0;
        let store = crate::disk::store();
        for (&p, s) in self.overlay.iter() {
            if let PageState::Private { frame, clean_block } = *s {
                pages.insert(p, frame);
                if let (Some(b), Some(st)) = (clean_block, store) {
                    st.free_block(b);
                }
                moved += 1;
            }
        }
        self.overlay.clear();
        self.resident -= moved;
        Ok((pages, moved))
    }

    /// Continue on top of a new template (after `snapshot_pages`): every
    /// mapping is dropped so formerly private pages become read-only shared
    /// frames on their next touch.
    pub fn rebase(&mut self, template: Arc<Template>) -> Result<(), &'static str> {
        self.template = template;
        if let Some(npt) = self.npt.take() {
            npt.destroy();
        }
        self.npt = Some(Npt::new().ok_or("out of memory")?);
        self.needs_flush = true;
        Ok(())
    }

    /// Free all frames and disk blocks.
    pub fn release(&mut self) {
        let store = crate::disk::store();
        for (_, s) in core::mem::take(&mut self.overlay) {
            match s {
                PageState::Private { frame, clean_block } => {
                    frame::free(frame);
                    if let (Some(b), Some(st)) = (clean_block, store) {
                        st.free_block(b);
                    }
                }
                PageState::Swapped(b) => {
                    if let Some(st) = store {
                        st.free_block(b);
                    }
                }
                _ => {}
            }
        }
        self.resident = 0;
        self.swapped = 0;
        if let Some(npt) = self.npt.take() {
            npt.destroy();
        }
    }

    /// Bytes of host memory this VM's guest pages currently occupy.
    pub fn resident_bytes(&self) -> u64 {
        (self.resident * PAGE_SIZE) as u64
    }

    #[allow(dead_code)]
    pub fn zero_page_for_tests() -> u64 {
        zero_frame_pa()
    }
}

impl Drop for GuestMemory {
    fn drop(&mut self) {
        self.release();
    }
}

#[allow(dead_code)]
pub fn zero_frame_is(pa: u64) -> bool {
    let s = frame_slice(pa);
    s.iter().all(|&b| b == 0)
}

#[allow(dead_code)]
pub fn scrub(pa: u64) {
    zero_frame(pa)
}
