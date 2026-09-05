//! Image area on the block device: kernels, initramfs archives and command
//! lines installed by `cargo xtask install-linux`.
//!
//! Entries are grouped into named *image sets*: the entries of one set share
//! a name and differ in kind, so several kernels can be installed side by
//! side and different VMs can boot different ones.
//!
//! Layout (byte offsets on the disk):
//!
//! ```text
//! IMAGE_AREA_OFFSET + 0      directory block (4 KiB)
//!     0..8    magic "CONCIMG2"
//!     8..12   u32 entry count
//!     16 + 64*i  entry i:
//!         0..32   set name, NUL padded
//!         32..36  u32 kind (1 kernel, 2 initrd, 3 cmdline)
//!         40..48  u64 byte offset on disk (4 KiB aligned)
//!         48..56  u64 size in bytes
//! IMAGE_AREA_OFFSET + 4096   image data
//! ```
//!
//! The page store owns everything below `IMAGE_AREA_OFFSET`.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::sync::SpinLock;
use crate::virtio::blk::{BlkError, VirtioBlk, SECTOR_SIZE};

/// The image area is a fixed 256 MiB window starting 1 MiB into the disk;
/// the page store uses every other block.  Keeping it near the start lets
/// disk.img stay sparse however large the page store is.
pub const IMAGE_AREA_OFFSET: u64 = 1 << 20;
pub const IMAGE_AREA_SIZE: u64 = 256 << 20;
pub const IMAGE_DIR_BLOCK: u64 = IMAGE_AREA_OFFSET / 4096;
pub const IMAGE_AREA_BLOCKS: u64 = IMAGE_AREA_SIZE / 4096;
const MAGIC: &[u8; 8] = b"CONCIMG2";

pub const KIND_KERNEL: u32 = 1;
pub const KIND_INITRD: u32 = 2;
pub const KIND_CMDLINE: u32 = 3;

#[derive(Clone, Debug)]
pub struct ImageEntry {
    pub name: String,
    pub kind: u32,
    pub offset: u64,
    pub size: u64,
}

impl ImageEntry {
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            KIND_KERNEL => "kernel",
            KIND_INITRD => "initrd",
            KIND_CMDLINE => "cmdline",
            _ => "?",
        }
    }
}

static IMAGES: SpinLock<Vec<ImageEntry>> = SpinLock::new(Vec::new());
static LOADED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Wait (up to `ms`) for the directory to have been read at boot.
pub async fn wait_loaded(ms: u64) -> bool {
    let deadline = crate::time::now() + crate::time::us_to_tsc(ms * 1000);
    while !LOADED.load(core::sync::atomic::Ordering::Acquire) {
        if crate::time::now() >= deadline || crate::virtio::blk::device().is_none() {
            return LOADED.load(core::sync::atomic::Ordering::Acquire);
        }
        crate::task::timer::sleep_ms(5).await;
    }
    true
}

/// Does the disk have room for an image area at all?
pub fn has_image_area(dev: &VirtioBlk) -> bool {
    dev.capacity_sectors * SECTOR_SIZE as u64 >= IMAGE_AREA_OFFSET + IMAGE_AREA_SIZE
}

/// Read the directory block and remember its entries.
pub async fn load_directory(dev: &Arc<VirtioBlk>) -> Result<Vec<ImageEntry>, BlkError> {
    if !has_image_area(dev) {
        LOADED.store(true, core::sync::atomic::Ordering::Release);
        return Ok(Vec::new());
    }
    let mut buf = alloc::vec![0u8; 4096];
    dev.read_sectors(IMAGE_AREA_OFFSET / SECTOR_SIZE as u64, &mut buf).await?;
    let mut out = Vec::new();
    if &buf[..8] != MAGIC {
        *IMAGES.lock() = Vec::new();
        LOADED.store(true, core::sync::atomic::Ordering::Release);
        return Ok(out);
    }
    LOADED.store(true, core::sync::atomic::Ordering::Release);
    let count = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]) as usize;
    for i in 0..count.min(63) {
        let e = &buf[16 + i * 64..16 + (i + 1) * 64];
        let name_end = e[..32].iter().position(|&b| b == 0).unwrap_or(32);
        let name = String::from_utf8_lossy(&e[..name_end]).into_owned();
        let kind = u32::from_le_bytes([e[32], e[33], e[34], e[35]]);
        let mut o = [0u8; 8];
        o.copy_from_slice(&e[40..48]);
        let mut s = [0u8; 8];
        s.copy_from_slice(&e[48..56]);
        let entry = ImageEntry { name, kind, offset: u64::from_le_bytes(o), size: u64::from_le_bytes(s) };
        if entry.size > 0 && entry.offset >= IMAGE_AREA_OFFSET + 4096 {
            out.push(entry);
        }
    }
    *IMAGES.lock() = out.clone();
    Ok(out)
}

pub fn images() -> Vec<ImageEntry> {
    IMAGES.lock().clone()
}

pub fn find(name: &str) -> Option<ImageEntry> {
    IMAGES.lock().iter().find(|e| e.name == name).cloned()
}

pub fn find_kind(kind: u32) -> Option<ImageEntry> {
    IMAGES.lock().iter().find(|e| e.kind == kind).cloned()
}

/// One bootable Linux image: a kernel, optionally an initramfs and a command
/// line, all installed under the same name.
#[derive(Clone, Debug)]
pub struct ImageSet {
    pub name: String,
    pub kernel: ImageEntry,
    pub initrd: Option<ImageEntry>,
    pub cmdline: Option<ImageEntry>,
}

impl ImageSet {
    pub fn bytes(&self) -> u64 {
        self.kernel.size + self.initrd.as_ref().map(|e| e.size).unwrap_or(0) + self.cmdline.as_ref().map(|e| e.size).unwrap_or(0)
    }
}

/// Every installed set, in directory order (a set without a kernel is not
/// bootable and is left out).
pub fn sets() -> Vec<ImageSet> {
    let entries = IMAGES.lock().clone();
    let mut out: Vec<ImageSet> = Vec::new();
    for e in entries.iter().filter(|e| e.kind == KIND_KERNEL) {
        let pick = |kind: u32| entries.iter().find(|o| o.name == e.name && o.kind == kind).cloned();
        out.push(ImageSet { name: e.name.clone(), kernel: e.clone(), initrd: pick(KIND_INITRD), cmdline: pick(KIND_CMDLINE) });
    }
    out
}

pub fn find_set(name: &str) -> Option<ImageSet> {
    sets().into_iter().find(|s| s.name == name)
}

/// The set a `linux create` without an explicit image uses: the first
/// installed one.
pub fn default_set() -> Option<ImageSet> {
    sets().into_iter().next()
}

/// Stream an image in 64 KiB chunks to `sink(offset_in_image, bytes)`.
pub async fn read_image(
    dev: &Arc<VirtioBlk>,
    entry: &ImageEntry,
    sink: &mut (dyn FnMut(u64, &[u8]) + Send),
) -> Result<(), BlkError> {
    const CHUNK: usize = 64 * 1024;
    let mut buf = alloc::vec![0u8; CHUNK];
    let mut done = 0u64;
    while done < entry.size {
        let disk_off = entry.offset + done;
        let sector = disk_off / SECTOR_SIZE as u64;
        let in_sector = (disk_off % SECTOR_SIZE as u64) as usize;
        let want = ((entry.size - done) as usize).min(CHUNK - in_sector);
        let sectors = (in_sector + want + SECTOR_SIZE - 1) / SECTOR_SIZE;
        dev.read_sectors(sector, &mut buf[..sectors * SECTOR_SIZE]).await?;
        sink(done, &buf[in_sector..in_sector + want]);
        done += want as u64;
    }
    Ok(())
}

/// Read a whole (small) image into memory.
pub async fn read_image_vec(dev: &Arc<VirtioBlk>, entry: &ImageEntry) -> Result<Vec<u8>, BlkError> {
    let mut v = Vec::with_capacity(entry.size as usize);
    read_image(dev, entry, &mut |_, bytes| v.extend_from_slice(bytes)).await?;
    Ok(v)
}
