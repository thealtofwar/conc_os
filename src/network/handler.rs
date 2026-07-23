use crate::{println, utils::FromSlice};
use alloc::collections::BTreeMap;
use core::{
    fmt::{Display, Formatter},
    net::Ipv4Addr,
};

struct MacAddress([u8; 6]);

impl MacAddress {
    pub fn new(slice: &[u8]) -> Self {
        MacAddress(*slice.as_array().expect("invalid length"))
    }
}

impl Display for MacAddress {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

pub struct NetworkInterface {
    mac: MacAddress,
    ipv4: Option<Ipv4Addr>,
}

pub struct ArpEntry {
    pub mac: MacAddress,
}

pub struct ArpCache {
    entries: BTreeMap<Ipv4Addr, ArpEntry>,
}

pub struct ArpPacket {
    hardware_type: u16,
    protocol_type: u16,
    hardware_len: u8,
    proto_len: u8,
    operation: u16,
    sender_mac: MacAddress,
    sender_addr: Ipv4Addr,
    target_mac: MacAddress,
    target_addr: Ipv4Addr,
}

pub enum EthernetFrame<'a> {
    Arp(ArpPacket),
    // Ipv4(Ipv4Packet<'a>),
    // Ipv6(Ipv6Packet<'a>),
    /// len, pkt
    Unknown(u16, &'a [u8]),
}

impl<'a> EthernetFrame<'a> {
    pub fn new(packet: &'a [u8]) -> Self {
        let ethertype = u16::from_be_bytes([packet[12], packet[13]]);
        match ethertype {
            0x0806 => {
                let arp = &packet[14..];

                Self::Arp(ArpPacket {
                    hardware_type: u16::from_be_slice(&arp[0..2]),
                    protocol_type: u16::from_be_slice(&arp[2..4]),
                    hardware_len: u8::from_be_slice(&arp[4..5]),
                    proto_len: u8::from_be_slice(&arp[5..6]),
                    operation: u16::from_be_slice(&arp[6..8]),
                    sender_mac: MacAddress::new(&arp[8..14]),
                    sender_addr: Ipv4Addr::from_octets(
                        *(arp[14..18].as_array().expect("invalid length")),
                    ),
                    target_mac: MacAddress::new(&arp[18..24]),
                    target_addr: Ipv4Addr::from_octets(
                        *(arp[24..28].as_array().expect("invalid length")),
                    ),
                })
            }
            _ => Self::Unknown(packet.len() as u16, packet),
        }
    }
}

pub fn handle_packet(frame: &EthernetFrame) {
    match frame {
        EthernetFrame::Arp(arp_packet) => {
            println!(
                "ARP op={} sender={} {} target={} {}",
                arp_packet.operation,
                arp_packet.sender_mac,
                arp_packet.sender_addr,
                arp_packet.target_mac,
                arp_packet.target_addr
            );
        }
        EthernetFrame::Unknown(len, items) => {
            println!("got unknown ethernet frame with len {len}")
        }
    }
}
