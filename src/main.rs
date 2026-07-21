#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

pub mod alloc;
pub mod constants;
pub mod gdt;
pub mod interrupts;
pub mod memory;
pub mod network;
pub mod pci;
pub mod serial;
pub mod virtio;
pub mod vga;

use bootloader::{BootInfo, entry_point};
use core::panic::PanicInfo;
use spin::Mutex;

use crate::{
    alloc::ppa::{PMM, PhysicalPageAllocator}, memory::{MAPPER, OFFSET}, network::init_virtio_net_pci, serial::{TTYErr, readline},
};

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("{info}");
    loop {}
}

entry_point!(kernel_main);

fn kernel_main(boot_info: &'static BootInfo) -> ! {
    gdt::init();
    interrupts::init_idt();
    MAPPER.call_once(|| unsafe { Mutex::new(memory::init(boot_info)) });
    PMM.call_once(|| Mutex::new(PhysicalPageAllocator::new(boot_info)));
    OFFSET.call_once(|| boot_info.physical_memory_offset);

    init_virtio_net_pci();
    println!("VGA!");
    

    loop {
        serial::print(format_args!("> "));
        let mut buffer = [0u8; 1024];
        match readline(&mut buffer) {
            Ok(count) => match str::from_utf8(&buffer[0..count]) {
                Ok(s) => serial::print(format_args!("{}\n", s)),
                Err(_) => serial::print(format_args!("line was not utf8")),
            },
            Err(TTYErr::BufferTooSmall) => serial::print(format_args!("line too long")),
            Err(TTYErr::SerialErr) => serial::print(format_args!("serial err")),
        }
    }
}
