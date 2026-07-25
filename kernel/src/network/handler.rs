use crate::{network::device::get_net_driver, println};
use alloc::borrow::ToOwned;
use alloc::vec;
use conc_os_net::arp::{ArpCache, ArpOperation, ArpPacket};
use conc_os_net::ethernet::{EtherType, EthernetFrame, MacAddress};
use conc_os_net::ipv4::{IPProtocol, Ipv4Packet, internet_checksum};
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
        Mutex::new(NetworkInterface {
            mac: MacAddress {
                addr: get_net_driver().lock().mac_address(),
            },
            ipv4: Some(Ipv4Addr::from_octets([192, 168, 100, 2])),
            arp: ArpCache::new(),
        })
    });
}

pub struct NetworkInterface {
    mac: MacAddress,
    ipv4: Option<Ipv4Addr>,
    arp: ArpCache,
}

impl NetworkInterface {
    pub fn handle_packet(&mut self, frame: &EthernetFrame) {
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

            operation: ArpOperation::Reply,

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

    pub fn send_ipv4(&self, dst: Ipv4Addr, packet: &Ipv4Packet) {
        // for now, we assume that the subnet mask is 255.255.255.0, and the gateway is 10.0.2.2
        let Some(my_ip) = self.ipv4 else {
            return;
        };

        let next_hop = if dst.octets()[0..3] == my_ip.octets()[0..3] {
            dst
        } else {
            Ipv4Addr::new(10, 0, 2, 2)
        };

        let Some(arp_addr) = self.arp.lookup(next_hop) else {
            unimplemented!("not in arp cache");
        };

        println!("arp addr is {}", arp_addr);

        self.send_frame(arp_addr, EtherType::IPV4, &packet.serialize());
    }

    pub fn handle_arp(&mut self, arp_packet: &ArpPacket) {
        if arp_packet.sender_mac != self.mac {
            self.arp
                .insert(arp_packet.sender_addr, arp_packet.sender_mac);
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

    pub fn handle_ipv4(&self, ipv4_packet: &Ipv4Packet) {
        let Some(my_ip) = self.ipv4 else {
            return;
        };

        if ipv4_packet.dest != my_ip {
            return;
        }

        match ipv4_packet.protocol {
            IPProtocol::ICMP => self.handle_icmp(ipv4_packet),
            IPProtocol::TCP => {}
            IPProtocol::UDP => {}
            IPProtocol::Unknown(_) => {
                println!("unknown ip proto")
            }
        }
    }

    pub fn handle_icmp(&self, packet: &Ipv4Packet) {
        // validate checksum
        if internet_checksum(packet.data) != 0 {
            println!("malformed ipv4 icmp packet");
        }

        if packet.data[0] == 8 && packet.data[1] == 0 {
            let mut new_data = packet.data.to_owned();

            new_data[0] = 0; // echo reply

            new_data[2] = 0; // zero out checksum
            new_data[3] = 0;

            let checksum = internet_checksum(&new_data);

            new_data[2..4].copy_from_slice(&checksum.to_be_bytes());

            // ping packet request
            let reply = Ipv4Packet {
                version_ihl: packet.version_ihl,
                dscp_ecn: packet.dscp_ecn,
                length: packet.length,
                id: packet.id,
                frag_offset: packet.frag_offset,
                dont_fragment: packet.dont_fragment,
                more_fragments: packet.more_fragments,
                ttl: packet.ttl,
                protocol: packet.protocol,
                checksum: 0,
                source: self.ipv4.expect("must have ipv4 to handle icmp packet"),
                dest: packet.source,
                options: &[],
                data: &new_data,
            };

            self.send_ipv4(reply.dest, &reply);
        } else {
            println!("unknown icmp packet type");
        }
    }
}
