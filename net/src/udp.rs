extern crate alloc;

use core::net::Ipv4Addr;

use alloc::vec::Vec;

use crate::ipv4::{Ipv4Packet, internet_checksum};
use crate::utils::FromSlice;

// 65527 = 65535 (u16 max) - 8 (UDP header size)
const MAX_UDP_PAYLOAD: usize = 65527;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UDPError {
    LengthOutOfRange,
    LengthMismatch,
    IncorrectChecksum,
}

pub struct UDPPacket<'a> {
    source_port: u16,
    dest_port: u16,
    datagram_length: u16,
    checksum: u16,
    data: &'a [u8],
}

impl<'a> UDPPacket<'a> {
    pub fn new(source_port: u16, dest_port: u16, data: &'a [u8]) -> Result<Self, UDPError> {
        Ok(Self {
            source_port,
            dest_port,
            datagram_length: u16::try_from(data.len() + 8)
                .map_err(|_| UDPError::LengthOutOfRange)?,
            checksum: 0, // recomputed during serialization
            data,
        })
    }

    pub fn source_port(&self) -> u16 {
        self.source_port
    }

    pub fn dest_port(&self) -> u16 {
        self.dest_port
    }

    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn from_packet(ip_packet: &'a Ipv4Packet) -> Result<Self, UDPError> {
        if ip_packet.total_length < 28 || ip_packet.data.len() < 8 || ip_packet.data.len() > 65535 {
            return Err(UDPError::LengthOutOfRange);
        }

        let source_port = u16::from_be_slice(&ip_packet.data[0..2]);

        let dest_port = u16::from_be_slice(&ip_packet.data[2..4]);

        let datagram_len = u16::from_be_slice(&ip_packet.data[4..6]);

        if datagram_len != ip_packet.data.len() as u16 {
            return Err(UDPError::LengthMismatch);
        }

        let checksum = u16::from_be_slice(&ip_packet.data[6..8]);

        if internet_checksum(&[
            &ip_packet.source.octets(),
            &ip_packet.dest.octets(),
            &[0x0, 0x11],
            &datagram_len.to_be_bytes(),
            ip_packet.data,
        ]) != 0
            && checksum != 0
        {
            return Err(UDPError::IncorrectChecksum);
        }

        Ok(UDPPacket {
            source_port,
            dest_port,
            datagram_length: datagram_len,
            checksum,
            data: &ip_packet.data[8..],
        })
    }

    pub fn serialize_ipv4(
        &self,
        source_addr: &Ipv4Addr,
        dest_addr: &Ipv4Addr,
    ) -> Result<Vec<u8>, UDPError> {
        let udp_len = u16::try_from(self.data.len()).map_err(|_| UDPError::LengthOutOfRange)?;

        if udp_len > MAX_UDP_PAYLOAD as u16 {
            return Err(UDPError::LengthOutOfRange);
        }

        let total_datagram_len: u16 = 8 + udp_len;

        let mut result = Vec::with_capacity(total_datagram_len as usize);

        let source_port_bytes = self.source_port.to_be_bytes();
        let dest_port_bytes = self.dest_port.to_be_bytes();

        result.extend_from_slice(&source_port_bytes);
        result.extend_from_slice(&dest_port_bytes);

        result.extend_from_slice(&total_datagram_len.to_be_bytes());

        let mut checksum = internet_checksum(&[
            &source_addr.octets(),
            &dest_addr.octets(),
            &[0x0, 0x11],
            &total_datagram_len.to_be_bytes(),
            &result, // result so far is the UDP header, which is used to calculate the checksum
            self.data,
        ]);

        if checksum == 0x0000 {
            checksum = 0xFFFF;
        }

        result.extend_from_slice(&checksum.to_be_bytes());

        result.extend_from_slice(self.data);

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ipv4::IPProtocol;

    const SRC: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 1);
    const DST: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);

    const PAYLOAD: [u8; 4] = [0xde, 0xad, 0xbe, 0xef];

    // A known-good datagram from 192.168.1.1:0x1234 to 10.0.0.1:0x5678. The length
    // and checksum were computed outside this crate, so the tests below do not end
    // up validating the implementation against itself.
    const VALID_UDP: [u8; 12] = [
        0x12, 0x34, // Source port
        0x56, 0x78, // Dest port
        0x00, 0x0c, // Length = 12 (8 byte header + 4 byte payload)
        0x2d, 0xe2, // Checksum
        0xde, 0xad, 0xbe, 0xef, // Payload
    ];

    // The same endpoints carrying no payload: the length is just the header.
    const VALID_UDP_EMPTY: [u8; 8] = [
        0x12, 0x34, // Source port
        0x56, 0x78, // Dest port
        0x00, 0x08, // Length = 8 (header only)
        0xcb, 0x87, // Checksum
    ];

    /// Wraps a UDP datagram in an IPv4 packet and returns the raw bytes.
    fn ipv4_wrap(source: Ipv4Addr, dest: Ipv4Addr, udp: &[u8]) -> Vec<u8> {
        ipv4_wrap_with_options(source, dest, &[], udp)
    }

    fn ipv4_wrap_with_options(
        source: Ipv4Addr,
        dest: Ipv4Addr,
        options: &[u8],
        udp: &[u8],
    ) -> Vec<u8> {
        let header_len = 20 + options.len();

        Ipv4Packet {
            version_ihl: 0x40 | (header_len / 4) as u8,
            dscp_ecn: 0x00,
            total_length: (header_len + udp.len()) as u16,
            id: 0x1234,
            frag_offset: 0,
            dont_fragment: true,
            more_fragments: false,
            ttl: 64,
            protocol: IPProtocol::UDP,
            checksum: 0, // recomputed by serialize
            source,
            dest,
            options,
            data: udp,
        }
        .serialize()
    }

    /// Recomputes the checksum of `datagram` over the IPv4 pseudo header, using
    /// whatever length the datagram itself declares.
    fn fill_checksum(datagram: &mut [u8], source: Ipv4Addr, dest: Ipv4Addr) {
        datagram[6..8].copy_from_slice(&[0x0, 0x0]);

        let declared_len = u16::from_be_slice(&datagram[4..6]);

        let checksum = internet_checksum(&[
            &source.octets(),
            &dest.octets(),
            &[0x0, 0x11],
            &declared_len.to_be_bytes(),
            datagram,
        ]);

        datagram[6..8].copy_from_slice(&checksum.to_be_bytes());
    }

    /// Builds a well formed datagram: correct length field, correct checksum.
    fn build_udp(
        source: Ipv4Addr,
        dest: Ipv4Addr,
        source_port: u16,
        dest_port: u16,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut datagram = Vec::new();

        datagram.extend_from_slice(&source_port.to_be_bytes());
        datagram.extend_from_slice(&dest_port.to_be_bytes());
        datagram.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        datagram.extend_from_slice(&[0x0, 0x0]); // checksum placeholder
        datagram.extend_from_slice(payload);

        fill_checksum(&mut datagram, source, dest);

        datagram
    }

    #[test]
    fn test_udp_builder_matches_capture() {
        // Guards the helper the other tests are built on.
        assert_eq!(
            build_udp(SRC, DST, 0x1234, 0x5678, &PAYLOAD),
            &VALID_UDP[..]
        );
        assert_eq!(
            build_udp(SRC, DST, 0x1234, 0x5678, &[]),
            &VALID_UDP_EMPTY[..]
        );
    }

    #[test]
    fn test_udp_parse_valid() {
        let bytes = ipv4_wrap(SRC, DST, &VALID_UDP);
        let ip = Ipv4Packet::from_packet(&bytes).expect("failed to parse valid IPv4 packet");

        let packet = UDPPacket::from_packet(&ip).expect("failed to parse valid UDP datagram");

        assert_eq!(packet.source_port, 0x1234);
        assert_eq!(packet.dest_port, 0x5678);
        // The UDP length covers the datagram only, not the enclosing IPv4 packet.
        assert_eq!(packet.datagram_length, 12);
        assert_eq!(packet.checksum, 0x2de2);
        assert_eq!(packet.data, &PAYLOAD);
    }

    #[test]
    fn test_udp_parse_empty_payload() {
        let bytes = ipv4_wrap(SRC, DST, &VALID_UDP_EMPTY);
        let ip = Ipv4Packet::from_packet(&bytes).expect("failed to parse valid IPv4 packet");

        let packet = UDPPacket::from_packet(&ip).expect("a header only datagram is valid");

        assert_eq!(packet.datagram_length, 8);
        assert_eq!(packet.data, &[]);
    }

    #[test]
    fn test_udp_parse_ignores_ipv4_options() {
        // With options present the IPv4 total length is 28 + payload, which the UDP
        // length field must not be compared against.
        let bytes = ipv4_wrap_with_options(SRC, DST, &[0x01, 0x01, 0x01, 0x01], &VALID_UDP);
        let ip = Ipv4Packet::from_packet(&bytes).expect("failed to parse IPv4 packet with options");

        let packet = UDPPacket::from_packet(&ip).expect("options do not change the UDP datagram");

        assert_eq!(packet.datagram_length, 12);
        assert_eq!(packet.data, &PAYLOAD);
    }

    #[test]
    fn test_udp_parse_accepts_zero_checksum() {
        let mut datagram = build_udp(SRC, DST, 0x1234, 0x5678, &PAYLOAD);
        datagram[6..8].copy_from_slice(&[0x0, 0x0]); // sender opted out of checksumming

        let bytes = ipv4_wrap(SRC, DST, &datagram);
        let ip = Ipv4Packet::from_packet(&bytes).expect("failed to parse valid IPv4 packet");

        let packet =
            UDPPacket::from_packet(&ip).expect("a zero checksum means unchecked, not invalid");

        assert_eq!(packet.checksum, 0);
        assert_eq!(packet.data, &PAYLOAD);
    }

    #[test]
    fn test_udp_parse_rejects_corrupt_payload() {
        let mut datagram = build_udp(SRC, DST, 0x1234, 0x5678, &PAYLOAD);
        datagram[8] ^= 0xff; // corrupt the payload, leaving a now stale checksum

        let bytes = ipv4_wrap(SRC, DST, &datagram);
        let ip = Ipv4Packet::from_packet(&bytes).expect("failed to parse valid IPv4 packet");

        assert_eq!(
            UDPPacket::from_packet(&ip).err(),
            Some(UDPError::IncorrectChecksum)
        );
    }

    #[test]
    fn test_udp_parse_rejects_corrupt_checksum_field() {
        let mut datagram = build_udp(SRC, DST, 0x1234, 0x5678, &PAYLOAD);
        datagram[7] ^= 0x01; // the datagram is intact, the checksum itself is not

        let bytes = ipv4_wrap(SRC, DST, &datagram);
        let ip = Ipv4Packet::from_packet(&bytes).expect("failed to parse valid IPv4 packet");

        assert_eq!(
            UDPPacket::from_packet(&ip).err(),
            Some(UDPError::IncorrectChecksum)
        );
    }

    #[test]
    fn test_udp_parse_rejects_wrong_pseudo_header() {
        // The checksum covers the addresses, so a datagram checksummed for one
        // destination must not verify against another.
        let datagram = build_udp(SRC, Ipv4Addr::new(10, 0, 0, 2), 0x1234, 0x5678, &PAYLOAD);

        let bytes = ipv4_wrap(SRC, DST, &datagram);
        let ip = Ipv4Packet::from_packet(&bytes).expect("failed to parse valid IPv4 packet");

        assert_eq!(
            UDPPacket::from_packet(&ip).err(),
            Some(UDPError::IncorrectChecksum)
        );
    }

    #[test]
    fn test_udp_parse_rejects_length_over_datagram() {
        let mut datagram = build_udp(SRC, DST, 0x1234, 0x5678, &PAYLOAD);
        // Claim 20 bytes while carrying 12, and keep the checksum consistent with
        // the claim so that only the length check can reject it.
        datagram[4..6].copy_from_slice(&20u16.to_be_bytes());
        fill_checksum(&mut datagram, SRC, DST);

        let bytes = ipv4_wrap(SRC, DST, &datagram);
        let ip = Ipv4Packet::from_packet(&bytes).expect("failed to parse valid IPv4 packet");

        assert_eq!(
            UDPPacket::from_packet(&ip).err(),
            Some(UDPError::LengthMismatch)
        );
    }

    #[test]
    fn test_udp_parse_rejects_length_under_datagram() {
        let mut datagram = build_udp(SRC, DST, 0x1234, 0x5678, &PAYLOAD);
        // Claim 10 bytes while carrying 12: the trailing payload byte pair is
        // unaccounted for.
        datagram[4..6].copy_from_slice(&10u16.to_be_bytes());
        fill_checksum(&mut datagram, SRC, DST);

        let bytes = ipv4_wrap(SRC, DST, &datagram);
        let ip = Ipv4Packet::from_packet(&bytes).expect("failed to parse valid IPv4 packet");

        assert_eq!(
            UDPPacket::from_packet(&ip).err(),
            Some(UDPError::LengthMismatch)
        );
    }

    #[test]
    fn test_udp_parse_rejects_length_below_header() {
        let mut datagram = build_udp(SRC, DST, 0x1234, 0x5678, &PAYLOAD);
        datagram[4..6].copy_from_slice(&4u16.to_be_bytes()); // shorter than the header itself
        fill_checksum(&mut datagram, SRC, DST);

        let bytes = ipv4_wrap(SRC, DST, &datagram);
        let ip = Ipv4Packet::from_packet(&bytes).expect("failed to parse valid IPv4 packet");

        assert_eq!(
            UDPPacket::from_packet(&ip).err(),
            Some(UDPError::LengthMismatch)
        );
    }

    #[test]
    fn test_udp_parse_checks_length_before_checksum() {
        // A datagram that is wrong in both ways is reported as the length error:
        // there is no point trusting a checksum over a datagram of unknown extent.
        let mut datagram = build_udp(SRC, DST, 0x1234, 0x5678, &PAYLOAD);
        datagram[4..6].copy_from_slice(&20u16.to_be_bytes());
        datagram[8] ^= 0xff;

        let bytes = ipv4_wrap(SRC, DST, &datagram);
        let ip = Ipv4Packet::from_packet(&bytes).expect("failed to parse valid IPv4 packet");

        assert_eq!(
            UDPPacket::from_packet(&ip).err(),
            Some(UDPError::LengthMismatch)
        );
    }

    #[test]
    fn test_udp_parse_rejects_truncated_header() {
        for len in [1, 4, 7] {
            let bytes = ipv4_wrap(SRC, DST, &VALID_UDP[..len]);
            let ip =
                Ipv4Packet::from_packet(&bytes).expect("the IPv4 wrapper is still well formed");

            assert_eq!(
                UDPPacket::from_packet(&ip).err(),
                Some(UDPError::LengthOutOfRange),
                "{len} bytes is shorter than the 8 byte UDP header"
            );
        }
    }

    #[test]
    fn test_udp_parse_rejects_truncated_header_with_ipv4_options() {
        // IPv4 options push the total length up without adding any UDP bytes, so a
        // minimum size derived from the IPv4 total length does not bound the payload.
        for len in [1, 4, 7] {
            let bytes = ipv4_wrap_with_options(SRC, DST, &[0x01; 4], &VALID_UDP[..len]);
            let ip =
                Ipv4Packet::from_packet(&bytes).expect("the IPv4 wrapper is still well formed");

            assert_eq!(
                UDPPacket::from_packet(&ip).err(),
                Some(UDPError::LengthOutOfRange),
                "{len} bytes is shorter than the 8 byte UDP header"
            );
        }
    }

    #[test]
    fn test_udp_parse_rejects_empty_ip_payload() {
        let bytes = ipv4_wrap(SRC, DST, &[]);
        let ip =
            Ipv4Packet::from_packet(&bytes).expect("an IPv4 packet with no payload is well formed");

        assert_eq!(
            UDPPacket::from_packet(&ip).err(),
            Some(UDPError::LengthOutOfRange)
        );
    }

    #[test]
    fn test_udp_new_sets_header_fields() {
        let packet = UDPPacket::new(0x1234, 0x5678, &PAYLOAD).expect("a 4 byte payload fits");

        assert_eq!(packet.source_port, 0x1234);
        assert_eq!(packet.dest_port, 0x5678);
        assert_eq!(packet.data, &PAYLOAD);
        // The length field covers the header as well as the payload.
        assert_eq!(packet.datagram_length, 12);
        // There is nothing to checksum until an address pair is known, so the
        // checksum stays zero until serialization computes it.
        assert_eq!(packet.checksum, 0);
    }

    #[test]
    fn test_udp_new_empty_payload() {
        let packet = UDPPacket::new(0x1234, 0x5678, &[]).expect("an empty payload is valid");

        assert_eq!(packet.datagram_length, 8);
        assert_eq!(packet.data, &[]);
    }

    #[test]
    fn test_udp_new_max_payload() {
        let payload = vec![0x41; MAX_UDP_PAYLOAD];

        let packet = UDPPacket::new(0x1234, 0x5678, &payload)
            .expect("the largest payload that fits the length field is valid");

        assert_eq!(packet.datagram_length, u16::MAX);
    }

    #[test]
    fn test_udp_new_rejects_oversized_payload() {
        // One byte past the largest payload whose datagram length still fits a u16.
        let payload = vec![0x41; MAX_UDP_PAYLOAD + 1];

        assert_eq!(
            UDPPacket::new(0x1234, 0x5678, &payload).err(),
            Some(UDPError::LengthOutOfRange)
        );
    }

    #[test]
    fn test_udp_new_rejects_payload_overflowing_length_field() {
        // A payload past the length field's own range must be rejected rather than
        // wrapped around into a small, plausible looking length.
        let payload = vec![0x41; u16::MAX as usize + 1];

        assert_eq!(
            UDPPacket::new(0x1234, 0x5678, &payload).err(),
            Some(UDPError::LengthOutOfRange)
        );
    }

    #[test]
    fn test_udp_new_serializes_to_capture() {
        // The constructor and the serializer together reproduce the known-good wire
        // bytes, using only the public API.
        let packet = UDPPacket::new(0x1234, 0x5678, &PAYLOAD).expect("a 4 byte payload fits");

        assert_eq!(
            packet
                .serialize_ipv4(&SRC, &DST)
                .expect("failed to serialize a 4 byte payload"),
            &VALID_UDP[..]
        );

        let empty = UDPPacket::new(0x1234, 0x5678, &[]).expect("an empty payload is valid");

        assert_eq!(
            empty
                .serialize_ipv4(&SRC, &DST)
                .expect("failed to serialize an empty payload"),
            &VALID_UDP_EMPTY[..]
        );
    }

    #[test]
    fn test_udp_new_round_trip() {
        let packet = UDPPacket::new(0xabcd, 0x0035, &PAYLOAD).expect("a 4 byte payload fits");

        let datagram = packet
            .serialize_ipv4(&SRC, &DST)
            .expect("failed to serialize a 4 byte payload");

        let bytes = ipv4_wrap(SRC, DST, &datagram);
        let ip = Ipv4Packet::from_packet(&bytes).expect("failed to parse valid IPv4 packet");

        let parsed = UDPPacket::from_packet(&ip).expect("a constructed datagram should parse back");

        assert_eq!(parsed.source_port, 0xabcd);
        assert_eq!(parsed.dest_port, 0x0035);
        assert_eq!(parsed.data, &PAYLOAD);
        // The length the constructor computed is the one that reaches the receiver.
        assert_eq!(parsed.datagram_length, packet.datagram_length);
    }

    #[test]
    fn test_udp_serialize_matches_capture() {
        let packet = UDPPacket {
            source_port: 0x1234,
            dest_port: 0x5678,
            datagram_length: 12,
            checksum: 0x2de2,
            data: &PAYLOAD,
        };

        let serialized = packet
            .serialize_ipv4(&SRC, &DST)
            .expect("failed to serialize a 4 byte payload");

        assert_eq!(serialized, &VALID_UDP[..]);
    }

    #[test]
    fn test_udp_serialize_empty_payload() {
        let packet = UDPPacket {
            source_port: 0x1234,
            dest_port: 0x5678,
            datagram_length: 8,
            checksum: 0xcb87,
            data: &[],
        };

        let serialized = packet
            .serialize_ipv4(&SRC, &DST)
            .expect("failed to serialize an empty payload");

        assert_eq!(serialized, &VALID_UDP_EMPTY[..]);
    }

    #[test]
    fn test_udp_serialize_checksum_verifies() {
        let packet = UDPPacket {
            source_port: 0xabcd,
            dest_port: 0x0035,
            datagram_length: 0, // recomputed on serialization
            checksum: 0,        // recomputed on serialization
            data: &PAYLOAD,
        };

        let serialized = packet
            .serialize_ipv4(&SRC, &DST)
            .expect("failed to serialize a 4 byte payload");

        let declared_len = u16::from_be_slice(&serialized[4..6]);
        assert_eq!(declared_len as usize, serialized.len());

        // A receiver summing the pseudo header and the datagram gets zero.
        assert_eq!(
            internet_checksum(&[
                &SRC.octets(),
                &DST.octets(),
                &[0x0, 0x11],
                &declared_len.to_be_bytes(),
                &serialized,
            ]),
            0
        );
    }

    #[test]
    fn test_udp_serialize_zero_checksum_sent_as_all_ones() {
        // RFC 768: a computed checksum of zero goes on the wire as all ones,
        // because zero means "the sender transmitted no checksum".
        let payload = [0xcb, 0x83]; // chosen so the checksum computes to zero

        let packet = UDPPacket {
            source_port: 0x1234,
            dest_port: 0x5678,
            datagram_length: 10,
            checksum: 0,
            data: &payload,
        };

        let serialized = packet
            .serialize_ipv4(&SRC, &DST)
            .expect("failed to serialize a 2 byte payload");

        assert_eq!(&serialized[6..8], &[0xff, 0xff]);
    }

    #[test]
    fn test_udp_serialize_round_trip() {
        let packet = UDPPacket {
            source_port: 0xabcd,
            dest_port: 0x0035,
            datagram_length: 0,
            checksum: 0,
            data: &PAYLOAD,
        };

        let datagram = packet
            .serialize_ipv4(&SRC, &DST)
            .expect("failed to serialize a 4 byte payload");

        let bytes = ipv4_wrap(SRC, DST, &datagram);
        let ip = Ipv4Packet::from_packet(&bytes).expect("failed to parse valid IPv4 packet");

        let parsed = UDPPacket::from_packet(&ip).expect("a serialized datagram should parse back");

        assert_eq!(parsed.source_port, 0xabcd);
        assert_eq!(parsed.dest_port, 0x0035);
        assert_eq!(parsed.data, &PAYLOAD);
    }

    #[test]
    fn test_udp_serialize_max_payload() {
        let payload = vec![0x41; MAX_UDP_PAYLOAD];

        let packet = UDPPacket {
            source_port: 0x1234,
            dest_port: 0x5678,
            datagram_length: 0,
            checksum: 0,
            data: &payload,
        };

        let serialized = packet
            .serialize_ipv4(&SRC, &DST)
            .expect("the largest payload that fits the length field is serializable");

        assert_eq!(serialized.len(), u16::MAX as usize);
        assert_eq!(u16::from_be_slice(&serialized[4..6]), u16::MAX);
    }

    #[test]
    fn test_udp_serialize_rejects_oversized_payload() {
        let payload = vec![0x41; MAX_UDP_PAYLOAD + 1];

        let packet = UDPPacket {
            source_port: 0x1234,
            dest_port: 0x5678,
            datagram_length: 0,
            checksum: 0,
            data: &payload,
        };

        assert_eq!(
            packet.serialize_ipv4(&SRC, &DST).err(),
            Some(UDPError::LengthOutOfRange)
        );
    }

    #[test]
    fn test_udp_serialize_rejects_payload_overflowing_length_field() {
        // A payload one past the length field's range must be rejected rather than
        // wrapped around into a small, plausible looking length.
        let payload = vec![0x41; u16::MAX as usize + 1];

        let packet = UDPPacket {
            source_port: 0x1234,
            dest_port: 0x5678,
            datagram_length: 0,
            checksum: 0,
            data: &payload,
        };

        assert_eq!(
            packet.serialize_ipv4(&SRC, &DST).err(),
            Some(UDPError::LengthOutOfRange)
        );
    }
}
