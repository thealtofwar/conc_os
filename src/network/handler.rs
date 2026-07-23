use crate::network::arp::{ArpCache, ArpOperation, ArpPacket};
use crate::{network::device::get_net_driver, println, utils::FromSlice};
use alloc::vec;
use alloc::{collections::BTreeMap, vec::Vec};
use core::{
    fmt::{Display, Formatter},
    net::Ipv4Addr,
};
use spin::{Mutex, Once};
use virtio_drivers::device::net::TxBuffer;

static NETWORK_INTERFACE: Once<Mutex<NetworkInterface>> = Once::new();

pub fn get_network_interface() -> &'static Mutex<NetworkInterface> {
    NETWORK_INTERFACE
        .r#try()
        .expect("network interface must be initialized")
}

pub fn init_network_interface() {
    NETWORK_INTERFACE.call_once(|| {
        Mutex::new(NetworkInterface {
            mac: MacAddress {
                addr: get_net_driver().lock().mac_address(),
            },
            ipv4: Some(Ipv4Addr::from_octets([10, 0, 2, 15])),
            arp: ArpCache::new(),
        })
    });
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MacAddress {
    pub addr: [u8; 6],
}

impl MacAddress {
    pub fn new(slice: &[u8]) -> Self {
        MacAddress {
            addr: *slice.as_array().expect("invalid length"),
        }
    }
}

impl Display for MacAddress {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.addr[0], self.addr[1], self.addr[2], self.addr[3], self.addr[4], self.addr[5]
        )
    }
}

#[repr(u16)]
pub enum EtherType {
    ARP = 0x0806,
}

pub struct NetworkInterface {
    mac: MacAddress,
    ipv4: Option<Ipv4Addr>,
    arp: ArpCache,
}

impl NetworkInterface {
    pub fn handle_packet(&mut self, frame: &EthernetFrame) {
        match frame {
            EthernetFrame::Arp(arp_packet) => {
                if arp_packet.sender_mac != self.mac {
                    self.arp
                        .insert(arp_packet.sender_addr, arp_packet.sender_mac);
                }

                if arp_packet.operation == ArpOperation::Request
                    && self.ipv4.is_some_and(|addr| arp_packet.target_addr == addr)
                {
                    let reply = ArpPacket {
                        hardware_type: 1,
                        protocol_type: 0x0800,
                        hardware_len: 6,
                        proto_len: 4,

                        operation: ArpOperation::Reply,

                        sender_mac: self.mac,
                        sender_addr: self.ipv4.unwrap(),

                        target_mac: arp_packet.sender_mac,
                        target_addr: arp_packet.sender_addr,
                    };

                    self.send_frame(arp_packet.sender_mac, EtherType::ARP, &reply.serialize());
                }
                println!(
                    "ARP op={} sender={} {} target={} {}",
                    arp_packet.operation as u16,
                    arp_packet.sender_mac,
                    arp_packet.sender_addr,
                    arp_packet.target_mac,
                    arp_packet.target_addr
                );
            }
            EthernetFrame::Unknown(ethertype, items) => {
                println!(
                    "got unknown ethernet frame with ethertype {ethertype} and len {}",
                    items.len()
                );
            }
        }
    }

    pub fn send_frame(&self, dst: MacAddress, ethertype: EtherType, payload: &[u8]) {
        let mut driver = get_net_driver().lock();

        let mut frame = vec![0; payload.len() + 14];

        // ---------------- Ethernet ----------------

        // Destination = broadcast
        frame[0..6].copy_from_slice(&dst.addr);

        // Source = our MAC
        frame[6..12].copy_from_slice(&self.mac.addr);

        // EtherType = ARP
        frame[12..14].copy_from_slice(&(ethertype as u16).to_be_bytes());

        frame[14..].copy_from_slice(payload);

        frame.resize(frame.len().max(60), 0);

        driver.send(TxBuffer::from(&frame)).unwrap();
    }
}

pub enum EthernetFrame<'a> {
    Arp(ArpPacket),
    // Ipv4(Ipv4Packet<'a>),
    // Ipv6(Ipv6Packet<'a>),
    /// ethertype, pkt
    Unknown(u16, &'a [u8]),
}

impl<'a> EthernetFrame<'a> {
    pub fn new(packet: &'a [u8]) -> Result<Self, ()> {
        let ethertype = u16::from_be_bytes([packet[12], packet[13]]);
        match ethertype {
            0x0806 => {
                let arp = &packet[14..];

                let operation = ArpOperation::try_from(u16::from_be_slice(&arp[6..8]))?;

                let hardware_type = u16::from_be_slice(&arp[0..2]);
                let protocol_type = u16::from_be_slice(&arp[2..4]);
                let hardware_len = u8::from_be_slice(&arp[4..5]);
                let proto_len = u8::from_be_slice(&arp[5..6]);

                if hardware_type != 1
                    || protocol_type != 0x0800
                    || hardware_len != 6
                    || proto_len != 4
                {
                    // reject malformed packets
                    return Err(());
                }

                Ok(Self::Arp(ArpPacket {
                    hardware_type: hardware_type,
                    protocol_type: protocol_type,
                    hardware_len: hardware_len,
                    proto_len: proto_len,
                    operation: operation,
                    sender_mac: MacAddress::new(&arp[8..14]),
                    sender_addr: Ipv4Addr::from_octets(
                        *(arp[14..18].as_array().expect("invalid length")),
                    ),
                    target_mac: MacAddress::new(&arp[18..24]),
                    target_addr: Ipv4Addr::from_octets(
                        *(arp[24..28].as_array().expect("invalid length")),
                    ),
                }))
            }
            _ => Ok(Self::Unknown(ethertype, packet)),
        }
    }
}
