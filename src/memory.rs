use bootloader::BootInfo;
use spin::{Mutex, Once};
use x86_64::{
    PhysAddr, VirtAddr,
    structures::paging::{OffsetPageTable, PageTable},
};

pub static OFFSET: Once<u64> = Once::new();
pub static MAPPER: Once<Mutex<OffsetPageTable>> = Once::new();

/// Returns a mutable reference to the active level 4 table from
///
/// This function is unsafe because the caller must guarantee that the
/// boot_info is correct. Also, this function must be only called once
/// to avoid aliasing `&mut` references (which is undefined behavior).
pub unsafe fn active_level_4_table(boot_info: &BootInfo) -> &'static mut PageTable {
    use x86_64::registers::control::Cr3;

    let (level_4_table_frame, _) = Cr3::read();

    let phys = level_4_table_frame.start_address();
    let virt = VirtAddr::new(boot_info.physical_memory_offset) + phys.as_u64();
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();

    unsafe { &mut *page_table_ptr }
}

/// Initialize a new OffsetPageTable.
///
/// This function is unsafe because the caller must guarantee that the
/// boot_info is correct. Also, this function must be only called once
/// to avoid aliasing `&mut` references (which is undefined behavior).
pub unsafe fn init(boot_info: &BootInfo) -> OffsetPageTable<'static> {
    unsafe {
        let level_4_table = active_level_4_table(boot_info);
        OffsetPageTable::new(
            level_4_table,
            VirtAddr::new(boot_info.physical_memory_offset),
        )
    }
}
