use crate::ipv4::{Ipv4Packet, internet_checksum};
use crate::utils::FromSlice;

pub struct UDPPacket<'a> {
    source_port: u16,
    dest_port: u16,
    datagram_length: u16,
    checksum: u16,
    data: &'a [u8],
}

impl<'a> UDPPacket<'a> {
    pub fn new(packet: &'a Ipv4Packet) -> Result<Self, ()> {
        let source_port = u16::from_be_slice(&packet.data[0..2]);

        let dest_port = u16::from_be_slice(&packet.data[2..4]);

        let datagram_len = u16::from_be_slice(&packet.data[4..6]);

        let checksum = u16::from_be_slice(&packet.data[6..8]);

        if internet_checksum(&[
            &packet.source.octets(),
            &packet.dest.octets(),
            &[0x0, 0x11],
            &packet.data,
        ]) != 0
        {
            return Err(());
        }

        Ok(UDPPacket {
            source_port,
            dest_port,
            datagram_length: datagram_len,
            checksum,
            data: &packet.data[8..],
        })
    }
}
