//! `web-test <vms> <active> [requests] [freeze_ms]`: the scale experiment.
//!
//! 1. Boot one Linux VM with the web server, warm it, snapshot it ("web").
//! 2. Clone the snapshot `vms` times straight into the frozen state.
//! 3. Drive `active` concurrent HTTP clients through the front-door proxy
//!    (a loopback listener routed by Host header, exactly like the SNI path
//!    minus TLS).  Half of the requests go to a hot set of `active` VMs,
//!    half are spread uniformly over all of them, so both warm and cold
//!    (thaw) latencies are measured.
//! 4. Read every sampled VM's request counter back and compare it with the
//!    number of requests it answered: the counter lives in guest memory and
//!    must survive every freeze/thaw.
//! 5. Report latencies, throughput, host memory, page store use and how much
//!    of the kernel text is still shared.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use super::manager::manager;
use super::vm::VmState;
use crate::mm::Bytes;
use crate::net::{self, http, proxy, Interface, Ipv4Addr};
use crate::sync::{OnceCell, SpinLock};
use crate::task::{self, timer};
use crate::time;

const PROXY_PORT: u16 = 8085;
static LO: OnceCell<Arc<Interface>> = OnceCell::new();

/// A loopback interface with a proxy listener, created once.
fn lo() -> Arc<Interface> {
    if let Some(i) = LO.get() {
        return i.clone();
    }
    let i = net::tcp::loopback_interface("lo-web");
    if let Err(e) = proxy::listen_on(i.clone(), PROXY_PORT) {
        println!("web-test: proxy listen failed: {}", e);
    }
    LO.init(i.clone());
    i
}

#[derive(Clone, Copy)]
struct Sample {
    us: u64,
    cold: bool,
    ok: bool,
}

fn percentile(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        0
    } else {
        sorted[((sorted.len() - 1) * p) / 100]
    }
}

fn ms(us: u64) -> String {
    format!("{}.{} ms", us / 1000, (us % 1000) / 100)
}

fn xorshift(x: &mut u64) -> u64 {
    let mut v = *x;
    v ^= v << 13;
    v ^= v >> 7;
    v ^= v << 17;
    *x = v;
    v
}

fn parse_hits(body: &str) -> Option<u64> {
    body.split_whitespace().find_map(|w| w.strip_prefix("hits=")).and_then(|v| v.trim().parse().ok())
}

fn latency_line(label: &str, v: &[u64]) {
    if v.is_empty() {
        println!("  {} latency: (no samples)", label);
        return;
    }
    let avg = v.iter().sum::<u64>() / v.len() as u64;
    println!(
        "  {} latency ({} requests): p50 {} p90 {} p99 {} max {} avg {}",
        label,
        v.len(),
        ms(percentile(v, 50)),
        ms(percentile(v, 90)),
        ms(percentile(v, 99)),
        ms(*v.last().unwrap()),
        ms(avg)
    );
}

/// Print the exit/time profile of a VM between two stat snapshots.
pub fn print_profile(label: &str, cur: &super::vm::VmStats, base: &super::vm::VmStats, requests: u64) {
    let d = |a: u64, b: u64| a.saturating_sub(b);
    let names = ["npf", "mmio", "io", "msr", "cpuid", "hlt", "intr", "other"];
    let mut exits = 0;
    let mut host_us = 0;
    let mut parts: Vec<String> = Vec::new();
    for i in 0..8 {
        let n = d(cur.exit_count[i], base.exit_count[i]);
        let us = d(cur.exit_host_us[i], base.exit_host_us[i]);
        exits += n;
        host_us += us;
        if n == 0 {
            continue;
        }
        let detail = match i {
            0 => format!(
                " (cow {} of which {} eager, zero {}, ro-map {}, dirty {}, pages loaded {})",
                d(cur.cow, base.cow),
                d(cur.eager_pages, base.eager_pages),
                d(cur.npf_zero, base.npf_zero),
                d(cur.npf_ro, base.npf_ro),
                d(cur.npf_dirty, base.npf_dirty),
                d(cur.pages_loaded, base.pages_loaded)
            ),
            1 => format!(" (lapic {}, vnet {})", d(cur.mmio_class[0], base.mmio_class[0]), d(cur.mmio_class[1], base.mmio_class[1])),
            2 => format!(
                " (pic {}, pit {}, uart {}, other {})",
                d(cur.io_class[0], base.io_class[0]),
                d(cur.io_class[1], base.io_class[1]),
                d(cur.io_class[2], base.io_class[2]),
                d(cur.io_class[3], base.io_class[3])
            ),
            _ => String::new(),
        };
        parts.push(format!("{} {}{} [{} us]", names[i], n, detail, us));
    }
    let guest_us = time::tsc_to_us(d(cur.guest_tsc, base.guest_tsc));
    println!(
        "{}: {} exits, {} us host handling, {} us in VMRUN (guest + nested overhead), {} us descheduled; {} runs, {} halts",
        label,
        exits,
        host_us,
        guest_us,
        d(cur.wait_us, base.wait_us),
        d(cur.runs, base.runs),
        d(cur.halts, base.halts)
    );
    println!("  {}", parts.join("; "));
    println!(
        "  injected {} (timer {}, uart {}, net {}, other {}); frames to guest {} from guest {}; thaws {} (last {} us) freezes {}; pages loaded {} written {}",
        d(cur.injected, base.injected),
        d(cur.inj[0], base.inj[0]),
        d(cur.inj[1], base.inj[1]),
        d(cur.inj[2], base.inj[2]),
        d(cur.inj[3], base.inj[3]),
        d(cur.frames_to_guest, base.frames_to_guest),
        d(cur.frames_from_guest, base.frames_from_guest),
        d(cur.thaws, base.thaws),
        cur.last_thaw_us,
        d(cur.freezes, base.freezes),
        d(cur.pages_loaded, base.pages_loaded),
        d(cur.pages_written, base.pages_written)
    );
    if requests > 1 {
        println!(
            "  per request: {} exits, {} us host, {} us VMRUN, {} us descheduled",
            exits / requests,
            host_us / requests,
            guest_us / requests,
            d(cur.wait_us, base.wait_us) / requests
        );
    }
}

/// Profile one cold request (VM frozen) followed by one warm request,
/// through the proxy, `iterations` times.
pub async fn coldstart(name: &str, iterations: usize) {
    use super::vm::{Command, VmState};
    let m = manager();
    let v = match m.find(name) {
        Some(v) if v.kind.is_linux() => v,
        _ => {
            println!("no such linux vm");
            return;
        }
    };
    let lo = lo();
    let here = Ipv4Addr([127, 0, 0, 1]);
    let host = format!("{}.conc", v.name);
    if let Some(snap) = v.origin.as_deref().and_then(|o| m.snapshot(o)) {
        println!("vm {} '{}' is a clone of '{}': learned write set {} pages", v.id, v.name, snap.name, snap.learned_pages().len());
    }
    for i in 0..iterations.max(1) {
        // Make sure the VM is frozen.
        let mut waited = 0;
        while v.state() != VmState::Frozen && waited < 10_000 {
            if v.state() == VmState::Idle {
                v.command(Command::Freeze);
                if v.wait_state(VmState::Frozen, 5000).await {
                    break;
                }
            }
            timer::sleep_ms(5).await;
            waited += 5;
        }
        if v.state() != VmState::Frozen {
            println!("vm {} did not freeze (state {})", v.name, v.state());
            return;
        }
        // Collect both phases first: printing is slow enough (every serial
        // byte is an exit when we run nested) to distort the next phase.
        let mut results = Vec::new();
        for (phase, label) in [("cold", "cold (thawed)"), ("warm", "warm")] {
            let base = v.stats();
            let ex0 = task::stats();
            let fired0 = timer::fired();
            let ticks0 = time::ticks();
            let irqs = || {
                let net = crate::virtio::net::device().map(|d| d.stats.rx_irqs.load(Ordering::Relaxed)).unwrap_or(0);
                let blk = crate::disk::device().map(|d| d.stats.irqs.load(Ordering::Relaxed)).unwrap_or(0);
                (net, blk)
            };
            let (net0, blk0) = irqs();
            let t0 = time::now();
            let r = http::get(lo.clone(), here, PROXY_PORT, &host, "/", 30_000).await;
            let total = time::tsc_to_us(time::now() - t0);
            let cur = v.stats();
            let ex1 = task::stats();
            let (net1, blk1) = irqs();
            let exec = format!(
                "host: {} executor polls, {} idle sleeps, {} timer interrupts ({} timers fired), {} virtio-net irqs, {} virtio-blk irqs",
                ex1.polls - ex0.polls,
                ex1.idle_enters - ex0.idle_enters,
                time::ticks() - ticks0,
                timer::fired() - fired0,
                net1 - net0,
                blk1 - blk0
            );
            let status = match &r {
                Ok(resp) => format!("http {}", resp.status),
                Err(e) => format!("error: {}", e),
            };
            // The proxy finishes its bookkeeping slightly after the client
            // has its answer; wait for the route that started after t0.
            let mut tl = None;
            for _ in 0..50 {
                tl = proxy::last_route().filter(|t| t.vm_id == v.id && t.accepted >= t0);
                if tl.map(|t| t.done != 0).unwrap_or(false) {
                    break;
                }
                timer::sleep_ms(2).await;
            }
            let client = *http::LAST_GET.lock();
            results.push((phase, label, status, total, t0, tl, client, base, cur, exec));
            // Let the guest settle before the next phase.
            timer::sleep_ms(20).await;
        }
        for (phase, label, status, total, t0, tl, client, base, cur, exec) in results {
            let rel = |t: u64| if t >= t0 { time::tsc_to_us(t - t0) } else { 0 };
            println!(
                "{} #{} {}: {} us total; client connected to proxy +{} us, first byte +{} us, done +{} us",
                label,
                i,
                status,
                total,
                rel(client.connected),
                rel(client.first_byte),
                rel(client.done)
            );
            if let Some(t) = tl {
                println!(
                    "  proxy: accepted +{} us, named +{} us, connected to guest +{} us, first byte from guest +{} us, closed +{} us",
                    rel(t.accepted),
                    rel(t.named),
                    rel(t.connected),
                    rel(t.first_byte),
                    rel(t.done)
                );
            }
            print_profile(&format!("  {} profile", phase), &cur, &base, 1);
            println!("  {}", exec);
        }
    }
}

pub async fn run(n: usize, active: usize, requests: usize, freeze_ms: u64) {
    let m = manager();
    let here = Ipv4Addr([127, 0, 0, 1]);
    let lo = lo();
    let mem_start = crate::mm::frame::stats();
    println!("web-test: {} vms, {} concurrent clients, {} requests, freeze after {} ms idle", n, active, requests, freeze_ms);

    // ------------------------------------------------ 1. base + snapshot --
    let snap = match m.snapshot("web") {
        Some(s) => {
            println!("using existing snapshot 'web' ({} pages)", s.pages.len());
            s
        }
        None => {
            let t0 = time::now();
            let base = match m.create_linux("web-base", 128 << 20, None, None).await {
                Ok(b) => b,
                Err(e) => {
                    println!("cannot create base vm: {}", e);
                    return;
                }
            };
            if !base.wait_console("webcounter: vm", 300_000).await {
                println!("guest web server did not start (state {}); is images/webcounter installed?", base.state());
                return;
            }
            let boot_us = time::tsc_to_us(time::now() - t0);
            // Warm the server once (the first request initialises Go's HTTP
            // state) and reset the counter so every clone starts at zero.
            let mut warmed = false;
            if let Some(iface) = base.link().and_then(|l| l.interface()) {
                for _ in 0..40 {
                    if http::get(iface.clone(), proxy::GUEST_IP, 80, "web-base", "/", 5000).await.is_ok() {
                        warmed = true;
                        break;
                    }
                    timer::sleep_ms(250).await;
                }
                let _ = http::get(iface.clone(), proxy::GUEST_IP, 80, "web-base", "/reset", 5000).await;
            }
            timer::sleep_ms(300).await;
            let t1 = time::now();
            let s = match m.snapshot_vm(&base, "web").await {
                Ok(s) => s,
                Err(e) => {
                    println!("snapshot failed: {}", e);
                    return;
                }
            };
            println!(
                "base vm '{}' booted{} in {}; snapshot 'web' taken in {}: {} pages ({}), kernel text {} pages",
                base.name,
                if warmed { " and warmed" } else { "" },
                ms(boot_us),
                ms(time::tsc_to_us(time::now() - t1)),
                s.pages.len(),
                Bytes(s.bytes()),
                s.text_pages.map(|(a, b)| b - a).unwrap_or(0)
            );
            s
        }
    };
    m.linux_freeze_ms.store(freeze_ms, Ordering::Relaxed);

    // ----------------------------------------------------------- 2. clones --
    let mem_before = crate::mm::frame::stats();
    let t2 = time::now();
    let mut names: Vec<String> = Vec::with_capacity(n);
    let mut max_us = 0;
    let mut made = 0usize;
    for i in 1..=n {
        let name = format!("vm{:04}", i);
        if m.find(&name).is_none() {
            let c0 = time::now();
            if let Err(e) = m.clone_vm("web", &name, true) {
                println!("clone {} failed: {}", name, e);
                break;
            }
            max_us = max_us.max(time::tsc_to_us(time::now() - c0));
            made += 1;
        }
        names.push(name);
        if i % 50 == 0 {
            task::yield_now().await;
        }
    }
    let clone_us = time::tsc_to_us(time::now() - t2);
    let mem_after = crate::mm::frame::stats();
    let clone_bytes = mem_before.free.saturating_sub(mem_after.free) as u64 * 4096;
    println!(
        "cloned {} vms in {} ({} avg, {} max per clone); host memory +{} ({} per frozen clone)",
        made,
        ms(clone_us),
        ms(clone_us / made.max(1) as u64),
        ms(max_us),
        Bytes(clone_bytes),
        Bytes(clone_bytes / made.max(1) as u64)
    );
    if names.is_empty() {
        return;
    }

    // ------------------------------------------------------------- 3. load --
    let names = Arc::new(names);
    let samples: Arc<SpinLock<Vec<Sample>>> = Arc::new(SpinLock::new(Vec::with_capacity(requests)));
    let expected: Arc<SpinLock<Vec<u64>>> = Arc::new(SpinLock::new(alloc::vec![0u64; names.len()]));
    let failures_shown = Arc::new(AtomicUsize::new(0));
    let failed_vms: Arc<SpinLock<Vec<String>>> = Arc::new(SpinLock::new(Vec::new()));
    let next = Arc::new(AtomicUsize::new(0));
    let hot = active.max(1).min(names.len());
    let t3 = time::now();
    let mut workers = Vec::new();
    for w in 0..active.max(1) {
        let names = names.clone();
        let samples = samples.clone();
        let expected = expected.clone();
        let failures_shown = failures_shown.clone();
        let failed_vms = failed_vms.clone();
        let next = next.clone();
        let lo = lo.clone();
        workers.push(task::spawn("web-load", async move {
            let mut rng = 0x9E37_79B9_7F4A_7C15u64 ^ ((w as u64 + 1).wrapping_mul(0x1234_5678_9ABC_DEF1));
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= requests {
                    break;
                }
                let r = xorshift(&mut rng);
                let idx = if r & 1 == 0 { ((r >> 8) as usize) % hot } else { ((r >> 8) as usize) % names.len() };
                let name = &names[idx];
                let cold = manager().find(name).map(|v| v.state() == VmState::Frozen).unwrap_or(false);
                let t = time::now();
                let resp = http::get(lo.clone(), here, PROXY_PORT, &format!("{}.conc", name), "/", 120_000).await;
                let us = time::tsc_to_us(time::now() - t);
                let ok = match resp {
                    Ok(r) if r.status == 200 && parse_hits(&r.text()).is_some() => {
                        let h = parse_hits(&r.text()).unwrap_or(0);
                        // Every answer carries the count after its own
                        // increment, so the largest one seen is exactly the
                        // number of requests the VM has served (whatever the
                        // order concurrent answers arrive in, and whether the
                        // VM was reused from an earlier run).
                        let mut exp = expected.lock();
                        exp[idx] = exp[idx].max(h);
                        true
                    }
                    Ok(r) => {
                        if failures_shown.fetch_add(1, Ordering::Relaxed) < 5 {
                            println!("  {}: http {} {:?}", name, r.status, r.text().trim());
                        }
                        failed_vms.lock().push(name.clone());
                        false
                    }
                    Err(e) => {
                        if failures_shown.fetch_add(1, Ordering::Relaxed) < 5 {
                            println!("  {}: {}", name, e);
                        }
                        failed_vms.lock().push(name.clone());
                        false
                    }
                };
                samples.lock().push(Sample { us, cold, ok });
            }
        }));
    }
    for w in workers {
        w.await;
    }
    let load_us = time::tsc_to_us(time::now() - t3);

    // ------------------------------------------- 4. verify the counters --
    let exp = expected.lock().clone();
    let mut targets: Vec<usize> = (0..hot).collect();
    let mut rng = 7u64;
    for _ in 0..200 {
        let r = xorshift(&mut rng) as usize % names.len();
        if exp[r] > 0 && !targets.contains(&r) {
            targets.push(r);
        }
        if targets.len() >= hot + 40 {
            break;
        }
    }
    let (mut verified, mut wrong) = (0, 0);
    let t4 = time::now();
    for idx in targets {
        if exp[idx] == 0 {
            continue;
        }
        match http::get(lo.clone(), here, PROXY_PORT, &format!("{}.conc", names[idx]), "/hits", 120_000).await {
            Ok(r) if r.status == 200 => {
                let h: u64 = r.text().trim().parse().unwrap_or(u64::MAX);
                verified += 1;
                if h != exp[idx] {
                    wrong += 1;
                    if wrong <= 5 {
                        println!("  {}: counter says {} but {} requests were answered", names[idx], h, exp[idx]);
                    }
                }
            }
            Ok(r) => println!("  {}: /hits answered {}", names[idx], r.status),
            Err(e) => println!("  {}: /hits failed: {}", names[idx], e),
        }
    }
    let verify_us = time::tsc_to_us(time::now() - t4);

    // ----------------------------------------------------------- 5. report --
    let all = samples.lock().clone();
    let mut cold: Vec<u64> = all.iter().filter(|s| s.cold && s.ok).map(|s| s.us).collect();
    let mut warm: Vec<u64> = all.iter().filter(|s| !s.cold && s.ok).map(|s| s.us).collect();
    cold.sort_unstable();
    warm.sort_unstable();
    let failed = all.iter().filter(|s| !s.ok).count();
    let touched = exp.iter().filter(|&&e| e > 0).count();
    println!(
        "load: {} requests in {} ({} req/s) from {} clients; {} failed; {} distinct vms answered",
        all.len(),
        ms(load_us),
        all.len() as u64 * 1_000_000 / load_us.max(1),
        active.max(1),
        failed,
        touched
    );
    latency_line("cold (vm was frozen)", &cold);
    latency_line("warm", &warm);
    println!("  counters: {} vms read back in {}, {} wrong", verified, ms(verify_us), wrong);
    let mut failed_names = failed_vms.lock().clone();
    failed_names.sort();
    failed_names.dedup();
    for name in failed_names.iter().take(4) {
        if let Some(v) = m.find(name) {
            let s = v.stats();
            println!(
                "  failed vm {} (id {}): state {} has_work {}; runs {} exits {} halts {} npf {} mmio {} injected {}; thaws {} freezes {}; private {} resident {} swapped {}; logs {:?}",
                name,
                v.id,
                v.state(),
                v.has_work(),
                s.runs,
                s.exits,
                s.halts,
                s.npf,
                s.mmio,
                s.injected,
                s.thaws,
                s.freezes,
                s.private_pages,
                s.resident_pages,
                s.swapped_pages,
                v.logs().iter().rev().take(4).collect::<Vec<_>>()
            );
            if let Some(l) = v.link() {
                println!(
                    "    link: to_guest {} sent {} received {} dropped {}",
                    l.pending_to_guest(),
                    l.sent_to_guest.load(Ordering::Relaxed),
                    l.received_from_guest.load(Ordering::Relaxed),
                    l.dropped.load(Ordering::Relaxed)
                );
            }
            v.command(super::vm::Command::Dump);
            timer::sleep_ms(50).await;
        }
    }

    let vms = m.list();
    let clones: Vec<_> = vms.iter().filter(|v| v.origin.as_deref() == Some("web")).collect();
    let (mut frozen, mut idle, mut running) = (0, 0, 0);
    let (mut private_total, mut private_max, mut text_total, mut text_max, mut resident_total) = (0usize, 0usize, 0usize, 0usize, 0usize);
    let (mut thaws, mut thaw_us_total, mut thaw_us_max, mut freezes) = (0u64, 0u64, 0u64, 0u64);
    for v in &clones {
        match v.state() {
            VmState::Frozen => frozen += 1,
            VmState::Idle => idle += 1,
            VmState::Running => running += 1,
            _ => {}
        }
        let s = v.stats();
        private_total += s.private_pages;
        private_max = private_max.max(s.private_pages);
        text_total += s.text_private_pages;
        text_max = text_max.max(s.text_private_pages);
        resident_total += s.resident_pages;
        freezes += s.freezes;
        if s.thaws > 0 {
            thaws += s.thaws;
            thaw_us_total += s.last_thaw_us;
            thaw_us_max = thaw_us_max.max(s.last_thaw_us);
        }
    }
    let nc = clones.len().max(1);
    let text_pages = snap.text_pages.map(|(a, b)| (b - a) as usize).unwrap_or(0);
    println!(
        "vms: {} clones ({} frozen, {} idle, {} running); private pages avg {} max {} ({} avg); resident now {}",
        clones.len(),
        frozen,
        idle,
        running,
        private_total / nc,
        private_max,
        Bytes((private_total / nc) as u64 * 4096),
        Bytes(resident_total as u64 * 4096)
    );
    println!(
        "  kernel text: {} pages ({}) in the snapshot, one copy in host memory for all clones; copy-on-write text pages per clone avg {} max {} ({}% of the text unshared at worst)",
        text_pages,
        Bytes(text_pages as u64 * 4096),
        text_total / nc,
        text_max,
        if text_pages > 0 { text_max * 100 / text_pages } else { 0 }
    );
    println!(
        "  freezes {} thaws {} (last thaw avg {} max {}); snapshot {} pages ({}) shared by every clone",
        freezes,
        thaws,
        ms(thaw_us_total / thaws.max(1)),
        ms(thaw_us_max),
        snap.pages.len(),
        Bytes(snap.bytes())
    );
    let mem_end = crate::mm::frame::stats();
    let used = mem_end.total.saturating_sub(mem_end.free) as u64 * 4096;
    println!(
        "host memory: {} used of {} (+{} since the test started); templates hold {} frames ({})",
        Bytes(used),
        Bytes(mem_end.total as u64 * 4096),
        Bytes(mem_start.free.saturating_sub(mem_end.free) as u64 * 4096),
        super::image::template_frames(),
        Bytes(super::image::template_frames() as u64 * 4096)
    );
    if let Some(st) = crate::disk::store() {
        let (u, t) = st.usage();
        println!(
            "page store: {} of {} blocks used ({} of {}), {} reads {} writes",
            u,
            t,
            Bytes(u as u64 * 4096),
            Bytes(t as u64 * 4096),
            st.reads.load(Ordering::Relaxed),
            st.writes.load(Ordering::Relaxed)
        );
    }
    proxy::print_status();
}
