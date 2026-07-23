use core::{
    pin::Pin,
    task::{Context, Poll},
};

use conquer_once::spin::OnceCell;
use crossbeam_queue::ArrayQueue;
use futures_util::{Stream, StreamExt, task::AtomicWaker};
use virtio_drivers::device::net::TxBuffer;

use crate::{
    get_net_driver,
    network::handler::{EthernetFrame, get_network_interface, init_network_interface},
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

    let dst = &packet[0..6];
    let src = &packet[6..12];
    let ethertype = u16::from_be_bytes([packet[12], packet[13]]);

    println!(
        "RX {} bytes dst={:02x?} src={:02x?} type=0x{:04x}",
        packet.len(),
        dst,
        src,
        ethertype,
    );

    if let Ok(pkt) = EthernetFrame::new(packet) {
        get_network_interface().lock().handle_packet(&pkt);
    }
}

async fn handle_queue_interrupt() {
    let mut driver = get_net_driver().lock();

    while let Ok(packet) = driver.receive() {
        process_rx(packet.packet());
        driver
            .recycle_rx_buffer(packet)
            .expect("rx buffer recycled");
    }
}

pub async fn network_task() {
    let mut stream = NetworkStream::new();
    init_network_interface();
    {
        let mut driver = get_net_driver().lock();

        let mac = driver.mac_address();

        let mut frame = [0u8; 60];

        // ---------------- Ethernet ----------------

        // Destination = broadcast
        frame[0..6].copy_from_slice(&[0xff; 6]);

        // Source = our MAC
        frame[6..12].copy_from_slice(&mac);

        // EtherType = ARP
        frame[12..14].copy_from_slice(&0x0806u16.to_be_bytes());

        // ---------------- ARP ----------------

        // Hardware type = Ethernet
        frame[14..16].copy_from_slice(&1u16.to_be_bytes());

        // Protocol type = IPv4
        frame[16..18].copy_from_slice(&0x0800u16.to_be_bytes());

        // Hardware address length
        frame[18] = 6;

        // Protocol address length
        frame[19] = 4;

        // Opcode = Request
        frame[20..22].copy_from_slice(&1u16.to_be_bytes());

        // Sender MAC
        frame[22..28].copy_from_slice(&mac);

        // Sender IP = 10.0.2.15
        frame[28..32].copy_from_slice(&[10, 0, 2, 15]);

        // Target MAC = unknown
        frame[32..38].fill(0);

        // Target IP = gateway
        frame[38..42].copy_from_slice(&[10, 0, 2, 2]);

        // Remaining bytes stay zero (Ethernet padding)

        driver.send(TxBuffer::from(&frame)).unwrap();
    }

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
