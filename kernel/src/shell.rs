//! Interactive serial shell.
//!
//! Serial receive interrupts (IRQ 4 via the I/O APIC) feed bytes into a
//! channel; the shell task assembles lines and dispatches commands.  Other
//! subsystems expose their commands through `commands::dispatch`.  While a VM
//! console is attached, every byte is forwarded to that VM instead, until
//! Ctrl-] is pressed.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch::{cpu, idt, ioapic};
use crate::hv::vm::VmHandle;
use crate::sync::{OnceCell, SpinLock};
use crate::task::channel::{channel, Receiver, Sender};
use crate::{console, mm, task, time};

static INPUT_TX: OnceCell<Sender<u8>> = OnceCell::new();
static ATTACHED: SpinLock<Option<Arc<VmHandle>>> = SpinLock::new(None);

const CTRL_RBRACKET: u8 = 0x1D;

fn serial_irq(_f: &mut idt::TrapFrame) {
    if let Some(tx) = INPUT_TX.get() {
        while let Some(b) = console::read_byte() {
            let _ = tx.send(b);
        }
    }
}

/// Wire up serial input and start the shell task.
pub fn init() {
    let (tx, rx) = channel::<u8>();
    INPUT_TX.init(tx);
    let vec = idt::alloc_vector();
    idt::register_handler(vec, serial_irq);
    ioapic::route(4, vec, false, false);
    console::enable_rx_interrupt();
    task::spawn_detached("shell", shell_task(rx));
}

/// Route console input to a VM until Ctrl-].
pub fn attach(vm: Arc<VmHandle>) {
    *ATTACHED.lock() = Some(vm);
}

fn detach() -> Option<Arc<VmHandle>> {
    ATTACHED.lock().take()
}

async fn shell_task(mut rx: Receiver<u8>) {
    let mut line: Vec<u8> = Vec::new();
    print!("conc_os> ");
    loop {
        let b = match rx.recv().await {
            Some(b) => b,
            None => return,
        };

        // Attached to a VM console?
        let attached = ATTACHED.lock().clone();
        if let Some(vm) = attached {
            if b == CTRL_RBRACKET || vm.is_finished() {
                vm.set_attached(false);
                detach();
                println!();
                println!("(detached from vm {} '{}', state {})", vm.id, vm.name, vm.state());
                print!("conc_os> ");
                continue;
            }
            vm.console_input(&[b]);
            continue;
        }

        match b {
            b'\r' | b'\n' => {
                println!();
                let cmd = String::from_utf8_lossy(&line).into_owned();
                line.clear();
                let trimmed = cmd.trim();
                if !trimmed.is_empty() {
                    dispatch(trimmed).await;
                }
                if ATTACHED.lock().is_none() {
                    print!("conc_os> ");
                }
            }
            0x08 | 0x7F => {
                if line.pop().is_some() {
                    print!("\x08 \x08");
                }
            }
            0x03 => {
                line.clear();
                println!("^C");
                print!("conc_os> ");
            }
            b if b.is_ascii_graphic() || b == b' ' => {
                if line.len() < 512 {
                    line.push(b);
                    console::_print(format_args!("{}", b as char));
                }
            }
            _ => {}
        }
    }
}

fn parse_u64(s: &str) -> Option<u64> {
    if let Some(h) = s.strip_prefix("0x") {
        u64::from_str_radix(h, 16).ok()
    } else {
        s.parse().ok()
    }
}

pub fn arg_u64(args: &[&str], i: usize, default: u64) -> u64 {
    args.get(i).and_then(|s| parse_u64(s)).unwrap_or(default)
}

/// Execute one command line.  Extended by subsystems as they come online.
pub async fn dispatch(line: &str) {
    let parts: Vec<&str> = line.split_whitespace().collect();
    let cmd = parts[0];
    let args = &parts[1..];
    match cmd {
        "help" | "?" => {
            println!("core:   help mem tasks timers uptime echo sleep <ms> exit [code] selftest [filter]");
            println!("        cpuid   spin <ms>");
            crate::commands::help();
        }
        "echo" => println!("{}", args.join(" ")),
        "uptime" => {
            let us = time::uptime_us();
            println!(
                "up {}.{:06} s, {} ticks, {} timer wakeups",
                us / 1_000_000,
                us % 1_000_000,
                time::ticks(),
                task::timer::fired()
            );
        }
        "mem" => {
            let fs = mm::frame::stats();
            let hs = mm::heap::stats();
            println!(
                "frames: {} total, {} free ({} / {})",
                fs.total,
                fs.free,
                mm::Bytes(fs.free as u64 * 4096),
                mm::Bytes(fs.total as u64 * 4096)
            );
            println!(
                "heap:   slab {} (free {}), large {}",
                mm::Bytes(hs.slab_bytes as u64),
                mm::Bytes(hs.slab_free_bytes as u64),
                mm::Bytes(hs.large_bytes as u64)
            );
        }
        "tasks" => {
            let st = task::stats();
            println!(
                "{} live, {} ready, {} spawned, {} finished, {} polls, {} idle entries",
                st.live, st.ready, st.spawned, st.finished, st.polls, st.idle_enters
            );
            for (id, name, polls) in task::list() {
                println!("  #{:<4} {:<24} polls={}", id, name, polls);
            }
        }
        "timers" => println!("{} pending timers, {} fired", task::timer::pending(), task::timer::fired()),
        "sleep" => {
            let ms = arg_u64(args, 0, 100);
            let t0 = time::now();
            task::timer::sleep_ms(ms).await;
            println!("slept {} us", time::tsc_to_us(time::now() - t0));
        }
        "spin" => {
            let ms = arg_u64(args, 0, 100);
            let t0 = time::now();
            time::busy_wait_us(ms * 1000);
            println!("spun {} us", time::tsc_to_us(time::now() - t0));
        }
        "cpuid" => {
            let brand = cpu::brand();
            println!("{}", core::str::from_utf8(&brand).unwrap_or("?").trim_matches(char::from(0)));
            let r = cpu::cpuid(0x8000_000A, 0);
            println!("svm: rev {} asids {} features {:#x}", r.eax & 0xFF, r.ebx, r.edx);
        }
        "selftest" => {
            crate::selftest::run(args.first().copied()).await;
        }
        "exit" | "quit" | "shutdown" => {
            let code = args.first().and_then(|s| parse_u64(s)).unwrap_or_else(|| {
                if crate::selftest::any_failed() {
                    2
                } else {
                    1
                }
            });
            println!("exiting with code {}", code);
            cpu::qemu_exit(code as u32);
        }
        "panic" => panic!("requested from shell"),
        _ => {
            if !crate::commands::dispatch(cmd, args).await {
                println!("unknown command: {} (try help)", cmd);
            }
        }
    }
}
