//! Hypervisor shell commands.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use super::manager::manager;
use super::vm::{kind, Command, VmState};
use crate::mm::Bytes;
use crate::shell::arg_u64;
use crate::task::{self, timer};
use crate::time;

pub fn help() {
    println!("hv:     hv   vms   vm create <name> <kind> [mem_kib]   vm info|logs|kill|freeze|thaw|reset <name>   vm killall");
    println!("        vm attach <name> (Ctrl-] detaches)   vm send <name> <text>   vm trace <name> [on|off]   vm devices <name>");
    println!("        vm ping <name> [count]   vm http <name> [path] [port]   (guest is 10.42.0.2 on its own link)");
    println!("        linux images   linux create [--image <set>] <name> [mem_mib] [cmdline...]");
    println!("        vm snapshot <vm> <name>   linux clone <snapshot> <name> [count] [run]   linux snapshots");
    println!("        vm profile <name> [reset]   vm coldstart <name> [iterations]   (exit/latency profiles)");
    println!("        web-test [vms=1000] [active=10] [requests=2*vms] [freeze_ms=2000]   (snapshot, clone, load through the proxy, report)");
    println!("        linux create <name> [mem_mib] [cmdline...]   linux images   hv set freeze_ms=<ms> linux_freeze_ms=<ms>");
    println!("        req <vm|service> <payload>   bench <target> <n> [concurrency]");
    println!("        svc create <name> <kind> [max] [mem_kib]   svc list   svc set <name> k=v..   svc delete <name>");
    println!("        swarm <n> <kind> [mem_kib]   scale-test [n]");
    println!("        kinds: echo primes counter spin fault sleepy hello");
}

fn fmt_us(us: u64) -> String {
    if us >= 1_000_000 {
        format!("{}.{:03} s", us / 1_000_000, (us % 1_000_000) / 1000)
    } else if us >= 1000 {
        format!("{}.{:03} ms", us / 1000, us % 1000)
    } else {
        format!("{} us", us)
    }
}

fn percentile(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = (sorted.len() - 1) * p / 100;
    sorted[idx]
}

pub async fn dispatch(cmd: &str, args: &[&str]) -> bool {
    match cmd {
        "hv" => {
            if args.first() == Some(&"set") {
                for kv in &args[1..] {
                    if let Some(v) = kv.strip_prefix("freeze_ms=") {
                        manager().default_freeze_ms.store(v.parse().unwrap_or(0), Ordering::Relaxed);
                    }
                    if let Some(v) = kv.strip_prefix("linux_freeze_ms=") {
                        manager().linux_freeze_ms.store(v.parse().unwrap_or(0), Ordering::Relaxed);
                    }
                    if let Some(v) = kv.strip_prefix("trace=") {
                        manager().trace_new.store(v == "1" || v == "on", Ordering::Relaxed);
                    }
                    if let Some(v) = kv.strip_prefix("linux_tests=") {
                        super::tests::LINUX_TESTS.store(v == "1" || v == "on", Ordering::Relaxed);
                    }
                    if let Some(v) = kv.strip_prefix("prefault=") {
                        manager().prefault.store(v == "1" || v == "on", Ordering::Relaxed);
                    }
                    if let Some(v) = kv.strip_prefix("eager_cow=") {
                        manager().eager_cow.store(v == "1" || v == "on", Ordering::Relaxed);
                    }
                    if let Some(v) = kv.strip_prefix("prefetch=") {
                        manager().prefetch.store(v == "1" || v == "on", Ordering::Relaxed);
                    }
                }
            }
            hv_status();
            true
        }
        "vms" => {
            list_vms();
            true
        }
        "vm" => {
            vm_cmd(args).await;
            true
        }
        "linux" => {
            linux_cmd(args).await;
            true
        }
        "req" => {
            if args.is_empty() {
                println!("usage: req <vm|service> <payload>");
                return true;
            }
            let payload = args[1..].join(" ");
            match manager().request(args[0], payload.into_bytes(), 10_000).await {
                Ok((r, us)) => println!("{} ({})", String::from_utf8_lossy(&r), fmt_us(us)),
                Err(e) => println!("error: {}", e),
            }
            true
        }
        "svc" => {
            svc_cmd(args).await;
            true
        }
        "bench" => {
            if args.len() < 2 {
                println!("usage: bench <target> <n> [concurrency]");
                return true;
            }
            let n = arg_u64(args, 1, 100) as usize;
            let conc = arg_u64(args, 2, 1) as usize;
            bench(args[0], n, conc.max(1)).await;
            true
        }
        "swarm" => {
            let n = arg_u64(args, 0, 100) as usize;
            let k = args.get(1).and_then(|s| kind::parse(s)).unwrap_or(kind::ECHO);
            let mem = arg_u64(args, 2, 512) * 1024;
            swarm(n, k, mem).await;
            true
        }
        "scale-test" => {
            let n = arg_u64(args, 0, 200) as usize;
            scale_test(n).await;
            true
        }
        "web-test" => {
            let n = arg_u64(args, 0, 1000) as usize;
            let active = arg_u64(args, 1, 10) as usize;
            let requests = arg_u64(args, 2, (n * 2) as u64) as usize;
            let freeze_ms = arg_u64(args, 3, 2000);
            super::webtest::run(n, active, requests, freeze_ms).await;
            true
        }
        _ => false,
    }
}

fn hv_status() {
    let h = super::host();
    let f = h.features;
    println!(
        "svm: {} rev {} asids {} npt={} nrip={} flushbyasid={} decode={} vmcbclean={}",
        if super::is_enabled() { "enabled" } else { "DISABLED" },
        f.revision,
        f.nasids,
        f.npt,
        f.nrip_save,
        f.flush_by_asid,
        f.decode_assists,
        f.vmcb_clean
    );
    let m = manager();
    let s = m.summary();
    println!(
        "vms: {} ({} running, {} idle, {} sleeping, {} frozen, {} linux); created {} finished {}; services {}",
        s.vms,
        s.running,
        s.idle,
        s.sleeping,
        s.frozen,
        s.linux,
        m.created.load(Ordering::Relaxed),
        m.finished.load(Ordering::Relaxed),
        s.services
    );
    println!(
        "guest memory: {} resident ({} pages), {} swapped pages, {} npt pages, {} template pages; freeze policy {} ms (linux {} ms); udp requests {}",
        Bytes(s.resident_pages as u64 * 4096),
        s.resident_pages,
        s.swapped_pages,
        s.npt_pages,
        super::image::template_frames(),
        m.default_freeze_ms.load(Ordering::Relaxed),
        m.linux_freeze_ms.load(Ordering::Relaxed),
        m.udp_requests.load(Ordering::Relaxed)
    );
    let fs = crate::mm::frame::stats();
    println!("host memory: {} free of {}", Bytes(fs.free as u64 * 4096), Bytes(fs.total as u64 * 4096));
}

fn list_vms() {
    let vms = manager().list();
    if vms.is_empty() {
        println!("no vms");
        return;
    }
    println!("{:<5} {:<20} {:<8} {:<10} {:>8} {:>7} {:>6} {:>8} {:>10}", "id", "name", "kind", "state", "mem", "res", "swap", "reqs", "cpu");
    for v in vms {
        let st = v.stats();
        println!(
            "{:<5} {:<20} {:<8} {:<10} {:>8} {:>7} {:>6} {:>8} {:>10}",
            v.id,
            v.name,
            format!("{}", v.kind),
            format!("{}", v.state()),
            if v.mem_size >= (1 << 20) { format!("{}M", v.mem_size >> 20) } else { format!("{}K", v.mem_size >> 10) },
            st.resident_pages,
            st.swapped_pages,
            st.requests,
            fmt_us(time::tsc_to_us(st.guest_tsc))
        );
    }
}

async fn vm_cmd(args: &[&str]) {
    let m = manager();
    match args.first().copied() {
        Some("create") => {
            if args.len() < 3 {
                println!("usage: vm create <name> <kind> [mem_kib]");
                return;
            }
            let k = match kind::parse(args[2]) {
                Some(k) => k,
                None => {
                    println!("unknown kind {}", args[2]);
                    return;
                }
            };
            let mem = arg_u64(args, 3, super::image::DEFAULT_MEM / 1024) * 1024;
            let t0 = time::now();
            match m.create_vm(args[1], k, mem, None) {
                Ok(h) => println!("created vm {} '{}' kind {} mem {} in {}", h.id, h.name, kind::name(k), Bytes(mem), fmt_us(time::tsc_to_us(time::now() - t0))),
                Err(e) => println!("error: {}", e),
            }
        }
        Some("list") | None => list_vms(),
        Some("info") => match args.get(1).and_then(|n| m.find(n)) {
            Some(v) => {
                let s = v.stats();
                println!("vm {} '{}' kind {} mem {} state {} service {}", v.id, v.name, v.kind, Bytes(v.mem_size), v.state(), v.service.as_deref().unwrap_or("-"));
                if let Some(r) = v.crash_reason() {
                    println!("  crash: {}", r);
                }
                println!(
                    "  runs {} exits {} (npf {} hcall {} intr {} cpuid {} io {} msr {} mmio {}) injected {} halts {} resets {} guest cpu {}",
                    s.runs, s.exits, s.npf, s.hcalls, s.intr, s.cpuid, s.io, s.msr, s.mmio, s.injected, s.halts, s.resets, fmt_us(time::tsc_to_us(s.guest_tsc))
                );
                println!(
                    "  memory: {} resident pages ({}), {} swapped, {} cow copies, {} npt pages, {} written, {} loaded",
                    s.resident_pages,
                    Bytes(s.resident_pages as u64 * 4096),
                    s.swapped_pages,
                    s.cow,
                    s.npt_pages,
                    s.pages_written,
                    s.pages_loaded
                );
                let t = v.touch_counts();
                let (wq, wc, wi, wx) = v.work_breakdown();
                println!(
                    "  autoscaler view: has_work {} (requests {}, commands {}, console bytes {}, extra_work {}); idle {} us; linux_freeze_ms {}",
                    v.has_work(),
                    wq,
                    wc,
                    wi,
                    wx,
                    v.idle_us(),
                    manager().linux_freeze_ms.load(Ordering::Relaxed)
                );
                println!(
                    "  image: {}; sharing: {} private pages ({} of them in kernel text), origin {}, proxied connections {}; activity marks: console {} link {} request {} other {}",
                    v.image.as_deref().unwrap_or("-"),
                    s.private_pages,
                    s.text_private_pages,
                    v.origin.as_deref().unwrap_or("boot"),
                    s.proxied,
                    t[0],
                    t[1],
                    t[2],
                    t[3]
                );
                println!(
                    "  requests {} wake last {} avg {} max {}; boot {}; console {} bytes; freezes {} (last {}) thaws {} (last {}); idle {}",
                    s.requests,
                    fmt_us(s.last_wake_us),
                    fmt_us(if s.wake_samples > 0 { s.wake_us_total / s.wake_samples } else { 0 }),
                    fmt_us(s.wake_us_max),
                    fmt_us(s.boot_us),
                    s.console_bytes,
                    s.freezes,
                    fmt_us(s.last_freeze_us),
                    s.thaws,
                    fmt_us(s.last_thaw_us),
                    fmt_us(v.idle_us())
                );
            }
            None => println!("no such vm"),
        },
        Some("logs") => match args.get(1).and_then(|n| m.find(n)) {
            Some(v) => {
                for l in v.logs() {
                    println!("  {}", l);
                }
            }
            None => println!("no such vm"),
        },
        Some("kill") | Some("freeze") | Some("thaw") | Some("reset") | Some("devices") => match args.get(1).and_then(|n| m.find(n)) {
            Some(v) => {
                let c = match args[0] {
                    "kill" => Command::Kill,
                    "freeze" => Command::Freeze,
                    "thaw" => Command::Thaw,
                    "reset" => Command::Reset,
                    _ => Command::Dump,
                };
                v.command(c);
                task::yield_now().await;
                timer::sleep_ms(5).await;
                if args[0] != "devices" {
                    println!("vm {} '{}': {}", v.id, v.name, v.state());
                }
            }
            None => println!("no such vm"),
        },
        Some("trace") => match args.get(1).and_then(|n| m.find(n)) {
            Some(v) => {
                let on = match args.get(2) {
                    Some(&"off") => false,
                    Some(&"on") | None => true,
                    _ => true,
                };
                v.trace.store(on, Ordering::Relaxed);
                println!("vm {} '{}': exit tracing {}", v.id, v.name, if on { "on" } else { "off" });
            }
            None => println!("no such vm"),
        },
        Some("send") => match args.get(1).and_then(|n| m.find(n)) {
            Some(v) => {
                let mut text = args[2..].join(" ");
                text.push('\n');
                v.console_input(text.as_bytes());
                println!("sent {} bytes to vm {} console", text.len(), v.name);
            }
            None => println!("no such vm"),
        },
        Some("profile") => match args.get(1).and_then(|n| m.find(n)) {
            Some(v) => {
                if args.get(2) == Some(&"reset") {
                    v.set_profile_base();
                    println!("vm {} '{}': profile baseline set", v.id, v.name);
                } else {
                    let base = v.profile_base().unwrap_or_default();
                    let cur = v.stats();
                    let req = cur.proxied.saturating_sub(base.proxied);
                    super::webtest::print_profile(&format!("vm {} '{}' since baseline, {} proxied requests", v.id, v.name, req), &cur, &base, req.max(1));
                }
            }
            None => println!("no such vm"),
        },
        Some("coldstart") => match args.get(1) {
            Some(name) => super::webtest::coldstart(name, arg_u64(args, 2, 1) as usize).await,
            None => println!("usage: vm coldstart <name> [iterations]"),
        },
        Some("snapshot") => match args.get(1).and_then(|n| m.find(n)) {
            Some(v) => {
                let name = match args.get(2) {
                    Some(n) => *n,
                    None => {
                        println!("usage: vm snapshot <vm> <snapshot-name>");
                        return;
                    }
                };
                let t0 = time::now();
                match m.snapshot_vm(&v, name).await {
                    Ok(t) => println!(
                        "snapshot '{}' of vm {} '{}' in {}: {} pages ({}); clone with: linux clone {} <name> [count]",
                        name,
                        v.id,
                        v.name,
                        fmt_us(time::tsc_to_us(time::now() - t0)),
                        t.pages.len(),
                        Bytes(t.bytes()),
                        name
                    ),
                    Err(e) => println!("snapshot failed: {}", e),
                }
            }
            None => println!("no such vm"),
        },
        Some("ping") | Some("http") => match args.get(1).and_then(|n| m.find(n)) {
            Some(v) => {
                let iface = match v.link().and_then(|l| l.interface()) {
                    Some(i) => i,
                    None => {
                        println!("vm '{}' has no network link", v.name);
                        return;
                    }
                };
                let guest = crate::net::Ipv4Addr([10, 42, 0, 2]);
                if args[0] == "ping" {
                    let count = arg_u64(args, 2, 3);
                    for i in 0..count {
                        match iface.ping(guest, 2000).await {
                            Some(us) => println!("reply from {} via {}: seq={} time={}.{:03} ms", guest, iface.name, i, us / 1000, us % 1000),
                            None => println!("request timed out (seq={})", i),
                        }
                    }
                } else {
                    let path = args.get(2).copied().unwrap_or("/");
                    let port = arg_u64(args, 3, 80) as u16;
                    let t0 = time::now();
                    match crate::net::http::get(iface.clone(), guest, port, &v.name, path, 10_000).await {
                        Ok(r) => println!("HTTP {} in {}: {}", r.status, fmt_us(time::tsc_to_us(time::now() - t0)), r.text().trim_end()),
                        Err(e) => println!("error: {}", e),
                    }
                }
            }
            None => println!("no such vm"),
        },
        Some("attach") => match args.get(1).and_then(|n| m.find(n)) {
            Some(v) => {
                if !v.kind.is_linux() {
                    println!("vm '{}' has no console (only linux vms do)", v.name);
                    return;
                }
                println!("attached to vm {} '{}' console; press Ctrl-] to detach", v.id, v.name);
                for l in v.logs().iter().rev().take(20).rev() {
                    println!("{}", l);
                }
                crate::shell::attach(v.clone());
                v.set_attached(true);
                v.touch();
            }
            None => println!("no such vm"),
        },
        Some("killall") => {
            let n = m.kill_all();
            m.wait_empty(5000).await;
            println!("killed {} vms", n);
        }
        Some(other) => println!("unknown vm subcommand {}", other),
    }
}

async fn linux_cmd(args: &[&str]) {
    let m = manager();
    match args.first().copied() {
        Some("create") => {
            // linux create [--image <set>] <name> [mem_mib] [cmdline...]
            let (image, rest) = match args.get(1).copied() {
                Some("--image") | Some("-i") => (args.get(2).copied(), &args[3.min(args.len())..]),
                _ => (None, &args[1.min(args.len())..]),
            };
            if rest.is_empty() || image == Some("") {
                println!("usage: linux create [--image <set>] <name> [mem_mib] [cmdline...]   ('linux images' lists sets)");
                return;
            }
            let mem = arg_u64(rest, 1, super::linux_boot::DEFAULT_MEM >> 20) << 20;
            let cmdline = if rest.len() > 2 { Some(rest[2..].join(" ")) } else { None };
            let t0 = time::now();
            match m.create_linux(rest[0], mem, cmdline.as_deref(), image).await {
                Ok(h) => println!(
                    "created linux vm {} '{}' from image '{}' with {} in {}; use 'vm attach {}' for its console",
                    h.id,
                    h.name,
                    h.image.as_deref().unwrap_or("?"),
                    Bytes(mem),
                    fmt_us(time::tsc_to_us(time::now() - t0)),
                    h.name
                ),
                Err(e) => println!("error: {}", e),
            }
        }
        Some("clone") => {
            if args.len() < 3 {
                println!("usage: linux clone <snapshot> <name> [count] [run]   (clones start frozen unless 'run')");
                return;
            }
            let count = arg_u64(args, 3, 1).max(1) as usize;
            let run = args.get(4) == Some(&"run");
            let t0 = time::now();
            let mut made = 0;
            let mut max_us = 0;
            for i in 1..=count {
                let name = if count == 1 { String::from(args[2]) } else { format!("{}{:04}", args[2], i) };
                let c0 = time::now();
                match m.clone_vm(args[1], &name, !run) {
                    Ok(h) => {
                        made += 1;
                        max_us = max_us.max(time::tsc_to_us(time::now() - c0));
                        if count == 1 {
                            println!("cloned vm {} '{}' from snapshot '{}' ({})", h.id, h.name, args[1], if run { "running" } else { "frozen" });
                        }
                    }
                    Err(e) => {
                        println!("clone {} failed: {}", name, e);
                        break;
                    }
                }
                if i % 50 == 0 {
                    task::yield_now().await;
                }
            }
            let total = time::tsc_to_us(time::now() - t0);
            if count > 1 {
                println!("cloned {} vms in {} ({} avg, {} max per clone)", made, fmt_us(total), fmt_us(total / made.max(1) as u64), fmt_us(max_us));
            }
        }
        Some("snapshots") => {
            let list = m.snapshots();
            if list.is_empty() {
                println!("no snapshots (take one with: vm snapshot <vm> <name>)");
            }
            for (name, t) in list {
                println!(
                    "{:<12} {:>7} pages {:>10}  from vm '{}'  text pages {:?}",
                    name,
                    t.pages.len(),
                    Bytes(t.bytes()),
                    t.origin.as_deref().unwrap_or("?"),
                    t.text_pages.map(|(a, b)| b - a)
                );
            }
        }
        Some("images") | None => {
            let sets = crate::disk::images::sets();
            if sets.is_empty() {
                println!("no bootable images installed (run: cargo xtask install-linux [--name <set>])");
                return;
            }
            println!("{:<14} {:>10} {:>10} {:>10}   default", "image", "kernel", "initramfs", "total");
            for (i, s) in sets.iter().enumerate() {
                println!(
                    "{:<14} {:>10} {:>10} {:>10}   {}",
                    s.name,
                    format!("{}", Bytes(s.kernel.size)),
                    format!("{}", Bytes(s.initrd.as_ref().map(|e| e.size).unwrap_or(0))),
                    format!("{}", Bytes(s.bytes())),
                    if i == 0 { "yes" } else { "" }
                );
            }
            println!("boot one with: linux create [--image <set>] <name> [mem_mib] [cmdline...]");
        }
        Some(other) => println!("unknown linux subcommand {}", other),
    }
}

async fn svc_cmd(args: &[&str]) {
    let m = manager();
    match args.first().copied() {
        Some("create") => {
            if args.len() < 3 {
                println!("usage: svc create <name> <kind> [max_replicas] [mem_kib]");
                return;
            }
            let k = match kind::parse(args[2]) {
                Some(k) => k,
                None => {
                    println!("unknown kind {}", args[2]);
                    return;
                }
            };
            let max = arg_u64(args, 3, 4) as usize;
            let mem = arg_u64(args, 4, super::image::DEFAULT_MEM / 1024) * 1024;
            match m.create_service(args[1], k, mem, max) {
                Ok(()) => println!("service '{}' created (kind {}, max {} replicas, {} each)", args[1], kind::name(k), max, Bytes(mem)),
                Err(e) => println!("error: {}", e),
            }
        }
        Some("list") | None => {
            for s in m.services() {
                let reps = m.replicas(&s.name);
                let states: Vec<String> = reps.iter().map(|r| format!("{}:{}", r.name, r.state())).collect();
                println!(
                    "{:<16} kind {:<8} replicas {}/{} (min {}) freeze {} ms destroy {} ms requests {} cold-starts {}  [{}]",
                    s.name,
                    kind::name(s.kind),
                    reps.len(),
                    s.max_replicas,
                    s.min_replicas,
                    s.freeze_after_ms,
                    s.destroy_after_ms,
                    s.requests,
                    s.cold_starts,
                    states.join(" ")
                );
            }
        }
        Some("set") => {
            if args.len() < 3 {
                println!("usage: svc set <name> freeze_ms=<n> destroy_ms=<n> max=<n> min=<n>");
                return;
            }
            let ok = m.set_service(args[1], |s| {
                for kv in &args[2..] {
                    if let Some((k, v)) = kv.split_once('=') {
                        let v: u64 = v.parse().unwrap_or(0);
                        match k {
                            "freeze_ms" => s.freeze_after_ms = v,
                            "destroy_ms" => s.destroy_after_ms = v,
                            "max" => s.max_replicas = (v as usize).max(1),
                            "min" => s.min_replicas = v as usize,
                            _ => {}
                        }
                    }
                }
            });
            println!("{}", if ok { "ok" } else { "no such service" });
        }
        Some("delete") => {
            let ok = args.get(1).map(|n| m.delete_service(n)).unwrap_or(false);
            println!("{}", if ok { "deleted" } else { "no such service" });
        }
        Some(other) => println!("unknown svc subcommand {}", other),
    }
}

/// Fire `n` requests at `target` with `conc` in flight and report latency.
pub async fn bench(target: &str, n: usize, conc: usize) {
    let target = String::from(target);
    let t0 = time::now();
    let mut handles = Vec::new();
    let per_worker = (n + conc - 1) / conc;
    for w in 0..conc {
        let tgt = target.clone();
        let count = per_worker.min(n.saturating_sub(w * per_worker));
        handles.push(task::spawn("bench", async move {
            let mut lat = Vec::with_capacity(count);
            let mut errors = 0usize;
            for i in 0..count {
                let payload = format!("bench {} {}", w, i);
                match manager().request(&tgt, payload.into_bytes(), 10_000).await {
                    Ok((_, us)) => lat.push(us),
                    Err(_) => errors += 1,
                }
            }
            (lat, errors)
        }));
    }
    let mut all = Vec::new();
    let mut errors = 0;
    for h in handles {
        let (l, e) = h.await;
        all.extend(l);
        errors += e;
    }
    let total_us = time::tsc_to_us(time::now() - t0).max(1);
    all.sort_unstable();
    println!(
        "bench {}: {} requests ({} errors) in {} -> {} req/s; latency p50 {} p90 {} p99 {} max {}",
        target,
        all.len(),
        errors,
        fmt_us(total_us),
        (all.len() as u64 * 1_000_000) / total_us,
        fmt_us(percentile(&all, 50)),
        fmt_us(percentile(&all, 90)),
        fmt_us(percentile(&all, 99)),
        fmt_us(*all.last().unwrap_or(&0))
    );
}

/// Create `n` VMs and send each one request.
pub async fn swarm(n: usize, k: u64, mem: u64) {
    let m = manager();
    let free0 = crate::mm::frame::stats().free;
    let t0 = time::now();
    let mut vms = Vec::with_capacity(n);
    for i in 0..n {
        match m.create_vm(&format!("swarm-{}", i), k, mem, None) {
            Ok(h) => vms.push(h),
            Err(e) => {
                println!("create failed at {}: {}", i, e);
                break;
            }
        }
        if i % 64 == 63 {
            task::yield_now().await;
        }
    }
    let t_create = time::tsc_to_us(time::now() - t0);
    let t1 = time::now();
    let mut lat = Vec::with_capacity(vms.len());
    let mut errors = 0;
    for v in &vms {
        match v.request(b"hello".to_vec(), 10_000).await {
            Ok(_) => lat.push(time::tsc_to_us(v.stats().last_wake_us)),
            Err(_) => errors += 1,
        }
    }
    let t_req = time::tsc_to_us(time::now() - t1);
    task::yield_now().await;
    let free1 = crate::mm::frame::stats().free;
    lat.sort_unstable();
    let s = m.summary();
    println!(
        "swarm: created {} vms in {} ({} per vm); first request round in {} ({} errors); host memory used {} ({} per vm); guest resident {} pages, npt {} pages",
        vms.len(),
        fmt_us(t_create),
        fmt_us(t_create / vms.len().max(1) as u64),
        fmt_us(t_req),
        errors,
        Bytes((free0.saturating_sub(free1) * 4096) as u64),
        Bytes((free0.saturating_sub(free1) * 4096 / vms.len().max(1)) as u64),
        s.resident_pages,
        s.npt_pages
    );
}

/// End-to-end scale-to-zero scenario across many VMs, printing a report.
pub async fn scale_test(n: usize) {
    let m = manager();
    let mem = 512 * 1024;
    println!("=== scale test: {} counter VMs of {} each ===", n, Bytes(mem));
    let free0 = crate::mm::frame::stats().free;

    let t0 = time::now();
    let mut vms = Vec::with_capacity(n);
    for i in 0..n {
        match m.create_vm(&format!("st-{}", i), kind::COUNTER, mem, None) {
            Ok(h) => vms.push(h),
            Err(e) => {
                println!("create failed at {}: {}", i, e);
                break;
            }
        }
        if i % 64 == 63 {
            task::yield_now().await;
        }
    }
    println!("[1] created {} vms in {}", vms.len(), fmt_us(time::tsc_to_us(time::now() - t0)));

    let t1 = time::now();
    let mut boot = Vec::new();
    for v in &vms {
        if v.request(b"x".to_vec(), 10_000).await.is_ok() {
            boot.push(v.stats().boot_us);
        }
    }
    boot.sort_unstable();
    let free_warm = crate::mm::frame::stats().free;
    let s = m.summary();
    println!(
        "[2] first request to all: {} total; guest boot-to-first-hypercall p50 {} p99 {}; resident {} pages ({} per vm), host memory used {}",
        fmt_us(time::tsc_to_us(time::now() - t1)),
        fmt_us(percentile(&boot, 50)),
        fmt_us(percentile(&boot, 99)),
        s.resident_pages,
        s.resident_pages / vms.len().max(1),
        Bytes((free0.saturating_sub(free_warm) * 4096) as u64)
    );

    let t2 = time::now();
    let mut warm = Vec::new();
    for v in &vms {
        if v.request(b"x".to_vec(), 10_000).await.is_ok() {
            warm.push(v.stats().last_wake_us);
        }
    }
    warm.sort_unstable();
    println!(
        "[3] warm request to all: {} total; wake latency p50 {} p99 {} max {}",
        fmt_us(time::tsc_to_us(time::now() - t2)),
        fmt_us(percentile(&warm, 50)),
        fmt_us(percentile(&warm, 99)),
        fmt_us(*warm.last().unwrap_or(&0))
    );

    let t3 = time::now();
    for v in &vms {
        v.wait_blocked(1000).await;
        v.command(Command::Freeze);
    }
    let mut frozen = 0;
    for v in &vms {
        if v.wait_state(VmState::Frozen, 10_000).await {
            frozen += 1;
        }
    }
    let free_frozen = crate::mm::frame::stats().free;
    let s = m.summary();
    let mut fz: Vec<u64> = vms.iter().map(|v| v.stats().last_freeze_us).collect();
    fz.sort_unstable();
    println!(
        "[4] froze {} vms in {}; per-vm freeze p50 {} p99 {}; resident {} pages, swapped {} pages, host memory used {} ({} per frozen vm)",
        frozen,
        fmt_us(time::tsc_to_us(time::now() - t3)),
        fmt_us(percentile(&fz, 50)),
        fmt_us(percentile(&fz, 99)),
        s.resident_pages,
        s.swapped_pages,
        Bytes((free0.saturating_sub(free_frozen) * 4096) as u64),
        Bytes((free0.saturating_sub(free_frozen) * 4096 / vms.len().max(1)) as u64)
    );

    let t4 = time::now();
    let mut cold = Vec::new();
    let mut ok = 0;
    for v in &vms {
        match v.request(b"x".to_vec(), 10_000).await {
            Ok(r) => {
                if r.starts_with(b"count=3") {
                    ok += 1;
                }
                cold.push(v.stats().last_wake_us);
            }
            Err(e) => println!("  request to {} failed: {}", v.name, e),
        }
    }
    cold.sort_unstable();
    let s = m.summary();
    println!(
        "[5] thawed {} vms in {} ({} kept their state); request latency from frozen p50 {} p99 {} max {}; resident {} pages",
        vms.len(),
        fmt_us(time::tsc_to_us(time::now() - t4)),
        ok,
        fmt_us(percentile(&cold, 50)),
        fmt_us(percentile(&cold, 99)),
        fmt_us(*cold.last().unwrap_or(&0)),
        s.resident_pages
    );

    let t5 = time::now();
    for v in &vms {
        v.command(Command::Kill);
    }
    m.wait_empty(30_000).await;
    let free_end = crate::mm::frame::stats().free;
    println!(
        "[6] destroyed all in {}; host memory delta after teardown: {} frames",
        fmt_us(time::tsc_to_us(time::now() - t5)),
        free0 as i64 - free_end as i64
    );
    println!("=== scale test done ===");
}
