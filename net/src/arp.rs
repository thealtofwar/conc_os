extern crate alloc;

use core::net::Ipv4Addr;

use alloc::collections::btree_map::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use crate::ethernet::MacAddress;
use crate::utils::FromSlice;

/// Frames held for a single unresolved address. A destination that never answers
/// must not be able to grow its queue without bound.
pub const MAX_PENDING_PER_ADDRESS: usize = 4;

/// Addresses that may await resolution at once. Pending entries are created by
/// our own outbound sends, but those can be driven by received traffic — an ICMP
/// echo reply goes back to whatever source address arrived — so the number of
/// distinct addresses is capped as well.
pub const MAX_PENDING_ADDRESSES: usize = 16;

enum ArpEntry {
    Resolved(MacAddress),
    /// Serialized IPv4 packets waiting on this address, oldest first. They lack
    /// only a destination MAC, so resolving the address is enough to send them.
    Pending(Vec<Vec<u8>>),
}

/// The outcome of parking a frame on an unresolved address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Queued {
    /// The frame is parked and no request is outstanding: send one.
    RequestNeeded,
    /// The frame is parked behind a request that has already gone out.
    AlreadyPending,
    /// The frame was dropped because a queue limit was reached.
    Dropped,
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

    /// The MAC for `ip`, if it is resolved. An address still awaiting a reply
    /// reads as absent, so that callers park behind the outstanding request
    /// rather than sending to a half known entry.
    pub fn lookup(&self, ip: Ipv4Addr) -> Option<MacAddress> {
        match self.entries.get(&ip) {
            Some(ArpEntry::Resolved(mac)) => Some(*mac),
            Some(ArpEntry::Pending(_)) | None => None,
        }
    }

    /// Records `mac` for `ip` and hands back every frame parked on it.
    ///
    /// Draining is tied to insertion so that a frame cannot be stranded by a
    /// caller that learns an address without remembering to check the queue.
    #[must_use = "frames parked on this address are dropped unless they are sent"]
    pub fn insert(&mut self, ip: Ipv4Addr, mac: MacAddress) -> Vec<Vec<u8>> {
        match self.entries.insert(ip, ArpEntry::Resolved(mac)) {
            Some(ArpEntry::Pending(queue)) => queue,
            Some(ArpEntry::Resolved(_)) | None => Vec::new(),
        }
    }

    /// Parks `frame` until `ip` resolves.
    ///
    /// Callers must consult [`ArpCache::lookup`] first: an address that already
    /// resolved has nothing to wait for, and the frame is reported as dropped.
    pub fn queue_pending(&mut self, ip: Ipv4Addr, frame: Vec<u8>) -> Queued {
        if let Some(entry) = self.entries.get_mut(&ip) {
            return match entry {
                ArpEntry::Pending(queue) if queue.len() < MAX_PENDING_PER_ADDRESS => {
                    queue.push(frame);
                    Queued::AlreadyPending
                }
                ArpEntry::Pending(_) | ArpEntry::Resolved(_) => Queued::Dropped,
            };
        }

        if self.pending_addresses() >= MAX_PENDING_ADDRESSES {
            return Queued::Dropped;
        }

        self.entries.insert(ip, ArpEntry::Pending(vec![frame]));
        Queued::RequestNeeded
    }

    /// Discards `ip`, dropping any frames parked on it.
    pub fn remove(&mut self, ip: Ipv4Addr) {
        self.entries.remove(&ip);
    }

    /// Whether `ip` is resolved. An address awaiting a reply is not.
    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        self.lookup(ip).is_some()
    }

    /// How many frames are parked on `ip`.
    pub fn pending_len(&self, ip: Ipv4Addr) -> usize {
        match self.entries.get(&ip) {
            Some(ArpEntry::Pending(queue)) => queue.len(),
            Some(ArpEntry::Resolved(_)) | None => 0,
        }
    }

    /// How many addresses are awaiting a reply. Resolved entries do not count
    /// against [`MAX_PENDING_ADDRESSES`].
    pub fn pending_addresses(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| matches!(entry, ArpEntry::Pending(_)))
            .count()
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArpError {
    LengthOutOfRange,
    IncorrectFormat,
    BadOperation,
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

    pub fn from_slice(packet_data: &[u8]) -> Result<Self, ArpError> {
        if packet_data.len() < 28 {
            return Err(ArpError::LengthOutOfRange);
        }

        let hardware_type = u16::from_be_slice(&packet_data[0..2]);
        let protocol_type = u16::from_be_slice(&packet_data[2..4]);
        let hardware_len = u8::from_be_slice(&packet_data[4..5]);
        let proto_len = u8::from_be_slice(&packet_data[5..6]);

        let operation = ArpOperation::try_from(u16::from_be_slice(&packet_data[6..8]))
            .map_err(|_| ArpError::BadOperation)?;

        if hardware_type != 1 || protocol_type != 0x0800 || hardware_len != 6 || proto_len != 4 {
            // reject malformed packets
            return Err(ArpError::IncorrectFormat);
        }
        Ok(ArpPacket {
            hardware_type,
            protocol_type,
            hardware_len,
            proto_len,
            operation,
            sender_mac: MacAddress::new(packet_data[8..14].try_into().expect("invalid length")),
            sender_addr: Ipv4Addr::from_octets(
                *(packet_data[14..18].as_array().expect("invalid length")),
            ),
            target_mac: MacAddress::new(packet_data[18..24].try_into().expect("invalid length")),
            target_addr: Ipv4Addr::from_octets(
                *(packet_data[24..28].as_array().expect("invalid length")),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac(last: u8) -> MacAddress {
        MacAddress::new([0x02, 0x00, 0x00, 0x00, 0x00, last])
    }

    fn ip(last: u8) -> Ipv4Addr {
        Ipv4Addr::new(192, 168, 1, last)
    }

    fn frame(marker: u8) -> Vec<u8> {
        vec![marker; 4]
    }

    #[test]
    fn test_arp_cache_resolves() {
        let mut cache = ArpCache::new();

        assert_eq!(cache.lookup(ip(1)), None);
        assert!(!cache.contains(ip(1)));

        assert!(cache.insert(ip(1), mac(1)).is_empty());

        assert_eq!(cache.lookup(ip(1)), Some(mac(1)));
        assert!(cache.contains(ip(1)));
    }

    #[test]
    fn test_arp_cache_pending_address_is_not_resolved() {
        // A parked address must not read as resolved, or the next send would
        // hand `send_frame` an address it never learned.
        let mut cache = ArpCache::new();

        assert_eq!(
            cache.queue_pending(ip(1), frame(0xaa)),
            Queued::RequestNeeded
        );

        assert_eq!(cache.lookup(ip(1)), None);
        assert!(!cache.contains(ip(1)));
        assert_eq!(cache.pending_len(ip(1)), 1);
    }

    #[test]
    fn test_arp_cache_requests_once_per_address() {
        // One ARP request per burst, not one per packet.
        let mut cache = ArpCache::new();

        assert_eq!(
            cache.queue_pending(ip(1), frame(0xaa)),
            Queued::RequestNeeded
        );
        assert_eq!(
            cache.queue_pending(ip(1), frame(0xbb)),
            Queued::AlreadyPending
        );
        assert_eq!(
            cache.queue_pending(ip(1), frame(0xcc)),
            Queued::AlreadyPending
        );

        assert_eq!(cache.pending_len(ip(1)), 3);
        assert_eq!(cache.pending_addresses(), 1);
    }

    #[test]
    fn test_arp_cache_insert_drains_pending_in_order() {
        let mut cache = ArpCache::new();

        let _ = cache.queue_pending(ip(1), frame(0xaa));
        let _ = cache.queue_pending(ip(1), frame(0xbb));

        assert_eq!(cache.insert(ip(1), mac(1)), vec![frame(0xaa), frame(0xbb)]);

        // The queue is handed over, not copied: the entry is now resolved and
        // holds nothing.
        assert_eq!(cache.lookup(ip(1)), Some(mac(1)));
        assert_eq!(cache.pending_len(ip(1)), 0);
        assert_eq!(cache.pending_addresses(), 0);
    }

    #[test]
    fn test_arp_cache_drains_only_once() {
        // A second reply for the same address must not resend the frames the
        // first one already flushed.
        let mut cache = ArpCache::new();

        let _ = cache.queue_pending(ip(1), frame(0xaa));

        assert_eq!(cache.insert(ip(1), mac(1)), vec![frame(0xaa)]);
        assert!(cache.insert(ip(1), mac(2)).is_empty());

        assert_eq!(cache.lookup(ip(1)), Some(mac(2)));
    }

    #[test]
    fn test_arp_cache_drains_only_the_resolved_address() {
        let mut cache = ArpCache::new();

        let _ = cache.queue_pending(ip(1), frame(0xaa));
        let _ = cache.queue_pending(ip(2), frame(0xbb));

        assert_eq!(cache.insert(ip(1), mac(1)), vec![frame(0xaa)]);

        assert_eq!(cache.pending_len(ip(2)), 1);
        assert_eq!(cache.lookup(ip(2)), None);
    }

    #[test]
    fn test_arp_cache_bounds_frames_per_address() {
        let mut cache = ArpCache::new();

        let _ = cache.queue_pending(ip(1), frame(0));
        for marker in 1..MAX_PENDING_PER_ADDRESS as u8 {
            assert_eq!(
                cache.queue_pending(ip(1), frame(marker)),
                Queued::AlreadyPending
            );
        }

        assert_eq!(cache.pending_len(ip(1)), MAX_PENDING_PER_ADDRESS);

        // The queue is full, so further frames for this address are dropped
        // rather than accumulating behind an address that may never answer.
        assert_eq!(cache.queue_pending(ip(1), frame(0xff)), Queued::Dropped);
        assert_eq!(cache.pending_len(ip(1)), MAX_PENDING_PER_ADDRESS);
    }

    #[test]
    fn test_arp_cache_bounds_pending_addresses() {
        let mut cache = ArpCache::new();

        for last in 0..MAX_PENDING_ADDRESSES as u8 {
            assert_eq!(
                cache.queue_pending(ip(last), frame(last)),
                Queued::RequestNeeded
            );
        }

        assert_eq!(cache.pending_addresses(), MAX_PENDING_ADDRESSES);

        let overflow = ip(MAX_PENDING_ADDRESSES as u8);
        assert_eq!(cache.queue_pending(overflow, frame(0xff)), Queued::Dropped);

        // A dropped frame must not leave a pending entry behind, or the table
        // would fill up with addresses no request was ever sent for.
        assert_eq!(cache.pending_addresses(), MAX_PENDING_ADDRESSES);
        assert_eq!(cache.pending_len(overflow), 0);
    }

    #[test]
    fn test_arp_cache_resolved_entries_do_not_fill_the_table() {
        let mut cache = ArpCache::new();

        for last in 0..MAX_PENDING_ADDRESSES as u8 * 2 {
            assert!(cache.insert(ip(last), mac(last)).is_empty());
        }

        assert_eq!(cache.pending_addresses(), 0);
        assert_eq!(
            cache.queue_pending(ip(0xff), frame(0xaa)),
            Queued::RequestNeeded
        );
    }

    #[test]
    fn test_arp_cache_queue_on_resolved_address_is_dropped() {
        // Callers check `lookup` first, so this is a misuse rather than a path
        // the stack takes; it is pinned here so it stays a visible drop.
        let mut cache = ArpCache::new();

        assert!(cache.insert(ip(1), mac(1)).is_empty());

        assert_eq!(cache.queue_pending(ip(1), frame(0xaa)), Queued::Dropped);
        assert_eq!(cache.lookup(ip(1)), Some(mac(1)));
    }

    #[test]
    fn test_arp_cache_remove_discards_pending() {
        let mut cache = ArpCache::new();

        let _ = cache.queue_pending(ip(1), frame(0xaa));
        cache.remove(ip(1));

        assert_eq!(cache.pending_addresses(), 0);
        assert_eq!(cache.pending_len(ip(1)), 0);

        // The address is unknown again, so the next send starts a fresh request.
        assert_eq!(
            cache.queue_pending(ip(1), frame(0xbb)),
            Queued::RequestNeeded
        );
    }

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
            sender_mac: MacAddress::new([0x11, 0x22, 0x33, 0x44, 0x55, 0x66]),
            sender_addr: Ipv4Addr::from_octets([192, 168, 1, 10]),
            target_mac: MacAddress::new([0x00, 0x00, 0x00, 0x00, 0x00, 0x00]),
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

        assert_eq!(
            ArpPacket::from_slice(&malformed),
            Err(ArpError::IncorrectFormat),
            "Should reject non-Ethernet hardware types"
        );
    }

    #[test]
    fn test_arp_deserialization_rejects_unsupported_protocol() {
        let mut malformed = VALID_ARP_REQUEST;
        malformed[2] = 0x86;
        malformed[3] = 0xDD; // Change protocol type to IPv6 (0x86DD)

        assert_eq!(
            ArpPacket::from_slice(&malformed),
            Err(ArpError::IncorrectFormat),
            "Should reject non-IPv4 protocol types"
        );
    }

    #[test]
    fn test_arp_deserialization_rejects_invalid_lengths() {
        let mut malformed_hw_len = VALID_ARP_REQUEST;
        malformed_hw_len[4] = 8; // Invalid MAC length
        assert_eq!(
            ArpPacket::from_slice(&malformed_hw_len),
            Err(ArpError::IncorrectFormat),
            "Should reject invalid hardware length"
        );

        let mut malformed_proto_len = VALID_ARP_REQUEST;
        malformed_proto_len[5] = 6; // Invalid IP length
        assert_eq!(
            ArpPacket::from_slice(&malformed_proto_len),
            Err(ArpError::IncorrectFormat),
            "Should reject invalid protocol length"
        );
    }

    #[test]
    fn test_arp_deserialization_rejects_unknown_operation() {
        // Only Request (1) and Reply (2) are defined; RARP opcodes and garbage are not.
        for opcode in [0u16, 3, 4, 0xffff] {
            let mut malformed = VALID_ARP_REQUEST;
            malformed[6..8].copy_from_slice(&opcode.to_be_bytes());

            assert_eq!(
                ArpPacket::from_slice(&malformed),
                Err(ArpError::BadOperation),
                "opcode {opcode} is not a supported ARP operation"
            );
        }
    }

    #[test]
    fn test_arp_deserialization_short_buffer() {
        // Anything short of the full 28 bytes must be reported rather than indexed
        // into: the last field read is `&packet_data[24..28]`.
        for len in [0, 8, 20, 27] {
            assert_eq!(
                ArpPacket::from_slice(&VALID_ARP_REQUEST[..len]),
                Err(ArpError::LengthOutOfRange),
                "a {len} byte buffer is shorter than an ARP packet"
            );
        }
    }

    #[test]
    fn test_arp_deserialization_ignores_trailing_bytes() {
        // Ethernet pads short frames, so a valid packet can arrive with trailing
        // filler that must not affect parsing.
        let mut padded = [0u8; 60];
        padded[..28].copy_from_slice(&VALID_ARP_REQUEST);

        let packet = ArpPacket::from_slice(&padded).expect("padding is not part of the packet");

        assert_eq!(packet.serialize(), VALID_ARP_REQUEST);
    }
}
