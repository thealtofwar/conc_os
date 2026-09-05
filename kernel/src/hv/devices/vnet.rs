//! virtio-mmio network device (virtio 1.0 "modern" MMIO transport) that
//! connects a Linux guest to a per-VM link into the conc_os network stack.
//!
//! The guest finds it through `virtio_mmio.device=4K@0xd0000000:5` on its
//! command line; no ACPI or device tree is needed.

use alloc::sync::Arc;
use alloc::vec::Vec;

use super::vq::DeviceQueue;
use crate::hv::memory::GuestMemory;
use crate::net::vmlink::VmLink;

pub const VNET_BASE: u64 = 0xD000_0000;
pub const VNET_SIZE: u64 = 0x1000;
pub const VNET_IRQ: u8 = 5;

const MAGIC: u32 = 0x7472_6976; // "virt"
const VERSION: u32 = 2;
const DEVICE_ID_NET: u32 = 1;
const VENDOR_ID: u32 = 0x434F_4E43; // "CONC"

const F_MAC: u64 = 1 << 5;
const F_STATUS: u64 = 1 << 16;
const F_VERSION_1: u64 = 1 << 32;
const DEVICE_FEATURES: u64 = F_MAC | F_STATUS | F_VERSION_1;

const STATUS_FEATURES_OK: u32 = 8;
const STATUS_DRIVER_OK: u32 = 4;

const QUEUE_MAX: u16 = 256;
const RX: usize = 0;
const TX: usize = 1;
/// virtio_net_hdr_v1 size (VERSION_1 negotiated).
const HDR_LEN: usize = 12;
const MAX_FRAME: usize = 1514;

#[derive(Clone)]
pub struct VirtioMmioNet {
    link: Arc<VmLink>,
    mac: [u8; 6],
    status: u32,
    dev_feat_sel: u32,
    drv_feat_sel: u32,
    driver_features: u64,
    queue_sel: u32,
    queues: [DeviceQueue; 2],
    isr: u32,
    /// Queue index the guest notified; serviced by the vCPU with guest memory.
    pub kicked: Option<u32>,
    pub tx_frames: u64,
    pub rx_frames: u64,
    pub rx_dropped: u64,
    pub irqs: u64,
}

impl VirtioMmioNet {
    pub fn new(link: Arc<VmLink>) -> Self {
        let mac = link.guest_mac.0;
        VirtioMmioNet {
            link,
            mac,
            status: 0,
            dev_feat_sel: 0,
            drv_feat_sel: 0,
            driver_features: 0,
            queue_sel: 0,
            queues: [DeviceQueue::new(QUEUE_MAX), DeviceQueue::new(QUEUE_MAX)],
            isr: 0,
            kicked: None,
            tx_frames: 0,
            rx_frames: 0,
            rx_dropped: 0,
            irqs: 0,
        }
    }

    pub fn link(&self) -> &Arc<VmLink> {
        &self.link
    }

    /// Replace the link (used when a VM is forked from a snapshot).
    pub fn set_link(&mut self, link: Arc<VmLink>) {
        self.link = link;
    }

    pub fn contains(gpa: u64) -> bool {
        gpa >= VNET_BASE && gpa < VNET_BASE + VNET_SIZE
    }

    fn reset(&mut self) {
        self.status = 0;
        self.driver_features = 0;
        self.queue_sel = 0;
        self.isr = 0;
        self.kicked = None;
        for q in self.queues.iter_mut() {
            q.reset();
        }
    }

    fn driver_ok(&self) -> bool {
        self.status & STATUS_DRIVER_OK != 0
    }

    pub fn irq_level(&self) -> bool {
        self.isr != 0
    }

    fn queue_mut(&mut self) -> Option<&mut DeviceQueue> {
        self.queues.get_mut(self.queue_sel as usize)
    }

    pub fn mmio_read(&mut self, off: u64, size: u8) -> u32 {
        let v = match off {
            0x000 => MAGIC,
            0x004 => VERSION,
            0x008 => DEVICE_ID_NET,
            0x00C => VENDOR_ID,
            0x010 => {
                if self.dev_feat_sel == 0 {
                    DEVICE_FEATURES as u32
                } else if self.dev_feat_sel == 1 {
                    (DEVICE_FEATURES >> 32) as u32
                } else {
                    0
                }
            }
            0x034 => QUEUE_MAX as u32,
            0x044 => self.queues.get(self.queue_sel as usize).map(|q| q.ready as u32).unwrap_or(0),
            0x060 => self.isr,
            0x070 => self.status,
            0x0FC => 0, // config generation
            0x100..=0x105 => self.mac[(off - 0x100) as usize] as u32,
            0x106 => 1, // status: link up
            0x107 => 0,
            0x108 | 0x109 => {
                // max_virtqueue_pairs = 1
                if off == 0x108 {
                    1
                } else {
                    0
                }
            }
            _ => 0,
        };
        match size {
            1 => v & 0xFF,
            2 => v & 0xFFFF,
            _ => v,
        }
    }

    pub fn mmio_write(&mut self, off: u64, _size: u8, v: u32) {
        match off {
            0x014 => self.dev_feat_sel = v,
            0x020 => {
                if self.drv_feat_sel == 0 {
                    self.driver_features = (self.driver_features & !0xFFFF_FFFF) | v as u64;
                } else if self.drv_feat_sel == 1 {
                    self.driver_features = (self.driver_features & 0xFFFF_FFFF) | ((v as u64) << 32);
                }
            }
            0x024 => self.drv_feat_sel = v,
            0x030 => self.queue_sel = v,
            0x038 => {
                if let Some(q) = self.queue_mut() {
                    q.size = (v as u16).min(QUEUE_MAX);
                }
            }
            0x044 => {
                if let Some(q) = self.queue_mut() {
                    q.ready = v & 1 != 0;
                    if q.ready {
                        q.last_avail = 0;
                        q.used_idx = 0;
                    }
                }
            }
            0x050 => {
                if v < 2 {
                    self.kicked = Some(v);
                }
            }
            0x064 => self.isr &= !v,
            0x070 => {
                if v == 0 {
                    self.reset();
                } else {
                    let mut s = v;
                    if s & STATUS_FEATURES_OK != 0 && self.driver_features & !DEVICE_FEATURES != 0 {
                        // Unsupported features requested: refuse.
                        s &= !STATUS_FEATURES_OK;
                    }
                    self.status = s;
                }
            }
            0x080 => {
                if let Some(q) = self.queue_mut() {
                    q.desc = (q.desc & !0xFFFF_FFFF) | v as u64;
                }
            }
            0x084 => {
                if let Some(q) = self.queue_mut() {
                    q.desc = (q.desc & 0xFFFF_FFFF) | ((v as u64) << 32);
                }
            }
            0x090 => {
                if let Some(q) = self.queue_mut() {
                    q.avail = (q.avail & !0xFFFF_FFFF) | v as u64;
                }
            }
            0x094 => {
                if let Some(q) = self.queue_mut() {
                    q.avail = (q.avail & 0xFFFF_FFFF) | ((v as u64) << 32);
                }
            }
            0x0A0 => {
                if let Some(q) = self.queue_mut() {
                    q.used = (q.used & !0xFFFF_FFFF) | v as u64;
                }
            }
            0x0A4 => {
                if let Some(q) = self.queue_mut() {
                    q.used = (q.used & 0xFFFF_FFFF) | ((v as u64) << 32);
                }
            }
            _ => {}
        }
    }

    /// Process a queue notification from the guest.
    pub async fn service_kick(&mut self, mem: &mut GuestMemory) -> Result<(), &'static str> {
        let q = match self.kicked.take() {
            Some(q) => q as usize,
            None => return Ok(()),
        };
        if !self.driver_ok() {
            return Ok(());
        }
        if q == TX {
            let mut frame = Vec::with_capacity(MAX_FRAME + HDR_LEN);
            loop {
                let chain = match self.queues[TX].pop(mem).await? {
                    Some(c) => c,
                    None => break,
                };
                frame.clear();
                DeviceQueue::read_chain(mem, &chain, HDR_LEN, &mut frame).await?;
                if !frame.is_empty() && frame.len() <= MAX_FRAME {
                    self.link.push_from_guest(frame.clone());
                    self.tx_frames += 1;
                }
                self.queues[TX].push_used(mem, chain.head, 0).await?;
            }
            // The driver reclaims transmitted buffers lazily and usually
            // asks not to be interrupted for them.
            if !self.queues[TX].no_interrupt(mem).await {
                self.isr |= 1;
            }
        } else {
            // RX buffers became available: deliver anything pending.
            self.deliver_rx(mem).await?;
        }
        Ok(())
    }

    /// Move frames from the host into the guest's receive queue.
    pub async fn deliver_rx(&mut self, mem: &mut GuestMemory) -> Result<bool, &'static str> {
        if !self.driver_ok() {
            return Ok(false);
        }
        let mut any = false;
        while self.link.has_to_guest() {
            let chain = match self.queues[RX].pop(mem).await? {
                Some(c) => c,
                None => break, // no buffers: leave the frames queued
            };
            let frame = match self.link.pop_to_guest() {
                Some(f) => f,
                None => {
                    // Nothing after all; give the buffer back unused.
                    self.queues[RX].last_avail = self.queues[RX].last_avail.wrapping_sub(1);
                    break;
                }
            };
            let cap = chain.writable_len();
            if cap < HDR_LEN + frame.len() {
                self.rx_dropped += 1;
                self.queues[RX].push_used(mem, chain.head, 0).await?;
                continue;
            }
            let mut data = Vec::with_capacity(HDR_LEN + frame.len());
            data.extend_from_slice(&[0u8; 10]);
            data.extend_from_slice(&1u16.to_le_bytes()); // num_buffers
            data.extend_from_slice(&frame);
            let n = DeviceQueue::write_chain(mem, &chain, &data).await?;
            self.queues[RX].push_used(mem, chain.head, n as u32).await?;
            self.rx_frames += 1;
            any = true;
        }
        if any && !self.queues[RX].no_interrupt(mem).await {
            self.isr |= 1;
            self.irqs += 1;
        }
        Ok(any)
    }

    pub fn debug_summary(&self) -> alloc::string::String {
        alloc::format!(
            "status={:#x} feat={:#x} rxq(size={} ready={} avail={} used={}) txq(size={} ready={}) isr={} tx={} rx={} drop={} pending_to_guest={}",
            self.status,
            self.driver_features,
            self.queues[RX].size,
            self.queues[RX].ready,
            self.queues[RX].last_avail,
            self.queues[RX].used_idx,
            self.queues[TX].size,
            self.queues[TX].ready,
            self.isr,
            self.tx_frames,
            self.rx_frames,
            self.rx_dropped,
            self.link.pending_to_guest()
        )
    }
}
