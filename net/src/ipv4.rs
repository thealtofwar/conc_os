extern crate alloc;

use alloc::vec::Vec;

use crate::utils::FromSlice;
use core::net::Ipv4Addr;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum IPProtocol {
    ICMP = 1,
    TCP = 6,
    UDP = 17,
    Unknown(u8),
}

impl From<u8> for IPProtocol {
    fn from(value: u8) -> Self {
        match value {
            1 => Self::ICMP,
            6 => Self::TCP,
            17 => Self::UDP,
            _ => Self::Unknown(value),
        }
    }
}

impl From<IPProtocol> for u8 {
    fn from(val: IPProtocol) -> Self {
        match val {
            IPProtocol::ICMP => 1,
            IPProtocol::TCP => 6,
            IPProtocol::UDP => 17,
            IPProtocol::Unknown(proto) => proto,
        }
    }
}

pub struct Ipv4Packet<'a> {
    pub version_ihl: u8,
    pub dscp_ecn: u8,
    pub length: u16,
    pub id: u16,
    pub frag_offset: u16,
    pub dont_fragment: bool,
    pub more_fragments: bool,
    pub ttl: u8,
    pub protocol: IPProtocol,
    pub checksum: u16,
    pub source: Ipv4Addr,
    pub dest: Ipv4Addr,
    pub options: &'a [u8],
    pub data: &'a [u8],
}

pub fn internet_checksum(packet: &[u8]) -> u16 {
    let mut sum: u32 = 0;

    for chunk in packet.chunks(2) {
        let word = if chunk.len() == 2 {
            u16::from_be_bytes([chunk[0], chunk[1]])
        } else {
            u16::from_be_bytes([chunk[0], 0])
        };

        sum += word as u32;
    }
    while (sum >> 16) > 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }

    !(sum as u16)
}

impl<'a> Ipv4Packet<'a> {
    pub fn new(packet: &'a [u8]) -> Result<Self, ()> {
        if packet.len() < 20 {
            return Err(());
        }

        let version_ihl = u8::from_be_slice(&packet[0..1]);

        let version = (version_ihl >> 4) & 0xf;

        if version != 4 {
            return Err(());
        }

        let ihl = version_ihl & 0xf;

        if ihl < 5 {
            return Err(());
        }

        let header_len = (ihl * 4) as usize;

        if header_len > packet.len() {
            return Err(());
        }

        let dscp_ecn: u8 = u8::from_be_slice(&packet[1..2]);

        let total_length = u16::from_be_slice(&packet[2..4]) as usize;

        if total_length > packet.len() || total_length < header_len {
            return Err(());
        }

        let packet = &packet[..total_length];

        let id = u16::from_be_slice(&packet[4..6]);

        let flags_frag_offset = u16::from_be_slice(&packet[6..8]);

        let frag_offset = flags_frag_offset & 0x1fff;

        let flags = flags_frag_offset >> 13;

        let evil_bit = flags & 0b100 != 0; // RFC 3514
        let dont_fragment = flags & 0b010 != 0;
        let more_fragments = flags & 0b001 != 0;

        if evil_bit {
            return Err(());
        }

        let ttl = u8::from_be_slice(&packet[8..9]);

        let protocol = IPProtocol::from(u8::from_be_slice(&packet[9..10]));

        let checksum = u16::from_be_slice(&packet[10..12]);

        if internet_checksum(&packet[..header_len]) != 0 {
            return Err(());
        }

        let src_addr = Ipv4Addr::from_octets(*(packet[12..16].as_array().expect("invalid length")));

        let dst_addr = Ipv4Addr::from_octets(*(packet[16..20].as_array().expect("invalid length")));

        let options = &packet[20..header_len];

        let data = &packet[header_len..];

        Ok(Ipv4Packet {
            version_ihl,
            dscp_ecn,
            length: total_length as u16,
            id,
            frag_offset,
            dont_fragment,
            more_fragments,
            ttl,
            protocol,
            checksum,
            source: src_addr,
            dest: dst_addr,
            options,
            data,
        })
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut result = Vec::new();

        result.push(self.version_ihl);
        result.push(self.dscp_ecn);
        result.extend_from_slice(&self.length.to_be_bytes());
        result.extend_from_slice(&self.id.to_be_bytes());

        let flags = (if self.dont_fragment { 0b010 } else { 0 })
            + if self.more_fragments { 0b001 } else { 0 };

        let flags_frag_offset = (self.frag_offset & 0x1fff) + (flags << 13);
        result.extend_from_slice(&flags_frag_offset.to_be_bytes());

        result.extend_from_slice(&self.ttl.to_be_bytes());
        result.push(self.protocol.into());

        for _ in 0..2 {
            // checksum
            result.push(0);
        }

        result.extend_from_slice(&self.source.octets());
        result.extend_from_slice(&self.dest.octets());
        result.extend_from_slice(self.options);
        result.extend_from_slice(self.data);

        let checksum = internet_checksum(&result[..20 + self.options.len()]).to_be_bytes();

        result[10..12].copy_from_slice(&checksum);

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! check_eq {
        ($data:expr, $result:expr) => {{
            let data = $data;

            let checksum = internet_checksum(&data);

            assert_eq!(checksum, $result);
        }};
    }

    #[test]
    fn test_internet_checksum() {
        // some examples taken from https://github.com/google/quiche/blob/main/quiche/common/internet_checksum_test.cc

        check_eq!([0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7], 0x220d);

        check_eq!(
            [0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7, 0x22, 0x0d],
            0
        );

        check_eq!([0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6], 0x2304);

        check_eq!([0xe3, 0x4f, 0x23, 0x96, 0x44, 0x27, 0x99, 0xf3], 0x1aff);

        check_eq!([0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02, 0x00], 0xfdff);

        check_eq!([], 0xffff);
    }

    fn get_valid_packet() -> Vec<u8> {
        vec![
            0x45, 0x00, 0x00, 0x18, // Version=4, IHL=5, DSCP/ECN=0, Length=24
            0x12, 0x34, 0x40, 0x00, // ID=0x1234, Flags=DF, FragOffset=0
            0x40, 0x11, 0x5c, 0xf7, // TTL=64, Protocol=17 (UDP), Checksum=0x5cf7
            0xc0, 0xa8, 0x01, 0x01, // Src=192.168.1.1
            0x0a, 0x00, 0x00, 0x01, // Dst=10.0.0.1
            0xde, 0xad, 0xbe, 0xef, // Data
        ]
    }

    #[test]
    fn test_ipv4_parse_and_serialize() {
        let packet_data = get_valid_packet();
        let packet = Ipv4Packet::new(&packet_data).expect("failed to parse valid packet");

        assert_eq!(packet.version_ihl, 0x45);
        assert_eq!(packet.dscp_ecn, 0x00);
        assert_eq!(packet.length, 24);
        assert_eq!(packet.id, 0x1234);
        assert_eq!(packet.dont_fragment, true);
        assert_eq!(packet.more_fragments, false);
        assert_eq!(packet.frag_offset, 0);
        assert_eq!(packet.ttl, 64);
        assert_eq!(packet.protocol, IPProtocol::UDP);
        assert_eq!(packet.checksum, 0x5cf7);
        assert_eq!(packet.source, Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(packet.dest, Ipv4Addr::new(10, 0, 0, 1));
        assert_eq!(packet.options.len(), 0);
        assert_eq!(packet.data, &[0xde, 0xad, 0xbe, 0xef]);

        let serialized = packet.serialize();
        assert_eq!(serialized, packet_data);
    }

    #[test]
    fn test_ipv4_with_options() {
        let packet_data: [u8; 28] = [
            0x46, 0x00, 0x00, 0x1c, // Version=4, IHL=6, DSCP/ECN=0, Length=28
            0x12, 0x34, 0x40, 0x00, // ID=0x1234, Flags=DF, Frag=0
            0x40, 0x11, 0x59, 0xf1, // TTL=64, Protocol=17 (UDP), Checksum=0x59f1
            0xc0, 0xa8, 0x01, 0x01, // Src=192.168.1.1
            0x0a, 0x00, 0x00, 0x01, // Dst=10.0.0.1
            0x01, 0x01, 0x01, 0x01, // Options (4 bytes of NOP)
            0xde, 0xad, 0xbe, 0xef, // Data
        ];

        let packet = Ipv4Packet::new(&packet_data).expect("failed to parse packet with options");
        assert_eq!(packet.options, &[0x01, 0x01, 0x01, 0x01]);
        assert_eq!(packet.data, &[0xde, 0xad, 0xbe, 0xef]);

        let serialized = packet.serialize();
        assert_eq!(serialized, packet_data);
    }

    #[test]
    fn test_ipv4_fragmented() {
        let mut data = get_valid_packet();
        // Modify the flags to set More Fragments (MF=1, DF=0)
        data[6] = 0x20;

        // Correct header checksum for the modified flags
        data[10] = 0x7c;
        data[11] = 0xf7;

        // A compliant IPv4 parser should successfully parse packet fragments.
        // This will expose the `if evil_bit || more_fragments || frag_offset != 0`
        // premature rejection in the current implementation.
        let packet = Ipv4Packet::new(&data).expect("Should parse standard fragmented IPv4 packets");

        assert_eq!(packet.more_fragments, true);
        assert_eq!(packet.dont_fragment, false);
        assert_eq!(packet.frag_offset, 0);
    }

    #[test]
    fn test_ipv4_invalid_version() {
        let mut data = get_valid_packet();
        data[0] = 0x55; // Version 5, IHL 5
        assert!(Ipv4Packet::new(&data).is_err());
    }

    #[test]
    fn test_ipv4_bad_checksum() {
        let mut data = get_valid_packet();
        data[11] = 0x00; // Corrupt the checksum
        assert!(Ipv4Packet::new(&data).is_err());
    }

    #[test]
    fn test_ipv4_too_short() {
        let data = get_valid_packet();
        let short_data = &data[..15]; // Less than the minimum 20 bytes
        assert!(Ipv4Packet::new(short_data).is_err());
    }

    #[test]
    fn test_ipv4_length_exceeds_buffer() {
        let mut data = get_valid_packet();
        data[2] = 0x00;
        data[3] = 0xFF; // Declared total_length is 255, but buffer is only 24 bytes
        assert!(Ipv4Packet::new(&data).is_err());
    }
}
