//! Async timers driven by the periodic APIC tick.
//!
//! Deadlines are TSC timestamps kept in an ordered map; the tick hook (in
//! interrupt context) wakes everything that has expired.  This is O(log n)
//! per operation, which keeps thousands of idle VMs with idle-timeouts cheap.

use alloc::collections::BTreeMap;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU64, Ordering};
use core::task::{Context, Poll, Waker};

use crate::sync::SpinLock;
use crate::time;

static TIMERS: SpinLock<BTreeMap<(u64, u64), Waker>> = SpinLock::new(BTreeMap::new());
static SEQ: AtomicU64 = AtomicU64::new(0);
static FIRED: AtomicU64 = AtomicU64::new(0);

/// Called from the timer interrupt.
pub fn on_tick() {
    let now = time::now();
    // Collect expired wakers under the lock, wake them outside it.
    let mut expired: [Option<Waker>; 32] = Default::default();
    loop {
        let mut n = 0;
        {
            let mut t = TIMERS.lock();
            while n < expired.len() {
                match t.first_key_value() {
                    Some((&(deadline, _), _)) if deadline <= now => {
                        let (_, w) = t.pop_first().unwrap();
                        expired[n] = Some(w);
                        n += 1;
                    }
                    _ => break,
                }
            }
        }
        if n == 0 {
            break;
        }
        for w in expired.iter_mut().take(n) {
            if let Some(w) = w.take() {
                w.wake();
            }
        }
        FIRED.fetch_add(n as u64, Ordering::Relaxed);
        if n < expired.len() {
            break;
        }
    }
    // Arm the one-shot for the next deadline (the tick handler caps the gap).
    let next = TIMERS.lock().first_key_value().map(|(&(d, _), _)| d);
    if let Some(d) = next {
        time::arm_timer_at(d);
    }
}

pub fn install() {
    time::set_tick_hook(on_tick);
}

pub fn pending() -> usize {
    TIMERS.lock().len()
}

pub fn fired() -> u64 {
    FIRED.load(Ordering::Relaxed)
}

/// Future that completes at a TSC deadline.
pub struct Sleep {
    deadline: u64,
    key: Option<(u64, u64)>,
}

impl Sleep {
    pub fn until(deadline: u64) -> Self {
        Sleep { deadline, key: None }
    }
    pub fn deadline(&self) -> u64 {
        self.deadline
    }
}

impl Future for Sleep {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if time::now() >= self.deadline {
            if let Some(k) = self.key.take() {
                TIMERS.lock().remove(&k);
            }
            return Poll::Ready(());
        }
        let mut t = TIMERS.lock();
        match self.key {
            Some(k) => {
                // Refresh the waker in case we were moved to another task.
                if let Some(w) = t.get_mut(&k) {
                    w.clone_from(cx.waker());
                }
            }
            None => {
                let k = (self.deadline, SEQ.fetch_add(1, Ordering::Relaxed));
                t.insert(k, cx.waker().clone());
                drop(t);
                self.key = Some(k);
                // Tickless: make sure the interrupt comes by then.
                time::arm_timer_at(self.deadline);
            }
        }
        Poll::Pending
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        if let Some(k) = self.key.take() {
            TIMERS.lock().remove(&k);
        }
    }
}

pub fn sleep_ms(ms: u64) -> Sleep {
    Sleep::until(time::now() + time::us_to_tsc(ms * 1000))
}

pub fn sleep_us(us: u64) -> Sleep {
    Sleep::until(time::now() + time::us_to_tsc(us))
}

/// Error returned by [`timeout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;

pub struct Timeout<F> {
    fut: F,
    sleep: Sleep,
}

impl<F: Future + Unpin> Future for Timeout<F> {
    type Output = Result<F::Output, Elapsed>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        if let Poll::Ready(v) = Pin::new(&mut this.fut).poll(cx) {
            return Poll::Ready(Ok(v));
        }
        match Pin::new(&mut this.sleep).poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(Elapsed)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Race `fut` against a deadline `ms` milliseconds away.
pub fn timeout<F: Future + Unpin>(ms: u64, fut: F) -> Timeout<F> {
    Timeout { fut, sleep: sleep_ms(ms) }
}

/// Like [`timeout`] but takes the deadline in microseconds.
pub fn timeout_us<F: Future + Unpin>(us: u64, fut: F) -> Timeout<F> {
    Timeout { fut, sleep: sleep_us(us) }
}

/// Race `fut` against an absolute TSC deadline.
pub fn timeout_until<F: Future + Unpin>(deadline_tsc: u64, fut: F) -> Timeout<F> {
    Timeout { fut, sleep: Sleep::until(deadline_tsc) }
}
