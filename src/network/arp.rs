use core::net::Ipv4Addr;

use alloc::collections::btree_map::BTreeMap;

use crate::network::handler::MacAddress;

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
#[derive(Clone, Copy, PartialEq, Eq)]
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

#[derive(Clone, Copy)]
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
        pkt[14..18].copy_from_slice(&self.sender_addr.to_bits().to_be_bytes());
        pkt[18..24].copy_from_slice(&self.target_mac.addr);
        pkt[24..28].copy_from_slice(&self.target_addr.to_bits().to_be_bytes());

        pkt
    }
}
