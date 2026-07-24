use alloc::vec::Vec;

use crate::utils::FromSlice;
use core::net::Ipv4Addr;

#[derive(Clone, Copy, PartialEq, Eq)]
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

impl Into<u8> for IPProtocol {
    fn into(self) -> u8 {
        match self {
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

        if evil_bit || more_fragments || frag_offset != 0 {
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

        let flags = 0b100
            + (if self.dont_fragment { 0b010 } else { 0 })
            + if self.more_fragments { 0b001 } else { 0 };
        let flags_frag_offset = self.frag_offset + flags << 13;
        result.extend_from_slice(&flags_frag_offset.to_be_bytes());

        result.extend_from_slice(&self.ttl.to_be_bytes());
        result.push(self.protocol.into());

        for _ in 0..2 {
            // checksum
            result.push(0);
        }

        result.extend_from_slice(&self.source.octets());
        result.extend_from_slice(&self.dest.octets());
        result.extend_from_slice(&self.options);
        result.extend_from_slice(&self.data);

        let checksum = internet_checksum(&result[..20 + self.options.len()]).to_be_bytes();

        result[10..12].copy_from_slice(&checksum);

        result
    }
}
