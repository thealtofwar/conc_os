#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

pub mod allocation;
pub mod apic;
pub mod constants;
pub mod gdt;
pub mod interrupts;
pub mod memory;
pub mod network;
pub mod pci;
pub mod serial;
pub mod task;
pub mod vga;
pub mod virtio;
pub mod mutex;

extern crate alloc;

use bootloader::{BootInfo, entry_point};
use core::panic::PanicInfo;
use spin::Mutex;

use crate::{
    allocation::{
        allocator::init_heap,
        ppa::{PMM, PhysicalPageAllocator},
    },
    apic::init_apic,
    memory::{MAPPER, OFFSET},
    network::{VIRTIO_NET, init_virtio_net_pci},
    serial::{TTYErr, readline},
};

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("{info}");
    loop {}
}

entry_point!(kernel_main);

fn init(boot_info: &'static BootInfo) {
    gdt::init();
    interrupts::init_idt();
    MAPPER.call_once(|| unsafe { Mutex::new(memory::init(boot_info)) });
    PMM.call_once(|| Mutex::new(PhysicalPageAllocator::new(boot_info)));
    OFFSET.call_once(|| boot_info.physical_memory_offset);
    init_heap();
    init_virtio_net_pci();
    lazy_static::initialize(&crate::serial::SERIAL_TTY);
    init_apic();
    x86_64::instructions::interrupts::enable();
}

fn kernel_main(boot_info: &'static BootInfo) -> ! {
    init(boot_info);
    println!("VGA!");
    println!(
        "{:?}",
        VIRTIO_NET
            .r#try()
            .expect("VIRTIO_NET initialized")
            .lock()
            .mac_address()
    );
    loop {
        x86_64::instructions::hlt();
    }
}
