use core::ptr::NonNull;
use spin::Mutex;
use virtio_drivers::{Hal, PAGE_SIZE, PhysAddr};
use x86_64::{VirtAddr, structures::paging::Translate};

use crate::{
    alloc::ppa::{PMM, PhysicalPageAllocator},
    memory::{MAPPER, OFFSET},
};

pub struct KernelHal;

unsafe impl Hal for KernelHal {
    fn dma_alloc(
        pages: usize,
        _direction: virtio_drivers::BufferDirection,
    ) -> (PhysAddr, NonNull<u8>) {
        let lock: &Mutex<PhysicalPageAllocator> = PMM.r#try().expect("PMM must be initialized");
        let mut allocator = lock.lock();
        let addr = allocator
            .alloc_contiguous_pages(pages)
            .expect("allocation failed");

        let ptr = (OFFSET.r#try().expect("offset must be initialized") + addr.as_u64()) as *mut u8;

        unsafe {
            // SAFETY: allocator guarantees pages * PAGE_SIZE bytes to us
            ptr.write_bytes(0, pages * PAGE_SIZE);
        }

        (addr.as_u64(), NonNull::new(ptr).unwrap())
    }

    unsafe fn dma_dealloc(paddr: PhysAddr, _vaddr: NonNull<u8>, pages: usize) -> i32 {
        let mut allocator = PMM.r#try().expect("PMM must be initialized").lock();
        allocator.dealloc_contiguous_pages(paddr, pages);
        0
    }

    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, _size: usize) -> NonNull<u8> {
        NonNull::new((OFFSET.r#try().expect("offset must be initialized") + paddr) as *mut u8)
            .unwrap()
    }

    unsafe fn share(
        buffer: NonNull<[u8]>,
        _direction: virtio_drivers::BufferDirection,
    ) -> PhysAddr {
        let virt = VirtAddr::new(buffer.as_ptr() as *mut u8 as u64);

        let frame = MAPPER
            .r#try()
            .expect("MAPPER must be initialized")
            .lock()
            .translate_addr(virt)
            .expect("buffer is not mapped");

        frame.as_u64()
    }

    unsafe fn unshare(
        _paddr: PhysAddr,
        _buffer: NonNull<[u8]>,
        _direction: virtio_drivers::BufferDirection,
    ) {
    }
}

