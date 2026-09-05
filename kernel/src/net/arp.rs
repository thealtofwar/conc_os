//! ARP: cache, request/reply handling and async resolution.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::{Interface, Ipv4Addr, Mac, ETH_ARP};
use crate::sync::SpinLock;
use crate::task::{timer, Notify};
use crate::time;

struct Entry {
    mac: Mac,
    at: u64,
}

pub struct ArpTable {
    entries: SpinLock<BTreeMap<Ipv4Addr, Entry>>,
    waiters: SpinLock<BTreeMap<Ipv4Addr, Arc<Notify>>>,
}

impl ArpTable {
    pub fn new() -> Self {
        ArpTable { entries: SpinLock::new(BTreeMap::new()), waiters: SpinLock::new(BTreeMap::new()) }
    }

    pub fn lookup(&self, ip: Ipv4Addr) -> Option<Mac> {
        self.entries.lock().get(&ip).map(|e| e.mac)
    }

    pub fn insert(&self, ip: Ipv4Addr, mac: Mac) {
        self.entries.lock().insert(ip, Entry { mac, at: time::now() });
        let w = self.waiters.lock().remove(&ip);
        if let Some(n) = w {
            n.notify_one();
        }
    }

    /// (ip, mac, age in ms)
    pub fn entries(&self) -> Vec<(Ipv4Addr, Mac, u64)> {
        let now = time::now();
        self.entries.lock().iter().map(|(ip, e)| (*ip, e.mac, time::tsc_to_us(now - e.at) / 1000)).collect()
    }

    /// Resolve `ip`, sending up to three requests.
    pub async fn resolve(&self, iface: &Interface, ip: Ipv4Addr) -> Option<Mac> {
        if let Some(m) = self.lookup(ip) {
            return Some(m);
        }
        let notify = {
            let mut w = self.waiters.lock();
            w.entry(ip).or_insert_with(|| Arc::new(Notify::new())).clone()
        };
        for _ in 0..3 {
            send_request(iface, ip);
            let _ = timer::timeout(300, notify.notified()).await;
            if let Some(m) = self.lookup(ip) {
                return Some(m);
            }
        }
        self.waiters.lock().remove(&ip);
        None
    }
}

impl Default for ArpTable {
    fn default() -> Self {
        Self::new()
    }
}

fn build(op: u16, sha: Mac, spa: Ipv4Addr, tha: Mac, tpa: Ipv4Addr) -> Vec<u8> {
    let mut p = Vec::with_capacity(28);
    p.extend_from_slice(&1u16.to_be_bytes()); // ethernet
    p.extend_from_slice(&0x0800u16.to_be_bytes()); // ipv4
    p.push(6);
    p.push(4);
    p.extend_from_slice(&op.to_be_bytes());
    p.extend_from_slice(&sha.0);
    p.extend_from_slice(&spa.0);
    p.extend_from_slice(&tha.0);
    p.extend_from_slice(&tpa.0);
    p
}

pub fn send_request(iface: &Interface, ip: Ipv4Addr) {
    let our_ip = iface.config().ip;
    let pkt = build(1, iface.mac, our_ip, Mac([0; 6]), ip);
    iface.send_frame(Mac::BROADCAST, ETH_ARP, &pkt);
}

/// Announce our address (gratuitous ARP) so peers learn it quickly.
pub fn announce(iface: &Interface) {
    let our_ip = iface.config().ip;
    let pkt = build(1, iface.mac, our_ip, Mac([0; 6]), our_ip);
    iface.send_frame(Mac::BROADCAST, ETH_ARP, &pkt);
}

pub fn handle(iface: &Interface, p: &[u8]) {
    if p.len() < 28 {
        return;
    }
    let htype = u16::from_be_bytes([p[0], p[1]]);
    let ptype = u16::from_be_bytes([p[2], p[3]]);
    if htype != 1 || ptype != 0x0800 || p[4] != 6 || p[5] != 4 {
        return;
    }
    let op = u16::from_be_bytes([p[6], p[7]]);
    let sha = Mac([p[8], p[9], p[10], p[11], p[12], p[13]]);
    let spa = Ipv4Addr([p[14], p[15], p[16], p[17]]);
    let tpa = Ipv4Addr([p[24], p[25], p[26], p[27]]);

    if !spa.is_unspecified() && !sha.is_broadcast() {
        iface.arp.insert(spa, sha);
    }
    let cfg = iface.config();
    if op == 1 && cfg.configured && tpa == cfg.ip {
        let reply = build(2, iface.mac, cfg.ip, sha, spa);
        iface.send_frame(sha, ETH_ARP, &reply);
    }
}
