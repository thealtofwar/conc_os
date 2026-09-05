//! Serial console (COM1) and the `print!`/`println!`/`log!` macros.

#![allow(dead_code)]

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::arch::cpu::{inb, outb};
use crate::sync::SpinLock;

const COM1: u16 = 0x3F8;

pub struct Serial {
    port: u16,
}

impl Serial {
    const fn new(port: u16) -> Self {
        Serial { port }
    }

    fn init(&self) {
        outb(self.port + 1, 0x00); // disable interrupts
        outb(self.port + 3, 0x80); // DLAB on
        outb(self.port + 0, 0x01); // divisor 1 -> 115200 baud
        outb(self.port + 1, 0x00);
        outb(self.port + 3, 0x03); // 8N1, DLAB off
        outb(self.port + 2, 0xC7); // FIFO on, clear, 14-byte threshold
        outb(self.port + 4, 0x0B); // DTR, RTS, OUT2
    }

    #[inline]
    fn tx_ready(&self) -> bool {
        inb(self.port + 5) & 0x20 != 0
    }

    pub fn write_byte(&self, b: u8) {
        while !self.tx_ready() {
            core::hint::spin_loop();
        }
        outb(self.port, b);
    }

    pub fn write_str_raw(&self, s: &str) {
        for b in s.bytes() {
            if b == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(b);
        }
    }

    /// Non-blocking read of one byte, if the receiver has one.
    pub fn read_byte(&self) -> Option<u8> {
        if inb(self.port + 5) & 0x01 != 0 {
            Some(inb(self.port))
        } else {
            None
        }
    }
}

impl Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_str_raw(s);
        Ok(())
    }
}

static SERIAL: SpinLock<Serial> = SpinLock::new(Serial::new(COM1));
static INITIALISED: AtomicBool = AtomicBool::new(false);
/// Set while a panic is being reported so that printing bypasses the lock.
static PANICKING: AtomicBool = AtomicBool::new(false);
static BYTES_WRITTEN: AtomicUsize = AtomicUsize::new(0);

pub fn init() {
    SERIAL.lock().init();
    INITIALISED.store(true, Ordering::Release);
}

pub fn set_panicking() {
    PANICKING.store(true, Ordering::Release);
    unsafe { SERIAL.force_unlock() };
}

pub fn read_byte() -> Option<u8> {
    SERIAL.lock().read_byte()
}

/// Write bytes verbatim (no newline translation): used to relay a guest's
/// serial console.
pub fn write_bytes(bytes: &[u8]) {
    if PANICKING.load(Ordering::Acquire) {
        return;
    }
    let guard = SERIAL.lock();
    for &b in bytes {
        guard.write_byte(b);
    }
    BYTES_WRITTEN.fetch_add(bytes.len(), Ordering::Relaxed);
}

/// Enable the "received data available" interrupt on COM1.
pub fn enable_rx_interrupt() {
    let s = SERIAL.lock();
    outb(s.port + 1, 0x01);
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    if PANICKING.load(Ordering::Acquire) {
        // Do not take the lock while panicking; the holder may be dead.
        let mut s = Serial::new(COM1);
        let _ = s.write_fmt(args);
        return;
    }
    let mut guard = SERIAL.lock();
    let _ = guard.write_fmt(args);
}

pub fn bytes_written() -> usize {
    BYTES_WRITTEN.load(Ordering::Relaxed)
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::console::_print(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::console::_print(format_args!("{}\n", format_args!($($arg)*))) };
}

/// Log line with an uptime prefix: `[   1.234567] message`.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {{
        let us = $crate::time::uptime_us();
        $crate::console::_print(format_args!("[{:5}.{:06}] {}\n", us / 1_000_000, us % 1_000_000, format_args!($($arg)*)));
    }};
}
