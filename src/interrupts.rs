use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use crate::{apic::LAPIC, network::VIRTIO_NET, println, serial::SERIAL_TTY, task::serial};
use lazy_static::lazy_static;

pub const COM1_VECTOR: u8 = 36;
pub const VIRTIO_NET_VECTOR: u8 = 0x60;

extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    println!("EXCEPTION: BREAKPOINT\n{:#?}", stack_frame);
}

extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    panic!("EXCEPTION: DOUBLE FAULT\n{:#?}", stack_frame);
}

extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    use x86_64::registers::control::Cr2;

    println!("EXCEPTION: PAGE FAULT");
    println!("Accessed Address: {:?}", Cr2::read());
    println!("Error Code: {:?}", error_code);
    println!("{:#?}", stack_frame);

    loop {
        x86_64::instructions::hlt();
    }
}

extern "x86-interrupt" fn gp_fault_handler(frame: InterruptStackFrame, error_code: u64) {
    panic!(
        "GENERAL PROTECTION FAULT (Error Code: {:#x}):\n{:#?}",
        error_code, frame
    );
}

extern "x86-interrupt" fn com1_interrupt_handler(_stack_frame: InterruptStackFrame) {
    while let Ok(b) = SERIAL_TTY.lock().inner_mut().try_receive_byte() {
        serial::add_byte(b);
    }

    if let Some(lapic_mutex) = crate::apic::LAPIC.r#try() {
        unsafe {
            lapic_mutex.lock().end_of_interrupt();
        }
    }
}

extern "x86-interrupt" fn virtio_irq(_frame: InterruptStackFrame) {
    println!("VirtIO interrupt!");

    VIRTIO_NET
        .r#try()
        .expect("virtio net should be init")
        .lock()
        .ack_interrupt();

    unsafe {
        LAPIC
            .r#try()
            .expect("lapic should be init")
            .lock()
            .end_of_interrupt()
    };
}

lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        idt.breakpoint.set_handler_fn(breakpoint_handler);
        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault_handler)
                .set_stack_index(crate::gdt::DOUBLE_FAULT_IST_INDEX);
        }
        idt.page_fault.set_handler_fn(page_fault_handler);
        idt.general_protection_fault
            .set_handler_fn(gp_fault_handler);
        idt[COM1_VECTOR].set_handler_fn(com1_interrupt_handler);
        idt[VIRTIO_NET_VECTOR].set_handler_fn(virtio_irq);
        idt
    };
}

pub fn init_idt() {
    IDT.load();
}
