use core::fmt::{Display, Formatter};

use crate::{arp::ArpPacket, ipv4::Ipv4Packet};

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

    pub fn broadcast() -> Self {
        MacAddress {
            addr: [255, 255, 255, 255, 255, 255],
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
    IPV4 = 0x0800,
    ARP = 0x0806,
}

pub enum EthernetFrame<'a> {
    Arp(ArpPacket),
    Ipv4(Ipv4Packet<'a>),
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

                Ok(Self::Arp(ArpPacket::from_slice(arp)?))
            }
            0x0800 => Ok(Self::Ipv4(Ipv4Packet::new(&packet[14..])?)),
            _ => Ok(Self::Unknown(ethertype, packet)),
        }
    }
}
