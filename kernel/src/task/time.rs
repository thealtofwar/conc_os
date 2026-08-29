use alloc::collections::BTreeMap;
use core::{
    pin::Pin,
    sync::atomic::{AtomicU64, Ordering},
    task::{
        Context,
        Poll::{self, Pending, Ready},
        Waker,
    },
};

use crate::{mutex::InterruptMutex, time::now_ms};

/// Sleepers, ordered by the deadline they are waiting for.
///
/// Keyed by `(deadline, id)` rather than by deadline alone so that two timers
/// expiring in the same millisecond can both be held. The ordering is what
/// makes expiry cheap: everything due forms a prefix of the map, so a tick
/// stops at the first entry that is not.
///
/// An [`InterruptMutex`] rather than a plain one, because the timer interrupt
/// takes this lock: a holder in normal context must not be interruptible, or it
/// would spin against itself.
static TIMERS: InterruptMutex<BTreeMap<(u64, u64), Waker>> = InterruptMutex::new(BTreeMap::new());

fn next_id() -> u64 {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Wakes every sleeper whose deadline has passed.
///
/// Called from the timer interrupt and nothing else. Waking happens with the
/// lock still held, which is only safe because these wakers are the executor's:
/// they push a task id onto its queue and touch nothing in this module.
pub(crate) fn expire(now: u64) {
    let mut timers = TIMERS.lock();

    while let Some(entry) = timers.first_entry() {
        if entry.key().0 > now {
            break;
        }
        entry.remove().wake();
    }
}

/// The deadline the next sleeper is waiting for, if any.
///
/// Nothing needs this while the timer is periodic. It is what a one-shot timer
/// would be programmed from, so it is derived here rather than by walking the
/// map from outside.
pub fn next_deadline() -> Option<u64> {
    TIMERS.lock().first_key_value().map(|((at, _), _)| *at)
}

/// Completes once the clock reaches a deadline fixed when the future was made.
pub struct TimeTask {
    deadline_ms: u64,
    id: u64,
}

impl TimeTask {
    /// Completes `duration_ms` milliseconds from now.
    ///
    /// The deadline is taken here rather than at first poll, so a future that
    /// is created and then left unpolled for a while still fires when it was
    /// asked to rather than that long after someone got around to it.
    pub fn new(duration_ms: u64) -> Self {
        Self {
            deadline_ms: now_ms() + duration_ms,
            id: next_id(),
        }
    }

    pub fn deadline_ms(&self) -> u64 {
        self.deadline_ms
    }
}

impl Future for TimeTask {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut timers = TIMERS.lock();

        // Tested while the lock is held: expiry needs the same lock and runs
        // with interrupts disabled, so the deadline cannot pass between this
        // test and the insert below and leave the waker parked forever.
        if now_ms() >= self.deadline_ms {
            timers.remove(&(self.deadline_ms, self.id));
            return Ready(());
        }

        timers.insert((self.deadline_ms, self.id), cx.waker().clone());
        Pending
    }
}

impl Drop for TimeTask {
    /// A future dropped before its deadline must not leave its waker behind:
    /// firing it later would wake a task that is no longer waiting on this.
    fn drop(&mut self) {
        TIMERS.lock().remove(&(self.deadline_ms, self.id));
    }
}
