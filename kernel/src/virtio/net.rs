//! virtio-net driver (legacy transport, no mergeable buffers).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use super::queue::{Buf, Virtqueue};
use super::VirtioDevice;
use crate::arch::idt;
use crate::mm::frame;
use crate::net::{Mac, NetDevice};
use crate::pci::PciDevice;
use crate::sync::{OnceCell, SpinLock};
use crate::task::Notify;

const F_MAC: u32 = 1 << 5;
const F_STATUS: u32 = 1 << 16;

const HDR_LEN: u32 = 10;
/// Offset of the packet within a 2 KiB buffer; the header lives at 0.
const DATA_OFF: u64 = 16;
const BUF_SIZE: u64 = 2048;
const RX_BUFS: usize = 128;
const TX_BUFS: usize = 64;
pub const MAX_FRAME: usize = 1514;

struct RxState {
    q: Virtqueue,
    bufs: Vec<u64>,
    head_to_buf: Vec<u16>,
}

struct TxState {
    q: Virtqueue,
    bufs: Vec<u64>,
    head_to_buf: Vec<u16>,
    free: Vec<u16>,
}

#[derive(Default)]
pub struct Stats {
    pub rx_frames: AtomicU64,
    pub tx_frames: AtomicU64,
    pub rx_irqs: AtomicU64,
    pub tx_drops: AtomicU64,
}

pub struct VirtioNet {
    dev: VirtioDevice,
    rx: SpinLock<RxState>,
    tx: SpinLock<TxState>,
    mac: Mac,
    rx_notify: Notify,
    pub stats: Stats,
}

static DEVICE: OnceCell<Arc<VirtioNet>> = OnceCell::new();

pub fn device() -> Option<&'static Arc<VirtioNet>> {
    DEVICE.get()
}

fn alloc_bufs(count: usize) -> Vec<u64> {
    let mut v = Vec::with_capacity(count);
    for _ in 0..count / 2 {
        let f = frame::alloc_zeroed().expect("virtio-net buffers");
        v.push(f);
        v.push(f + BUF_SIZE);
    }
    v
}

fn post_rx(rx: &mut RxState, buf: u16) {
    let pa = rx.bufs[buf as usize];
    let head = rx
        .q
        .add(&[
            Buf { addr: pa, len: HDR_LEN, write: true },
            Buf { addr: pa + DATA_OFF, len: (BUF_SIZE - DATA_OFF) as u32, write: true },
        ])
        .expect("rx queue full");
    rx.head_to_buf[head as usize] = buf;
}

fn rx_irq(_f: &mut idt::TrapFrame) {
    if let Some(d) = DEVICE.get() {
        if !d.dev.has_msix() {
            d.dev.isr();
        }
        d.stats.rx_irqs.fetch_add(1, Ordering::Relaxed);
        d.rx_notify.notify_one();
    }
}

fn tx_irq(_f: &mut idt::TrapFrame) {
    if let Some(d) = DEVICE.get() {
        if !d.dev.has_msix() {
            d.dev.isr();
        }
        // Shared INTx: this may really be an RX interrupt.
        d.rx_notify.notify_one();
    }
}

pub fn probe(pci: &PciDevice) {
    let mut dev = match VirtioDevice::new(pci) {
        Some(d) => d,
        None => {
            log!("virtio-net {}: no I/O BAR", pci.addr);
            return;
        }
    };
    let features = dev.negotiate(F_MAC | F_STATUS);
    let mut mac = [0u8; 6];
    for (i, b) in mac.iter_mut().enumerate() {
        *b = dev.config_read8(i as u16);
    }
    let rxq = match dev.setup_queue(0) {
        Some(q) => q,
        None => {
            log!("virtio-net: no rx queue");
            return;
        }
    };
    let mut txq = match dev.setup_queue(1) {
        Some(q) => q,
        None => {
            log!("virtio-net: no tx queue");
            return;
        }
    };
    txq.set_no_interrupt(true);
    let vectors = dev.setup_interrupts(2);
    idt::register_handler(vectors[0], rx_irq);
    if vectors[1] != vectors[0] {
        idt::register_handler(vectors[1], tx_irq);
    }

    let rx_count = RX_BUFS.min(rxq.size() as usize / 2);
    let tx_count = TX_BUFS.min(txq.size() as usize / 2);
    let mut rx = RxState { bufs: alloc_bufs(rx_count), head_to_buf: alloc::vec![0; rxq.size() as usize], q: rxq };
    for i in 0..rx_count {
        post_rx(&mut rx, i as u16);
    }
    rx.q.kick();
    let tx = TxState {
        bufs: alloc_bufs(tx_count),
        head_to_buf: alloc::vec![0; txq.size() as usize],
        free: (0..tx_count as u16).collect(),
        q: txq,
    };

    dev.driver_ok();
    log!(
        "virtio-net {}: mac {} features {:#x} rx/tx queues {}/{} {}",
        pci.addr,
        Mac(mac),
        features,
        rx.q.size(),
        tx.q.size(),
        if dev.has_msix() { "msi-x" } else { "intx" }
    );
    let net = VirtioNet {
        dev,
        rx: SpinLock::new(rx),
        tx: SpinLock::new(tx),
        mac: Mac(mac),
        rx_notify: Notify::new(),
        stats: Stats::default(),
    };
    DEVICE.init(Arc::new(net));
}

impl VirtioNet {
    fn reclaim_tx(tx: &mut TxState) {
        while let Some((head, _)) = tx.q.pop_used() {
            tx.free.push(tx.head_to_buf[head as usize]);
        }
    }

    pub fn link_up(&self) -> bool {
        if self.dev.features & F_STATUS != 0 {
            self.dev.config_read16(6) & 1 != 0
        } else {
            true
        }
    }
}

impl NetDevice for VirtioNet {
    fn mac(&self) -> Mac {
        self.mac
    }

    fn send(&self, frame: &[u8]) -> bool {
        if frame.len() > MAX_FRAME || frame.is_empty() {
            return false;
        }
        let mut tx = self.tx.lock();
        Self::reclaim_tx(&mut tx);
        let buf = match tx.free.pop() {
            Some(b) => b,
            None => {
                self.stats.tx_drops.fetch_add(1, Ordering::Relaxed);
                return false;
            }
        };
        let pa = tx.bufs[buf as usize];
        unsafe {
            core::ptr::write_bytes(pa as *mut u8, 0, HDR_LEN as usize);
            core::ptr::copy_nonoverlapping(frame.as_ptr(), (pa + DATA_OFF) as *mut u8, frame.len());
        }
        // Pad short frames to the Ethernet minimum.
        let len = frame.len().max(60);
        if len > frame.len() {
            unsafe { core::ptr::write_bytes((pa + DATA_OFF) as *mut u8, 0, 0) };
            unsafe {
                core::ptr::write_bytes((pa + DATA_OFF + frame.len() as u64) as *mut u8, 0, len - frame.len())
            };
        }
        match tx.q.add(&[
            Buf { addr: pa, len: HDR_LEN, write: false },
            Buf { addr: pa + DATA_OFF, len: len as u32, write: false },
        ]) {
            Some(head) => {
                tx.head_to_buf[head as usize] = buf;
                tx.q.kick();
                self.stats.tx_frames.fetch_add(1, Ordering::Relaxed);
                true
            }
            None => {
                tx.free.push(buf);
                self.stats.tx_drops.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    fn recv(&self) -> Option<Vec<u8>> {
        let mut rx = self.rx.lock();
        let (head, len) = rx.q.pop_used()?;
        let buf = rx.head_to_buf[head as usize];
        let pa = rx.bufs[buf as usize];
        let data_len = (len as usize).saturating_sub(HDR_LEN as usize).min((BUF_SIZE - DATA_OFF) as usize);
        let frame = unsafe { core::slice::from_raw_parts((pa + DATA_OFF) as *const u8, data_len) }.to_vec();
        post_rx(&mut rx, buf);
        rx.q.kick();
        self.stats.rx_frames.fetch_add(1, Ordering::Relaxed);
        Some(frame)
    }

    fn rx_notify(&self) -> &Notify {
        &self.rx_notify
    }
}
