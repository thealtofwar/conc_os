use bootloader::{
    BootInfo,
    bootinfo::MemoryRegionType::{self},
};
use spin::{Mutex, Once};
use x86_64::{PhysAddr, VirtAddr};

use crate::constants::PAGE_SIZE;

pub static PMM: Once<Mutex<PhysicalPageAllocator>> = Once::new();

pub struct PhysicalPageAllocator {
    bitmap: &'static mut [u8],
    num_frames: usize,
}

impl PhysicalPageAllocator {
    pub fn new(boot_info: &'static BootInfo) -> Self {
        // Highest physical address in the memory map.
        let max_phys = boot_info
            .memory_map
            .iter()
            .map(|r| r.range.end_addr())
            .max()
            .unwrap() as usize;

        let num_frames = max_phys.div_ceil(PAGE_SIZE);
        let bitmap_bytes = num_frames.div_ceil(8);
        let bitmap_pages = bitmap_bytes.div_ceil(PAGE_SIZE);

        // Find a usable region large enough to hold the bitmap.
        let region = boot_info
            .memory_map
            .iter()
            .find(|r| {
                r.region_type == MemoryRegionType::Usable
                    && (r.range.end_addr() - r.range.start_addr()) as usize
                        >= bitmap_pages * PAGE_SIZE
            })
            .expect("no usable region large enough for bitmap");

        let bitmap_phys = PhysAddr::new(region.range.start_addr());

        let bitmap_virt = VirtAddr::new(boot_info.physical_memory_offset + bitmap_phys.as_u64());

        let bitmap = unsafe {
            core::slice::from_raw_parts_mut(bitmap_virt.as_mut_ptr::<u8>(), bitmap_bytes)
        };

        let mut allocator = Self { bitmap, num_frames };

        // Everything starts allocated.
        allocator.bitmap.fill(0xff);

        // Free all usable regions.
        for region in boot_info.memory_map.iter() {
            if region.region_type == MemoryRegionType::Usable {
                allocator.mark_region_free(
                    region.range.start_addr() as usize,
                    region.range.end_addr() as usize,
                );
            }
        }

        // Reserve the bitmap itself.
        allocator.mark_region_used(
            bitmap_phys.as_u64() as usize,
            bitmap_phys.as_u64() as usize + bitmap_pages * PAGE_SIZE,
        );

        allocator
    }

    pub fn alloc_contiguous_pages(&mut self, count: usize) -> Option<PhysAddr> {
        let start = self.find_contiguous_pages(count)?;

        for page in start..start + count {
            self.set_used(page);
        }

        Some(PhysAddr::new((start * 4096) as u64))
    }

    pub fn dealloc_contiguous_pages(&mut self, paddr: u64, count: usize) {
        let page = (paddr / 4096) as usize;

        for page in page..page + count {
            self.set_unused(page);
        }
    }

    fn mark_region_free(&mut self, start: usize, end: usize) {
        let first = start / PAGE_SIZE;
        let last = end.div_ceil(PAGE_SIZE);

        for frame in first..last {
            self.set_bit(frame, false);
        }
    }

    fn mark_region_used(&mut self, start: usize, end: usize) {
        let first = start / PAGE_SIZE;
        let last = end.div_ceil(PAGE_SIZE);

        for frame in first..last {
            self.set_used(frame);
        }
    }

    fn set_unused(&mut self, page: usize) {
        self.set_bit(page, false);
    }

    fn set_used(&mut self, page: usize) {
        self.set_bit(page, true);
    }

    fn set_bit(&mut self, frame: usize, used: bool) {
        let byte = frame / 8;
        let bit = frame % 8;

        if used {
            self.bitmap[byte] |= 1 << bit;
        } else {
            self.bitmap[byte] &= !(1 << bit);
        }
    }

    fn find_contiguous_pages(&self, count: usize) -> Option<usize> {
        if count == 0 {
            return None;
        }

        let mut run_start = 0;
        let mut run_length = 0;

        for page in 0..self.num_frames {
            if self.is_free(page) {
                if run_length == 0 {
                    run_start = page;
                }

                run_length += 1;

                if run_length == count {
                    return Some(run_start);
                }
            } else {
                run_length = 0;
            }
        }

        None
    }

    fn is_free(&self, page: usize) -> bool {
        let byte = page / 8;
        let bit = page % 8;

        (self.bitmap[byte] & (1 << bit)) == 0
    }
}
