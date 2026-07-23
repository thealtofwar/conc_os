use core::{
    pin::Pin,
    task::{Context, Poll},
};

use conquer_once::spin::OnceCell;
use crossbeam_queue::ArrayQueue;
use futures_util::{Stream, task::AtomicWaker};

use crate::println;

static SERIAL_QUEUE: OnceCell<ArrayQueue<u8>> = OnceCell::uninit();
static WAKER: AtomicWaker = AtomicWaker::new();

pub(crate) fn add_byte(byte: u8) {
    if let Ok(queue) = SERIAL_QUEUE.try_get() {
        if queue.push(byte).is_err() {
            println!("WARNING: serial queue full; dropping serial input");
        } else {
            WAKER.wake();
        }
    } else {
        println!("WARNING: serial queue uninitialized");
    }
}

pub struct SerialStream {
    _private: (),
}

impl Default for SerialStream {
    fn default() -> Self {
        Self::new()
    }
}

impl SerialStream {
    pub fn new() -> Self {
        SERIAL_QUEUE
            .try_init_once(|| ArrayQueue::new(100))
            .expect("ScancodeStream::new should only be called once");
        SerialStream { _private: () }
    }
}

impl Stream for SerialStream {
    type Item = u8;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<u8>> {
        let queue = SERIAL_QUEUE.try_get().expect("not initialized");

        if let Some(byte) = queue.pop() {
            // data available
            return Poll::Ready(Some(byte));
        }

        WAKER.register(cx.waker());

        match queue.pop() {
            Some(byte) => {
                WAKER.take();
                Poll::Ready(Some(byte))
            }
            None => Poll::Pending,
        }
    }
}
