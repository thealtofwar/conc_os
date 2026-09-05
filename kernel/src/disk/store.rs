//! Page store: 4 KiB blocks on the block device, used to hold the memory of
//! frozen (scaled-to-zero) VMs.
//!
//! Block 0 is a superblock; the rest are allocated with an in-memory bitmap.
//! Contents do not need to survive a reboot (VMs do not), so the bitmap is
//! rebuilt empty at boot and the superblock exists mainly to detect a disk
//! that was formatted by us.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::mm::frame_slice;
use crate::sync::SpinLock;
use crate::virtio::blk::{BlkError, VirtioBlk};

pub const BLOCK_SIZE: u64 = 4096;
pub const SECTORS_PER_BLOCK: u64 = 8;
const MAGIC: &[u8; 8] = b"CONCOS01";

pub struct PageStore {
    dev: Arc<VirtioBlk>,
    nblocks: u32,
    /// Blocks available for pages (excludes the superblock and image area).
    usable: u32,
    bitmap: SpinLock<Bitmap>,
    used: AtomicU32,
    pub reads: AtomicU64,
    pub writes: AtomicU64,
}

struct Bitmap {
    words: Vec<u64>,
    hint: usize,
}

impl PageStore {
    pub fn new(dev: Arc<VirtioBlk>) -> Self {
        let nblocks = (dev.capacity_sectors / SECTORS_PER_BLOCK).min(u32::MAX as u64) as u32;
        let words = (nblocks as usize + 63) / 64;
        let mut bm = Bitmap { words: alloc::vec![0u64; words], hint: 0 };
        // Reserve block 0 (superblock) and any bits beyond the end.
        let mut reserved = 1u32;
        bm.words[0] |= 1;
        for b in nblocks as usize..words * 64 {
            bm.words[b / 64] |= 1 << (b % 64);
        }
        // Raw-disk tests scribble in a small scratch window.
        for b in super::SCRATCH_BLOCKS {
            if b < nblocks as usize {
                bm.words[b / 64] |= 1 << (b % 64);
                reserved += 1;
            }
        }
        // Installed kernel/initramfs images live in a fixed window that
        // swapped pages must not touch.
        if super::images::has_image_area(&dev) {
            let start = super::images::IMAGE_DIR_BLOCK as usize;
            let end = (start + super::images::IMAGE_AREA_BLOCKS as usize).min(nblocks as usize);
            for b in start..end {
                bm.words[b / 64] |= 1 << (b % 64);
            }
            reserved += (end - start) as u32;
        }
        PageStore {
            dev,
            nblocks,
            usable: nblocks.saturating_sub(reserved),
            bitmap: SpinLock::new(bm),
            used: AtomicU32::new(0),
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
        }
    }

    /// Write the superblock.
    pub async fn format(&self) -> Result<(), BlkError> {
        let mut sb = alloc::vec![0u8; BLOCK_SIZE as usize];
        sb[..8].copy_from_slice(MAGIC);
        sb[8..12].copy_from_slice(&self.nblocks.to_le_bytes());
        self.dev.write_sectors(0, &sb).await
    }

    pub fn alloc_block(&self) -> Option<u32> {
        let mut bm = self.bitmap.lock();
        let n = bm.words.len();
        let mut w = bm.hint;
        for _ in 0..n {
            if w >= n {
                w = 0;
            }
            let word = bm.words[w];
            if word != u64::MAX {
                let bit = (!word).trailing_zeros() as usize;
                bm.words[w] |= 1 << bit;
                bm.hint = w;
                self.used.fetch_add(1, Ordering::Relaxed);
                return Some((w * 64 + bit) as u32);
            }
            w += 1;
        }
        None
    }

    pub fn free_block(&self, b: u32) {
        let mut bm = self.bitmap.lock();
        let (w, bit) = (b as usize / 64, b as usize % 64);
        debug_assert!(bm.words[w] & (1 << bit) != 0, "double free of block {}", b);
        bm.words[w] &= !(1 << bit);
        if w < bm.hint {
            bm.hint = w;
        }
        self.used.fetch_sub(1, Ordering::Relaxed);
    }

    pub async fn write_block(&self, b: u32, pa: u64) -> Result<(), BlkError> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.dev.write_sectors(b as u64 * SECTORS_PER_BLOCK, frame_slice(pa)).await
    }

    pub async fn read_block(&self, b: u32, pa: u64) -> Result<(), BlkError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.dev.read_sectors(b as u64 * SECTORS_PER_BLOCK, frame_slice(pa)).await
    }

    /// Allocate a block and write the frame at `pa` to it.
    pub async fn store_frame(&self, pa: u64) -> Option<u32> {
        let b = self.alloc_block()?;
        match self.write_block(b, pa).await {
            Ok(()) => Some(b),
            Err(_) => {
                self.free_block(b);
                None
            }
        }
    }

    /// Read block `b` into the frame at `pa`.
    pub async fn load_frame(&self, b: u32, pa: u64) -> bool {
        self.read_block(b, pa).await.is_ok()
    }

    /// (used, usable) blocks.
    pub fn usage(&self) -> (u32, u32) {
        (self.used.load(Ordering::Relaxed), self.usable)
    }
}
