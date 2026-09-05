//! Cooperative async executor.
//!
//! Every long-lived activity in conc_os — the shell, the network stack, disk
//! I/O, and every virtual CPU — is an async task.  A task that is waiting on
//! something (a packet, a timer, a request for its VM) is simply not polled,
//! so it costs nothing.  Wakers may be invoked from interrupt handlers; the
//! ready queue is protected by an interrupt-safe spinlock and the executor
//! idles with `sti; hlt` so no wakeup is ever lost.

#![allow(dead_code)]

pub mod channel;
pub mod timer;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::task::Wake;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use core::task::{Context, Poll, Waker};

use crate::arch::cpu;
use crate::sync::SpinLock;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct TaskId(pub u64);

struct Task {
    id: TaskId,
    name: &'static str,
    future: Pin<Box<dyn Future<Output = ()> + Send>>,
    /// Set when the task has been pushed on the ready queue and not yet
    /// polled; prevents duplicate queue entries.
    queued: Arc<AtomicBool>,
    polls: u64,
}

struct TaskWaker {
    id: TaskId,
    queued: Arc<AtomicBool>,
}

impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        if !self.queued.swap(true, Ordering::AcqRel) {
            READY.lock().push_back(self.id);
        }
    }
}

static READY: SpinLock<VecDeque<TaskId>> = SpinLock::new(VecDeque::new());
static TASKS: SpinLock<BTreeMap<TaskId, Task>> = SpinLock::new(BTreeMap::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static POLLS: AtomicU64 = AtomicU64::new(0);
static IDLE_ENTERS: AtomicU64 = AtomicU64::new(0);
static TASKS_SPAWNED: AtomicUsize = AtomicUsize::new(0);
static TASKS_FINISHED: AtomicUsize = AtomicUsize::new(0);

/// Handle to await a spawned task's result.
pub struct JoinHandle<T> {
    inner: Arc<SpinLock<JoinInner<T>>>,
}

struct JoinInner<T> {
    result: Option<T>,
    waker: Option<Waker>,
}

impl<T> Future for JoinHandle<T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut g = self.inner.lock();
        if let Some(v) = g.result.take() {
            Poll::Ready(v)
        } else {
            g.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl<T> JoinHandle<T> {
    pub fn is_finished(&self) -> bool {
        self.inner.lock().result.is_some()
    }
}

/// Spawn a task.  Safe to call from anywhere, including other tasks.
pub fn spawn<T, F>(name: &'static str, fut: F) -> JoinHandle<T>
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    let inner = Arc::new(SpinLock::new(JoinInner { result: None, waker: None }));
    let inner2 = inner.clone();
    let wrapped = async move {
        let v = fut.await;
        let waker = {
            let mut g = inner2.lock();
            g.result = Some(v);
            g.waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
    };
    let id = TaskId(NEXT_ID.fetch_add(1, Ordering::Relaxed));
    let queued = Arc::new(AtomicBool::new(true));
    let task = Task { id, name, future: Box::pin(wrapped), queued, polls: 0 };
    TASKS.lock().insert(id, task);
    READY.lock().push_back(id);
    TASKS_SPAWNED.fetch_add(1, Ordering::Relaxed);
    JoinHandle { inner }
}

/// Spawn a task whose result nobody cares about.
pub fn spawn_detached<F>(name: &'static str, fut: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let _ = spawn(name, fut);
}

/// Poll one ready task.  Returns false if the ready queue was empty.
fn poll_one() -> bool {
    let id = match READY.lock().pop_front() {
        Some(id) => id,
        None => return false,
    };
    let mut task = match TASKS.lock().remove(&id) {
        Some(t) => t,
        None => return true, // finished meanwhile
    };
    task.queued.store(false, Ordering::Release);
    let waker = Waker::from(Arc::new(TaskWaker { id, queued: task.queued.clone() }));
    let mut cx = Context::from_waker(&waker);
    task.polls += 1;
    POLLS.fetch_add(1, Ordering::Relaxed);
    match task.future.as_mut().poll(&mut cx) {
        Poll::Ready(()) => {
            TASKS_FINISHED.fetch_add(1, Ordering::Relaxed);
        }
        Poll::Pending => {
            TASKS.lock().insert(id, task);
        }
    }
    true
}

/// Run the executor forever.
pub fn run() -> ! {
    loop {
        while poll_one() {}
        // Idle: re-check the queue with interrupts off, then sleep atomically.
        cpu::cli();
        if READY.lock().is_empty() {
            IDLE_ENTERS.fetch_add(1, Ordering::Relaxed);
            cpu::sti_hlt();
        } else {
            cpu::sti();
        }
    }
}

/// Run ready tasks until the queue drains, without sleeping.  Used by code
/// that needs to make progress on other tasks from a synchronous context.
pub fn run_until_idle() {
    while poll_one() {}
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub live: usize,
    pub ready: usize,
    pub spawned: usize,
    pub finished: usize,
    pub polls: u64,
    pub idle_enters: u64,
}

pub fn stats() -> Stats {
    Stats {
        live: TASKS.lock().len(),
        ready: READY.lock().len(),
        spawned: TASKS_SPAWNED.load(Ordering::Relaxed),
        finished: TASKS_FINISHED.load(Ordering::Relaxed),
        polls: POLLS.load(Ordering::Relaxed),
        idle_enters: IDLE_ENTERS.load(Ordering::Relaxed),
    }
}

/// Snapshot of live tasks: (id, name, polls).
pub fn list() -> alloc::vec::Vec<(u64, &'static str, u64)> {
    TASKS.lock().values().map(|t| (t.id.0, t.name, t.polls)).collect()
}

/// Yield to other ready tasks once.
pub fn yield_now() -> YieldNow {
    YieldNow { yielded: false }
}

pub struct YieldNow {
    yielded: bool,
}

impl Future for YieldNow {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.yielded {
            Poll::Ready(())
        } else {
            self.yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

/// A one-shot / level-triggered notification, similar to tokio's `Notify`.
/// `notify_one` stores a permit if nobody is waiting, so a notification sent
/// before `notified().await` is not lost.
pub struct Notify {
    inner: SpinLock<NotifyInner>,
}

struct NotifyInner {
    permit: bool,
    waker: Option<Waker>,
}

impl Notify {
    pub const fn new() -> Self {
        Notify { inner: SpinLock::new(NotifyInner { permit: false, waker: None }) }
    }

    /// Wake the waiter (or store a permit).  Safe from interrupt context.
    pub fn notify_one(&self) {
        let waker = {
            let mut g = self.inner.lock();
            g.permit = true;
            g.waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
    }

    pub fn notified(&self) -> Notified<'_> {
        Notified { notify: self }
    }

    /// Consume a pending permit without waiting.
    pub fn try_take(&self) -> bool {
        let mut g = self.inner.lock();
        core::mem::replace(&mut g.permit, false)
    }
}

impl Default for Notify {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Notified<'a> {
    notify: &'a Notify,
}

impl Future for Notified<'_> {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut g = self.notify.inner.lock();
        if g.permit {
            g.permit = false;
            g.waker = None;
            Poll::Ready(())
        } else {
            g.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

/// A broadcast wait queue: any number of tasks can wait; `wake_all` releases
/// every one of them.  Waiters must re-check their condition (spurious
/// wakeups are allowed).
pub struct WaitQueue {
    inner: SpinLock<WaitQueueInner>,
}

struct WaitQueueInner {
    wakers: alloc::vec::Vec<Waker>,
    /// Incremented by every `wake_all`, so a wait created before a wakeup
    /// (but polled after it) still completes: no lost wakeups.
    epoch: u64,
}

impl WaitQueue {
    pub const fn new() -> Self {
        WaitQueue { inner: SpinLock::new(WaitQueueInner { wakers: alloc::vec::Vec::new(), epoch: 0 }) }
    }

    pub fn wake_all(&self) {
        let ws: alloc::vec::Vec<Waker> = {
            let mut g = self.inner.lock();
            g.epoch = g.epoch.wrapping_add(1);
            core::mem::take(&mut g.wakers)
        };
        for w in ws {
            w.wake();
        }
    }

    /// Future that completes at the first `wake_all` after it was created
    /// (create it *before* re-checking the condition you are waiting for).
    pub fn wait(&self) -> WaitFuture<'_> {
        let epoch = self.inner.lock().epoch;
        WaitFuture { q: self, epoch, registered: false }
    }

    pub fn waiters(&self) -> usize {
        self.inner.lock().wakers.len()
    }
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}

pub struct WaitFuture<'a> {
    q: &'a WaitQueue,
    epoch: u64,
    registered: bool,
}

impl Future for WaitFuture<'_> {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut g = self.q.inner.lock();
        if g.epoch != self.epoch {
            return Poll::Ready(());
        }
        if !self.registered {
            g.wakers.push(cx.waker().clone());
            drop(g);
            self.registered = true;
        }
        Poll::Pending
    }
}

/// Drive several futures to completion concurrently, collecting results in
/// input order.
pub struct JoinAll<F: Future + Unpin> {
    futs: alloc::vec::Vec<Option<F>>,
    results: alloc::vec::Vec<Option<F::Output>>,
    remaining: usize,
}

pub fn join_all<F: Future + Unpin>(futs: alloc::vec::Vec<F>) -> JoinAll<F> {
    let n = futs.len();
    let mut results = alloc::vec::Vec::with_capacity(n);
    results.resize_with(n, || None);
    JoinAll { futs: futs.into_iter().map(Some).collect(), results, remaining: n }
}

impl<F: Future + Unpin> Unpin for JoinAll<F> {}

impl<F: Future + Unpin> Future for JoinAll<F> {
    type Output = alloc::vec::Vec<F::Output>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        for i in 0..this.futs.len() {
            if let Some(f) = this.futs[i].as_mut() {
                if let Poll::Ready(v) = Pin::new(f).poll(cx) {
                    this.results[i] = Some(v);
                    this.futs[i] = None;
                    this.remaining -= 1;
                }
            }
        }
        if this.remaining == 0 {
            Poll::Ready(this.results.iter_mut().map(|r| r.take().expect("join_all result")).collect())
        } else {
            Poll::Pending
        }
    }
}

/// Poll a future once with a no-op waker; handy for non-blocking checks.
pub fn poll_once<F: Future>(fut: Pin<&mut F>) -> Poll<F::Output> {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    fut.poll(&mut cx)
}
