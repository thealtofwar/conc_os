#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

pub mod acpi_handling;
pub mod allocation;
pub mod apic;
pub mod constants;
pub mod gdt;
pub mod interrupts;
pub mod memory;
pub mod mutex;
pub mod network;
pub mod pci;
pub mod serial;
pub mod task;
pub mod vga;
pub mod virtio;

extern crate alloc;

use bootloader::{BootInfo, entry_point};
use core::panic::PanicInfo;
use futures_util::StreamExt;
use spin::Mutex;
use virtio_drivers::device::net::TxBuffer;

use crate::{
    acpi_handling::init_acpi,
    allocation::{
        allocator::init_heap,
        ppa::{PMM, PhysicalPageAllocator},
    },
    apic::init_apic,
    memory::{MAPPER, OFFSET},
    network::device::{get_net_driver, init_virtio_net_pci},
    serial::{TTYErr, readline},
    task::{
        Task,
        executor::Executor,
        network::{NetworkEvent, NetworkStream, network_task},
        serial::SerialStream,
    },
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
    init_acpi();
    lazy_static::initialize(&crate::serial::SERIAL_TTY);
    init_apic();
    init_virtio_net_pci();
    x86_64::instructions::interrupts::enable();
}

fn kernel_main(boot_info: &'static BootInfo) -> ! {
    init(boot_info);
    println!("VGA!");
    println!("{:?}", get_net_driver().lock().mac_address());

    let mut executor = Executor::new();
    executor.spawn(Task::new(handle_serial()));
    executor.spawn(Task::new(network_task()));
    executor.run();
}

async fn handle_serial() {
    let mut stream = SerialStream::new();
    loop {
        serial::print(format_args!("> "));
        let mut buffer = [0u8; 1024];
        match readline(&mut stream, &mut buffer).await {
            Ok(count) => match str::from_utf8(&buffer[0..count]) {
                Ok(s) => serial::print(format_args!("{}\n", s)),
                Err(_) => serial::print(format_args!("line was not utf8")),
            },
            Err(TTYErr::BufferTooSmall) => serial::print(format_args!("line too long")),
            Err(TTYErr::SerialErr) => serial::print(format_args!("serial err")),
        }
    }
}
