//! Generates the interrupt stubs: one 16-byte stub per vector that pushes a
//! (dummy) error code and the vector number, then jumps to `isr_common`.

use std::env;
use std::fs;
use std::path::Path;

fn main() {
    let out = env::var("OUT_DIR").unwrap();
    let mut s = String::new();
    s.push_str(".section .text\n");
    s.push_str(".balign 16\n");
    s.push_str(".global isr_stubs\n");
    s.push_str("isr_stubs:\n");
    for v in 0..256u32 {
        let has_error_code = matches!(v, 8 | 10..=14 | 17 | 21 | 29 | 30);
        s.push_str(".balign 16\n");
        if !has_error_code {
            s.push_str("    push 0\n");
        }
        s.push_str(&format!("    push {}\n", v));
        s.push_str("    jmp isr_common\n");
    }
    s.push_str(".balign 16\n");
    s.push_str("isr_common:\n");
    for r in ["rax", "rcx", "rdx", "rbx", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15"] {
        s.push_str(&format!("    push {}\n", r));
    }
    s.push_str("    mov rdi, rsp\n");
    s.push_str("    cld\n");
    s.push_str("    call interrupt_dispatch\n");
    for r in ["r15", "r14", "r13", "r12", "r11", "r10", "r9", "r8", "rdi", "rsi", "rbp", "rbx", "rdx", "rcx", "rax"] {
        s.push_str(&format!("    pop {}\n", r));
    }
    s.push_str("    add rsp, 16\n");
    s.push_str("    iretq\n");
    fs::write(Path::new(&out).join("isr_stubs.s"), s).unwrap();

    // Make the guest image path available to `include_bytes!`.
    let guest = env::var("GUEST_ELF").unwrap_or_default();
    println!("cargo:rustc-env=GUEST_ELF={}", guest);
    println!("cargo:rerun-if-env-changed=GUEST_ELF");
    if !guest.is_empty() {
        println!("cargo:rerun-if-changed={}", guest);
    }
    println!("cargo:rerun-if-changed=build.rs");
}
