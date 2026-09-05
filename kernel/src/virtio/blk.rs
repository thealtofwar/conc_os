//! virtio-blk driver (legacy transport) with an async request interface.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU64, Ordering};
use core::task::{Context, Poll, Waker};

use super::queue::{Buf, Virtqueue};
use super::VirtioDevice;
use crate::arch::idt;
use crate::pci::PciDevice;
use crate::sync::{OnceCell, SpinLock};
use crate::task::WaitQueue;

pub const SECTOR_SIZE: usize = 512;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlkError {
    Busy,
    IoError,
    Unsupported,
    OutOfRange,
    BadLength,
}

#[repr(C)]
struct ReqHdr {
    ty: u32,
    reserved: u32,
    sector: u64,
}

#[repr(C, align(16))]
struct ReqMem {
    hdr: ReqHdr,
    status: u8,
}

/// A request that has been handed to the device and not yet completed.
struct Inflight {
    id: u64,
    waker: Option<Waker>,
    mem: Box<ReqMem>,
}

struct State {
    q: Virtqueue,
    /// Indexed by head descriptor.
    inflight: Vec<Option<Inflight>>,
    /// Completed requests waiting to be observed: id -> status byte.
    done: BTreeMap<u64, u8>,
    next_id: u64,
}

#[derive(Default)]
pub struct Stats {
    pub reads: AtomicU64,
    pub writes: AtomicU64,
    pub irqs: AtomicU64,
    pub errors: AtomicU64,
    pub queue_full_waits: AtomicU64,
}

pub struct VirtioBlk {
    dev: VirtioDevice,
    state: SpinLock<State>,
    /// Woken whenever descriptors are returned to the free list.
    space: WaitQueue,
    pub capacity_sectors: u64,
    pub stats: Stats,
}

static DEVICE: OnceCell<Arc<VirtioBlk>> = OnceCell::new();

pub fn device() -> Option<&'static Arc<VirtioBlk>> {
    DEVICE.get()
}

fn irq(_f: &mut idt::TrapFrame) {
    if let Some(d) = DEVICE.get() {
        if !d.dev.has_msix() {
            d.dev.isr();
        }
        d.stats.irqs.fetch_add(1, Ordering::Relaxed);
        d.complete();
    }
}

pub fn probe(pci: &PciDevice) {
    let mut dev = match VirtioDevice::new(pci) {
        Some(d) => d,
        None => {
            log!("virtio-blk {}: no I/O BAR", pci.addr);
            return;
        }
    };
    dev.negotiate(0);
    let capacity = dev.config_read64(0);
    let q = match dev.setup_queue(0) {
        Some(q) => q,
        None => {
            log!("virtio-blk: no request queue");
            return;
        }
    };
    let vectors = dev.setup_interrupts(1);
    idt::register_handler(vectors[0], irq);
    let size = q.size() as usize;
    dev.driver_ok();
    log!(
        "virtio-blk {}: {} sectors ({}) queue {} {}",
        pci.addr,
        capacity,
        crate::mm::Bytes(capacity * SECTOR_SIZE as u64),
        size,
        if dev.has_msix() { "msi-x" } else { "intx" }
    );
    let mut inflight = Vec::with_capacity(size);
    inflight.resize_with(size, || None);
    DEVICE.init(Arc::new(VirtioBlk {
        dev,
        state: SpinLock::new(State { q, inflight, done: BTreeMap::new(), next_id: 1 }),
        space: WaitQueue::new(),
        capacity_sectors: capacity,
        stats: Stats::default(),
    }));
}

impl VirtioBlk {
    /// Move finished requests from the used ring to the `done` map.
    fn complete(&self) {
        let mut wakers: Vec<Waker> = Vec::new();
        let mut freed = false;
        {
            let mut st = self.state.lock();
            while let Some((head, _)) = st.q.pop_used() {
                freed = true;
                if let Some(mut r) = st.inflight[head as usize].take() {
                    let status = r.mem.status;
                    st.done.insert(r.id, status);
                    if let Some(w) = r.waker.take() {
                        wakers.push(w);
                    }
                }
            }
        }
        for w in wakers {
            w.wake();
        }
        if freed {
            self.space.wake_all();
        }
    }

    fn try_submit(&self, ty: u32, sector: u64, buf_pa: u64, len: u32) -> Result<(u16, u64), BlkError> {
        let mem = Box::new(ReqMem { hdr: ReqHdr { ty, reserved: 0, sector }, status: 0xFF });
        let hdr_pa = &mem.hdr as *const ReqHdr as u64;
        let status_pa = &mem.status as *const u8 as u64;
        let mut st = self.state.lock();
        let mut bufs = [
            Buf { addr: hdr_pa, len: 16, write: false },
            Buf { addr: buf_pa, len, write: ty == T_IN },
            Buf { addr: status_pa, len: 1, write: true },
        ];
        let chain: &[Buf] = if len == 0 {
            bufs[1] = bufs[2];
            &bufs[..2]
        } else {
            &bufs[..]
        };
        let head = st.q.add(chain).ok_or(BlkError::Busy)?;
        let id = st.next_id;
        st.next_id += 1;
        st.inflight[head as usize] = Some(Inflight { id, waker: None, mem });
        st.q.kick();
        Ok((head, id))
    }

    /// Submit, waiting for descriptors when the ring is full.
    async fn submit(&self, ty: u32, sector: u64, buf_pa: u64, len: u32) -> Result<(u16, u64), BlkError> {
        loop {
            match self.try_submit(ty, sector, buf_pa, len) {
                Err(BlkError::Busy) => {
                    self.stats.queue_full_waits.fetch_add(1, Ordering::Relaxed);
                    // Completions may already be sitting in the used ring.
                    self.complete();
                    let wait = self.space.wait();
                    // Re-check after registering so a completion that raced
                    // with us is not missed.
                    match self.try_submit(ty, sector, buf_pa, len) {
                        Err(BlkError::Busy) => wait.await,
                        other => return other,
                    }
                }
                other => return other,
            }
        }
    }

    fn check(&self, sector: u64, len: usize) -> Result<(), BlkError> {
        if len == 0 || len % SECTOR_SIZE != 0 {
            return Err(BlkError::BadLength);
        }
        if sector + (len / SECTOR_SIZE) as u64 > self.capacity_sectors {
            return Err(BlkError::OutOfRange);
        }
        Ok(())
    }

    /// Read whole sectors into `buf` (which must be physically contiguous,
    /// i.e. any kernel heap allocation).  The buffer must outlive the
    /// request: do not drop this future before it completes.
    pub async fn read_sectors(&self, sector: u64, buf: &mut [u8]) -> Result<(), BlkError> {
        self.check(sector, buf.len())?;
        let (head, id) = self.submit(T_IN, sector, buf.as_mut_ptr() as u64, buf.len() as u32).await?;
        self.stats.reads.fetch_add(1, Ordering::Relaxed);
        Completion { blk: self, head, id }.await
    }

    pub async fn write_sectors(&self, sector: u64, buf: &[u8]) -> Result<(), BlkError> {
        self.check(sector, buf.len())?;
        let (head, id) = self.submit(T_OUT, sector, buf.as_ptr() as u64, buf.len() as u32).await?;
        self.stats.writes.fetch_add(1, Ordering::Relaxed);
        Completion { blk: self, head, id }.await
    }

    pub async fn flush(&self) -> Result<(), BlkError> {
        let (head, id) = self.submit(T_FLUSH, 0, 0, 0).await?;
        Completion { blk: self, head, id }.await
    }

    pub fn has_msix(&self) -> bool {
        self.dev.has_msix()
    }

    /// Requests currently owned by the device.
    pub fn inflight(&self) -> usize {
        self.state.lock().inflight.iter().filter(|s| s.is_some()).count()
    }
}

struct Completion<'a> {
    blk: &'a VirtioBlk,
    head: u16,
    id: u64,
}

impl Future for Completion<'_> {
    type Output = Result<(), BlkError>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut st = self.blk.state.lock();
        if let Some(status) = st.done.remove(&self.id) {
            drop(st);
            return Poll::Ready(match status {
                0 => Ok(()),
                2 => {
                    self.blk.stats.errors.fetch_add(1, Ordering::Relaxed);
                    Err(BlkError::Unsupported)
                }
                _ => {
                    self.blk.stats.errors.fetch_add(1, Ordering::Relaxed);
                    Err(BlkError::IoError)
                }
            });
        }
        match st.inflight[self.head as usize].as_mut() {
            Some(r) if r.id == self.id => r.waker = Some(cx.waker().clone()),
            _ => panic!("virtio-blk: request {} lost", self.id),
        }
        Poll::Pending
    }
}
