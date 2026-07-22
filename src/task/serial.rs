use conquer_once::spin::OnceCell;
use crossbeam_queue::ArrayQueue;

use crate::println;

static SERIAL_QUEUE: OnceCell<ArrayQueue<u8>> = OnceCell::uninit();

pub(crate) fn add_byte(scancode: u8) {
    if let Ok(queue) = SERIAL_QUEUE.try_get() {
        if queue.push(scancode).is_err() {
            println!("WARNING: serial queue full; dropping serial input");
        }
    } else {
        println!("WARNING: serial queue uninitialized");
    }
}
