//! Unbounded multi-producer single-consumer async channel.
//!
//! `Sender::send` is synchronous and interrupt-safe, so device interrupt
//! handlers can hand data to tasks directly.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

use crate::sync::SpinLock;

struct Inner<T> {
    queue: VecDeque<T>,
    waker: Option<Waker>,
    senders: usize,
    receiver_alive: bool,
}

pub struct Sender<T> {
    inner: Arc<SpinLock<Inner<T>>>,
}

pub struct Receiver<T> {
    inner: Arc<SpinLock<Inner<T>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendError;

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Arc::new(SpinLock::new(Inner {
        queue: VecDeque::new(),
        waker: None,
        senders: 1,
        receiver_alive: true,
    }));
    (Sender { inner: inner.clone() }, Receiver { inner })
}

impl<T> Sender<T> {
    pub fn send(&self, v: T) -> Result<(), SendError> {
        let waker = {
            let mut g = self.inner.lock();
            if !g.receiver_alive {
                return Err(SendError);
            }
            g.queue.push_back(v);
            g.waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.inner.lock().queue.len()
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.inner.lock().senders += 1;
        Sender { inner: self.inner.clone() }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let waker = {
            let mut g = self.inner.lock();
            g.senders -= 1;
            if g.senders == 0 {
                g.waker.take()
            } else {
                None
            }
        };
        if let Some(w) = waker {
            w.wake();
        }
    }
}

impl<T> Receiver<T> {
    /// Wait for the next value.  Returns `None` once all senders are gone and
    /// the queue is empty.
    pub fn recv(&mut self) -> Recv<'_, T> {
        Recv { rx: self }
    }

    pub fn try_recv(&mut self) -> Option<T> {
        self.inner.lock().queue.pop_front()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().queue.is_empty()
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let mut g = self.inner.lock();
        g.receiver_alive = false;
        g.queue.clear();
    }
}

pub struct Recv<'a, T> {
    rx: &'a mut Receiver<T>,
}

impl<T> Future for Recv<'_, T> {
    type Output = Option<T>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut g = self.rx.inner.lock();
        if let Some(v) = g.queue.pop_front() {
            return Poll::Ready(Some(v));
        }
        if g.senders == 0 {
            return Poll::Ready(None);
        }
        g.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl<T> Unpin for Recv<'_, T> {}
