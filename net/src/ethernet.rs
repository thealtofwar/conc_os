use core::fmt::{Display, Formatter};

use crate::{
    arp::{ArpError, ArpPacket},
    ipv4::{Ipv4Error, Ipv4Packet},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

#[derive(Debug, PartialEq, Eq)]
pub enum EthernetFrame<'a> {
    Arp(ArpPacket),
    Ipv4(Ipv4Packet<'a>),
    // Ipv6(Ipv6Packet<'a>),
    /// ethertype, pkt
    Unknown(u16, &'a [u8]),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EthernetError {
    Ipv4Err(Ipv4Error),
    ArpError(ArpError),
    TooShort,
}

impl<'a> EthernetFrame<'a> {
    pub fn new(packet: &'a [u8]) -> Result<Self, EthernetError> {
        if packet.len() < 14 {
            return Err(EthernetError::TooShort);
        }
        let ethertype = u16::from_be_bytes([packet[12], packet[13]]);
        match ethertype {
            0x0806 => {
                let arp = &packet[14..];

                Ok(Self::Arp(
                    ArpPacket::from_slice(arp).map_err(EthernetError::ArpError)?,
                ))
            }
            0x0800 => Ok(Self::Ipv4(
                Ipv4Packet::new(&packet[14..]).map_err(EthernetError::Ipv4Err)?,
            )),
            _ => Ok(Self::Unknown(ethertype, &packet)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::arp::ArpOperation;
    use crate::ipv4::IPProtocol;
    use core::net::Ipv4Addr;

    const SRC_MAC: [u8; 6] = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
    const PAYLOAD: [u8; 4] = [0xde, 0xad, 0xbe, 0xef];

    /// Wraps `payload` in an Ethernet frame and returns the raw bytes.
    fn frame(ethertype: u16, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::new();

        frame.extend_from_slice(&MacAddress::broadcast().addr);
        frame.extend_from_slice(&SRC_MAC);
        frame.extend_from_slice(&ethertype.to_be_bytes());
        frame.extend_from_slice(payload);

        frame
    }

    fn arp_request() -> ArpPacket {
        ArpPacket {
            hardware_type: 1,
            protocol_type: 0x0800,
            hardware_len: 6,
            proto_len: 4,
            operation: ArpOperation::Request,
            sender_mac: MacAddress::new(&SRC_MAC),
            sender_addr: Ipv4Addr::new(192, 168, 1, 10),
            target_mac: MacAddress::new(&[0x00; 6]),
            target_addr: Ipv4Addr::new(192, 168, 1, 20),
        }
    }

    fn ipv4_packet(payload: &[u8]) -> Vec<u8> {
        Ipv4Packet {
            version_ihl: 0x45,
            dscp_ecn: 0x00,
            total_length: (20 + payload.len()) as u16,
            id: 0x1234,
            frag_offset: 0,
            dont_fragment: true,
            more_fragments: false,
            ttl: 64,
            protocol: IPProtocol::UDP,
            checksum: 0, // recomputed by serialize
            source: Ipv4Addr::new(192, 168, 1, 1),
            dest: Ipv4Addr::new(10, 0, 0, 1),
            options: &[],
            data: payload,
        }
        .serialize()
    }

    #[test]
    fn test_ethernet_parses_arp() {
        let arp = arp_request();
        let bytes = frame(0x0806, &arp.serialize());

        match EthernetFrame::new(&bytes).expect("failed to parse valid ARP frame") {
            EthernetFrame::Arp(parsed) => assert_eq!(parsed, arp),
            _ => panic!("an 0x0806 frame carries ARP"),
        }
    }

    #[test]
    fn test_ethernet_parses_ipv4() {
        let bytes = frame(0x0800, &ipv4_packet(&PAYLOAD));

        match EthernetFrame::new(&bytes).expect("failed to parse valid IPv4 frame") {
            EthernetFrame::Ipv4(packet) => {
                assert_eq!(packet.protocol, IPProtocol::UDP);
                assert_eq!(packet.source, Ipv4Addr::new(192, 168, 1, 1));
                assert_eq!(packet.data, &PAYLOAD);
            }
            _ => panic!("an 0x0800 frame carries IPv4"),
        }
    }

    #[test]
    fn test_ethernet_unknown_ethertype() {
        let bytes = frame(0x86dd, &PAYLOAD); // IPv6, which this crate does not parse

        match EthernetFrame::new(&bytes).expect("an unhandled ethertype is not an error") {
            EthernetFrame::Unknown(ethertype, _data) => {
                assert_eq!(ethertype, 0x86dd);
            }
            _ => panic!("0x86dd is not a handled ethertype"),
        }
    }

    #[test]
    fn test_ethernet_propagates_arp_error() {
        let mut arp = arp_request().serialize();
        arp[6..8].copy_from_slice(&9u16.to_be_bytes()); // not a defined ARP operation

        let bytes = frame(0x0806, &arp);

        assert_eq!(
            EthernetFrame::new(&bytes).err(),
            Some(EthernetError::ArpError(ArpError::BadOperation))
        );
    }

    #[test]
    fn test_ethernet_propagates_arp_length_error() {
        let bytes = frame(0x0806, &arp_request().serialize()[..20]);

        assert_eq!(
            EthernetFrame::new(&bytes).err(),
            Some(EthernetError::ArpError(ArpError::LengthOutOfRange))
        );
    }

    #[test]
    fn test_ethernet_propagates_ipv4_error() {
        let mut packet = ipv4_packet(&PAYLOAD);
        packet[0] = 0x55; // version 5

        let bytes = frame(0x0800, &packet);

        assert_eq!(
            EthernetFrame::new(&bytes).err(),
            Some(EthernetError::Ipv4Err(Ipv4Error::VersionMismatch))
        );
    }

    #[test]
    fn test_ethernet_propagates_ipv4_length_error() {
        let bytes = frame(0x0800, &ipv4_packet(&PAYLOAD)[..16]);

        assert_eq!(
            EthernetFrame::new(&bytes).err(),
            Some(EthernetError::Ipv4Err(Ipv4Error::LengthOutOfRange))
        );
    }

    #[test]
    fn test_ethernet_rejects_truncated_frame() {
        // A frame shorter than the 14 byte header does not even carry an ethertype.
        // `EthernetError` has no variant for this yet, so only the fact that it is
        // reported rather than indexed past the end is asserted here.
        let bytes = frame(0x0800, &ipv4_packet(&PAYLOAD));

        for len in [0, 12, 13] {
            assert_eq!(
                EthernetFrame::new(&bytes[..len]),
                Err(EthernetError::TooShort),
                "a {len} byte frame is shorter than the Ethernet header"
            );
        }
    }
}
