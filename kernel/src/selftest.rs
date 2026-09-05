//! Built-in self tests, run from the shell (`selftest [filter]`) and by
//! `cargo xtask test`.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::arch::paging::{self, Mapper};
use crate::mm::frame;
use crate::task::{self, channel, timer, Notify};
use crate::time;

pub type TestResult = Result<(), String>;
pub type TestFn = fn() -> Pin<Box<dyn Future<Output = TestResult> + Send>>;

static ANY_FAILED: AtomicBool = AtomicBool::new(false);

pub fn any_failed() -> bool {
    ANY_FAILED.load(Ordering::Relaxed)
}

macro_rules! check {
    ($cond:expr, $($arg:tt)*) => {
        if !($cond) {
            return Err(format!($($arg)*));
        }
    };
}
pub(crate) use check;

macro_rules! tests {
    ($($name:ident),* $(,)?) => {
        &[$((stringify!($name), (|| alloc::boxed::Box::pin($name())) as TestFn)),*]
    };
}
pub(crate) use tests;

fn core_tests() -> &'static [(&'static str, TestFn)] {
    tests![
        heap_small,
        heap_large,
        frames,
        paging_map,
        timer_sleep,
        timer_timeout,
        channel_producer_consumer,
        notify_permit,
        yield_interleave,
    ]
}

async fn heap_small() -> TestResult {
    let mut v: Vec<Box<[u8; 40]>> = Vec::new();
    for i in 0..5000u32 {
        v.push(Box::new([i as u8; 40]));
    }
    for (i, b) in v.iter().enumerate() {
        check!(b[0] == i as u8 && b[39] == i as u8, "heap corruption at {}", i);
    }
    drop(v);
    // Slab classes keep their frames, so run the same pattern twice: the
    // second round must not consume any additional frames.
    let round = || {
        let mut w: Vec<Vec<u8>> = Vec::new();
        for i in 0..2000usize {
            w.push(alloc::vec![i as u8; (i % 3000) + 1]);
        }
        for (i, b) in w.iter().enumerate() {
            if !(b[0] == i as u8 && b.len() == (i % 3000) + 1) {
                return Err(format!("vec corruption at {}", i));
            }
        }
        Ok(())
    };
    round()?;
    let before = frame::stats().free;
    round()?;
    let after = frame::stats().free;
    check!(after == before, "heap not in steady state: {} -> {} free frames", before, after);
    Ok(())
}

async fn heap_large() -> TestResult {
    let before = frame::stats().free;
    {
        let a = alloc::vec![1u8; 3 << 20];
        let b = alloc::vec![2u8; 5 << 20];
        check!(a[a.len() - 1] == 1 && b[b.len() - 1] == 2, "large alloc contents");
        let mut c = Vec::with_capacity(16);
        for i in 0..200_000u32 {
            c.push(i);
        }
        check!(c[199_999] == 199_999, "vec growth");
    }
    let after = frame::stats().free;
    check!(after == before, "large allocations leaked frames: {} -> {}", before, after);
    Ok(())
}

async fn frames() -> TestResult {
    let before = frame::stats().free;
    let mut fs = Vec::new();
    for _ in 0..3000 {
        let f = frame::alloc().ok_or("frame alloc failed")?;
        check!(f % 4096 == 0, "unaligned frame {:#x}", f);
        fs.push(f);
    }
    fs.sort_unstable();
    for w in fs.windows(2) {
        check!(w[0] != w[1], "duplicate frame {:#x}", w[0]);
    }
    for f in fs {
        frame::free(f);
    }
    let c = frame::alloc_contiguous(64, 16).ok_or("contiguous alloc failed")?;
    check!(c % (16 * 4096) == 0, "contiguous alignment {:#x}", c);
    frame::free_contiguous(c, 64);
    let after = frame::stats().free;
    check!(after == before, "frame leak: {} -> {}", before, after);
    Ok(())
}

async fn paging_map() -> TestResult {
    let before = frame::stats().free;
    let mut alloc = || frame::alloc_zeroed().unwrap_or(0);
    let mut m = Mapper::create(&mut alloc).ok_or("mapper create")?;
    let target = frame::alloc_zeroed().ok_or("frame")?;
    check!(m.map_4k(0x1234_5000, target, paging::WRITABLE, &mut alloc), "map_4k");
    check!(m.map_2m(0x4000_0000, 0x1000_0000, paging::WRITABLE | paging::USER, &mut alloc), "map_2m");
    let (pa, fl) = m.translate(0x1234_5678).ok_or("translate 4k")?;
    check!(pa == target + 0x678, "4k translation wrong: {:#x}", pa);
    check!(fl & paging::WRITABLE != 0, "4k flags");
    let (pa2, fl2) = m.translate(0x4012_3456).ok_or("translate 2m")?;
    check!(pa2 == 0x1012_3456, "2m translation wrong: {:#x}", pa2);
    check!(fl2 & paging::HUGE != 0 && fl2 & paging::USER != 0, "2m flags");
    check!(m.translate(0x9999_0000).is_none(), "unmapped address translated");
    let mut leaves = 0;
    m.for_each_leaf(0, 1 << 40, &mut |_va, _e, _lvl| leaves += 1);
    check!(leaves == 2, "expected 2 leaves, got {}", leaves);
    check!(m.unmap_4k(0x1234_5000) == Some(target), "unmap");
    check!(m.translate(0x1234_5000).is_none(), "still mapped after unmap");
    m.free_tables(&mut |pa| frame::free(pa));
    frame::free(target);
    let after = frame::stats().free;
    check!(after == before, "paging leaked frames: {} -> {}", before, after);
    Ok(())
}

async fn timer_sleep() -> TestResult {
    let t0 = time::now();
    timer::sleep_ms(30).await;
    let dt = time::tsc_to_us(time::now() - t0);
    check!(dt >= 30_000, "sleep too short: {} us", dt);
    check!(dt < 400_000, "sleep too long: {} us", dt);
    Ok(())
}

async fn timer_timeout() -> TestResult {
    let n = Notify::new();
    let r = timer::timeout(20, n.notified()).await;
    check!(r.is_err(), "timeout should have elapsed");
    n.notify_one();
    let r = timer::timeout(1000, n.notified()).await;
    check!(r.is_ok(), "permit not delivered");
    Ok(())
}

async fn channel_producer_consumer() -> TestResult {
    let (tx, mut rx) = channel::channel::<u32>();
    let tx2 = tx.clone();
    task::spawn_detached("test-producer", async move {
        for i in 0..1000u32 {
            let _ = tx2.send(i);
            if i % 100 == 0 {
                task::yield_now().await;
            }
        }
    });
    drop(tx);
    let mut expected = 0u32;
    while let Some(v) = rx.recv().await {
        check!(v == expected, "out of order: got {} expected {}", v, expected);
        expected += 1;
    }
    check!(expected == 1000, "received {} of 1000", expected);
    Ok(())
}

async fn notify_permit() -> TestResult {
    let n = alloc::sync::Arc::new(Notify::new());
    let n2 = n.clone();
    let counter = alloc::sync::Arc::new(AtomicUsize::new(0));
    let c2 = counter.clone();
    let h = task::spawn("test-notified", async move {
        n2.notified().await;
        c2.fetch_add(1, Ordering::Relaxed);
        7u32
    });
    timer::sleep_ms(5).await;
    check!(counter.load(Ordering::Relaxed) == 0, "woke without notification");
    n.notify_one();
    let v = timer::timeout(1000, h).await.map_err(|_| "join timed out")?;
    check!(v == 7, "join result");
    Ok(())
}

async fn yield_interleave() -> TestResult {
    let log = alloc::sync::Arc::new(crate::sync::SpinLock::new(Vec::<u8>::new()));
    let mut handles = Vec::new();
    for id in 0..3u8 {
        let l = log.clone();
        handles.push(task::spawn("test-yield", async move {
            for _ in 0..3 {
                l.lock().push(id);
                task::yield_now().await;
            }
        }));
    }
    for h in handles {
        h.await;
    }
    let l = log.lock();
    check!(l.len() == 9, "expected 9 entries, got {}", l.len());
    // Round-robin means the first three entries are three different tasks.
    check!(l[0] != l[1] && l[1] != l[2] && l[0] != l[2], "tasks did not interleave: {:?}", &l[..]);
    Ok(())
}

/// Run all registered tests (optionally filtered by substring).
pub async fn run(filter: Option<&str>) {
    let mut suites: Vec<(&str, &[(&str, TestFn)])> = alloc::vec![("core", core_tests())];
    suites.extend(crate::commands::test_suites());

    let mut passed = 0;
    let mut failed = 0;
    let t_start = time::now();
    for (suite, tests) in suites {
        for (name, f) in tests.iter() {
            let full = format!("{}::{}", suite, name);
            if let Some(flt) = filter {
                if !full.contains(flt) {
                    continue;
                }
            }
            print!("  test {:<40} ", full);
            let t0 = time::now();
            let r = f().await;
            let dt = time::tsc_to_us(time::now() - t0);
            match r {
                Ok(()) => {
                    passed += 1;
                    println!("ok    ({} us)", dt);
                }
                Err(e) => {
                    failed += 1;
                    println!("FAILED: {}", e);
                }
            }
        }
    }
    let total_us = time::tsc_to_us(time::now() - t_start);
    if failed > 0 {
        ANY_FAILED.store(true, Ordering::Relaxed);
    }
    println!("SELFTEST: {} passed, {} failed ({} ms)", passed, failed, total_us / 1000);
}
