//! TCP, provided by smoltcp.
//!
//! Every `Interface` owns a lazily created smoltcp `Interface` in `Medium::Ip`
//! mode: our own code keeps doing Ethernet, ARP, ICMP, UDP and DHCP, and
//! hands smoltcp only the IPv4 packets whose protocol is TCP.  Packets that
//! smoltcp wants to send come back as complete IPv4 packets and go out through
//! the interface's ARP cache.  One engine per interface means one socket
//! namespace per VM link, which is what lets every Linux guest use the same
//! address.
//!
//! smoltcp is polled synchronously under the engine lock: on every incoming
//! segment, after every socket operation, and from a 10 ms ticker for its
//! retransmission and delayed-close timers.  Wakers registered on the sockets
//! turn the whole thing into ordinary async streams.

#![allow(dead_code)]

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::future::poll_fn;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use core::task::Poll;

use smoltcp::iface::{Config, Interface as SmolInterface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp::{CongestionControl, Socket, SocketBuffer, State};
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address};

use super::{IfConfig, Interface, Ipv4Addr};
use crate::sync::SpinLock;
use crate::task::{self, timer, Notify};
use crate::time;

/// Bytes of buffering per direction per socket.
pub const SOCKET_BUF: usize = 64 * 1024;
/// Listening sockets kept armed per listener (the accept backlog).
const LISTEN_POOL: usize = 64;
/// Abort a connection whose peer stops answering for this long.
const USER_TIMEOUT_MS: u64 = 30_000;
const TICK_MS: u64 = 10;

static SEGMENTS_IN: AtomicU64 = AtomicU64::new(0);
static PACKETS_OUT: AtomicU64 = AtomicU64::new(0);
static ACCEPTED: AtomicU64 = AtomicU64::new(0);
static CONNECTED: AtomicU64 = AtomicU64::new(0);
static ENGINES: AtomicU64 = AtomicU64::new(0);
static NEXT_PORT: AtomicU16 = AtomicU16::new(0);

fn now_instant() -> Instant {
    Instant::from_micros(time::uptime_us() as i64)
}

fn to_smol(ip: Ipv4Addr) -> Ipv4Address {
    Ipv4Address::from(ip.0)
}

fn from_smol(a: IpAddress) -> Ipv4Addr {
    match a {
        IpAddress::Ipv4(v) => Ipv4Addr(v.octets()),
    }
}

// --------------------------------------------------------------- device ---

/// smoltcp's view of an interface: queues of complete IPv4 packets.
#[derive(Default)]
struct IpDevice {
    rx: VecDeque<Vec<u8>>,
    tx: Vec<Vec<u8>>,
}

struct IpRxToken(Vec<u8>);

impl RxToken for IpRxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

struct IpTxToken<'a>(&'a mut Vec<Vec<u8>>);

impl TxToken for IpTxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut b = vec![0u8; len];
        let r = f(&mut b);
        self.0.push(b);
        r
    }
}

impl Device for IpDevice {
    type RxToken<'a> = IpRxToken;
    type TxToken<'a> = IpTxToken<'a>;

    fn receive(&mut self, _now: Instant) -> Option<(IpRxToken, IpTxToken<'_>)> {
        let p = self.rx.pop_front()?;
        Some((IpRxToken(p), IpTxToken(&mut self.tx)))
    }

    fn transmit(&mut self, _now: Instant) -> Option<IpTxToken<'_>> {
        Some(IpTxToken(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ip;
        c.max_transmission_unit = 1500;
        c
    }
}

// --------------------------------------------------------------- engine ---

pub struct Engine {
    smol: SmolInterface,
    dev: IpDevice,
    sockets: SocketSet<'static>,
    /// Sockets whose stream was dropped; removed once fully closed.
    orphans: Vec<SocketHandle>,
    polls: u64,
}

impl Engine {
    fn new(iface: &Interface, cfg: &IfConfig) -> Box<Engine> {
        let mut dev = IpDevice::default();
        let mut config = Config::new(HardwareAddress::Ip);
        config.random_seed = time::now() ^ ((iface.id as u64) << 40);
        let smol = SmolInterface::new(config, &mut dev, now_instant());
        let mut e = Box::new(Engine { smol, dev, sockets: SocketSet::new(Vec::new()), orphans: Vec::new(), polls: 0 });
        e.apply_config(cfg);
        ENGINES.fetch_add(1, Ordering::Relaxed);
        e
    }

    fn apply_config(&mut self, cfg: &IfConfig) {
        let prefix = cfg.mask.to_u32().count_ones() as u8;
        self.smol.update_ip_addrs(|a| {
            a.clear();
            let _ = a.push(IpCidr::new(IpAddress::Ipv4(to_smol(cfg.ip)), prefix));
        });
        self.smol.routes_mut().remove_default_ipv4_route();
        if !cfg.gateway.is_unspecified() {
            let _ = self.smol.routes_mut().add_default_ipv4_route(to_smol(cfg.gateway));
        }
    }

    /// Run smoltcp, reap closed orphans and push its output onto the wire.
    fn poll(&mut self, iface: &Interface) {
        self.polls += 1;
        let now = now_instant();
        self.smol.poll(now, &mut self.dev, &mut self.sockets);
        if !self.orphans.is_empty() {
            let sockets = &mut self.sockets;
            self.orphans.retain(|h| {
                let s = sockets.get::<Socket>(*h);
                if s.state() == State::Closed {
                    sockets.remove(*h);
                    false
                } else {
                    true
                }
            });
        }
        for p in self.dev.tx.drain(..) {
            PACKETS_OUT.fetch_add(1, Ordering::Relaxed);
            iface.send_ip_packet(&p);
        }
    }

    fn due(&mut self) -> bool {
        let now = now_instant();
        match self.smol.poll_at(now, &self.sockets) {
            Some(at) => at <= now,
            None => false,
        }
    }

    fn new_socket() -> Socket<'static> {
        let mut s = Socket::new(SocketBuffer::new(vec![0u8; SOCKET_BUF]), SocketBuffer::new(vec![0u8; SOCKET_BUF]));
        s.set_congestion_control(CongestionControl::Reno);
        s.set_ack_delay(None);
        s.set_nagle_enabled(false);
        s.set_timeout(Some(Duration::from_millis(USER_TIMEOUT_MS)));
        s
    }
}

/// Holder for an interface's lazily created engine.
pub struct EngineCell {
    inner: SpinLock<Option<Box<Engine>>>,
    present: AtomicBool,
}

impl EngineCell {
    pub const fn new() -> Self {
        EngineCell { inner: SpinLock::new(None), present: AtomicBool::new(false) }
    }

    pub fn is_present(&self) -> bool {
        self.present.load(Ordering::Relaxed)
    }
}

/// Run `f` on the interface's engine (creating it if the interface has an
/// address), then poll.  `None` if the interface is unusable.
fn with_engine<R>(iface: &Interface, f: impl FnOnce(&mut Engine) -> R) -> Option<R> {
    let mut guard = iface.tcp.inner.lock();
    if guard.is_none() {
        if iface.is_closed() {
            return None;
        }
        let cfg = iface.config();
        if !cfg.configured || cfg.ip.is_unspecified() {
            return None;
        }
        *guard = Some(Engine::new(iface, &cfg));
        iface.tcp.present.store(true, Ordering::Relaxed);
    }
    let e = guard.as_mut().unwrap();
    let r = f(e);
    e.poll(iface);
    Some(r)
}

/// Start the timer ticker.
pub fn init() {
    task::spawn_detached("tcp-timer", ticker());
}

async fn ticker() {
    loop {
        timer::sleep_ms(TICK_MS).await;
        for iface in super::interfaces() {
            if !iface.tcp.is_present() {
                continue;
            }
            let mut guard = iface.tcp.inner.lock();
            if let Some(e) = guard.as_mut() {
                if e.due() {
                    e.poll(&iface);
                }
            }
        }
    }
}

/// An IPv4 packet carrying TCP arrived on `iface`.
pub fn handle(iface: &Interface, packet: &[u8]) {
    SEGMENTS_IN.fetch_add(1, Ordering::Relaxed);
    with_engine(iface, |e| e.dev.rx.push_back(packet.to_vec()));
}

/// The interface's address or gateway changed.
pub fn config_changed(iface: &Interface) {
    if !iface.tcp.is_present() {
        return;
    }
    let cfg = iface.config();
    let mut guard = iface.tcp.inner.lock();
    if let Some(e) = guard.as_mut() {
        e.apply_config(&cfg);
    }
}

/// The interface is going away: abort everything so waiters wake up.
pub fn interface_removed(iface: &Interface) {
    let mut guard = iface.tcp.inner.lock();
    if let Some(e) = guard.as_mut() {
        for (_, s) in e.sockets.iter_mut() {
            let smoltcp::socket::Socket::Tcp(t) = s;
            t.abort();
        }
        e.poll(iface);
    }
}

// ---------------------------------------------------------------- stream ---

pub struct TcpStream {
    iface: Arc<Interface>,
    h: SocketHandle,
}

fn socket_error(state: State) -> &'static str {
    match state {
        State::Closed => "connection closed",
        _ => "connection not open",
    }
}

impl TcpStream {
    /// Open a connection to `remote:port` through `iface`.
    pub async fn connect(iface: Arc<Interface>, remote: Ipv4Addr, port: u16, timeout_ms: u64) -> Result<TcpStream, &'static str> {
        // Resolve the next hop first so the SYN does not wait on ARP.
        let cfg = iface.config();
        let hop = iface.next_hop(&cfg, remote);
        if iface.arp.resolve(&iface, hop).await.is_none() {
            return Err("next hop did not answer ARP");
        }
        let h = with_engine(&iface, |e| {
            let mut s = Engine::new_socket();
            let local_port = 40000 + (NEXT_PORT.fetch_add(1, Ordering::Relaxed) % 20000);
            let remote_ep: IpEndpoint = (to_smol(remote), port).into();
            match s.connect(e.smol.context(), remote_ep, local_port) {
                Ok(()) => Ok(e.sockets.add(s)),
                Err(_) => Err("connect: invalid state"),
            }
        })
        .ok_or("interface not configured")??;
        let stream = TcpStream { iface, h };
        let wait = poll_fn(|cx| {
            match with_engine(&stream.iface, |e| {
                let s = e.sockets.get_mut::<Socket>(h);
                match s.state() {
                    State::Established | State::CloseWait => Some(Ok(())),
                    State::Closed | State::TimeWait => Some(Err("connection refused")),
                    _ => {
                        s.register_send_waker(cx.waker());
                        None
                    }
                }
            }) {
                Some(Some(r)) => Poll::Ready(r),
                Some(None) => Poll::Pending,
                None => Poll::Ready(Err("interface gone")),
            }
        });
        match timer::timeout(timeout_ms, Box::pin(wait)).await {
            Ok(Ok(())) => {
                CONNECTED.fetch_add(1, Ordering::Relaxed);
                Ok(stream)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err("connect timed out"),
        }
    }

    /// Read available bytes; `Ok(0)` means the peer closed.
    pub async fn read(&self, buf: &mut [u8]) -> Result<usize, &'static str> {
        poll_fn(|cx| {
            match with_engine(&self.iface, |e| {
                let s = e.sockets.get_mut::<Socket>(self.h);
                if s.can_recv() {
                    Some(Ok(s.recv_slice(buf).unwrap_or(0)))
                } else if !s.may_recv() {
                    Some(Ok(0))
                } else {
                    s.register_recv_waker(cx.waker());
                    None
                }
            }) {
                Some(Some(r)) => Poll::Ready(r),
                Some(None) => Poll::Pending,
                None => Poll::Ready(Err("interface gone")),
            }
        })
        .await
    }

    /// Append to `buf` until it holds `want` bytes, the peer closes, or
    /// `timeout_ms` passes.  Returns whether `want` was reached.
    pub async fn read_until(&self, buf: &mut Vec<u8>, want: usize, timeout_ms: u64) -> Result<bool, &'static str> {
        let deadline = time::now() + time::us_to_tsc(timeout_ms * 1000);
        let mut tmp = [0u8; 4096];
        while buf.len() < want {
            if time::now() >= deadline {
                return Ok(false);
            }
            let n = match timer::timeout_until(deadline, Box::pin(self.read(&mut tmp))).await {
                Ok(r) => r?,
                Err(_) => return Ok(false),
            };
            if n == 0 {
                return Ok(false);
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        Ok(true)
    }

    /// Queue all of `data` (waiting for buffer space as needed).
    pub async fn write_all(&self, data: &[u8]) -> Result<(), &'static str> {
        let mut off = 0;
        while off < data.len() {
            let n = poll_fn(|cx| {
                match with_engine(&self.iface, |e| {
                    let s = e.sockets.get_mut::<Socket>(self.h);
                    if !s.may_send() {
                        Some(Err(socket_error(s.state())))
                    } else if s.can_send() {
                        Some(Ok(s.send_slice(&data[off..]).unwrap_or(0)))
                    } else {
                        s.register_send_waker(cx.waker());
                        None
                    }
                }) {
                    Some(Some(r)) => Poll::Ready(r),
                    Some(None) => Poll::Pending,
                    None => Poll::Ready(Err("interface gone")),
                }
            })
            .await?;
            off += n;
        }
        Ok(())
    }

    /// Wait until everything queued has been acknowledged (or `timeout_ms`).
    pub async fn flush(&self, timeout_ms: u64) -> bool {
        let deadline = time::now() + time::us_to_tsc(timeout_ms * 1000);
        loop {
            let done = with_engine(&self.iface, |e| {
                let s = e.sockets.get::<Socket>(self.h);
                s.send_queue() == 0 || !s.may_send()
            })
            .unwrap_or(true);
            if done || time::now() >= deadline {
                return done;
            }
            timer::sleep_ms(2).await;
        }
    }

    /// Send FIN after the queued data.
    pub fn close(&self) {
        with_engine(&self.iface, |e| e.sockets.get_mut::<Socket>(self.h).close());
    }

    pub fn abort(&self) {
        with_engine(&self.iface, |e| e.sockets.get_mut::<Socket>(self.h).abort());
    }

    pub fn state(&self) -> State {
        with_engine(&self.iface, |e| e.sockets.get::<Socket>(self.h).state()).unwrap_or(State::Closed)
    }

    pub fn peer(&self) -> Option<(Ipv4Addr, u16)> {
        with_engine(&self.iface, |e| e.sockets.get::<Socket>(self.h).remote_endpoint().map(|ep| (from_smol(ep.addr), ep.port))).flatten()
    }

    pub fn interface(&self) -> &Arc<Interface> {
        &self.iface
    }

    /// The port on our side of the connection.
    pub fn local_port(&self) -> Option<u16> {
        with_engine(&self.iface, |e| e.sockets.get::<Socket>(self.h).local_endpoint().map(|ep| ep.port)).flatten()
    }

    pub fn is_closed(&self) -> bool {
        self.state() == State::Closed
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        with_engine(&self.iface, |e| {
            e.sockets.get_mut::<Socket>(self.h).close();
            e.orphans.push(self.h);
        });
    }
}

// -------------------------------------------------------------- listener ---

pub struct TcpListener {
    iface: Arc<Interface>,
    port: u16,
    pool: SpinLock<Vec<SocketHandle>>,
}

impl TcpListener {
    /// Listen on `port` on one interface.
    pub fn bind(iface: Arc<Interface>, port: u16) -> Result<TcpListener, &'static str> {
        let pool = with_engine(&iface, |e| {
            let mut v = Vec::new();
            for _ in 0..LISTEN_POOL {
                let mut s = Engine::new_socket();
                if s.listen(port).is_err() {
                    return Err("listen failed");
                }
                v.push(e.sockets.add(s));
            }
            Ok(v)
        })
        .ok_or("interface not configured")??;
        Ok(TcpListener { iface, port, pool: SpinLock::new(pool) })
    }

    /// Wait for the next established connection.
    pub async fn accept(&self) -> TcpStream {
        loop {
            let got = poll_fn(|cx| {
                let mut pool = self.pool.lock();
                let r = with_engine(&self.iface, |e| {
                    let mut found = None;
                    for (i, h) in pool.iter().enumerate() {
                        let s = e.sockets.get_mut::<Socket>(*h);
                        match s.state() {
                            State::Established | State::CloseWait => {
                                found = Some(i);
                                break;
                            }
                            State::Listen | State::SynReceived => {}
                            // Half-open attempt that died: re-arm the slot.
                            _ => {
                                s.abort();
                                let _ = s.listen(self.port);
                            }
                        }
                    }
                    match found {
                        Some(i) => {
                            let mut fresh = Engine::new_socket();
                            let _ = fresh.listen(self.port);
                            let accepted = core::mem::replace(&mut pool[i], e.sockets.add(fresh));
                            Some(accepted)
                        }
                        None => {
                            for h in pool.iter() {
                                e.sockets.get_mut::<Socket>(*h).register_send_waker(cx.waker());
                            }
                            None
                        }
                    }
                });
                match r {
                    Some(Some(h)) => Poll::Ready(Some(h)),
                    Some(None) => Poll::Pending,
                    None => Poll::Ready(None),
                }
            })
            .await;
            match got {
                Some(h) => {
                    ACCEPTED.fetch_add(1, Ordering::Relaxed);
                    return TcpStream { iface: self.iface.clone(), h };
                }
                None => {
                    // Interface unusable: back off rather than spin.
                    timer::sleep_ms(100).await;
                }
            }
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        let pool = core::mem::take(&mut *self.pool.lock());
        with_engine(&self.iface, |e| {
            for h in pool {
                e.sockets.get_mut::<Socket>(h).abort();
                e.sockets.remove(h);
            }
        });
    }
}

/// Copy bytes from `a` to `b` until `a` reaches EOF or either side fails.
/// Closes `b` for writing afterwards.  Returns bytes copied.
pub async fn pump(a: &TcpStream, b: &TcpStream) -> usize {
    let mut buf = [0u8; 8192];
    let mut total = 0;
    loop {
        let n = match a.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if b.write_all(&buf[..n]).await.is_err() {
            break;
        }
        total += n;
    }
    b.close();
    total
}

// ---------------------------------------------------------------- status ---

pub fn print_status() {
    println!(
        "tcp: {} engines; segments in {} packets out {} accepted {} connected {}",
        ENGINES.load(Ordering::Relaxed),
        SEGMENTS_IN.load(Ordering::Relaxed),
        PACKETS_OUT.load(Ordering::Relaxed),
        ACCEPTED.load(Ordering::Relaxed),
        CONNECTED.load(Ordering::Relaxed)
    );
    for iface in super::interfaces() {
        if !iface.tcp.is_present() {
            continue;
        }
        let guard = iface.tcp.inner.lock();
        let e = match guard.as_ref() {
            Some(e) => e,
            None => continue,
        };
        let mut lines = Vec::new();
        let mut listening = 0;
        for (_, s) in e.sockets.iter() {
            let smoltcp::socket::Socket::Tcp(t) = s;
            if t.state() == State::Listen {
                listening += 1;
                continue;
            }
            lines.push(format!(
                "    {:?} -> {:?} {} sendq={} recvq={}",
                t.local_endpoint().map(|ep| (from_smol(ep.addr), ep.port)),
                t.remote_endpoint().map(|ep| (from_smol(ep.addr), ep.port)),
                t.state(),
                t.send_queue(),
                t.recv_queue()
            ));
        }
        println!("  {} (id {}): {} sockets, {} listening, {} polls", iface.name, iface.id, e.sockets.iter().count(), listening, e.polls);
        for l in lines {
            println!("{}", l);
        }
    }
}

// ------------------------------------------------------------- self test ---

/// A device whose transmitted frames come straight back.
struct LoopDevice {
    q: SpinLock<VecDeque<Vec<u8>>>,
    notify: Notify,
}

impl super::NetDevice for LoopDevice {
    fn mac(&self) -> super::Mac {
        super::Mac([0x02, 0, 0, 0, 0, 0x7f])
    }
    fn send(&self, frame: &[u8]) -> bool {
        self.q.lock().push_back(frame.to_vec());
        self.notify.notify_one();
        true
    }
    fn recv(&self) -> Option<Vec<u8>> {
        self.q.lock().pop_front()
    }
    fn rx_notify(&self) -> &Notify {
        &self.notify
    }
}

/// Create a loopback interface (127.0.0.1/8) whose frames come straight back;
/// tests use it to talk to our own listeners.
pub fn loopback_interface(name: &str) -> Arc<Interface> {
    let dev = Arc::new(LoopDevice { q: SpinLock::new(VecDeque::new()), notify: Notify::new() });
    super::add_interface(name, dev, Ipv4Addr([127, 0, 0, 1]), Ipv4Addr([255, 0, 0, 0]))
}

/// Connect to ourselves over a loopback interface, push 300 KiB through and
/// read a reply — exercises handshake, windowing, close, and the wakers.
pub async fn self_test() -> Result<(), String> {
    let ip = Ipv4Addr([127, 0, 0, 1]);
    let iface = loopback_interface("lo");
    let listener = TcpListener::bind(iface.clone(), 8099).map_err(|e| format!("bind: {}", e))?;
    const N: usize = 300 * 1024;
    let server = task::spawn("tcp-test-server", async move {
        let s = listener.accept().await;
        let mut got = Vec::new();
        s.read_until(&mut got, N, 10_000).await.ok();
        let ok = got.iter().enumerate().all(|(i, b)| *b == (i * 7) as u8);
        let reply = format!("got {} bytes ok={}", got.len(), ok);
        let _ = s.write_all(reply.as_bytes()).await;
        s.flush(2000).await;
        s.close();
        (got.len(), ok)
    });
    let t0 = time::now();
    let client = TcpStream::connect(iface.clone(), ip, 8099, 3000).await.map_err(|e| format!("connect: {}", e))?;
    let payload: Vec<u8> = (0..N).map(|i| (i * 7) as u8).collect();
    client.write_all(&payload).await.map_err(|e| format!("write: {}", e))?;
    client.close();
    let mut reply = Vec::new();
    client.read_until(&mut reply, 1000, 10_000).await.ok();
    let (received, ok) = timer::timeout(10_000, server).await.map_err(|_| "server did not finish")?;
    let us = time::tsc_to_us(time::now() - t0);
    drop(client);
    super::remove_interface(iface.id);
    if received != N || !ok {
        return Err(format!("server received {} of {} bytes (ok={})", received, N, ok));
    }
    let text = String::from_utf8_lossy(&reply);
    if !text.starts_with(&format!("got {} bytes ok=true", N)) {
        return Err(format!("unexpected reply {:?}", text));
    }
    print!("({} KiB in {} ms) ", N / 1024, us / 1000);
    Ok(())
}
