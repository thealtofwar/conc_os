#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

pub mod allocation;
pub mod constants;
pub mod gdt;
pub mod interrupts;
pub mod memory;
pub mod network;
pub mod pci;
pub mod serial;
pub mod task;
pub mod virtio;
pub mod vga;

extern crate alloc;

use alloc::{boxed::Box, string::String, vec::{self, Vec}};
use bootloader::{BootInfo, entry_point};
use core::panic::PanicInfo;
use spin::Mutex;

use crate::{
    allocation::{allocator::init_heap, ppa::{PMM, PhysicalPageAllocator}}, memory::{MAPPER, OFFSET}, network::{VIRTIO_NET, init_virtio_net_pci}, serial::{TTYErr, readline},
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
    init_heap();
    init_virtio_net_pci();
    println!("VGA!");
    println!("{:?}", VIRTIO_NET.r#try().expect("VIRTIO_NET initialized").lock().mac_address());
    let v: Vec<u8> = Vec::with_capacity(10);
    let b = Box::new(42);
    let s = String::new();
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
