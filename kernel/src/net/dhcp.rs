//! Minimal DHCP client (DISCOVER / OFFER / REQUEST / ACK).

use alloc::sync::Arc;
use alloc::vec::Vec;

use super::udp::UdpSocket;
use super::{arp, Interface, Ipv4Addr};
use crate::task::timer;
use crate::time;

#[derive(Clone, Copy, Debug, Default)]
pub struct Lease {
    pub ip: Ipv4Addr,
    pub mask: Ipv4Addr,
    pub gateway: Ipv4Addr,
    pub dns: Ipv4Addr,
    pub server: Ipv4Addr,
    pub lease_secs: u32,
}

const MAGIC: [u8; 4] = [0x63, 0x82, 0x53, 0x63];

fn build(iface: &Interface, xid: u32, msg_type: u8, requested: Option<Ipv4Addr>, server: Option<Ipv4Addr>) -> Vec<u8> {
    let mut p = Vec::with_capacity(300);
    p.push(1); // BOOTREQUEST
    p.push(1); // ethernet
    p.push(6);
    p.push(0);
    p.extend_from_slice(&xid.to_be_bytes());
    p.extend_from_slice(&[0, 0]); // secs
    p.extend_from_slice(&0x8000u16.to_be_bytes()); // broadcast flag
    p.extend_from_slice(&[0; 16]); // ciaddr, yiaddr, siaddr, giaddr
    p.extend_from_slice(&iface.mac.0);
    p.extend_from_slice(&[0; 10]);
    p.extend_from_slice(&[0; 192]); // sname + file
    p.extend_from_slice(&MAGIC);
    p.extend_from_slice(&[53, 1, msg_type]);
    p.extend_from_slice(&[61, 7, 1]);
    p.extend_from_slice(&iface.mac.0);
    p.extend_from_slice(&[55, 4, 1, 3, 6, 51]);
    if let Some(ip) = requested {
        p.extend_from_slice(&[50, 4]);
        p.extend_from_slice(&ip.0);
    }
    if let Some(s) = server {
        p.extend_from_slice(&[54, 4]);
        p.extend_from_slice(&s.0);
    }
    p.extend_from_slice(&[12, 7]);
    p.extend_from_slice(b"conc-os");
    p.push(255);
    while p.len() < 300 {
        p.push(0);
    }
    p
}

struct Parsed {
    msg_type: u8,
    yiaddr: Ipv4Addr,
    lease: Lease,
}

fn parse(p: &[u8], xid: u32) -> Option<Parsed> {
    if p.len() < 240 || p[0] != 2 || u32::from_be_bytes([p[4], p[5], p[6], p[7]]) != xid || p[236..240] != MAGIC {
        return None;
    }
    let yiaddr = Ipv4Addr([p[16], p[17], p[18], p[19]]);
    let mut lease = Lease { ip: yiaddr, ..Default::default() };
    let mut msg_type = 0;
    let mut i = 240;
    while i < p.len() {
        let code = p[i];
        if code == 255 {
            break;
        }
        if code == 0 {
            i += 1;
            continue;
        }
        if i + 1 >= p.len() {
            break;
        }
        let len = p[i + 1] as usize;
        let v = &p[i + 2..(i + 2 + len).min(p.len())];
        match code {
            53 if !v.is_empty() => msg_type = v[0],
            1 if v.len() >= 4 => lease.mask = Ipv4Addr([v[0], v[1], v[2], v[3]]),
            3 if v.len() >= 4 => lease.gateway = Ipv4Addr([v[0], v[1], v[2], v[3]]),
            6 if v.len() >= 4 => lease.dns = Ipv4Addr([v[0], v[1], v[2], v[3]]),
            51 if v.len() >= 4 => lease.lease_secs = u32::from_be_bytes([v[0], v[1], v[2], v[3]]),
            54 if v.len() >= 4 => lease.server = Ipv4Addr([v[0], v[1], v[2], v[3]]),
            _ => {}
        }
        i += 2 + len;
    }
    Some(Parsed { msg_type, yiaddr, lease })
}

async fn wait_reply(sock: &UdpSocket, xid: u32, want: u8, timeout_ms: u64) -> Option<Parsed> {
    let deadline = time::now() + time::us_to_tsc(timeout_ms * 1000);
    loop {
        let now = time::now();
        if now >= deadline {
            return None;
        }
        let remaining = time::tsc_to_us(deadline - now) / 1000 + 1;
        let d = match timer::timeout(remaining, alloc::boxed::Box::pin(sock.recv())).await {
            Ok(d) => d,
            Err(_) => return None,
        };
        if let Some(r) = parse(&d.data, xid) {
            if r.msg_type == want {
                return Some(r);
            }
        }
    }
}

/// One full DHCP exchange.
pub async fn run_once(iface: &Interface, attempt: u32) -> Option<Lease> {
    let sock = UdpSocket::bind(68).ok()?;
    let xid = (time::now() as u32) ^ 0xC0DE_0000 ^ attempt;
    let discover = build(iface, xid, 1, None, None);
    sock.send_to_from(Ipv4Addr::UNSPECIFIED, &discover, Ipv4Addr::BROADCAST, 67).await;
    let offer = wait_reply(&sock, xid, 2, 2000).await?;
    let request = build(iface, xid, 3, Some(offer.yiaddr), Some(offer.lease.server));
    sock.send_to_from(Ipv4Addr::UNSPECIFIED, &request, Ipv4Addr::BROADCAST, 67).await;
    let ack = wait_reply(&sock, xid, 5, 2000).await?;
    Some(ack.lease)
}

/// Background task: configure the interface via DHCP, falling back to the
/// QEMU user-network defaults if no server answers.
pub async fn client_task(iface: Arc<Interface>) {
    for attempt in 0..4 {
        if let Some(l) = run_once(&iface, attempt).await {
            iface.set_config(l.ip, l.mask, l.gateway, l.dns, "dhcp");
            log!("dhcp: ip {} mask {} gw {} dns {} lease {} s", l.ip, l.mask, l.gateway, l.dns, l.lease_secs);
            arp::announce(&iface);
            return;
        }
        log!("dhcp: no response (attempt {})", attempt + 1);
    }
    let ip = Ipv4Addr([10, 0, 2, 15]);
    iface.set_config(ip, Ipv4Addr([255, 255, 255, 0]), Ipv4Addr([10, 0, 2, 2]), Ipv4Addr([10, 0, 2, 3]), "static-fallback");
    log!("dhcp: giving up, using static {}", ip);
    arp::announce(&iface);
}
