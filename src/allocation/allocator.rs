use linked_list_allocator::LockedHeap;

use crate::{allocation::ppa::PMM, memory::OFFSET};

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

pub fn init_heap() {
    let addr = PMM.r#try().expect("PMM must be initialized before ALLOCATOR").lock().alloc_contiguous_pages(4096).expect("16MiB allocation failed");
    let vaddr = OFFSET.r#try().expect("offset must be initialized") + addr.as_u64();
    // new
    unsafe {
        ALLOCATOR.lock().init(vaddr as *mut u8, 4096 * 4096);
    }
}