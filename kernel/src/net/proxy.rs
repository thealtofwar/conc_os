//! The front door: a TCP proxy that routes each incoming connection to a
//! Linux VM chosen by name, without terminating TLS.
//!
//! For TLS the name comes from the ClientHello's `server_name` extension
//! (SNI); for plain HTTP from the `Host` header.  The first DNS label is the
//! VM name (`vm0007.conc` -> VM `vm0007`).  The proxy then opens a TCP
//! connection to the guest over its private link and splices bytes both ways.
//! Connecting to a frozen VM thaws it: the guest never knows it was gone.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use super::tcp::{self, TcpListener, TcpStream};
use super::{Interface, Ipv4Addr};
use crate::hv::vm::VmState;
use crate::task::{self, timer};
use crate::time;

/// Where every guest lives on its link.
pub const GUEST_IP: Ipv4Addr = Ipv4Addr([10, 42, 0, 2]);
pub const TLS_PORT: u16 = 443;
pub const HTTP_PORT: u16 = 80;
const SNIFF_LIMIT: usize = 16 * 1024;
const SNIFF_TIMEOUT_MS: u64 = 5000;
const CONNECT_TIMEOUT_MS: u64 = 30_000;
const SPLICE_IDLE_LIMIT_MS: u64 = 120_000;

#[derive(Default)]
pub struct Stats {
    pub accepted: AtomicU64,
    pub routed: AtomicU64,
    pub no_name: AtomicU64,
    pub unknown_vm: AtomicU64,
    pub connect_failed: AtomicU64,
    pub cold: AtomicU64,
    pub warm: AtomicU64,
    pub cold_connect_us: AtomicU64,
    pub warm_connect_us: AtomicU64,
    pub cold_connect_max_us: AtomicU64,
    pub warm_connect_max_us: AtomicU64,
    pub bytes_to_vm: AtomicU64,
    pub bytes_from_vm: AtomicU64,
    pub active: AtomicU64,
}

pub static STATS: Stats = Stats {
    accepted: AtomicU64::new(0),
    routed: AtomicU64::new(0),
    no_name: AtomicU64::new(0),
    unknown_vm: AtomicU64::new(0),
    connect_failed: AtomicU64::new(0),
    cold: AtomicU64::new(0),
    warm: AtomicU64::new(0),
    cold_connect_us: AtomicU64::new(0),
    warm_connect_us: AtomicU64::new(0),
    cold_connect_max_us: AtomicU64::new(0),
    warm_connect_max_us: AtomicU64::new(0),
    bytes_to_vm: AtomicU64::new(0),
    bytes_from_vm: AtomicU64::new(0),
    active: AtomicU64::new(0),
};

/// Host timestamps (TSC) of the last routed connection, for profiling.
#[derive(Clone, Copy, Debug, Default)]
pub struct RouteTiming {
    pub vm_id: u32,
    pub accepted: u64,
    pub named: u64,
    pub connected: u64,
    pub first_byte: u64,
    pub done: u64,
    pub cold: bool,
}

pub static LAST_ROUTE: crate::sync::SpinLock<Option<RouteTiming>> = crate::sync::SpinLock::new(None);

pub fn last_route() -> Option<RouteTiming> {
    *LAST_ROUTE.lock()
}

fn max_store(a: &AtomicU64, v: u64) {
    let mut cur = a.load(Ordering::Relaxed);
    while v > cur {
        match a.compare_exchange_weak(cur, v, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(c) => cur = c,
        }
    }
}

/// Start the proxy on the primary interface once it has an address.
pub fn start() {
    task::spawn_detached("proxy-main", async {
        let iface = match super::interface() {
            Some(i) => i.clone(),
            None => return,
        };
        if !iface.wait_configured(60_000).await {
            log!("proxy: primary interface never configured; not listening");
            return;
        }
        for port in [TLS_PORT, HTTP_PORT] {
            match listen_on(iface.clone(), port) {
                Ok(()) => log!("proxy: listening on {}:{} ({})", iface.config().ip, port, if port == TLS_PORT { "SNI" } else { "Host header" }),
                Err(e) => log!("proxy: cannot listen on port {}: {}", port, e),
            }
        }
    });
}

/// Listen on one interface and route everything that arrives.
pub fn listen_on(iface: Arc<Interface>, port: u16) -> Result<(), &'static str> {
    let listener = TcpListener::bind(iface, port)?;
    task::spawn_detached("proxy-accept", async move {
        loop {
            let client = listener.accept().await;
            STATS.accepted.fetch_add(1, Ordering::Relaxed);
            task::spawn_detached("proxy-conn", handle_conn(client));
        }
    });
    Ok(())
}

pub enum Sniff {
    Name(String),
    NeedMore,
    Bad,
}

/// Extract the routing name from the first bytes of a connection.
pub fn sniff(buf: &[u8]) -> Sniff {
    if buf.is_empty() {
        return Sniff::NeedMore;
    }
    if buf[0] == 0x16 {
        return sniff_tls(buf);
    }
    sniff_http(buf)
}

fn sniff_tls(buf: &[u8]) -> Sniff {
    if buf.len() < 5 {
        return Sniff::NeedMore;
    }
    if buf[1] != 3 {
        return Sniff::Bad;
    }
    let rec_len = u16::from_be_bytes([buf[3], buf[4]]) as usize;
    if rec_len > SNIFF_LIMIT {
        return Sniff::Bad;
    }
    if buf.len() < 5 + rec_len {
        return Sniff::NeedMore;
    }
    let h = &buf[5..5 + rec_len];
    // Handshake header: type(1) len(3)
    if h.len() < 4 || h[0] != 1 {
        return Sniff::Bad;
    }
    let mut p = 4;
    // client_version(2) random(32)
    p += 34;
    // session id
    if p >= h.len() {
        return Sniff::Bad;
    }
    p += 1 + h[p] as usize;
    // cipher suites
    if p + 2 > h.len() {
        return Sniff::Bad;
    }
    p += 2 + u16::from_be_bytes([h[p], h[p + 1]]) as usize;
    // compression methods
    if p >= h.len() {
        return Sniff::Bad;
    }
    p += 1 + h[p] as usize;
    // extensions
    if p + 2 > h.len() {
        return Sniff::Bad; // no extensions: no SNI
    }
    let ext_len = u16::from_be_bytes([h[p], h[p + 1]]) as usize;
    p += 2;
    let end = (p + ext_len).min(h.len());
    while p + 4 <= end {
        let typ = u16::from_be_bytes([h[p], h[p + 1]]);
        let len = u16::from_be_bytes([h[p + 2], h[p + 3]]) as usize;
        p += 4;
        if p + len > end {
            return Sniff::Bad;
        }
        if typ == 0 {
            // server_name: list_len(2) { type(1) len(2) name }
            let e = &h[p..p + len];
            let mut q = 2;
            while q + 3 <= e.len() {
                let nt = e[q];
                let nl = u16::from_be_bytes([e[q + 1], e[q + 2]]) as usize;
                q += 3;
                if q + nl > e.len() {
                    return Sniff::Bad;
                }
                if nt == 0 {
                    return match core::str::from_utf8(&e[q..q + nl]) {
                        Ok(s) => Sniff::Name(String::from(s)),
                        Err(_) => Sniff::Bad,
                    };
                }
                q += nl;
            }
            return Sniff::Bad;
        }
        p += len;
    }
    Sniff::Bad
}

fn sniff_http(buf: &[u8]) -> Sniff {
    const METHODS: [&[u8]; 8] = [b"GET ", b"POST ", b"HEAD ", b"PUT ", b"DELETE ", b"OPTIONS ", b"PATCH ", b"CONNECT "];
    let n = buf.len().min(8);
    if !METHODS.iter().any(|m| buf[..n].starts_with(&m[..m.len().min(n)])) {
        return Sniff::Bad;
    }
    if buf.len() > SNIFF_LIMIT {
        return Sniff::Bad;
    }
    let end = match buf.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(e) => e,
        None => return Sniff::NeedMore,
    };
    let head = match core::str::from_utf8(&buf[..end]) {
        Ok(s) => s,
        Err(_) => return Sniff::Bad,
    };
    for line in head.split("\r\n").skip(1) {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("host") {
                let host = v.trim();
                let host = host.split(':').next().unwrap_or(host);
                return Sniff::Name(String::from(host));
            }
        }
    }
    Sniff::Bad
}

/// The VM name is the first DNS label.
pub fn vm_name(server_name: &str) -> &str {
    server_name.split('.').next().unwrap_or(server_name).trim()
}

async fn handle_conn(client: TcpStream) {
    STATS.active.fetch_add(1, Ordering::Relaxed);
    route(client).await;
    STATS.active.fetch_sub(1, Ordering::Relaxed);
}

async fn route(client: TcpStream) {
    let mut timing = RouteTiming { accepted: time::now(), ..Default::default() };
    // 1. Sniff the name from the first bytes.
    let mut buf = Vec::new();
    let deadline = time::now() + time::us_to_tsc(SNIFF_TIMEOUT_MS * 1000);
    let name = loop {
        match sniff(&buf) {
            Sniff::Name(n) => break Some(n),
            Sniff::Bad => break None,
            Sniff::NeedMore => {
                let mut tmp = [0u8; 4096];
                let n = match timer::timeout_until(deadline, alloc::boxed::Box::pin(client.read(&mut tmp))).await {
                    Ok(Ok(n)) => n,
                    _ => 0,
                };
                if n == 0 {
                    break None;
                }
                buf.extend_from_slice(&tmp[..n]);
                if buf.len() > SNIFF_LIMIT {
                    break None;
                }
            }
        }
    };
    let name = match name {
        Some(n) => n,
        None => {
            STATS.no_name.fetch_add(1, Ordering::Relaxed);
            client.abort();
            return;
        }
    };

    // 2. Find the VM and its link.
    let vm = match crate::hv::manager::manager().find(vm_name(&name)) {
        Some(v) if v.kind.is_linux() && !v.is_finished() => v,
        _ => {
            STATS.unknown_vm.fetch_add(1, Ordering::Relaxed);
            if buf[0] != 0x16 {
                let _ = client.write_all(b"HTTP/1.0 502 Bad Gateway\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nno such vm\n").await;
                client.flush(1000).await;
            }
            client.close();
            return;
        }
    };
    let iface = match vm.link().and_then(|l| l.interface()) {
        Some(i) => i,
        None => {
            STATS.unknown_vm.fetch_add(1, Ordering::Relaxed);
            client.abort();
            return;
        }
    };
    let cold = vm.state() == VmState::Frozen;
    timing.named = time::now();
    timing.vm_id = vm.id;
    timing.cold = cold;
    let target_port = client_port(&client);

    // 3. Connect to the guest (this thaws a frozen VM).
    let t0 = time::now();
    let upstream = match TcpStream::connect(iface, GUEST_IP, target_port, CONNECT_TIMEOUT_MS).await {
        Ok(s) => s,
        Err(e) => {
            STATS.connect_failed.fetch_add(1, Ordering::Relaxed);
            let s = vm.stats();
            let link = vm.link();
            log!(
                "proxy: {} -> vm {} port {}: connect failed: {} (state {}, has_work {}, link to_guest {} sent {} recv {}; runs {} exits {} halts {} thaws {} freezes {}; {} private pages)",
                name,
                vm.name,
                target_port,
                e,
                vm.state(),
                vm.has_work(),
                link.as_ref().map(|l| l.pending_to_guest()).unwrap_or(0),
                link.as_ref().map(|l| l.sent_to_guest.load(Ordering::Relaxed)).unwrap_or(0),
                link.as_ref().map(|l| l.received_from_guest.load(Ordering::Relaxed)).unwrap_or(0),
                s.runs,
                s.exits,
                s.halts,
                s.thaws,
                s.freezes,
                s.private_pages
            );
            vm.command(crate::hv::vm::Command::Dump);
            if buf[0] != 0x16 {
                let _ = client.write_all(b"HTTP/1.0 504 Gateway Timeout\r\nConnection: close\r\n\r\nguest did not answer\n").await;
                client.flush(1000).await;
            }
            client.close();
            return;
        }
    };
    let us = time::tsc_to_us(time::now() - t0);
    timing.connected = time::now();
    *LAST_ROUTE.lock() = Some(timing);
    if cold {
        STATS.cold.fetch_add(1, Ordering::Relaxed);
        STATS.cold_connect_us.fetch_add(us, Ordering::Relaxed);
        max_store(&STATS.cold_connect_max_us, us);
    } else {
        STATS.warm.fetch_add(1, Ordering::Relaxed);
        STATS.warm_connect_us.fetch_add(us, Ordering::Relaxed);
        max_store(&STATS.warm_connect_max_us, us);
    }
    STATS.routed.fetch_add(1, Ordering::Relaxed);
    vm.update_stats(|s| s.proxied += 1);

    // 4. Splice.
    if upstream.write_all(&buf).await.is_err() {
        client.abort();
        return;
    }
    STATS.bytes_to_vm.fetch_add(buf.len() as u64, Ordering::Relaxed);
    let client = Arc::new(client);
    let upstream = Arc::new(upstream);
    let (c2, u2) = (client.clone(), upstream.clone());
    let down = task::spawn("proxy-down", async move {
        // First byte from the guest is timed separately, then plain splice.
        let mut first = [0u8; 8192];
        let mut n = 0usize;
        let mut first_byte = 0u64;
        match u2.read(&mut first).await {
            Ok(k) if k > 0 => {
                first_byte = time::now();
                n = k;
                if c2.write_all(&first[..k]).await.is_ok() {
                    n += tcp::pump(&u2, &c2).await;
                } else {
                    c2.close();
                }
            }
            _ => c2.close(),
        }
        STATS.bytes_from_vm.fetch_add(n as u64, Ordering::Relaxed);
        (n, first_byte)
    });
    let n = tcp::pump(&client, &upstream).await;
    STATS.bytes_to_vm.fetch_add(n as u64, Ordering::Relaxed);
    // The guest closes once it has answered; give it a bounded time.
    match timer::timeout(SPLICE_IDLE_LIMIT_MS, down).await {
        Ok((_, first_byte)) => timing.first_byte = first_byte,
        Err(_) => {
            upstream.abort();
            client.abort();
        }
    }
    timing.done = time::now();
    *LAST_ROUTE.lock() = Some(timing);
}

/// The port the client connected to on our side, so 80 goes to the guest's
/// 80 and 443 to its 443 (and test listeners on other ports go to 443).
fn client_port(client: &TcpStream) -> u16 {
    match client.local_port() {
        Some(HTTP_PORT) => HTTP_PORT,
        Some(p) if p >= 8000 && p < 8443 => HTTP_PORT,
        _ => TLS_PORT,
    }
}

/// A minimal TLS 1.2 ClientHello carrying `sni` (for tests and load
/// generation without a TLS stack).
pub fn client_hello(sni: &str) -> Vec<u8> {
    let mut ext = Vec::new();
    // server_name
    let name = sni.as_bytes();
    let mut sn = Vec::new();
    sn.extend_from_slice(&((name.len() + 3) as u16).to_be_bytes());
    sn.push(0);
    sn.extend_from_slice(&(name.len() as u16).to_be_bytes());
    sn.extend_from_slice(name);
    ext.extend_from_slice(&0u16.to_be_bytes());
    ext.extend_from_slice(&(sn.len() as u16).to_be_bytes());
    ext.extend_from_slice(&sn);
    // supported_groups: x25519, secp256r1
    ext.extend_from_slice(&[0x00, 0x0a, 0x00, 0x06, 0x00, 0x04, 0x00, 0x1d, 0x00, 0x17]);
    // ec_point_formats: uncompressed
    ext.extend_from_slice(&[0x00, 0x0b, 0x00, 0x02, 0x01, 0x00]);
    // signature_algorithms
    ext.extend_from_slice(&[0x00, 0x0d, 0x00, 0x0a, 0x00, 0x08, 0x04, 0x03, 0x08, 0x04, 0x04, 0x01, 0x05, 0x01]);
    // supported_versions: TLS 1.2 only (no key_share needed)
    ext.extend_from_slice(&[0x00, 0x2b, 0x00, 0x03, 0x02, 0x03, 0x03]);

    let mut body = Vec::new();
    body.extend_from_slice(&[0x03, 0x03]);
    for i in 0..32u8 {
        body.push(i.wrapping_mul(37).wrapping_add(11));
    }
    body.push(0); // session id
    let suites: [u16; 4] = [0xC02B, 0xC02F, 0xC02C, 0xC030];
    body.extend_from_slice(&((suites.len() * 2) as u16).to_be_bytes());
    for s in suites {
        body.extend_from_slice(&s.to_be_bytes());
    }
    body.extend_from_slice(&[1, 0]); // compression: null
    body.extend_from_slice(&(ext.len() as u16).to_be_bytes());
    body.extend_from_slice(&ext);

    let mut hs = Vec::new();
    hs.push(1);
    let l = body.len();
    hs.extend_from_slice(&[(l >> 16) as u8, (l >> 8) as u8, l as u8]);
    hs.extend_from_slice(&body);

    let mut rec = Vec::new();
    rec.extend_from_slice(&[0x16, 0x03, 0x01]);
    rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
    rec.extend_from_slice(&hs);
    rec
}

pub fn print_status() {
    let s = &STATS;
    let cold = s.cold.load(Ordering::Relaxed);
    let warm = s.warm.load(Ordering::Relaxed);
    println!(
        "proxy: accepted {} routed {} active {}; no-name {} unknown-vm {} connect-failed {}; {} KiB to vms, {} KiB from vms",
        s.accepted.load(Ordering::Relaxed),
        s.routed.load(Ordering::Relaxed),
        s.active.load(Ordering::Relaxed),
        s.no_name.load(Ordering::Relaxed),
        s.unknown_vm.load(Ordering::Relaxed),
        s.connect_failed.load(Ordering::Relaxed),
        s.bytes_to_vm.load(Ordering::Relaxed) / 1024,
        s.bytes_from_vm.load(Ordering::Relaxed) / 1024
    );
    println!(
        "  cold (thawed) routes {}: connect avg {} ms max {} ms;  warm routes {}: connect avg {} ms max {} ms",
        cold,
        if cold > 0 { s.cold_connect_us.load(Ordering::Relaxed) / cold / 1000 } else { 0 },
        s.cold_connect_max_us.load(Ordering::Relaxed) / 1000,
        warm,
        if warm > 0 { s.warm_connect_us.load(Ordering::Relaxed) / warm / 1000 } else { 0 },
        s.warm_connect_max_us.load(Ordering::Relaxed) / 1000
    );
}
