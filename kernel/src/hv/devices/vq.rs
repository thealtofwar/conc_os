//! Device-side view of a split virtqueue living in guest memory.
//!
//! The guest driver owns the descriptor table and the available ring; the
//! device (us) consumes available chains and returns them through the used
//! ring.  All accesses go through `GuestMemory`, so they are copy-on-write
//! aware and may page in swapped pages.

use alloc::vec::Vec;

use crate::hv::memory::GuestMemory;

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;
const MAX_CHAIN: usize = 64;

#[derive(Clone, Copy, Debug)]
pub struct DescBuf {
    pub addr: u64,
    pub len: u32,
    /// Device-writable (guest expects data here).
    pub write: bool,
}

/// A chain of descriptors popped from the available ring.
pub struct Chain {
    pub head: u16,
    pub bufs: Vec<DescBuf>,
}

impl Chain {
    pub fn readable_len(&self) -> usize {
        self.bufs.iter().filter(|b| !b.write).map(|b| b.len as usize).sum()
    }
    pub fn writable_len(&self) -> usize {
        self.bufs.iter().filter(|b| b.write).map(|b| b.len as usize).sum()
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DeviceQueue {
    pub size: u16,
    pub max: u16,
    pub desc: u64,
    pub avail: u64,
    pub used: u64,
    pub ready: bool,
    pub last_avail: u16,
    pub used_idx: u16,
}

async fn read_u16(mem: &mut GuestMemory, gpa: u64) -> Result<u16, &'static str> {
    let mut b = [0u8; 2];
    mem.read(gpa, &mut b).await?;
    Ok(u16::from_le_bytes(b))
}

impl DeviceQueue {
    pub fn new(max: u16) -> Self {
        DeviceQueue { size: 0, max, desc: 0, avail: 0, used: 0, ready: false, last_avail: 0, used_idx: 0 }
    }

    pub fn reset(&mut self) {
        let max = self.max;
        *self = DeviceQueue::new(max);
    }

    fn valid(&self) -> bool {
        self.ready && self.size > 0 && self.desc != 0 && self.avail != 0 && self.used != 0
    }

    /// Has the driver asked not to be interrupted (VRING_AVAIL_F_NO_INTERRUPT)?
    pub async fn no_interrupt(&self, mem: &mut GuestMemory) -> bool {
        if !self.valid() {
            return false;
        }
        match read_u16(mem, self.avail).await {
            Ok(f) => f & 1 != 0,
            Err(_) => false,
        }
    }

    /// Take the next available chain, if any.
    pub async fn pop(&mut self, mem: &mut GuestMemory) -> Result<Option<Chain>, &'static str> {
        if !self.valid() {
            return Ok(None);
        }
        let avail_idx = read_u16(mem, self.avail + 2).await?;
        if avail_idx == self.last_avail {
            return Ok(None);
        }
        let slot = self.last_avail % self.size;
        let head = read_u16(mem, self.avail + 4 + 2 * slot as u64).await?;
        self.last_avail = self.last_avail.wrapping_add(1);

        let mut bufs = Vec::new();
        let mut idx = head;
        loop {
            if idx >= self.size || bufs.len() >= MAX_CHAIN {
                return Err("bad virtqueue descriptor chain");
            }
            let mut d = [0u8; 16];
            mem.read(self.desc + 16 * idx as u64, &mut d).await?;
            let addr = u64::from_le_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]);
            let len = u32::from_le_bytes([d[8], d[9], d[10], d[11]]);
            let flags = u16::from_le_bytes([d[12], d[13]]);
            let next = u16::from_le_bytes([d[14], d[15]]);
            bufs.push(DescBuf { addr, len, write: flags & DESC_F_WRITE != 0 });
            if flags & DESC_F_NEXT == 0 {
                break;
            }
            idx = next;
        }
        Ok(Some(Chain { head, bufs }))
    }

    /// Return a chain to the guest with `len` bytes written.
    pub async fn push_used(&mut self, mem: &mut GuestMemory, head: u16, len: u32) -> Result<(), &'static str> {
        if !self.valid() {
            return Err("virtqueue not ready");
        }
        let slot = self.used_idx % self.size;
        let mut e = [0u8; 8];
        e[..4].copy_from_slice(&(head as u32).to_le_bytes());
        e[4..].copy_from_slice(&len.to_le_bytes());
        mem.write(self.used + 4 + 8 * slot as u64, &e).await?;
        self.used_idx = self.used_idx.wrapping_add(1);
        mem.write(self.used + 2, &self.used_idx.to_le_bytes()).await?;
        Ok(())
    }

    /// Gather the device-readable part of a chain, skipping `skip` bytes.
    pub async fn read_chain(mem: &mut GuestMemory, chain: &Chain, skip: usize, out: &mut Vec<u8>) -> Result<(), &'static str> {
        let mut to_skip = skip;
        for b in chain.bufs.iter().filter(|b| !b.write) {
            let len = b.len as usize;
            if to_skip >= len {
                to_skip -= len;
                continue;
            }
            let start = out.len();
            out.resize(start + len - to_skip, 0);
            mem.read(b.addr + to_skip as u64, &mut out[start..]).await?;
            to_skip = 0;
        }
        Ok(())
    }

    /// Scatter `data` into the device-writable part of a chain.  Returns the
    /// number of bytes written.
    pub async fn write_chain(mem: &mut GuestMemory, chain: &Chain, data: &[u8]) -> Result<usize, &'static str> {
        let mut done = 0usize;
        for b in chain.bufs.iter().filter(|b| b.write) {
            if done >= data.len() {
                break;
            }
            let n = (b.len as usize).min(data.len() - done);
            mem.write(b.addr, &data[done..done + n]).await?;
            done += n;
        }
        Ok(done)
    }
}
