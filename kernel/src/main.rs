//! conc_os: a type-1 hypervisor where VMs are first-class citizens.
//!
//! Boot flow: UEFI firmware loads this PE image → `efi_main` grabs the memory
//! map and framebuffer, exits boot services → `early_boot` builds our own
//! page tables and stack → `kernel_main` brings up memory, interrupts, timers,
//! devices, the async executor and finally the hypervisor.

#![no_std]
#![no_main]
#![allow(clippy::missing_safety_doc)]

extern crate alloc;

#[macro_use]
mod console;
mod arch;
mod commands;
mod disk;
mod hv;
mod mm;
mod net;
mod pci;
mod selftest;
mod shell;
mod sync;
mod task;
mod time;
mod uefi;
mod virtio;

use core::panic::PanicInfo;

use arch::cpu;
use uefi::BootInfo;

static mut BOOT_INFO: BootInfo = BootInfo::empty();

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[no_mangle]
pub extern "efiapi" fn efi_main(image: uefi::Handle, st: *mut uefi::SystemTable) -> uefi::Status {
    console::init();
    println!();
    println!("conc_os {} -- type-1 hypervisor -- booting via UEFI", VERSION);
    unsafe { uefi::con_out(st, "conc_os: booting; console is on the serial port\n") };

    let info = unsafe { &mut *core::ptr::addr_of_mut!(BOOT_INFO) };
    if let Err(e) = unsafe { uefi::exit_boot_services(image, st, info) } {
        println!("ExitBootServices failed: {:#x}", e);
        cpu::qemu_exit(2);
    }
    cpu::cli();
    unsafe { early_boot(info) }
}

/// Runs on the firmware stack with firmware page tables, right after
/// `ExitBootServices`.  Sets up our own and jumps to `kernel_main`.
unsafe fn early_boot(info: &'static BootInfo) -> ! {
    mm::early::init(info);
    let cr3 = arch::paging::build_kernel_tables(info);
    const STACK_PAGES: usize = 256; // 1 MiB
    let stack = mm::early::alloc_pages(STACK_PAGES);
    let stack_top = stack + (STACK_PAGES * 4096) as u64;
    arch::switch_stack_and_call(cr3, stack_top, kernel_main, info as *const BootInfo as u64)
}

fn print_memmap(info: &BootInfo) {
    let mut total_ram = 0u64;
    let mut usable = 0u64;
    for d in info.descriptors() {
        if d.is_ram() {
            total_ram += d.pages * 4096;
        }
        if d.is_free_after_boot() {
            usable += d.pages * 4096;
        }
    }
    log!(
        "memory map: {} entries, {} RAM, {} reclaimable",
        info.memmap_len,
        mm::Bytes(total_ram),
        mm::Bytes(usable)
    );
}

extern "sysv64" fn kernel_main(info_ptr: u64) -> ! {
    let info: &'static BootInfo = unsafe { &*(info_ptr as *const BootInfo) };

    arch::enable_cpu_features();
    mm::frame::init(info);
    mm::heap::init();
    arch::gdt::init();
    arch::idt::init();
    arch::apic::init();
    arch::ioapic::init();
    time::init();
    task::timer::install();
    cpu::sti();

    let brand = cpu::brand();
    let brand_str = core::str::from_utf8(&brand).unwrap_or("?").trim_matches(char::from(0)).trim();
    let vendor = cpu::vendor();
    log!("cpu: {} ({})", brand_str, core::str::from_utf8(&vendor).unwrap_or("?"));
    log!(
        "cpu: svm={} vmx={} x2apic={} phys-bits={} apic-id={} ({})",
        cpu::has_svm(),
        cpu::has_vmx(),
        cpu::has_x2apic(),
        cpu::phys_addr_bits(),
        arch::apic::id(),
        if arch::apic::is_x2apic() { "x2apic" } else { "xapic" }
    );
    log!(
        "clock: tsc {} MHz, apic timer {} MHz, tick {} Hz",
        time::tsc_per_ms() / 1000,
        time::apic_ticks_per_ms() / 1000,
        time::TICK_HZ
    );
    print_memmap(info);
    let fs = mm::frame::stats();
    log!("frames: {} usable, {} free ({})", fs.total, fs.free, mm::Bytes(fs.free as u64 * 4096));
    if let Some(fb) = &info.framebuffer {
        log!(
            "framebuffer: {}x{} stride {} at {:#x} (format {})",
            fb.width, fb.height, fb.stride, fb.base, fb.pixel_format
        );
    }
    if info.rsdp != 0 {
        log!("acpi: rsdp at {:#x}", info.rsdp);
    }

    pci::init();
    virtio::init();
    net::init();
    disk::init();
    hv::init();
    net::proxy::start();
    shell::init();
    log!("boot complete; type 'help' for commands");
    task::run()
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    console::set_panicking();
    println!();
    println!("!!! KERNEL PANIC: {}", info);
    cpu::qemu_exit(3)
}
