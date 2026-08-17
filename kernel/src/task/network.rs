use core::{
    pin::Pin,
    task::{Context, Poll},
};

use conc_os_net::ethernet::{EthernetFrame, MacAddress};
use conquer_once::spin::OnceCell;
use crossbeam_queue::ArrayQueue;
use futures_util::{Stream, StreamExt, task::AtomicWaker};

use crate::{
    get_net_driver,
    network::handler::{get_network_interface, init_network_interface},
    println,
};

static NET_EVENTS: OnceCell<ArrayQueue<NetworkEvent>> = OnceCell::uninit();
static NET_WAKER: AtomicWaker = AtomicWaker::new();

#[derive(Copy, Clone, Debug)]
pub enum NetworkEvent {
    Queue,
    ConfigChange,
}

pub(crate) fn add_event(evt: NetworkEvent) {
    if let Ok(queue) = NET_EVENTS.try_get() {
        if queue.push(evt).is_err() {
            println!("WARNING: serial queue full; dropping serial input");
        } else {
            NET_WAKER.wake();
        }
    } else {
        println!("WARNING: serial queue uninitialized");
    }
}

pub struct NetworkStream {
    _private: (),
}

impl Default for NetworkStream {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkStream {
    pub fn new() -> Self {
        NET_EVENTS
            .try_init_once(|| ArrayQueue::new(100))
            .expect("ScancodeStream::new should only be called once");
        NetworkStream { _private: () }
    }
}

impl Stream for NetworkStream {
    type Item = NetworkEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let queue = NET_EVENTS.try_get().expect("not initialized");

        if let Some(evt) = queue.pop() {
            // data available
            return Poll::Ready(Some(evt));
        }

        NET_WAKER.register(cx.waker());

        match queue.pop() {
            Some(evt) => {
                NET_WAKER.take();
                Poll::Ready(Some(evt))
            }
            None => Poll::Pending,
        }
    }
}

fn process_rx(packet: &[u8]) {
    if packet.len() < 14 {
        println!("Short Ethernet frame ({})", packet.len());
        return;
    }

    let dst = MacAddress::new(&packet[0..6]);
    let src = MacAddress::new(&packet[6..12]);
    let ethertype = u16::from_be_bytes([packet[12], packet[13]]);

    if ethertype == 0x0806
        || (ethertype == 0x0800 && { dst.addr == get_net_driver().lock().mac_address() })
    {
        println!(
            "RX {} bytes dst={} src={} type=0x{:04x}",
            packet.len(),
            dst,
            src,
            ethertype,
        );
    }

    if let Ok(pkt) = EthernetFrame::new(packet) {
        get_network_interface().lock().handle_packet(&pkt, dst);
    } else {
        println!("rejected malformed packet")
    }
}

async fn handle_queue_interrupt() {
    loop {
        let packet = {
            let mut driver = get_net_driver().lock();

            match driver.receive() {
                Ok(packet) => packet,
                Err(_) => break,
            }
        };

        process_rx(packet.packet());

        {
            let mut driver = get_net_driver().lock();
            driver
                .recycle_rx_buffer(packet)
                .expect("rx buffer recycled");
        }
    }
}

pub async fn network_task() {
    let mut stream = NetworkStream::new();
    init_network_interface();

    loop {
        while let Some(event) = stream.next().await {
            match event {
                NetworkEvent::Queue => {
                    handle_queue_interrupt().await;
                }

                NetworkEvent::ConfigChange => {
                    println!("got a config change");
                }
            }
        }
    }
}
