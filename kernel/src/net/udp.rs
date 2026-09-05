//! UDP sockets.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU16, AtomicU64, Ordering};

use super::{fold, interface, sum, Interface, Ipv4Addr, IP_UDP};
use crate::sync::SpinLock;
use crate::task::Notify;

const MAX_QUEUE: usize = 256;

pub struct Datagram {
    pub data: Vec<u8>,
    pub src: Ipv4Addr,
    pub src_port: u16,
    pub dst: Ipv4Addr,
}

struct SockInner {
    port: u16,
    queue: SpinLock<VecDeque<Datagram>>,
    notify: Notify,
    dropped: AtomicU64,
}

pub struct Registry {
    socks: SpinLock<BTreeMap<u16, Arc<SockInner>>>,
    next_ephemeral: AtomicU16,
}

impl Registry {
    pub fn new() -> Self {
        Registry { socks: SpinLock::new(BTreeMap::new()), next_ephemeral: AtomicU16::new(49152) }
    }
    pub fn bound_ports(&self) -> Vec<u16> {
        self.socks.lock().keys().copied().collect()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

pub struct UdpSocket {
    inner: Arc<SockInner>,
    iface: &'static Arc<Interface>,
}

impl UdpSocket {
    /// Bind a port (0 picks an ephemeral one).
    pub fn bind(port: u16) -> Result<UdpSocket, &'static str> {
        let iface = interface().ok_or("no network interface")?;
        let mut socks = iface.udp.socks.lock();
        let port = if port == 0 {
            let mut p;
            loop {
                p = iface.udp.next_ephemeral.fetch_add(1, Ordering::Relaxed);
                if p < 49152 {
                    iface.udp.next_ephemeral.store(49152, Ordering::Relaxed);
                    continue;
                }
                if !socks.contains_key(&p) {
                    break;
                }
            }
            p
        } else {
            if socks.contains_key(&port) {
                return Err("port in use");
            }
            port
        };
        let inner = Arc::new(SockInner {
            port,
            queue: SpinLock::new(VecDeque::new()),
            notify: Notify::new(),
            dropped: AtomicU64::new(0),
        });
        socks.insert(port, inner.clone());
        Ok(UdpSocket { inner, iface })
    }

    pub fn port(&self) -> u16 {
        self.inner.port
    }

    pub fn try_recv(&self) -> Option<Datagram> {
        self.inner.queue.lock().pop_front()
    }

    pub async fn recv(&self) -> Datagram {
        loop {
            if let Some(d) = self.try_recv() {
                return d;
            }
            self.inner.notify.notified().await;
        }
    }

    fn build(&self, src_ip: Ipv4Addr, dst: Ipv4Addr, dst_port: u16, data: &[u8]) -> Vec<u8> {
        let len = (8 + data.len()) as u16;
        let mut p = Vec::with_capacity(len as usize);
        p.extend_from_slice(&self.inner.port.to_be_bytes());
        p.extend_from_slice(&dst_port.to_be_bytes());
        p.extend_from_slice(&len.to_be_bytes());
        p.extend_from_slice(&[0, 0]);
        p.extend_from_slice(data);
        // Pseudo header + datagram checksum.
        let mut acc = sum(0, &src_ip.0);
        acc = sum(acc, &dst.0);
        acc += IP_UDP as u32;
        acc += len as u32;
        acc = sum(acc, &p);
        let mut c = fold(acc);
        if c == 0 {
            c = 0xFFFF;
        }
        let cb = c.to_be_bytes();
        p[6] = cb[0];
        p[7] = cb[1];
        p
    }

    pub async fn send_to(&self, data: &[u8], dst: Ipv4Addr, dst_port: u16) -> bool {
        let src = self.iface.config().ip;
        let p = self.build(src, dst, dst_port, data);
        self.iface.send_ipv4(dst, IP_UDP, &p).await
    }

    /// Send with an explicit source address (DHCP uses 0.0.0.0).
    pub async fn send_to_from(&self, src: Ipv4Addr, data: &[u8], dst: Ipv4Addr, dst_port: u16) -> bool {
        let p = self.build(src, dst, dst_port, data);
        self.iface.send_ipv4_from(src, dst, IP_UDP, &p).await
    }

    /// Send using cached ARP state only; never blocks.
    pub fn send_to_now(&self, data: &[u8], dst: Ipv4Addr, dst_port: u16) -> bool {
        let src = self.iface.config().ip;
        let p = self.build(src, dst, dst_port, data);
        self.iface.send_ipv4_cached(dst, IP_UDP, &p)
    }

    pub fn dropped(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        self.iface.udp.socks.lock().remove(&self.inner.port);
    }
}

pub fn handle(iface: &Interface, src: Ipv4Addr, dst: Ipv4Addr, p: &[u8]) {
    if p.len() < 8 {
        return;
    }
    let src_port = u16::from_be_bytes([p[0], p[1]]);
    let dst_port = u16::from_be_bytes([p[2], p[3]]);
    let len = u16::from_be_bytes([p[4], p[5]]) as usize;
    if len < 8 || len > p.len() {
        return;
    }
    let sock = match iface.udp.socks.lock().get(&dst_port) {
        Some(s) => s.clone(),
        None => return,
    };
    let data = p[8..len].to_vec();
    {
        let mut q = sock.queue.lock();
        if q.len() >= MAX_QUEUE {
            sock.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        q.push_back(Datagram { data, src, src_port, dst });
    }
    sock.notify.notify_one();
}
