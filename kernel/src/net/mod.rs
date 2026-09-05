//! Network stack: Ethernet, ARP, IPv4, ICMP, UDP, TCP and a DHCP client.
//!
//! The primary interface sits on the virtio-net card.  Each Linux VM gets
//! its own point-to-point interface (`vmlink`), so guests can have identical
//! addresses: the interface, not the address, identifies the peer.

#![allow(dead_code)]

pub mod arp;
pub mod dhcp;
pub mod http;
pub mod proxy;
pub mod tcp;
pub mod udp;
pub mod vmlink;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};

use crate::selftest::{check, tests, TestFn, TestResult};
use crate::sync::{OnceCell, SpinLock};
use crate::task::{self, timer, Notify};
use crate::time;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash, Default)]
pub struct Mac(pub [u8; 6]);

impl Mac {
    pub const BROADCAST: Mac = Mac([0xFF; 6]);
    pub fn is_broadcast(&self) -> bool {
        self.0 == [0xFF; 6]
    }
    pub fn is_multicast(&self) -> bool {
        self.0[0] & 1 != 0
    }
}

impl core::fmt::Display for Mac {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let m = self.0;
        write!(f, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash, Default)]
pub struct Ipv4Addr(pub [u8; 4]);

impl Ipv4Addr {
    pub const UNSPECIFIED: Ipv4Addr = Ipv4Addr([0; 4]);
    pub const BROADCAST: Ipv4Addr = Ipv4Addr([255; 4]);

    pub fn to_u32(self) -> u32 {
        u32::from_be_bytes(self.0)
    }
    pub fn from_u32(v: u32) -> Self {
        Ipv4Addr(v.to_be_bytes())
    }
    pub fn is_unspecified(&self) -> bool {
        self.0 == [0; 4]
    }
    pub fn is_broadcast(&self) -> bool {
        self.0 == [255; 4]
    }
    pub fn parse(s: &str) -> Option<Ipv4Addr> {
        let mut out = [0u8; 4];
        let mut n = 0;
        for part in s.split('.') {
            if n >= 4 {
                return None;
            }
            out[n] = part.parse().ok()?;
            n += 1;
        }
        if n == 4 {
            Some(Ipv4Addr(out))
        } else {
            None
        }
    }
}

impl core::fmt::Display for Ipv4Addr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}.{}.{}", self.0[0], self.0[1], self.0[2], self.0[3])
    }
}

pub const ETH_IPV4: u16 = 0x0800;
pub const ETH_ARP: u16 = 0x0806;
pub const IP_ICMP: u8 = 1;
pub const IP_TCP: u8 = 6;
pub const IP_UDP: u8 = 17;

/// Internet checksum over `data`, folded, ones-complemented.
pub fn checksum(data: &[u8]) -> u16 {
    fold(sum(0, data))
}

pub fn sum(mut acc: u32, data: &[u8]) -> u32 {
    let mut i = 0;
    while i + 1 < data.len() {
        acc += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        acc += (data[i] as u32) << 8;
    }
    acc
}

pub fn fold(mut acc: u32) -> u16 {
    while acc >> 16 != 0 {
        acc = (acc & 0xFFFF) + (acc >> 16);
    }
    !(acc as u16)
}

/// Abstraction over a network card or virtual link.
pub trait NetDevice: Send + Sync {
    fn mac(&self) -> Mac;
    /// Queue a frame; false if it had to be dropped.
    fn send(&self, frame: &[u8]) -> bool;
    /// Non-blocking receive.
    fn recv(&self) -> Option<Vec<u8>>;
    /// Signalled when frames may be available.
    fn rx_notify(&self) -> &Notify;
    /// True if the device runs `Interface::handle_frame` itself instead of
    /// queueing frames for a receive task.
    fn inline_rx(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct IfConfig {
    pub ip: Ipv4Addr,
    pub mask: Ipv4Addr,
    pub gateway: Ipv4Addr,
    pub dns: Ipv4Addr,
    pub configured: bool,
    pub source: &'static str,
}

#[derive(Default)]
pub struct Stats {
    pub rx_frames: AtomicU64,
    pub tx_frames: AtomicU64,
    pub rx_dropped: AtomicU64,
    pub rx_arp: AtomicU64,
    pub rx_ip: AtomicU64,
    pub rx_icmp: AtomicU64,
    pub rx_udp: AtomicU64,
    pub rx_tcp: AtomicU64,
    pub rx_other: AtomicU64,
    pub tx_failed: AtomicU64,
}

struct IcmpWaiter {
    notify: Arc<Notify>,
    reply_at: Option<u64>,
}

pub struct Interface {
    pub id: u32,
    pub name: String,
    dev: Arc<dyn NetDevice>,
    pub mac: Mac,
    cfg: SpinLock<IfConfig>,
    pub arp: arp::ArpTable,
    pub udp: udp::Registry,
    pub tcp: tcp::EngineCell,
    icmp: SpinLock<BTreeMap<(u16, u16), IcmpWaiter>>,
    ip_id: AtomicU16,
    icmp_seq: AtomicU16,
    pub stats: Stats,
    pub configured: Notify,
    closed: AtomicBool,
}

static PRIMARY: OnceCell<Arc<Interface>> = OnceCell::new();
static INTERFACES: SpinLock<Vec<Arc<Interface>>> = SpinLock::new(Vec::new());
static NEXT_IF_ID: AtomicU32 = AtomicU32::new(1);

/// The primary (virtio-net) interface.
pub fn interface() -> Option<&'static Arc<Interface>> {
    PRIMARY.get()
}

pub fn interfaces() -> Vec<Arc<Interface>> {
    INTERFACES.lock().clone()
}

pub fn interface_by_id(id: u32) -> Option<Arc<Interface>> {
    INTERFACES.lock().iter().find(|i| i.id == id).cloned()
}

/// Bring the stack up on the virtio-net device, if there is one.
pub fn init() {
    let dev = match crate::virtio::net::device() {
        Some(d) => d.clone(),
        None => {
            log!("net: no network device");
            return;
        }
    };
    let iface = Arc::new(Interface::new(0, String::from("eth0"), dev));
    PRIMARY.init(iface.clone());
    INTERFACES.lock().push(iface.clone());
    let i2 = iface.clone();
    task::spawn_detached("net-rx", async move { i2.rx_loop().await });
    task::spawn_detached("dhcp", dhcp::client_task(iface));
    tcp::init();
}

/// Add a statically configured interface (a VM link) and start its receive
/// task.
pub fn add_interface(name: &str, dev: Arc<dyn NetDevice>, ip: Ipv4Addr, mask: Ipv4Addr) -> Arc<Interface> {
    let id = NEXT_IF_ID.fetch_add(1, Ordering::Relaxed);
    let inline = dev.inline_rx();
    let iface = Arc::new(Interface::new(id, String::from(name), dev));
    iface.set_config(ip, mask, Ipv4Addr::UNSPECIFIED, Ipv4Addr::UNSPECIFIED, "static");
    INTERFACES.lock().push(iface.clone());
    if !inline {
        let i2 = iface.clone();
        task::spawn_detached("if-rx", async move { i2.rx_loop().await });
    }
    iface
}

pub fn remove_interface(id: u32) {
    let removed = {
        let mut list = INTERFACES.lock();
        let pos = list.iter().position(|i| i.id == id);
        pos.map(|p| list.remove(p))
    };
    if let Some(i) = removed {
        i.closed.store(true, Ordering::Release);
        i.dev.rx_notify().notify_one();
        tcp::interface_removed(&i);
    }
}

impl Interface {
    pub fn new(id: u32, name: String, dev: Arc<dyn NetDevice>) -> Self {
        Interface {
            id,
            name,
            mac: dev.mac(),
            dev,
            cfg: SpinLock::new(IfConfig::default()),
            arp: arp::ArpTable::new(),
            udp: udp::Registry::new(),
            tcp: tcp::EngineCell::new(),
            icmp: SpinLock::new(BTreeMap::new()),
            ip_id: AtomicU16::new(1),
            icmp_seq: AtomicU16::new(1),
            stats: Stats::default(),
            configured: Notify::new(),
            closed: AtomicBool::new(false),
        }
    }

    pub fn config(&self) -> IfConfig {
        *self.cfg.lock()
    }

    pub fn set_config(&self, ip: Ipv4Addr, mask: Ipv4Addr, gateway: Ipv4Addr, dns: Ipv4Addr, source: &'static str) {
        *self.cfg.lock() = IfConfig { ip, mask, gateway, dns, configured: true, source };
        tcp::config_changed(self);
        self.configured.notify_one();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Wait until the interface has an address (or `ms` elapsed).
    pub async fn wait_configured(&self, ms: u64) -> bool {
        let deadline = time::now() + time::us_to_tsc(ms * 1000);
        while !self.config().configured {
            if time::now() >= deadline {
                return false;
            }
            let _ = timer::timeout(50, self.configured.notified()).await;
        }
        true
    }

    async fn rx_loop(&self) {
        loop {
            if self.is_closed() {
                return;
            }
            while let Some(f) = self.dev.recv() {
                self.handle_frame(&f);
            }
            self.dev.rx_notify().notified().await;
        }
    }

    /// Process one received Ethernet frame.
    pub fn handle_frame(&self, f: &[u8]) {
        self.stats.rx_frames.fetch_add(1, Ordering::Relaxed);
        if f.len() < 14 {
            self.stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let dst = Mac([f[0], f[1], f[2], f[3], f[4], f[5]]);
        if dst != self.mac && !dst.is_broadcast() {
            self.stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let ethertype = u16::from_be_bytes([f[12], f[13]]);
        let payload = &f[14..];
        match ethertype {
            ETH_ARP => {
                self.stats.rx_arp.fetch_add(1, Ordering::Relaxed);
                arp::handle(self, payload);
            }
            ETH_IPV4 => {
                self.stats.rx_ip.fetch_add(1, Ordering::Relaxed);
                self.handle_ipv4(payload);
            }
            _ => {
                self.stats.rx_other.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn handle_ipv4(&self, p: &[u8]) {
        if p.len() < 20 || p[0] >> 4 != 4 {
            return;
        }
        let ihl = ((p[0] & 0xF) as usize) * 4;
        let total = u16::from_be_bytes([p[2], p[3]]) as usize;
        if ihl < 20 || total < ihl || p.len() < total {
            return;
        }
        if checksum(&p[..ihl]) != 0 {
            return;
        }
        let proto = p[9];
        let src = Ipv4Addr([p[12], p[13], p[14], p[15]]);
        let dst = Ipv4Addr([p[16], p[17], p[18], p[19]]);
        let cfg = self.config();
        let for_us = dst == cfg.ip || dst.is_broadcast() || !cfg.configured || self.is_subnet_broadcast(&cfg, dst);
        if !for_us {
            return;
        }
        let payload = &p[ihl..total];
        match proto {
            IP_ICMP => {
                self.stats.rx_icmp.fetch_add(1, Ordering::Relaxed);
                self.handle_icmp(src, payload);
            }
            IP_UDP => {
                self.stats.rx_udp.fetch_add(1, Ordering::Relaxed);
                udp::handle(self, src, dst, payload);
            }
            IP_TCP => {
                self.stats.rx_tcp.fetch_add(1, Ordering::Relaxed);
                tcp::handle(self, &p[..total]);
            }
            _ => {
                self.stats.rx_other.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn is_subnet_broadcast(&self, cfg: &IfConfig, ip: Ipv4Addr) -> bool {
        cfg.configured && ip.to_u32() == (cfg.ip.to_u32() | !cfg.mask.to_u32())
    }

    fn handle_icmp(&self, src: Ipv4Addr, p: &[u8]) {
        if p.len() < 8 || checksum(p) != 0 {
            return;
        }
        match p[0] {
            8 => {
                let mut reply = p.to_vec();
                reply[0] = 0;
                reply[2] = 0;
                reply[3] = 0;
                let c = checksum(&reply).to_be_bytes();
                reply[2] = c[0];
                reply[3] = c[1];
                let _ = self.send_ipv4_cached(src, IP_ICMP, &reply);
            }
            0 => {
                let id = u16::from_be_bytes([p[4], p[5]]);
                let seq = u16::from_be_bytes([p[6], p[7]]);
                let mut w = self.icmp.lock();
                if let Some(e) = w.get_mut(&(id, seq)) {
                    e.reply_at = Some(time::now());
                    e.notify.notify_one();
                }
            }
            _ => {}
        }
    }

    /// Send a raw Ethernet frame.
    pub fn send_frame(&self, dst: Mac, ethertype: u16, payload: &[u8]) -> bool {
        let mut f = Vec::with_capacity(14 + payload.len());
        f.extend_from_slice(&dst.0);
        f.extend_from_slice(&self.mac.0);
        f.extend_from_slice(&ethertype.to_be_bytes());
        f.extend_from_slice(payload);
        let ok = self.dev.send(&f);
        if ok {
            self.stats.tx_frames.fetch_add(1, Ordering::Relaxed);
        } else {
            self.stats.tx_failed.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }

    fn in_subnet(&self, cfg: &IfConfig, ip: Ipv4Addr) -> bool {
        (ip.to_u32() & cfg.mask.to_u32()) == (cfg.ip.to_u32() & cfg.mask.to_u32())
    }

    pub fn next_hop(&self, cfg: &IfConfig, dst: Ipv4Addr) -> Ipv4Addr {
        if dst.is_broadcast() || !cfg.configured || cfg.gateway.is_unspecified() || self.in_subnet(cfg, dst) {
            dst
        } else {
            cfg.gateway
        }
    }

    fn build_ipv4(&self, src: Ipv4Addr, dst: Ipv4Addr, proto: u8, payload: &[u8]) -> Vec<u8> {
        let total = 20 + payload.len();
        let mut p = Vec::with_capacity(total);
        p.push(0x45);
        p.push(0);
        p.extend_from_slice(&(total as u16).to_be_bytes());
        p.extend_from_slice(&self.ip_id.fetch_add(1, Ordering::Relaxed).to_be_bytes());
        p.extend_from_slice(&0x4000u16.to_be_bytes()); // DF
        p.push(64);
        p.push(proto);
        p.extend_from_slice(&[0, 0]);
        p.extend_from_slice(&src.0);
        p.extend_from_slice(&dst.0);
        let c = checksum(&p[..20]).to_be_bytes();
        p[10] = c[0];
        p[11] = c[1];
        p.extend_from_slice(payload);
        p
    }

    /// Send an IPv4 packet using only cached ARP information (never waits).
    pub fn send_ipv4_cached(&self, dst: Ipv4Addr, proto: u8, payload: &[u8]) -> bool {
        let cfg = self.config();
        let hop = self.next_hop(&cfg, dst);
        let mac = if hop.is_broadcast() || self.is_subnet_broadcast(&cfg, hop) {
            Mac::BROADCAST
        } else {
            match self.arp.lookup(hop) {
                Some(m) => m,
                None => {
                    arp::send_request(self, hop);
                    return false;
                }
            }
        };
        let pkt = self.build_ipv4(cfg.ip, dst, proto, payload);
        self.send_frame(mac, ETH_IPV4, &pkt)
    }

    /// Send a complete IPv4 packet (header included) using cached ARP
    /// information; a missing entry triggers a request and drops the packet.
    pub fn send_ip_packet(&self, pkt: &[u8]) -> bool {
        if pkt.len() < 20 {
            return false;
        }
        let dst = Ipv4Addr([pkt[16], pkt[17], pkt[18], pkt[19]]);
        let cfg = self.config();
        let hop = self.next_hop(&cfg, dst);
        let mac = if hop.is_broadcast() || self.is_subnet_broadcast(&cfg, hop) {
            Mac::BROADCAST
        } else {
            match self.arp.lookup(hop) {
                Some(m) => m,
                None => {
                    arp::send_request(self, hop);
                    self.stats.tx_failed.fetch_add(1, Ordering::Relaxed);
                    return false;
                }
            }
        };
        self.send_frame(mac, ETH_IPV4, pkt)
    }

    /// Send an IPv4 packet, resolving the next hop if necessary.
    pub async fn send_ipv4(&self, dst: Ipv4Addr, proto: u8, payload: &[u8]) -> bool {
        let src = self.config().ip;
        self.send_ipv4_from(src, dst, proto, payload).await
    }

    pub async fn send_ipv4_from(&self, src: Ipv4Addr, dst: Ipv4Addr, proto: u8, payload: &[u8]) -> bool {
        let cfg = self.config();
        let hop = self.next_hop(&cfg, dst);
        let mac = if hop.is_broadcast() || self.is_subnet_broadcast(&cfg, hop) {
            Mac::BROADCAST
        } else {
            match self.arp.resolve(self, hop).await {
                Some(m) => m,
                None => {
                    self.stats.tx_failed.fetch_add(1, Ordering::Relaxed);
                    return false;
                }
            }
        };
        let pkt = self.build_ipv4(src, dst, proto, payload);
        self.send_frame(mac, ETH_IPV4, &pkt)
    }

    /// ICMP echo; returns the round-trip time in microseconds.
    pub async fn ping(&self, dst: Ipv4Addr, timeout_ms: u64) -> Option<u64> {
        let id = 0xC0C0u16;
        let seq = self.icmp_seq.fetch_add(1, Ordering::Relaxed);
        let notify = Arc::new(Notify::new());
        self.icmp.lock().insert((id, seq), IcmpWaiter { notify: notify.clone(), reply_at: None });
        let mut pkt = Vec::with_capacity(64);
        pkt.extend_from_slice(&[8, 0, 0, 0]);
        pkt.extend_from_slice(&id.to_be_bytes());
        pkt.extend_from_slice(&seq.to_be_bytes());
        for i in 0..56u8 {
            pkt.push(i);
        }
        let c = checksum(&pkt).to_be_bytes();
        pkt[2] = c[0];
        pkt[3] = c[1];
        let t0 = time::now();
        let sent = self.send_ipv4(dst, IP_ICMP, &pkt).await;
        let result = if sent {
            match timer::timeout(timeout_ms, notify.notified()).await {
                Ok(()) => self.icmp.lock().get(&(id, seq)).and_then(|w| w.reply_at).map(|t| time::tsc_to_us(t - t0)),
                Err(_) => None,
            }
        } else {
            None
        };
        self.icmp.lock().remove(&(id, seq));
        result
    }
}

// ---------------------------------------------------------------- shell ---

pub fn help() {
    println!("net:    net   ifaces   ping <ip> [count]   arp   dhcp   udp-echo <port>   udp-send <ip> <port> <text>   tcp   proxy");
}

fn print_iface_stats(iface: &Interface) {
    let c = iface.config();
    let s = &iface.stats;
    println!(
        "{} (id {}): mac {} ip {} mask {} gw {} ({}); rx {} ({} arp, {} ip, {} icmp, {} udp, {} tcp, {} other, {} dropped) tx {} ({} failed)",
        iface.name,
        iface.id,
        iface.mac,
        c.ip,
        c.mask,
        c.gateway,
        if c.configured { c.source } else { "unconfigured" },
        s.rx_frames.load(Ordering::Relaxed),
        s.rx_arp.load(Ordering::Relaxed),
        s.rx_ip.load(Ordering::Relaxed),
        s.rx_icmp.load(Ordering::Relaxed),
        s.rx_udp.load(Ordering::Relaxed),
        s.rx_tcp.load(Ordering::Relaxed),
        s.rx_other.load(Ordering::Relaxed),
        s.rx_dropped.load(Ordering::Relaxed),
        s.tx_frames.load(Ordering::Relaxed),
        s.tx_failed.load(Ordering::Relaxed)
    );
}

pub async fn dispatch(cmd: &str, args: &[&str]) -> bool {
    if cmd == "tcp" {
        tcp::print_status();
        return true;
    }
    if cmd == "proxy" {
        proxy::print_status();
        return true;
    }
    let iface = match interface() {
        Some(i) => i,
        None => {
            if matches!(cmd, "net" | "ifaces" | "ping" | "arp" | "dhcp" | "udp-echo" | "udp-send") {
                println!("no network interface");
                return true;
            }
            return false;
        }
    };
    match cmd {
        "net" => {
            print_iface_stats(iface);
            if let Some(d) = crate::virtio::net::device() {
                println!(
                    "driver: {} rx irqs, {} tx drops, link {}",
                    d.stats.rx_irqs.load(Ordering::Relaxed),
                    d.stats.tx_drops.load(Ordering::Relaxed),
                    if d.link_up() { "up" } else { "down" }
                );
            }
            true
        }
        "ifaces" => {
            for i in interfaces() {
                print_iface_stats(&i);
            }
            true
        }
        "ping" => {
            let ip = match args.first().and_then(|s| Ipv4Addr::parse(s)) {
                Some(ip) => ip,
                None => {
                    println!("usage: ping <ip> [count]");
                    return true;
                }
            };
            let count = crate::shell::arg_u64(args, 1, 3);
            for i in 0..count {
                match iface.ping(ip, 1000).await {
                    Some(us) => println!("reply from {}: seq={} time={}.{:03} ms", ip, i, us / 1000, us % 1000),
                    None => println!("request timed out (seq={})", i),
                }
            }
            true
        }
        "arp" => {
            for i in interfaces() {
                for (ip, mac, age_ms) in i.arp.entries() {
                    println!("{:<8} {:<16} {}  {} ms", i.name, format!("{}", ip), mac, age_ms);
                }
            }
            true
        }
        "dhcp" => {
            match dhcp::run_once(iface, 0).await {
                Some(l) => {
                    iface.set_config(l.ip, l.mask, l.gateway, l.dns, "dhcp");
                    println!("lease: ip {} mask {} gw {} dns {} ({} s)", l.ip, l.mask, l.gateway, l.dns, l.lease_secs);
                }
                None => println!("no DHCP response"),
            }
            true
        }
        "udp-echo" => {
            let port = crate::shell::arg_u64(args, 0, 7777) as u16;
            match udp::UdpSocket::bind(port) {
                Ok(sock) => {
                    println!("udp echo server on port {}", port);
                    task::spawn_detached("udp-echo", async move {
                        loop {
                            let d = sock.recv().await;
                            let text = String::from_utf8_lossy(&d.data);
                            log!("udp-echo: {} bytes from {}:{}: {}", d.data.len(), d.src, d.src_port, text.trim_end());
                            let mut reply = Vec::from(&b"echo: "[..]);
                            reply.extend_from_slice(&d.data);
                            sock.send_to(&reply, d.src, d.src_port).await;
                        }
                    });
                }
                Err(e) => println!("bind failed: {}", e),
            }
            true
        }
        "udp-send" => {
            let ip = args.first().and_then(|s| Ipv4Addr::parse(s));
            let port = args.get(1).and_then(|s| s.parse::<u16>().ok());
            match (ip, port) {
                (Some(ip), Some(port)) => {
                    let text = args[2.min(args.len())..].join(" ");
                    match udp::UdpSocket::bind(0) {
                        Ok(sock) => {
                            let ok = sock.send_to(text.as_bytes(), ip, port).await;
                            println!("sent {} bytes to {}:{}: {}", text.len(), ip, port, ok);
                        }
                        Err(e) => println!("bind failed: {}", e),
                    }
                }
                _ => println!("usage: udp-send <ip> <port> <text>"),
            }
            true
        }
        _ => false,
    }
}

// ---------------------------------------------------------------- tests ---

pub fn tests() -> &'static [(&'static str, TestFn)] {
    tests![device_present, configured, ping_gateway, udp_dns_query, tcp_loopback]
}

async fn device_present() -> TestResult {
    check!(interface().is_some(), "no network interface");
    Ok(())
}

async fn configured() -> TestResult {
    let iface = interface().ok_or("no interface")?;
    check!(iface.wait_configured(15_000).await, "interface not configured after 15 s");
    let c = iface.config();
    check!(!c.ip.is_unspecified(), "no ip");
    Ok(())
}

async fn ping_gateway() -> TestResult {
    let iface = interface().ok_or("no interface")?;
    check!(iface.wait_configured(15_000).await, "not configured");
    let gw = iface.config().gateway;
    check!(!gw.is_unspecified(), "no gateway");
    let mut ok = 0;
    for _ in 0..3 {
        if iface.ping(gw, 2000).await.is_some() {
            ok += 1;
        }
    }
    check!(ok >= 1, "no ping replies from {}", gw);
    Ok(())
}

/// Sends a DNS query for "localhost" to the configured resolver and expects
/// any UDP response back — exercises UDP TX, ARP, routing and UDP RX.
async fn udp_dns_query() -> TestResult {
    let iface = interface().ok_or("no interface")?;
    check!(iface.wait_configured(15_000).await, "not configured");
    let dns = iface.config().dns;
    check!(!dns.is_unspecified(), "no dns server");
    let sock = udp::UdpSocket::bind(0).map_err(String::from)?;
    let mut q: Vec<u8> = Vec::new();
    q.extend_from_slice(&[0xBE, 0xEF, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    q.extend_from_slice(&[9]);
    q.extend_from_slice(b"localhost");
    q.extend_from_slice(&[0, 0, 1, 0, 1]);
    let mut got = None;
    for _ in 0..3 {
        check!(sock.send_to(&q, dns, 53).await, "udp send failed");
        if let Ok(d) = timer::timeout(2000, Box::pin(sock.recv())).await {
            got = Some(d);
            break;
        }
    }
    let d = got.ok_or("no DNS response")?;
    check!(d.data.len() >= 12 && d.data[0] == 0xBE && d.data[1] == 0xEF, "unexpected DNS response");
    Ok(())
}

/// TCP against QEMU's user-network: connect to the gateway's DNS port over
/// TCP (slirp accepts it) and expect the handshake to complete.
async fn tcp_loopback() -> TestResult {
    tcp::self_test().await
}
