extern crate alloc;

use core::net::Ipv4Addr;

use alloc::collections::btree_map::BTreeMap;

use crate::ethernet::MacAddress;
use crate::utils::FromSlice;

pub struct ArpEntry {
    pub mac: MacAddress,
}

pub struct ArpCache {
    entries: BTreeMap<Ipv4Addr, ArpEntry>,
}

impl Default for ArpCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ArpCache {
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    pub fn lookup(&self, ip: Ipv4Addr) -> Option<MacAddress> {
        self.entries.get(&ip).map(|entry| entry.mac)
    }

    pub fn insert(&mut self, ip: Ipv4Addr, mac: MacAddress) {
        self.entries.insert(ip, ArpEntry { mac });
    }

    pub fn remove(&mut self, ip: Ipv4Addr) {
        self.entries.remove(&ip);
    }

    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        self.entries.contains_key(&ip)
    }
}

#[repr(u16)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArpOperation {
    Request = 1,
    Reply = 2,
}

impl TryFrom<u16> for ArpOperation {
    type Error = (); // error means the arp operation was not 1 or 2

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(ArpOperation::Request),
            2 => Ok(ArpOperation::Reply),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArpPacket {
    pub hardware_type: u16,
    pub protocol_type: u16,
    pub hardware_len: u8,
    pub proto_len: u8,
    pub operation: ArpOperation,
    pub sender_mac: MacAddress,
    pub sender_addr: Ipv4Addr,
    pub target_mac: MacAddress,
    pub target_addr: Ipv4Addr,
}

impl ArpPacket {
    pub fn serialize(&self) -> [u8; 28] {
        let mut pkt = [0u8; 28];
        pkt[0..2].copy_from_slice(&self.hardware_type.to_be_bytes());
        pkt[2..4].copy_from_slice(&self.protocol_type.to_be_bytes());
        pkt[4..5].copy_from_slice(&self.hardware_len.to_be_bytes());
        pkt[5..6].copy_from_slice(&self.proto_len.to_be_bytes());
        pkt[6..8].copy_from_slice(&(self.operation as u16).to_be_bytes());
        pkt[8..14].copy_from_slice(&self.sender_mac.addr);
        pkt[14..18].copy_from_slice(&self.sender_addr.octets());
        pkt[18..24].copy_from_slice(&self.target_mac.addr);
        pkt[24..28].copy_from_slice(&self.target_addr.octets());

        pkt
    }

    pub fn from_slice(packet_data: &[u8]) -> Result<Self, ()> {
        if packet_data.len() < 28 {
            return Err(());
        }

        let hardware_type = u16::from_be_slice(&packet_data[0..2]);
        let protocol_type = u16::from_be_slice(&packet_data[2..4]);
        let hardware_len = u8::from_be_slice(&packet_data[4..5]);
        let proto_len = u8::from_be_slice(&packet_data[5..6]);

        let operation = ArpOperation::try_from(u16::from_be_slice(&packet_data[6..8]))?;

        if hardware_type != 1 || protocol_type != 0x0800 || hardware_len != 6 || proto_len != 4 {
            // reject malformed packets
            return Err(());
        }
        Ok(ArpPacket {
            hardware_type,
            protocol_type,
            hardware_len,
            proto_len,
            operation,
            sender_mac: MacAddress::new(&packet_data[8..14]),
            sender_addr: Ipv4Addr::from_octets(
                *(packet_data[14..18].as_array().expect("invalid length")),
            ),
            target_mac: MacAddress::new(&packet_data[18..24]),
            target_addr: Ipv4Addr::from_octets(
                *(packet_data[24..28].as_array().expect("invalid length")),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_arp_operation_from() {
        let req: u16 = 1;

        assert_eq!(ArpOperation::try_from(req), Ok(ArpOperation::Request));

        let reply: u16 = 2;

        assert_eq!(ArpOperation::try_from(reply), Ok(ArpOperation::Reply));

        let other = 0;
        let other2 = 5;

        assert_eq!(ArpOperation::try_from(other), Err(()));
        assert_eq!(ArpOperation::try_from(other2), Err(()));
    }

    // A known-good raw byte array of a standard ARP Request packet
    // Hardware: Ethernet (1), Protocol: IPv4 (0x0800)
    const VALID_ARP_REQUEST: [u8; 28] = [
        0x00, 0x01, // Hardware type: Ethernet (1)
        0x08, 0x00, // Protocol type: IPv4 (0x0800)
        0x06, // Hardware size: 6 (MAC)
        0x04, // Protocol size: 4 (IPv4)
        0x00, 0x01, // Opcode: Request (1)
        0x11, 0x22, 0x33, 0x44, 0x55, 0x66, // Sender MAC
        192, 168, 1, 10, // Sender IP
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // Target MAC (zeroed out in request)
        192, 168, 1, 20, // Target IP
    ];

    #[test]
    fn test_arp_serialization() {
        // Construct the packet using assumed valid structs based on your definitions
        let packet = ArpPacket {
            hardware_type: 1,
            protocol_type: 0x0800,
            hardware_len: 6,
            proto_len: 4,
            operation: ArpOperation::try_from(1).unwrap(), // Assuming Request = 1
            sender_mac: MacAddress::new(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66]),
            sender_addr: Ipv4Addr::from_octets([192, 168, 1, 10]),
            target_mac: MacAddress::new(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
            target_addr: Ipv4Addr::from_octets([192, 168, 1, 20]),
        };

        assert_eq!(packet.serialize(), VALID_ARP_REQUEST);
        assert_eq!(ArpPacket::from_slice(&packet.serialize()), Ok(packet));
    }

    #[test]
    fn test_arp_deserialization_success() {
        let packet =
            ArpPacket::from_slice(&VALID_ARP_REQUEST).expect("Failed to parse valid ARP packet");

        assert_eq!(packet.hardware_type, 1);
        assert_eq!(packet.protocol_type, 0x0800);
        assert_eq!(packet.hardware_len, 6);
        assert_eq!(packet.proto_len, 4);
        assert_eq!(packet.sender_mac.addr, [0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
        assert_eq!(packet.sender_addr.octets(), [192, 168, 1, 10]);
        assert_eq!(packet.target_mac.addr, [0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(packet.target_addr.octets(), [192, 168, 1, 20]);

        assert_eq!(packet.serialize(), VALID_ARP_REQUEST);
    }

    #[test]
    fn test_arp_deserialization_rejects_unsupported_hardware_type() {
        let mut malformed = VALID_ARP_REQUEST;
        malformed[1] = 0x02; // Change hardware type to Experimental Ethernet (2)

        assert!(
            ArpPacket::from_slice(&malformed).is_err(),
            "Should reject non-Ethernet hardware types"
        );
    }

    #[test]
    fn test_arp_deserialization_rejects_unsupported_protocol() {
        let mut malformed = VALID_ARP_REQUEST;
        malformed[2] = 0x86;
        malformed[3] = 0xDD; // Change protocol type to IPv6 (0x86DD)

        assert!(
            ArpPacket::from_slice(&malformed).is_err(),
            "Should reject non-IPv4 protocol types"
        );
    }

    #[test]
    fn test_arp_deserialization_rejects_invalid_lengths() {
        let mut malformed_hw_len = VALID_ARP_REQUEST;
        malformed_hw_len[4] = 8; // Invalid MAC length
        assert!(
            ArpPacket::from_slice(&malformed_hw_len).is_err(),
            "Should reject invalid hardware length"
        );

        let mut malformed_proto_len = VALID_ARP_REQUEST;
        malformed_proto_len[5] = 6; // Invalid IP length
        assert!(
            ArpPacket::from_slice(&malformed_proto_len).is_err(),
            "Should reject invalid protocol length"
        );
    }

    #[test]
    fn test_arp_deserialization_short_buffer() {
        // A buffer that is too short for a complete ARP packet (e.g., 20 bytes instead of 28).
        // This test will FAIL with your current implementation because `from_slice` panics
        // when indexing `&packet_data[24..28]` instead of gracefully returning an Err(()).
        let short_buffer = &VALID_ARP_REQUEST[0..20];

        let result = ArpPacket::from_slice(short_buffer);
        assert!(
            result.is_err(),
            "Parsing a short buffer should return an Error, not panic"
        );
    }
}
