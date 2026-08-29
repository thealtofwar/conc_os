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
pub mod rng;
pub mod serial;
pub mod task;
pub mod time;
pub mod vga;
pub mod virtio;

extern crate alloc;

use bootloader::{BootInfo, entry_point};
use core::panic::PanicInfo;
use spin::Mutex;

use crate::{
    acpi_handling::init_acpi,
    allocation::{
        allocator::init_heap,
        ppa::{PMM, PhysicalPageAllocator},
    },
    apic::init_apic,
    memory::{MAPPER, OFFSET},
    network::device::{get_net_driver, init_virtio_net_pci},
    rng::init_entropy,
    serial::{TTYErr, readline},
    task::{Task, executor::Executor, network::network_task, serial::SerialStream, time::TimeTask},
    time::init_timer,
};

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("{info}");
    // Also out the serial port: VGA is the only channel a remote hypervisor
    // console will not necessarily show you, and a panic that is invisible
    // reads exactly like a hang. If the panic happened while SERIAL_TTY was
    // held this deadlocks, in which case the VGA line above is still there.
    serial::print(format_args!("PANIC: {info}\n"));
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
    // Before interrupts are unmasked: calibration polls the PIT, and anything
    // servicing an IRQ inside that loop would skew the measurement.
    init_timer();
    init_virtio_net_pci();
    println!("entropy: {:?}", init_entropy());
    x86_64::instructions::interrupts::enable();
}

fn kernel_main(boot_info: &'static BootInfo) -> ! {
    init(boot_info);
    println!("VGA!");
    println!("{:?}", get_net_driver().lock().mac_address());

    let mut executor = Executor::new();
    executor.spawn(Task::new(handle_serial()));
    executor.spawn(Task::new(network_task()));
    executor.spawn(Task::new(time_task()));
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

async fn time_task() {
    loop {
        println!("1s passed, time is {}", time::now_ms());
        TimeTask::new(1000).await;
    }
}
