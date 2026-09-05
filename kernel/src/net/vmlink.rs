//! A point-to-point Ethernet link between the host network stack and one
//! Linux VM.  The VM side is a virtio-mmio network device driven by the vCPU
//! task; the host side is a `NetDevice` owned by a per-VM `Interface`.
//!
//! Frames from the guest are handed to the interface synchronously (the vCPU
//! task runs the stack for them); frames to the guest are queued and the vCPU
//! is nudged to deliver them, which also thaws a frozen VM.

use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::{Interface, Mac, NetDevice};
use crate::hv::vm::VmHandle;
use crate::sync::SpinLock;
use crate::task::Notify;

const QUEUE_CAP: usize = 512;

pub struct VmLink {
    handle: Arc<VmHandle>,
    /// MAC of the host side of the link.
    pub mac: Mac,
    /// MAC the guest's virtio device reports.
    pub guest_mac: Mac,
    iface: SpinLock<Option<Weak<Interface>>>,
    iface_id: AtomicU32,
    to_guest: SpinLock<VecDeque<Vec<u8>>>,
    from_guest: SpinLock<VecDeque<Vec<u8>>>,
    rx_notify: Notify,
    pub sent_to_guest: AtomicU64,
    pub received_from_guest: AtomicU64,
    pub dropped: AtomicU64,
}

impl VmLink {
    pub fn new(handle: Arc<VmHandle>) -> Arc<VmLink> {
        let id = handle.id;
        Arc::new(VmLink {
            handle,
            mac: Mac([0x52, 0x54, 0x00, 0xC0, 0x00, 0x01]),
            guest_mac: Mac([0x52, 0x54, 0x00, 0xC0, (id >> 8) as u8, id as u8]),
            iface: SpinLock::new(None),
            iface_id: AtomicU32::new(u32::MAX),
            to_guest: SpinLock::new(VecDeque::new()),
            from_guest: SpinLock::new(VecDeque::new()),
            rx_notify: Notify::new(),
            sent_to_guest: AtomicU64::new(0),
            received_from_guest: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        })
    }

    /// Bind the link to the interface built on top of it.
    pub fn attach(&self, iface: &Arc<Interface>) {
        *self.iface.lock() = Some(Arc::downgrade(iface));
        self.iface_id.store(iface.id, Ordering::Release);
    }

    pub fn iface_id(&self) -> u32 {
        self.iface_id.load(Ordering::Acquire)
    }

    pub fn interface(&self) -> Option<Arc<Interface>> {
        self.iface.lock().as_ref().and_then(|w| w.upgrade())
    }

    /// Frame transmitted by the guest (called by the virtio device model on
    /// the vCPU task).  Runs the network stack for it right away.
    pub fn push_from_guest(&self, frame: Vec<u8>) {
        self.received_from_guest.fetch_add(1, Ordering::Relaxed);
        // Guest-originated frames do not count as activity for the idle
        // policy: background chatter must not keep a VM from freezing.
        // Anything a client sends arrives on the other direction.
        if let Some(iface) = self.interface() {
            iface.handle_frame(&frame);
            return;
        }
        let mut q = self.from_guest.lock();
        if q.len() >= QUEUE_CAP {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        q.push_back(frame);
        drop(q);
        self.rx_notify.notify_one();
    }

    /// Next frame waiting for the guest.
    pub fn pop_to_guest(&self) -> Option<Vec<u8>> {
        self.to_guest.lock().pop_front()
    }

    pub fn has_to_guest(&self) -> bool {
        !self.to_guest.lock().is_empty()
    }

    pub fn pending_to_guest(&self) -> usize {
        self.to_guest.lock().len()
    }

    pub fn vm_id(&self) -> u32 {
        self.handle.id
    }
}

impl NetDevice for VmLink {
    fn mac(&self) -> Mac {
        self.mac
    }

    fn send(&self, frame: &[u8]) -> bool {
        {
            let mut q = self.to_guest.lock();
            if q.len() >= QUEUE_CAP {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            q.push_back(frame.to_vec());
        }
        self.sent_to_guest.fetch_add(1, Ordering::Relaxed);
        // Network traffic is activity: it keeps the idle-freeze policy away
        // while a connection is in progress.
        self.handle.touch_from(1);
        self.handle.extra_work.store(true, Ordering::Release);
        self.handle.notify.notify_one();
        true
    }

    fn recv(&self) -> Option<Vec<u8>> {
        self.from_guest.lock().pop_front()
    }

    fn rx_notify(&self) -> &Notify {
        &self.rx_notify
    }

    fn inline_rx(&self) -> bool {
        true
    }
}
