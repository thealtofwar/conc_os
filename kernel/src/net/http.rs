//! A tiny HTTP/1.0 client over `TcpStream`: used by the shell, the self
//! tests and the load generator to talk to guests directly.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::tcp::TcpStream;
use super::{Interface, Ipv4Addr};
use crate::task::timer;
use crate::time;

pub struct Response {
    pub status: u16,
    pub headers: String,
    pub body: Vec<u8>,
}

impl Response {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Client-side timestamps (host TSC) of the last `get`, for profiling.
#[derive(Clone, Copy, Debug, Default)]
pub struct GetTiming {
    pub start: u64,
    pub connected: u64,
    pub first_byte: u64,
    pub done: u64,
}

pub static LAST_GET: crate::sync::SpinLock<GetTiming> = crate::sync::SpinLock::new(GetTiming { start: 0, connected: 0, first_byte: 0, done: 0 });

/// `GET path` from `ip:port` through `iface`, with `host` in the Host header.
pub async fn get(iface: Arc<Interface>, ip: Ipv4Addr, port: u16, host: &str, path: &str, timeout_ms: u64) -> Result<Response, String> {
    let deadline = time::now() + time::us_to_tsc(timeout_ms * 1000);
    let mut timing = GetTiming { start: time::now(), ..Default::default() };
    let s = TcpStream::connect(iface, ip, port, timeout_ms).await.map_err(|e| format!("connect {}:{}: {}", ip, port, e))?;
    timing.connected = time::now();
    let req = format!("GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: conc_os\r\nConnection: close\r\n\r\n", path, host);
    s.write_all(req.as_bytes()).await.map_err(|e| format!("send: {}", e))?;
    let mut raw = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        if time::now() >= deadline {
            return Err(String::from("response timed out"));
        }
        let n = match timer::timeout_until(deadline, Box::pin(s.read(&mut tmp))).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(format!("read: {}", e)),
            Err(_) => return Err(String::from("response timed out")),
        };
        if n == 0 {
            break;
        }
        if raw.is_empty() {
            timing.first_byte = time::now();
        }
        raw.extend_from_slice(&tmp[..n]);
        if raw.len() > 4 << 20 {
            return Err(String::from("response too large"));
        }
    }
    s.close();
    timing.done = time::now();
    *LAST_GET.lock() = timing;
    parse(raw)
}

fn parse(raw: Vec<u8>) -> Result<Response, String> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or("malformed response (no end of headers)")?;
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status = head.split_whitespace().nth(1).and_then(|s| s.parse::<u16>().ok()).ok_or("malformed status line")?;
    let body = raw[split + 4..].to_vec();
    Ok(Response { status, headers: head, body })
}
