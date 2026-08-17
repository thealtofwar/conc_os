use crate::rng::FromRand;
use crate::{network::device::get_net_driver, println};
use alloc::borrow::ToOwned;
use alloc::vec;
use conc_os_net::arp::{ArpCache, ArpOperation, ArpPacket, Queued};
use conc_os_net::dhcp::{self, DHCPOffer};
use conc_os_net::ethernet::{EtherType, EthernetFrame, MacAddress};
use conc_os_net::ipv4::{IPProtocol, Ipv4Error, Ipv4Packet, internet_checksum};
use conc_os_net::udp::{UDPError, UDPPacket};
use core::net::Ipv4Addr;
use spin::{Mutex, Once};
use virtio_drivers::device::net::TxBuffer;

static NETWORK_INTERFACE: Once<Mutex<NetworkInterface>> = Once::new();

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

        let xid = u32::from_rand().map_err(DHCPError::RNGError)?;

        let data = dhcp::build_discover(self.mac, xid);

        let id = u16::from_rand().map_err(DHCPError::RNGError)?;

        let datagram = UDPPacket::new(dhcp::CLIENT_PORT, dhcp::SERVER_PORT, &data)
            .map_err(DHCPError::UDPError)?;

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
            && datagram.dest_port() == dhcp::CLIENT_PORT
            && datagram.source_port() == dhcp::SERVER_PORT
        {
            self.handle_dhcp(datagram, xid);
        }
    }

    fn handle_dhcp(&mut self, packet: UDPPacket, xid: u32) {
        let offer = match DHCPOffer::from_message(packet.data(), xid, self.mac) {
            Ok(offer) => offer,
            // Offers are broadcast, so an exchange that is not ours is the
            // ordinary case and not worth reporting.
            Err(dhcp::DHCPError::XidMismatch | dhcp::DHCPError::ClientMacMismatch) => return,
            Err(e) => {
                println!("ignoring DHCP reply: {e:?}");
                return;
            }
        };

        self.ipv4 = Some(offer.offered_addr);
        self.subnet_mask = Some(offer.subnet_mask);
        self.gateway = Some(offer.gateway);

        println!(
            "got ip addr {}, mask {}, gw {}",
            offer.offered_addr, offer.subnet_mask, offer.gateway
        );
    }
}
