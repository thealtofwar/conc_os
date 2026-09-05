//! conc_os guest image.
//!
//! This tiny freestanding program runs inside every VM.  The hypervisor
//! starts it in 64-bit mode at `_start` with:
//!
//! * `rdi` = service kind (which behaviour to run)
//! * `rsi` = guest-physical address of the request/response mailbox
//! * `rdx` = guest memory size in bytes
//!
//! Guests talk to the hypervisor exclusively through `vmmcall` hypercalls.
//! When a guest asks for the next request and none is queued, the hypervisor
//! stops scheduling it — that is what "scale to zero" means from inside.

#![no_std]
#![no_main]

use core::arch::{asm, global_asm};
use core::fmt::Write;

global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "    cld",
    "    call guest_main",
    "2:  hlt",
    "    jmp 2b",
);

/// Hypercall numbers (shared with the hypervisor's `hv::hcall` module).
pub mod hc {
    pub const LOG: u64 = 0;
    pub const WAIT_REQUEST: u64 = 1;
    pub const RESPOND: u64 = 2;
    pub const EXIT: u64 = 3;
    pub const UPTIME_US: u64 = 4;
    pub const YIELD: u64 = 5;
    pub const SLEEP_MS: u64 = 6;
}

/// Service kinds.
pub mod kind {
    pub const ECHO: u64 = 0;
    pub const PRIMES: u64 = 1;
    pub const COUNTER: u64 = 2;
    pub const SPIN: u64 = 3;
    pub const FAULT: u64 = 4;
    pub const SLEEPY: u64 = 5;
    pub const HELLO: u64 = 6;
}

#[inline(always)]
fn hypercall(n: u64, a: u64, b: u64, c: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "vmmcall",
            inout("rax") n => ret,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            options(nostack)
        );
    }
    ret
}

fn log(s: &str) {
    hypercall(hc::LOG, s.as_ptr() as u64, s.len() as u64, 0);
}

fn wait_request(buf: &mut [u8]) -> usize {
    hypercall(hc::WAIT_REQUEST, buf.as_mut_ptr() as u64, buf.len() as u64, 0) as usize
}

fn respond(s: &[u8]) {
    hypercall(hc::RESPOND, s.as_ptr() as u64, s.len() as u64, 0);
}

fn exit(code: u64) -> ! {
    hypercall(hc::EXIT, code, 0, 0);
    loop {
        unsafe { asm!("hlt") }
    }
}

fn uptime_us() -> u64 {
    hypercall(hc::UPTIME_US, 0, 0, 0)
}

fn sleep_ms(ms: u64) {
    hypercall(hc::SLEEP_MS, ms, 0, 0);
}

/// A fixed-capacity byte buffer implementing `fmt::Write`.
struct Buf<const N: usize> {
    data: [u8; N],
    len: usize,
}

impl<const N: usize> Buf<N> {
    const fn new() -> Self {
        Buf { data: [0; N], len: 0 }
    }
    fn as_bytes(&self) -> &[u8] {
        &self.data[..self.len]
    }
    fn clear(&mut self) {
        self.len = 0;
    }
}

impl<const N: usize> Write for Buf<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let room = N - self.len;
        let n = s.len().min(room);
        self.data[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

macro_rules! glog {
    ($($arg:tt)*) => {{
        let mut b: Buf<256> = Buf::new();
        let _ = write!(b, $($arg)*);
        log(core::str::from_utf8(b.as_bytes()).unwrap_or("?"));
    }};
}

fn parse_u64(s: &[u8]) -> Option<u64> {
    let s = trim(s);
    if s.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for &c in s {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((c - b'0') as u64)?;
    }
    Some(v)
}

fn trim(s: &[u8]) -> &[u8] {
    let mut a = 0;
    let mut b = s.len();
    while a < b && (s[a] == b' ' || s[a] == b'\n' || s[a] == b'\r' || s[a] == b'\t') {
        a += 1;
    }
    while b > a && (s[b - 1] == b' ' || s[b - 1] == b'\n' || s[b - 1] == b'\r' || s[b - 1] == b'\t') {
        b -= 1;
    }
    &s[a..b]
}

static mut REQ: [u8; 4096] = [0; 4096];
static mut COUNTER: u64 = 0;
/// Sieve bitmap: bit i set => i is composite.  Supports N up to 2^20.
static mut SIEVE: [u64; 16384] = [0; 16384];

fn count_primes(n: u64) -> u64 {
    let n = n.min((16384 * 64 - 1) as u64) as usize;
    let sieve = unsafe { &mut *core::ptr::addr_of_mut!(SIEVE) };
    let words = n / 64 + 1;
    for w in sieve[..words].iter_mut() {
        *w = 0;
    }
    let mut count = 0u64;
    let mut i = 2usize;
    while i <= n {
        if sieve[i / 64] & (1 << (i % 64)) == 0 {
            count += 1;
            let mut j = i * i;
            while j <= n {
                sieve[j / 64] |= 1 << (j % 64);
                j += i;
            }
        }
        i += 1;
    }
    count
}

/// Simple non-cryptographic 64-bit hash (FNV-1a) used by the echo service so
/// the reply proves the guest actually looked at the payload.
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[no_mangle]
pub extern "C" fn guest_main(kind: u64, mailbox: u64, mem_size: u64) -> ! {
    let req = unsafe { &mut *core::ptr::addr_of_mut!(REQ) };
    let mut out: Buf<4096> = Buf::new();

    match kind {
        kind::HELLO => {
            glog!("hello from a conc_os guest! mem={} KiB mailbox={:#x}", mem_size / 1024, mailbox);
            exit(0);
        }
        kind::SPIN => {
            let mut iter: u64 = 0;
            let mut x: u64 = 1;
            loop {
                for _ in 0..5_000_000u64 {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                }
                iter += 1;
                glog!("spin: {} rounds (x={:#x}) at {} us", iter, x, uptime_us());
            }
        }
        kind::FAULT => {
            glog!("fault: about to write to an unmapped address");
            unsafe {
                // 1 GiB is beyond any guest's mapped region: #PF inside the guest.
                let p = 0x4000_0000usize as *mut u64;
                core::ptr::write_volatile(p, 42);
            }
            glog!("fault: still alive?!");
            exit(1);
        }
        _ => {}
    }

    // Request/response services.
    loop {
        let n = wait_request(&mut req[..]);
        let payload = &req[..n.min(req.len())];
        out.clear();
        match kind {
            kind::ECHO => {
                let _ = write!(out, "echo[{}]: ", n);
                for &b in payload {
                    let _ = out.write_str(core::str::from_utf8(&[b.to_ascii_uppercase()]).unwrap_or("?"));
                }
                let _ = write!(out, " (fnv1a={:016x})", fnv1a(payload));
            }
            kind::PRIMES => match parse_u64(payload) {
                Some(limit) => {
                    let t0 = uptime_us();
                    let c = count_primes(limit);
                    let dt = uptime_us() - t0;
                    let _ = write!(out, "primes<={}: {} ({} us)", limit, c, dt);
                }
                None => {
                    let _ = write!(out, "error: expected a number");
                }
            },
            kind::COUNTER => {
                let c = unsafe {
                    let p = core::ptr::addr_of_mut!(COUNTER);
                    *p += 1;
                    *p
                };
                let _ = write!(out, "count={}", c);
            }
            kind::SLEEPY => {
                let ms = parse_u64(payload).unwrap_or(10);
                let t0 = uptime_us();
                sleep_ms(ms);
                let _ = write!(out, "slept {} ms (measured {} us)", ms, uptime_us() - t0);
            }
            _ => {
                let _ = write!(out, "unknown service kind {}", kind);
            }
        }
        respond(out.as_bytes());
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    glog!("guest panic: {}", info);
    exit(0xFF)
}
