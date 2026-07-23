use core::net::Ipv4Addr;

struct MacAddress([u8; 6]);

pub struct NetworkInterface {
    mac: MacAddress,
    ipv4: Option<Ipv4Addr>,
}
