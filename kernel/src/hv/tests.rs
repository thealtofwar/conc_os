//! Hypervisor self tests.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::manager::manager;
use super::vm::{kind, Command, VmState};
use crate::mm::frame;
use crate::selftest::{check, tests, TestFn, TestResult};
use crate::task::timer;
use crate::time;

pub fn tests() -> &'static [(&'static str, TestFn)] {
    tests![
        enabled,
        hello_vm,
        echo_request,
        counter_state,
        primes_compute,
        fault_isolated,
        sleep_hypercall,
        preemption_fairness,
        freeze_thaw_state,
        service_scale_to_zero,
        many_vms_freeze_wake,
        linux_boot_shell,
        linux_freeze_thaw,
        linux_network,
        linux_proxy,
        linux_snapshot_clone,
        linux_image_sets,
    ]
}

const MEM: u64 = 512 * 1024;

fn text(v: &[u8]) -> String {
    String::from_utf8_lossy(v).into_owned()
}

async fn enabled() -> TestResult {
    check!(super::is_enabled(), "SVM not enabled");
    Ok(())
}

async fn hello_vm() -> TestResult {
    let m = manager();
    let v = m.create_vm("t-hello", kind::HELLO, MEM, None).map_err(String::from)?;
    check!(v.wait_finished(5000).await, "hello vm did not finish");
    check!(v.state() == VmState::Exited(0), "state {} (crash: {:?})", v.state(), v.crash_reason());
    let logs = v.logs();
    check!(logs.iter().any(|l| l.contains("hello from a conc_os guest")), "no hello log: {:?}", logs);
    Ok(())
}

async fn echo_request() -> TestResult {
    let m = manager();
    let v = m.create_vm("t-echo", kind::ECHO, MEM, None).map_err(String::from)?;
    let r = v.request(b"abc xyz".to_vec(), 5000).await.map_err(|e| format!("{}", e))?;
    let t = text(&r);
    check!(t.contains("ABC XYZ"), "unexpected reply {:?}", t);
    let r2 = v.request(b"second".to_vec(), 5000).await.map_err(|e| format!("{}", e))?;
    check!(text(&r2).contains("SECOND"), "unexpected second reply {:?}", text(&r2));
    check!(v.wait_blocked(2000).await && v.state() == VmState::Idle, "vm not idle after requests: {}", v.state());
    v.command(Command::Kill);
    check!(v.wait_finished(2000).await, "kill did not finish");
    Ok(())
}

async fn counter_state() -> TestResult {
    let m = manager();
    let v = m.create_vm("t-counter", kind::COUNTER, MEM, None).map_err(String::from)?;
    for i in 1..=3 {
        let r = v.request(Vec::new(), 5000).await.map_err(|e| format!("{}", e))?;
        check!(text(&r) == format!("count={}", i), "reply {:?} at iteration {}", text(&r), i);
    }
    v.command(Command::Kill);
    v.wait_finished(2000).await;
    Ok(())
}

async fn primes_compute() -> TestResult {
    let m = manager();
    let v = m.create_vm("t-primes", kind::PRIMES, MEM, None).map_err(String::from)?;
    let r = v.request(b"1000".to_vec(), 10_000).await.map_err(|e| format!("{}", e))?;
    check!(text(&r).starts_with("primes<=1000: 168"), "reply {:?}", text(&r));
    let r = v.request(b"100000".to_vec(), 20_000).await.map_err(|e| format!("{}", e))?;
    check!(text(&r).starts_with("primes<=100000: 9592"), "reply {:?}", text(&r));
    v.command(Command::Kill);
    v.wait_finished(2000).await;
    Ok(())
}

async fn fault_isolated() -> TestResult {
    let m = manager();
    let v = m.create_vm("t-fault", kind::FAULT, MEM, None).map_err(String::from)?;
    check!(v.wait_finished(5000).await, "fault vm did not finish");
    check!(v.state() == VmState::Crashed, "state {}", v.state());
    let reason = v.crash_reason().unwrap_or_default();
    check!(reason.contains("#PF"), "unexpected crash reason: {}", reason);
    // The host is unaffected: another VM works fine afterwards.
    let e = m.create_vm("t-after-fault", kind::ECHO, MEM, None).map_err(String::from)?;
    let r = e.request(b"ok".to_vec(), 5000).await.map_err(|e| format!("{}", e))?;
    check!(text(&r).contains("OK"), "echo after fault: {:?}", text(&r));
    e.command(Command::Kill);
    e.wait_finished(2000).await;
    Ok(())
}

async fn sleep_hypercall() -> TestResult {
    let m = manager();
    let v = m.create_vm("t-sleepy", kind::SLEEPY, MEM, None).map_err(String::from)?;
    let t0 = time::now();
    let r = v.request(b"40".to_vec(), 5000).await.map_err(|e| format!("{}", e))?;
    let dt = time::tsc_to_us(time::now() - t0);
    check!(text(&r).starts_with("slept 40 ms"), "reply {:?}", text(&r));
    check!(dt >= 40_000, "returned too early: {} us", dt);
    v.command(Command::Kill);
    v.wait_finished(2000).await;
    Ok(())
}

/// Two CPU-bound guests must not starve a third VM or the host.
async fn preemption_fairness() -> TestResult {
    let m = manager();
    let s1 = m.create_vm("t-spin1", kind::SPIN, MEM, None).map_err(String::from)?;
    let s2 = m.create_vm("t-spin2", kind::SPIN, MEM, None).map_err(String::from)?;
    timer::sleep_ms(50).await;
    let e = m.create_vm("t-echo-fair", kind::ECHO, MEM, None).map_err(String::from)?;
    let t0 = time::now();
    let r = e.request(b"fair".to_vec(), 5000).await.map_err(|e| format!("{}", e))?;
    let dt = time::tsc_to_us(time::now() - t0);
    check!(text(&r).contains("FAIR"), "reply {:?}", text(&r));
    let ticks0 = time::ticks();
    timer::sleep_ms(50).await;
    check!(time::ticks() > ticks0, "timer stalled while guests were spinning");
    let st1 = s1.stats();
    let st2 = s2.stats();
    check!(st1.runs > 1 && st2.runs > 1, "spinners were not time-sliced: runs {} {}", st1.runs, st2.runs);
    check!(st1.intr > 0 || st2.intr > 0, "no INTR exits: preemption not happening");
    for v in [s1, s2, e] {
        v.command(Command::Kill);
        v.wait_finished(2000).await;
    }
    check!(dt < 3_000_000, "echo took {} us under load", dt);
    Ok(())
}

async fn freeze_thaw_state() -> TestResult {
    let m = manager();
    let v = m.create_vm("t-freeze", kind::COUNTER, MEM, None).map_err(String::from)?;
    let r = v.request(Vec::new(), 5000).await.map_err(|e| format!("{}", e))?;
    check!(text(&r) == "count=1", "reply {:?}", text(&r));
    check!(v.wait_blocked(2000).await, "not idle");
    let before = v.stats();
    check!(before.resident_pages > 0, "no resident pages before freeze");
    v.command(Command::Freeze);
    check!(v.wait_state(VmState::Frozen, 5000).await, "did not freeze: {}", v.state());
    let frozen = v.stats();
    if crate::disk::store().is_some() {
        check!(frozen.resident_pages == 0, "{} pages still resident after freeze", frozen.resident_pages);
        check!(frozen.swapped_pages == before.resident_pages, "swapped {} != resident before {}", frozen.swapped_pages, before.resident_pages);
    }
    check!(frozen.npt_pages == 0, "npt not released");
    let r = v.request(Vec::new(), 10_000).await.map_err(|e| format!("{}", e))?;
    check!(text(&r) == "count=2", "state lost across freeze: {:?}", text(&r));
    let after = v.stats();
    check!(after.thaws == 1, "thaws {}", after.thaws);
    check!(after.resident_pages <= before.resident_pages, "resident grew: {} -> {}", before.resident_pages, after.resident_pages);
    v.wait_blocked(2000).await;
    v.command(Command::Freeze);
    check!(v.wait_state(VmState::Frozen, 5000).await, "second freeze failed");
    let r = v.request(Vec::new(), 10_000).await.map_err(|e| format!("{}", e))?;
    check!(text(&r) == "count=3", "state lost across second freeze: {:?}", text(&r));
    v.command(Command::Kill);
    v.wait_finished(2000).await;
    Ok(())
}

async fn service_scale_to_zero() -> TestResult {
    let m = manager();
    m.create_service("t-svc", kind::COUNTER, MEM, 2).map_err(String::from)?;
    m.set_service("t-svc", |s| {
        s.freeze_after_ms = 100;
        s.destroy_after_ms = 400;
    });
    let (r, _) = m.request("t-svc", Vec::new(), 10_000).await.map_err(|e| format!("{}", e))?;
    check!(text(&r) == "count=1", "reply {:?}", text(&r));
    check!(m.replicas("t-svc").len() == 1, "expected 1 replica");
    let deadline = time::now() + time::us_to_tsc(5_000_000);
    let mut saw_frozen = false;
    while time::now() < deadline {
        let reps = m.replicas("t-svc");
        if reps.iter().any(|r| r.state() == VmState::Frozen) {
            saw_frozen = true;
        }
        if reps.is_empty() {
            break;
        }
        timer::sleep_ms(20).await;
    }
    check!(m.replicas("t-svc").is_empty(), "replica was not destroyed after idle timeout");
    check!(saw_frozen, "replica was never frozen before destruction");
    let (r, us) = m.request("t-svc", Vec::new(), 10_000).await.map_err(|e| format!("{}", e))?;
    check!(text(&r) == "count=1", "fresh replica reply {:?}", text(&r));
    let svc = m.service("t-svc").ok_or("service vanished")?;
    check!(svc.cold_starts == 2, "cold starts {}", svc.cold_starts);
    check!(us < 2_000_000, "cold start took {} us", us);
    m.delete_service("t-svc");
    timer::sleep_ms(20).await;
    Ok(())
}

async fn many_vms_freeze_wake() -> TestResult {
    const N: usize = 48;
    let m = manager();
    let free0 = frame::stats().free;
    let pooled0 = super::npt::pooled_tables();
    let mut vms = Vec::new();
    for i in 0..N {
        vms.push(m.create_vm(&format!("t-many-{}", i), kind::COUNTER, MEM, None).map_err(String::from)?);
    }
    for v in &vms {
        let r = v.request(Vec::new(), 10_000).await.map_err(|e| format!("{}", e))?;
        check!(text(&r) == "count=1", "reply {:?}", text(&r));
    }
    let warm = m.summary();
    check!(warm.resident_pages >= N, "resident pages {}", warm.resident_pages);
    for v in &vms {
        v.wait_blocked(2000).await;
        v.command(Command::Freeze);
    }
    for v in &vms {
        check!(v.wait_state(VmState::Frozen, 10_000).await, "{} did not freeze", v.name);
    }
    let cold = m.summary();
    if crate::disk::store().is_some() {
        check!(cold.resident_pages == 0, "{} resident pages while all frozen", cold.resident_pages);
    }
    check!(cold.npt_pages == 0, "npt pages while frozen: {}", cold.npt_pages);
    let mut worst = 0;
    for v in &vms {
        let r = v.request(Vec::new(), 10_000).await.map_err(|e| format!("{}", e))?;
        check!(text(&r) == "count=2", "state lost in {}: {:?}", v.name, text(&r));
        worst = worst.max(v.stats().last_wake_us);
    }
    check!(worst < 1_000_000, "worst wake from frozen {} us", worst);
    for v in &vms {
        v.command(Command::Kill);
    }
    check!(m.wait_empty(10_000).await, "vms did not all terminate");
    let free1 = frame::stats().free;
    // Page-table frames park in the NPT pool instead of returning to the
    // allocator (see hv/npt.rs); they are still ours.
    let pooled1 = super::npt::pooled_tables();
    check!(free1 + pooled1 + 64 >= free0 + pooled0, "frames leaked: {} -> {} (pooled tables {} -> {})", free0, free1, pooled0, pooled1);
    Ok(())
}

/// Linux tests can be switched off (`hv set linux_tests=0`) on hosts whose
/// QEMU cannot run Linux guests under SVM.
pub static LINUX_TESTS: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

fn linux_available() -> bool {
    LINUX_TESTS.load(core::sync::atomic::Ordering::Relaxed)
        && crate::disk::images::find_kind(crate::disk::images::KIND_KERNEL).is_some()
}

/// Boot the installed Linux kernel to a busybox shell, run a command on its
/// serial console, then power it off.
fn linux_skip_reason() -> &'static str {
    if !LINUX_TESTS.load(core::sync::atomic::Ordering::Relaxed) {
        "linux tests disabled on this host (Windows QEMU truncates SVM segment bases; use --wsl)"
    } else {
        "no linux kernel installed; run cargo xtask install-linux"
    }
}

async fn linux_boot_shell() -> TestResult {
    if !linux_available() {
        println!("(skipped: {}) ", linux_skip_reason());
        return Ok(());
    }
    let m = manager();
    let v = m.create_linux("t-linux", 128 << 20, None, None).await?;
    check!(v.wait_console("conc_os linux guest", 300_000).await, "linux did not reach the shell (state {}, crash {:?}, last logs {:?})", v.state(), v.crash_reason(), v.logs().iter().rev().take(6).collect::<Vec<_>>());
    timer::sleep_ms(300).await;
    v.console_input(b"echo marker-$((6*7))-ok; cat /proc/uptime\n");
    check!(v.wait_console("marker-42-ok", 30_000).await, "shell did not answer (logs {:?})", v.logs().iter().rev().take(8).collect::<Vec<_>>());
    let s = v.stats();
    check!(s.injected > 0, "no interrupts were injected");
    check!(s.halts > 0, "guest never halted (no idle)");
    // Idle Linux must not burn CPU: guest time over 1 s of quiet is small.
    timer::sleep_ms(500).await;
    let g0 = v.stats().guest_tsc;
    timer::sleep_ms(1000).await;
    let g1 = v.stats().guest_tsc;
    let busy_us = time::tsc_to_us(g1 - g0);
    check!(busy_us < 400_000, "idle linux used {} us of cpu in 1 s", busy_us);
    v.console_input(b"poweroff -f\n");
    check!(v.wait_finished(30_000).await, "poweroff did not stop the vm (state {})", v.state());
    check!(matches!(v.state(), VmState::Exited(_)), "unexpected final state {} ({:?})", v.state(), v.crash_reason());
    Ok(())
}

/// A quiet Linux VM can be frozen to disk and comes back on console input.
async fn linux_freeze_thaw() -> TestResult {
    if !linux_available() {
        println!("(skipped: {}) ", linux_skip_reason());
        return Ok(());
    }
    let m = manager();
    let v = m.create_linux("t-linux-fz", 128 << 20, None, None).await?;
    check!(v.wait_console("conc_os linux guest", 300_000).await, "linux did not boot (state {}, crash {:?})", v.state(), v.crash_reason());
    timer::sleep_ms(300).await;
    // Freeze while halted.
    let mut frozen = false;
    for _ in 0..50 {
        if v.state() == VmState::Idle {
            v.command(Command::Freeze);
            if v.wait_state(VmState::Frozen, 2000).await {
                frozen = true;
                break;
            }
        }
        timer::sleep_ms(10).await;
    }
    check!(frozen, "could not freeze the linux vm (state {})", v.state());
    let st = v.stats();
    if crate::disk::store().is_some() {
        check!(st.resident_pages == 0, "{} pages resident while frozen", st.resident_pages);
    }
    check!(st.swapped_pages > 100, "too few pages swapped: {}", st.swapped_pages);
    // Stays frozen while nothing happens.
    timer::sleep_ms(500).await;
    check!(v.state() == VmState::Frozen, "vm woke up on its own: {}", v.state());
    // Console input thaws it and the shell still works.
    v.console_input(b"echo thawed-$((2*21))\n");
    check!(v.wait_console("thawed-42", 60_000).await, "shell did not answer after thaw (state {}, logs {:?})", v.state(), v.logs().iter().rev().take(6).collect::<Vec<_>>());
    check!(v.stats().thaws == 1, "thaws {}", v.stats().thaws);
    v.command(Command::Kill);
    check!(v.wait_finished(10_000).await, "kill did not finish");
    Ok(())
}

/// The guest's virtio-mmio NIC: ping it over its link, then fetch a page from
/// the web server its init script starts.
async fn linux_network() -> TestResult {
    if !linux_available() {
        println!("(skipped: {}) ", linux_skip_reason());
        return Ok(());
    }
    let m = manager();
    let v = m.create_linux("t-linux-net", 128 << 20, None, None).await?;
    check!(v.wait_console("conc_os linux guest", 300_000).await, "linux did not boot (state {}, crash {:?})", v.state(), v.crash_reason());
    let iface = v.link().and_then(|l| l.interface()).ok_or("vm has no network interface")?;
    let guest = crate::net::Ipv4Addr([10, 42, 0, 2]);
    let mut rtt = None;
    for _ in 0..40 {
        if let Some(us) = iface.ping(guest, 1000).await {
            rtt = Some(us);
            break;
        }
        timer::sleep_ms(250).await;
    }
    let rtt = rtt.ok_or("guest did not answer ping")?;
    let mut resp = None;
    let mut last_err = String::new();
    for _ in 0..40 {
        match crate::net::http::get(iface.clone(), guest, 80, &v.name, "/", 5000).await {
            Ok(r) => {
                resp = Some(r);
                break;
            }
            Err(e) => {
                last_err = e;
                timer::sleep_ms(250).await;
            }
        }
    }
    let r = match resp {
        Some(r) => r,
        None => return Err(format!("no HTTP response from guest: {}", last_err)),
    };
    check!(r.status == 200, "http status {}", r.status);
    check!(!r.body.is_empty(), "empty body");
    print!("(rtt {} us, {} body bytes) ", rtt, r.body.len());
    v.command(Command::Kill);
    check!(v.wait_finished(10_000).await, "kill did not finish");
    Ok(())
}

/// Route by name through the proxy: plain HTTP by Host header and a TLS
/// ClientHello by SNI, then freeze the VM and show a request thaws it with
/// its request counter intact.
async fn linux_proxy() -> TestResult {
    use crate::net::proxy;
    if !linux_available() {
        println!("(skipped: {}) ", linux_skip_reason());
        return Ok(());
    }
    let m = manager();
    let v = m.create_linux("t-web", 128 << 20, None, None).await?;
    check!(v.wait_console("conc_os linux guest", 300_000).await, "linux did not boot (state {}, crash {:?})", v.state(), v.crash_reason());
    let lo = crate::net::tcp::loopback_interface("lo-proxy");
    proxy::listen_on(lo.clone(), 8443).map_err(|e| format!("proxy listen: {}", e))?;
    proxy::listen_on(lo.clone(), 8080).map_err(|e| format!("proxy listen: {}", e))?;
    let here = crate::net::Ipv4Addr([127, 0, 0, 1]);
    // HTTP by Host header (retry while the guest's server is starting).
    let mut first = None;
    let mut last_err = String::new();
    for _ in 0..60 {
        match crate::net::http::get(lo.clone(), here, 8080, "t-web.conc", "/", 5000).await {
            Ok(r) if r.status == 200 => {
                first = Some(r);
                break;
            }
            Ok(r) => last_err = format!("status {}", r.status),
            Err(e) => last_err = e,
        }
        timer::sleep_ms(500).await;
    }
    let first = match first {
        Some(r) => r,
        None => return Err(format!("no answer through the proxy: {}", last_err)),
    };
    let body = first.text();
    let go_server = body.contains("hits=");
    if go_server {
        check!(body.contains("hits=1"), "first answer: {:?}", body.trim());
        check!(body.contains(&format!("vm={}", v.id)), "hypercall vm id missing: {:?}", body.trim());
    }
    // Unknown name gets a 502 without touching any VM.
    let bad = crate::net::http::get(lo.clone(), here, 8080, "nosuchvm.conc", "/", 5000).await?;
    check!(bad.status == 502, "unknown vm answered {}", bad.status);
    if go_server {
        // TLS: send a ClientHello with SNI, expect a ServerHello back.
        let s = crate::net::tcp::TcpStream::connect(lo.clone(), here, 8443, 5000).await?;
        s.write_all(&proxy::client_hello("t-web.conc")).await?;
        let mut resp = Vec::new();
        s.read_until(&mut resp, 6, 10_000).await?;
        check!(resp.len() >= 6, "no TLS answer through the proxy ({} bytes)", resp.len());
        check!(resp[0] == 0x16 && resp[5] == 0x02, "expected a ServerHello, got {:02x?}", &resp[..6]);
        s.abort();
    }
    // Let the closing connections finish (a late ACK would thaw the VM
    // again, which is correct but confuses the count below), then freeze.
    timer::sleep_ms(500).await;
    let mut frozen = false;
    for _ in 0..200 {
        if v.state() == VmState::Idle {
            v.command(Command::Freeze);
            if v.wait_state(VmState::Frozen, 5000).await {
                frozen = true;
                break;
            }
        }
        timer::sleep_ms(20).await;
    }
    check!(frozen, "could not freeze the vm (state {})", v.state());
    let thaws_before = v.stats().thaws;
    let t0 = time::now();
    let again = crate::net::http::get(lo.clone(), here, 8080, "t-web.conc", "/", 60_000).await?;
    let cold_us = time::tsc_to_us(time::now() - t0);
    check!(again.status == 200, "status after thaw {}", again.status);
    check!(v.stats().thaws == thaws_before + 1, "thaws {} (was {})", v.stats().thaws, thaws_before);
    if go_server {
        check!(again.text().contains("hits=2"), "counter after thaw: {:?}", again.text().trim());
    }
    print!("(cold request {} ms, {}) ", cold_us / 1000, if go_server { "go server" } else { "busybox httpd" });
    crate::net::remove_interface(lo.id);
    v.command(Command::Kill);
    check!(v.wait_finished(10_000).await, "kill did not finish");
    Ok(())
}

/// Boot two VMs from two different installed image sets at the same time and
/// check each runs its own kernel.  Skipped unless a second set is installed
/// (`cargo xtask install-linux --name <set> --kernel <other-vmlinux>`).
async fn linux_image_sets() -> TestResult {
    if !linux_available() {
        println!("(skipped: {}) ", linux_skip_reason());
        return Ok(());
    }
    let sets = crate::disk::images::sets();
    if sets.len() < 2 {
        println!("(skipped: only {} image set installed) ", sets.len());
        return Ok(());
    }
    let m = manager();
    // An unknown image is refused before anything is created.
    let bad = m.create_linux("t-nope", 128 << 20, None, Some("no-such-image")).await;
    check!(bad.is_err(), "unknown image was accepted");
    let msg = bad.err().unwrap_or_default();
    check!(msg.contains("no image"), "unexpected error for unknown image: {}", msg);

    // Boot one VM from each of the first two sets, concurrently.
    let mut vms = Vec::new();
    for (i, s) in sets.iter().take(2).enumerate() {
        let v = m.create_linux(&format!("t-img{}", i), 128 << 20, None, Some(&s.name)).await?;
        check!(v.image.as_deref() == Some(s.name.as_str()), "vm labelled '{:?}', expected '{}'", v.image, s.name);
        vms.push((s.name.clone(), v));
    }
    let mut versions = Vec::new();
    for (set, v) in &vms {
        check!(
            v.wait_console("conc_os linux guest", 300_000).await,
            "image '{}' did not boot (state {}, crash {:?})",
            set,
            v.state(),
            v.crash_reason()
        );
        let banner = v
            .logs()
            .iter()
            .find(|l| l.contains("conc_os linux guest"))
            .cloned()
            .unwrap_or_default();
        // "conc_os linux guest: Linux 5.10.233 booted; ..."
        let ver = String::from(banner.split_whitespace().nth(4).unwrap_or("?"));
        versions.push((set.clone(), ver));
    }
    // Both are alive at the same time, each on its own kernel image.
    for (set, v) in &vms {
        check!(!v.is_finished(), "vm from image '{}' died: {}", set, v.state());
    }
    let list: Vec<String> = versions.iter().map(|(s, v)| format!("{} -> Linux {}", s, v)).collect();
    print!("({}) ", list.join(", "));
    if versions[0].1 == versions[1].1 {
        print!("(same kernel version in both sets) ");
    }
    for (_, v) in &vms {
        v.command(Command::Kill);
    }
    for (set, v) in &vms {
        check!(v.wait_finished(10_000).await, "vm from image '{}' did not stop", set);
    }
    Ok(())
}

/// Snapshot a booted VM, clone it (frozen) and show the clones resume with
/// independent counters, their own ids, and almost no private memory.
async fn linux_snapshot_clone() -> TestResult {
    use crate::net::proxy;
    if !linux_available() {
        println!("(skipped: {}) ", linux_skip_reason());
        return Ok(());
    }
    let m = manager();
    let base = m.create_linux("t-base", 128 << 20, None, None).await?;
    check!(base.wait_console("conc_os linux guest", 300_000).await, "linux did not boot (state {}, crash {:?})", base.state(), base.crash_reason());
    let go_server = base.wait_console("webcounter: vm", 30_000).await;
    timer::sleep_ms(500).await;
    let t0 = time::now();
    let snap = m.snapshot_vm(&base, "t-snap").await?;
    let snap_us = time::tsc_to_us(time::now() - t0);
    check!(snap.resume.is_some(), "snapshot has no resume state");
    check!(snap.pages.len() > 1000, "snapshot too small: {} pages", snap.pages.len());
    // The base keeps running on top of the snapshot.
    base.console_input(b"echo still-$((5*5))-alive\n");
    check!(base.wait_console("still-25-alive", 60_000).await, "base vm broke after the snapshot");
    // Two frozen clones.
    let a = m.clone_vm("t-snap", "t-clone-a", true)?;
    let b = m.clone_vm("t-snap", "t-clone-b", true)?;
    timer::sleep_ms(50).await;
    check!(a.state() == VmState::Frozen, "clone should start frozen, is {}", a.state());
    check!(a.stats().resident_pages == 0, "frozen clone has {} resident pages", a.stats().resident_pages);
    // The clone answers on its console: it is a live, independent guest.
    a.console_input(b"echo clone-$((6*7))-here\n");
    let t1 = time::now();
    check!(a.wait_console("clone-42-here", 60_000).await, "clone did not answer on its console (state {}, logs {:?})", a.state(), a.logs().iter().rev().take(5).collect::<Vec<_>>());
    let first_us = time::tsc_to_us(time::now() - t1);
    check!(a.stats().thaws == 1, "clone thaws {}", a.stats().thaws);
    let mut cold_http_us = 0;
    if go_server {
        let lo = crate::net::tcp::loopback_interface("lo-snap");
        proxy::listen_on(lo.clone(), 8081).map_err(|e| format!("proxy listen: {}", e))?;
        let here = crate::net::Ipv4Addr([127, 0, 0, 1]);
        for (v, host) in [(&a, "t-clone-a.conc"), (&b, "t-clone-b.conc")] {
            let t = time::now();
            let r = crate::net::http::get(lo.clone(), here, 8081, host, "/", 120_000).await?;
            if v.id == b.id {
                cold_http_us = time::tsc_to_us(time::now() - t);
            }
            check!(r.status == 200, "{}: status {}", host, r.status);
            let body = r.text();
            check!(body.contains(&format!("vm={} ", v.id)), "{}: wrong vm id in {:?}", host, body.trim());
            check!(body.contains("hits=1 "), "{}: counter not fresh in {:?}", host, body.trim());
        }
        // Counters are independent: a second request to a.
        let r = crate::net::http::get(lo.clone(), here, 8081, "t-clone-a.conc", "/", 60_000).await?;
        check!(r.text().contains("hits=2 "), "second request: {:?}", r.text().trim());
        // The base never served anything: still zero.
        let r = crate::net::http::get(lo.clone(), here, 8081, "t-base.conc", "/hits", 60_000).await?;
        check!(r.text().trim() == "0", "base counter {:?}", r.text().trim());
        crate::net::remove_interface(lo.id);
    }
    let sa = a.stats();
    check!(sa.private_pages < 4000, "clone dirtied {} pages", sa.private_pages);
    check!(sa.text_private_pages < 64, "clone copied {} kernel text pages", sa.text_private_pages);
    print!(
        "(snapshot {} pages in {} ms; first console answer {} ms, cold http {} ms; clone private {} pages, {} in text) ",
        snap.pages.len(),
        snap_us / 1000,
        first_us / 1000,
        cold_http_us / 1000,
        sa.private_pages,
        sa.text_private_pages
    );
    for v in [&base, &a, &b] {
        v.command(Command::Kill);
    }
    for v in [&base, &a, &b] {
        check!(v.wait_finished(10_000).await, "kill did not finish");
    }
    Ok(())
}
