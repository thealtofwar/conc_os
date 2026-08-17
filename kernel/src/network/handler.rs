use crate::rng::FromRand;
use crate::{network::device::get_net_driver, println};
use alloc::borrow::ToOwned;
use alloc::vec;
use conc_os_net::arp::{ArpCache, ArpOperation, ArpPacket, Queued};
use conc_os_net::ethernet::{EtherType, EthernetFrame, MacAddress};
use conc_os_net::ipv4::{IPProtocol, Ipv4Error, Ipv4Packet, internet_checksum};
use conc_os_net::udp::{UDPError, UDPPacket};
use conc_os_net::utils::FromSlice;
use core::net::Ipv4Addr;
use spin::{Mutex, Once};
use virtio_drivers::device::net::TxBuffer;

static NETWORK_INTERFACE: Once<Mutex<NetworkInterface>> = Once::new();
const DHCP_FIXED_LEN: usize = 240;
const DHCP_MAGIC_COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];

pub fn get_network_interface() -> &'static Mutex<NetworkInterface> {
    NETWORK_INTERFACE
        .r#try()
        .expect("network interface must be initialized")
}

pub fn init_network_interface() {
    NETWORK_INTERFACE.call_once(|| {
        let mut interface = NetworkInterface {
            mac: MacAddress {
                addr: get_net_driver().lock().mac_address(),
            },
            ipv4: None,
            gateway: None,
            subnet_mask: None,
            arp: ArpCache::new(),
            initializing_dhcp_xid: None,
        };

        interface.init_dhcp().expect("dhcp initialized");

        Mutex::new(interface)
    });
}

#[derive(Debug, PartialEq, Eq)]
pub enum DHCPError {
    RNGError(virtio_drivers::Error),
    UDPError(UDPError),
    IPv4Error(Ipv4Error),
    AlreadyStarted,
    InvalidTLV,
    MissingData,
}

pub struct NetworkInterface {
    mac: MacAddress,
    ipv4: Option<Ipv4Addr>,
    gateway: Option<Ipv4Addr>,
    subnet_mask: Option<Ipv4Addr>,
    arp: ArpCache,
    initializing_dhcp_xid: Option<u32>,
}

impl NetworkInterface {
    pub fn handle_packet(&mut self, frame: &EthernetFrame, dst: MacAddress) {
        if dst.addr != self.mac.addr && dst != MacAddress::broadcast() {
            return; // reject ethernet packets not for us
        }

        match frame {
            EthernetFrame::Arp(arp_packet) => {
                self.handle_arp(arp_packet);
            }
            EthernetFrame::Ipv4(ipv4_packet) => {
                self.handle_ipv4(ipv4_packet);
            }
            EthernetFrame::Unknown(_ethertype, _items) => {
                // println!(
                //     "got unknown ethernet frame with ethertype {ethertype} and len {}",
                //     items.len()
                // );
            }
        }
    }

    pub fn send_frame(&self, dst: MacAddress, ethertype: EtherType, payload: &[u8]) {
        let mut driver = get_net_driver().lock();

        let mut frame = vec![0; payload.len() + 14];

        // ---------------- Ethernet ----------------

        // destination
        frame[0..6].copy_from_slice(&dst.addr);

        // Source = our MAC
        frame[6..12].copy_from_slice(&self.mac.addr);

        // ethertype
        frame[12..14].copy_from_slice(&(ethertype as u16).to_be_bytes());

        frame[14..].copy_from_slice(payload);

        frame.resize(frame.len().max(60), 0);

        driver.send(TxBuffer::from(&frame)).unwrap();
    }

    pub fn send_arp_request(&self, target_addr: Ipv4Addr) {
        let request = ArpPacket {
            hardware_type: 1,
            protocol_type: 0x0800,
            hardware_len: 6,
            proto_len: 4,

            operation: ArpOperation::Request,

            sender_mac: self.mac,
            sender_addr: self.ipv4.unwrap(),

            target_mac: MacAddress::new(&[0, 0, 0, 0, 0, 0]),
            target_addr,
        };

        self.send_frame(
            MacAddress::broadcast(),
            EtherType::ARP,
            &request.serialize(),
        );
    }

    /// Sends `packet` to `dst`, resolving the next hop first if it is unknown.
    ///
    /// A packet whose next hop has not resolved yet is parked in the ARP cache
    /// and an ARP request goes out in its place. [`NetworkInterface::handle_arp`]
    /// sends it when the reply arrives. Nothing here waits: this runs inside the
    /// receive path, which is the only thing that can deliver that reply.
    pub fn send_ipv4(&mut self, dst: Ipv4Addr, packet: &Ipv4Packet) {
        // for now, we assume that the subnet mask is 255.255.255.0, and the gateway is 10.0.2.2
        let Some(my_ip) = self.ipv4 else {
            return;
        };

        let Some(mask) = self.subnet_mask else {
            return;
        };

        let dst_net = Ipv4Addr::from(u32::from(dst) & u32::from(mask));
        let my_net = Ipv4Addr::from(u32::from(my_ip) & u32::from(mask));

        let next_hop = if dst_net == my_net {
            dst
        } else {
            if let Some(gateway) = self.gateway {
                gateway
            } else {
                return;
            }
        };

        // Serialized up front: what gets parked is a finished IPv4 packet that
        // lacks only a destination MAC, so flushing it later is just a send.
        let frame = packet.serialize();

        if let Some(arp_addr) = self.arp.lookup(next_hop) {
            self.send_frame(arp_addr, EtherType::IPV4, &frame);
            return;
        }

        match self.arp.queue_pending(next_hop, frame) {
            Queued::RequestNeeded => self.send_arp_request(next_hop),
            Queued::AlreadyPending => {}
            Queued::Dropped => println!("dropped packet for {next_hop}: ARP queue full"),
        }
    }

    pub fn handle_arp(&mut self, arp_packet: &ArpPacket) {
        if arp_packet.sender_mac != self.mac {
            // `insert` hands back whatever was parked on this address, so
            // learning an address always flushes its queue.
            for frame in self
                .arp
                .insert(arp_packet.sender_addr, arp_packet.sender_mac)
            {
                self.send_frame(arp_packet.sender_mac, EtherType::IPV4, &frame);
            }
        }

        if arp_packet.operation == ArpOperation::Request
            && self.ipv4.is_some_and(|addr| arp_packet.target_addr == addr)
        {
            let reply = ArpPacket {
                hardware_type: 1,
                protocol_type: 0x0800,
                hardware_len: 6,
                proto_len: 4,

                operation: ArpOperation::Reply,

                sender_mac: self.mac,
                sender_addr: self.ipv4.unwrap(),

                target_mac: arp_packet.sender_mac,
                target_addr: arp_packet.sender_addr,
            };

            self.send_frame(arp_packet.sender_mac, EtherType::ARP, &reply.serialize());
        }
        println!(
            "ARP op={} sender={} {} target={} {}",
            arp_packet.operation as u16,
            arp_packet.sender_mac,
            arp_packet.sender_addr,
            arp_packet.target_mac,
            arp_packet.target_addr
        );
    }

    pub fn handle_ipv4(&mut self, ipv4_packet: &Ipv4Packet) {
        if ipv4_packet.more_fragments || ipv4_packet.frag_offset != 0 {
            // don't handle fragmented ipv4 packets
            return;
        }

        // Accept only what is addressed to this host. Stated as an allow-list so
        // that having no address yet fails closed: without the DHCP arm, a packet
        // arriving before the lease is bound has nothing to match against.
        let for_us = ipv4_packet.dest == Ipv4Addr::BROADCAST
            || self.ipv4.is_some_and(|my_ip| ipv4_packet.dest == my_ip)
            // A server may answer the discover by unicast to our MAC, at an
            // address we do not hold yet.
            || (self.ipv4.is_none() && self.initializing_dhcp_xid.is_some());

        if !for_us {
            return;
        }

        match ipv4_packet.protocol {
            IPProtocol::ICMP => self.handle_icmp(ipv4_packet),
            IPProtocol::TCP => {}
            IPProtocol::UDP => match UDPPacket::from_packet(ipv4_packet) {
                Ok(datagram) => self.handle_udp(datagram),
                Err(e) => println!("failed to parse packet {e:?}"),
            },
            IPProtocol::Unknown(_) => {
                println!("unknown ip proto")
            }
        }
    }

    pub fn handle_icmp(&mut self, packet: &Ipv4Packet) {
        // validate checksum
        if internet_checksum(&[packet.data]) != 0 {
            println!("malformed ipv4 icmp packet");
            return;
        }

        if packet.data[0] == 8 && packet.data[1] == 0 {
            let Some(src) = self.ipv4 else {
                return;
            };

            let mut new_data = packet.data.to_owned();

            new_data[0] = 0; // echo reply

            new_data[2] = 0; // zero out checksum
            new_data[3] = 0;

            let checksum = internet_checksum(&[&new_data]);

            new_data[2..4].copy_from_slice(&checksum.to_be_bytes());

            // ping packet request
            let reply = Ipv4Packet {
                version_ihl: packet.version_ihl,
                dscp_ecn: packet.dscp_ecn,
                total_length: packet.total_length,
                id: packet.id,
                frag_offset: packet.frag_offset,
                dont_fragment: packet.dont_fragment,
                more_fragments: packet.more_fragments,
                ttl: packet.ttl,
                protocol: packet.protocol,
                checksum: 0,
                source: src,
                dest: packet.source,
                options: &[],
                data: &new_data,
            };

            self.send_ipv4(reply.dest, &reply);
        } else {
            println!("unknown icmp packet type");
        }
    }

    pub fn init_dhcp(&mut self) -> Result<(), DHCPError> {
        if self.initializing_dhcp_xid.is_some() {
            return Err(DHCPError::AlreadyStarted);
        }

        let mut data = [0u8; 300];

        let xid = u32::from_rand().map_err(DHCPError::RNGError)?;

        // op, hardware type, hardware addr len, hops
        data[0..4].copy_from_slice(&[1, 1, 6, 0]);
        // random xid
        data[4..8].copy_from_slice(&xid.to_ne_bytes());
        //  [8..10] is seconds since first transmission, we leave this at 0
        data[10..12].copy_from_slice(&0x8000u16.to_be_bytes()); // broadcast flags
        //  [12..28] are ip addresses which we don't know, we leave them at 0
        data[28..34].copy_from_slice(&self.mac.addr);
        //  [34..44] is padding for the hardware address
        //  [44..108] is the server name field, leave at 0
        //  [108..236] is the file field, leave at 0
        // dhcp magic cookie
        data[236..240].copy_from_slice(&0x63825363u32.to_be_bytes());

        // dhcp type, length, values, then 0xff
        data[240..248].copy_from_slice(&[53, 1, 1, 55, 2, 1, 3, 0xff]);

        let id = u16::from_rand().map_err(DHCPError::RNGError)?;

        let datagram = UDPPacket::new(68, 67, &data).map_err(DHCPError::UDPError)?;

        let source = Ipv4Addr::UNSPECIFIED; // we don't know our IP address right now
        let dest = Ipv4Addr::BROADCAST;

        let ip_data = datagram
            .serialize_ipv4(&source, &dest)
            .map_err(DHCPError::UDPError)?;

        let pkt = Ipv4Packet::new(&ip_data, source, dest, id, IPProtocol::UDP)
            .map_err(DHCPError::IPv4Error)?;

        self.send_frame(MacAddress::broadcast(), EtherType::IPV4, &pkt.serialize());

        self.initializing_dhcp_xid = Some(xid);

        Ok(())
    }

    pub fn handle_udp(&mut self, datagram: UDPPacket) {
        if let Some(xid) = self.initializing_dhcp_xid
            && datagram.dest_port() == 68
            && datagram.source_port() == 67
        {
            self.handle_dhcp(datagram, xid);
        }
    }

    /// returns Ok((gateway, subnet_mask))
    fn parse_tlv(
        &self,
        end: &[u8],
        dhcp_packet_type: u8,
    ) -> Result<(Ipv4Addr, Ipv4Addr), DHCPError> {
        let mut seen_opt_53 = false;
        let mut gateway: Option<Ipv4Addr> = None;
        let mut subnet_mask: Option<Ipv4Addr> = None;

        let mut cur = 0;
        while (cur + 1) < end.len() {
            let tag = end[cur];
            let len = end[cur + 1] as usize;

            if cur + len + 2 > end.len() {
                return Err(DHCPError::InvalidTLV);
            }

            let data = &end[cur + 2..cur + len + 2];

            match tag {
                1 => {
                    if len != 4 {
                        return Err(DHCPError::InvalidTLV);
                    }

                    subnet_mask = Some(Ipv4Addr::from_octets(
                        data[0..4].try_into().expect("correct len"),
                    ));
                }
                3 => {
                    if len != 4 {
                        return Err(DHCPError::InvalidTLV);
                    }

                    gateway = Some(Ipv4Addr::from_octets(
                        data[0..4].try_into().expect("correct len"),
                    ));
                }
                53 => {
                    if len != 1 || end[cur + 2] != dhcp_packet_type {
                        return Err(DHCPError::InvalidTLV);
                    }
                    seen_opt_53 = true;
                }
                255 => {
                    if seen_opt_53
                        && let Some(gateway) = gateway
                        && let Some(subnet_mask) = subnet_mask
                    {
                        return Ok((gateway, subnet_mask));
                    }
                    return Err(DHCPError::MissingData);
                }
                _ => {}
            }
            // 2 for tag-len, then the value
            cur += 2 + len;
        }
        Err(DHCPError::InvalidTLV)
    }

    fn handle_dhcp(&mut self, packet: UDPPacket, xid: u32) {
        if packet.len() < DHCP_FIXED_LEN {
            return;
        }

        let msg = packet.data();

        if msg[0..3] != [2, 1, 6] || msg[236..240] != DHCP_MAGIC_COOKIE {
            return;
        }

        if u32::from_ne_slice(&msg[4..8]) != xid {
            return;
        }

        if msg[28..34] != self.mac.addr || msg[34..40] != [0u8; 6] {
            return;
        }

        let Ok((gateway, subnet_mask)) = self.parse_tlv(&msg[240..], 2) else {
            return;
        };

        self.gateway = Some(gateway);
        self.subnet_mask = Some(subnet_mask);
        self.ipv4 = Some(Ipv4Addr::from_octets(
            msg[16..20].try_into().expect("right length"),
        ));
        println!(
            "got ip addr {}, mask {}, gw {}",
            self.ipv4.unwrap(),
            self.subnet_mask.unwrap(),
            self.gateway.unwrap()
        );
    }
}
