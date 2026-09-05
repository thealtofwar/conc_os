//! Disk subsystem: the virtio-blk device, the page store built on it, and
//! the image area holding installed kernels.

#![allow(dead_code)]

pub mod images;
pub mod store;

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use crate::mm::frame;
use crate::selftest::{check, tests, TestFn, TestResult};
use crate::sync::OnceCell;
use crate::task;
use crate::virtio::blk::{VirtioBlk, SECTOR_SIZE};

/// Sectors [SCRATCH_SECTOR, 2048) are reserved for raw-disk tests (blocks
/// 128..256); the page store never hands them out.
pub const SCRATCH_SECTOR: u64 = 1024;
pub const SCRATCH_BLOCKS: core::ops::Range<usize> = 128..256;
use store::PageStore;

static STORE: OnceCell<Arc<PageStore>> = OnceCell::new();

pub fn device() -> Option<&'static Arc<VirtioBlk>> {
    crate::virtio::blk::device()
}

pub fn store() -> Option<&'static Arc<PageStore>> {
    STORE.get()
}

/// Wait (up to `ms`) for the page store to finish formatting.
pub async fn wait_store(ms: u64) -> Option<&'static Arc<PageStore>> {
    let deadline = crate::time::now() + crate::time::us_to_tsc(ms * 1000);
    while STORE.get().is_none() {
        if crate::time::now() >= deadline || device().is_none() {
            return None;
        }
        task::timer::sleep_ms(5).await;
    }
    STORE.get()
}

pub fn init() {
    let dev = match device() {
        Some(d) => d.clone(),
        None => {
            log!("disk: no block device");
            return;
        }
    };
    task::spawn_detached("disk-init", async move {
        let st = PageStore::new(dev.clone());
        match st.format().await {
            Ok(()) => {
                let (used, total) = st.usage();
                log!(
                    "disk: page store ready, {} of {} blocks used ({} capacity)",
                    used,
                    total,
                    crate::mm::Bytes(total as u64 * 4096)
                );
                STORE.init(Arc::new(st));
            }
            Err(e) => log!("disk: failed to format page store: {:?}", e),
        }
        match images::load_directory(&dev).await {
            Ok(list) if list.is_empty() => {
                if images::has_image_area(&dev) {
                    log!("disk: image area present, no images installed (cargo xtask install-linux)");
                } else {
                    log!("disk: too small for an image area ({} needed)", crate::mm::Bytes(images::IMAGE_AREA_OFFSET + images::IMAGE_AREA_SIZE));
                }
            }
            Ok(list) => {
                for e in &list {
                    log!("disk: image '{}' ({}) {} at {:#x}", e.name, e.kind_name(), crate::mm::Bytes(e.size), e.offset);
                }
            }
            Err(e) => log!("disk: cannot read image directory: {:?}", e),
        }
    });
}

// ---------------------------------------------------------------- shell ---

pub fn help() {
    println!("disk:   disk   disk-rw <sector>   images");
}

pub async fn dispatch(cmd: &str, args: &[&str]) -> bool {
    match cmd {
        "disk" => {
            match device() {
                Some(d) => {
                    println!(
                        "virtio-blk: {} sectors ({}), {} reads, {} writes, {} irqs, {} errors, {} full-ring waits",
                        d.capacity_sectors,
                        crate::mm::Bytes(d.capacity_sectors * SECTOR_SIZE as u64),
                        d.stats.reads.load(Ordering::Relaxed),
                        d.stats.writes.load(Ordering::Relaxed),
                        d.stats.irqs.load(Ordering::Relaxed),
                        d.stats.errors.load(Ordering::Relaxed),
                        d.stats.queue_full_waits.load(Ordering::Relaxed)
                    );
                }
                None => println!("no block device"),
            }
            if let Some(s) = store() {
                let (used, total) = s.usage();
                println!(
                    "page store: {} / {} blocks used, {} page reads, {} page writes",
                    used,
                    total,
                    s.reads.load(Ordering::Relaxed),
                    s.writes.load(Ordering::Relaxed)
                );
            }
            true
        }
        "images" => {
            let list = images::images();
            if list.is_empty() {
                println!("no images installed (run: cargo xtask install-linux)");
            }
            for e in list {
                println!("{:<12} {:<8} {:>10}  at disk offset {:#x}", e.name, e.kind_name(), crate::mm::Bytes(e.size), e.offset);
            }
            true
        }
        "disk-rw" => {
            let sector = crate::shell::arg_u64(args, 0, SCRATCH_SECTOR);
            match rw_test(sector).await {
                Ok(us) => println!("write+read of sector {} ok ({} us)", sector, us),
                Err(e) => println!("failed: {}", e),
            }
            true
        }
        _ => false,
    }
}

async fn rw_test(sector: u64) -> Result<u64, String> {
    let dev = device().ok_or("no block device")?;
    let t0 = crate::time::now();
    let mut wbuf = alloc::vec![0u8; 4096];
    for (i, b) in wbuf.iter_mut().enumerate() {
        *b = (i as u8) ^ (sector as u8);
    }
    dev.write_sectors(sector, &wbuf).await.map_err(|e| format!("write: {:?}", e))?;
    let mut rbuf = alloc::vec![0u8; 4096];
    dev.read_sectors(sector, &mut rbuf).await.map_err(|e| format!("read: {:?}", e))?;
    if rbuf != wbuf {
        return Err("readback mismatch".into());
    }
    Ok(crate::time::tsc_to_us(crate::time::now() - t0))
}

// ---------------------------------------------------------------- tests ---

pub fn tests() -> &'static [(&'static str, TestFn)] {
    tests![device_present, rw_roundtrip, concurrent_requests, pagestore_roundtrip]
}

async fn device_present() -> TestResult {
    check!(device().is_some(), "no block device");
    Ok(())
}

async fn rw_roundtrip() -> TestResult {
    rw_test(SCRATCH_SECTOR).await.map(|_| ())
}

async fn concurrent_requests() -> TestResult {
    let dev = device().ok_or("no block device")?;
    let mut handles = Vec::new();
    for i in 0..16u64 {
        let d = dev.clone();
        handles.push(task::spawn("test-blk", async move {
            let sector = SCRATCH_SECTOR + 8 + i * 8;
            let mut w = alloc::vec![0u8; 4096];
            for (k, b) in w.iter_mut().enumerate() {
                *b = (k as u8).wrapping_mul(i as u8 + 1);
            }
            if d.write_sectors(sector, &w).await.is_err() {
                return false;
            }
            let mut r = alloc::vec![0u8; 4096];
            if d.read_sectors(sector, &mut r).await.is_err() {
                return false;
            }
            r == w
        }));
    }
    for h in handles {
        check!(h.await, "concurrent request failed");
    }
    Ok(())
}

async fn pagestore_roundtrip() -> TestResult {
    let st = wait_store(5000).await.ok_or("page store not ready")?;
    let (used0, _) = st.usage();
    let mut blocks = Vec::new();
    let mut frames = Vec::new();
    for i in 0..32u32 {
        let f = frame::alloc().ok_or("frame")?;
        let s = crate::mm::frame_slice(f);
        for (k, b) in s.iter_mut().enumerate() {
            *b = (k as u32 ^ i.wrapping_mul(2654435761)) as u8;
        }
        let blk = st.store_frame(f).await.ok_or("store_frame failed")?;
        blocks.push(blk);
        frames.push(f);
    }
    for (i, &blk) in blocks.iter().enumerate() {
        let f = frame::alloc_zeroed().ok_or("frame")?;
        check!(st.load_frame(blk, f).await, "load_frame failed");
        let a = crate::mm::frame_slice(f);
        let b = crate::mm::frame_slice(frames[i]);
        check!(a == b, "page {} mismatch after disk round trip", i);
        frame::free(f);
    }
    for blk in blocks {
        st.free_block(blk);
    }
    for f in frames {
        frame::free(f);
    }
    let (used1, _) = st.usage();
    check!(used0 == used1, "page store leaked blocks: {} -> {}", used0, used1);
    Ok(())
}
