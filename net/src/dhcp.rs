//! Message encoding for the DHCP client (RFC 2131).
//!
//! Everything here is a pure transform on a UDP payload: building the message a
//! client broadcasts, and reading the reply back. Sending, retransmission and
//! lease state live with the interface that owns them.

use core::net::Ipv4Addr;
use core::ops::Range;

use crate::ethernet::MacAddress;
use crate::utils::FromSlice;

/// The ports a DHCP exchange runs over. They are fixed, so a reply can be
/// recognised before its contents are trusted.
pub const CLIENT_PORT: u16 = 68;
pub const SERVER_PORT: u16 = 67;

/// The BOOTP header plus the magic cookie: everything before the options.
pub const FIXED_LEN: usize = 240;

/// BOOTP relays and older servers may drop anything shorter than a 300 byte
/// message, so a discover is padded out to that length rather than ending after
/// its options.
pub const DISCOVER_LEN: usize = 300;

const MAGIC_COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];

const OP_BOOT_REQUEST: u8 = 1;
const OP_BOOT_REPLY: u8 = 2;
const HTYPE_ETHERNET: u8 = 1;
const HLEN_ETHERNET: u8 = 6;

/// Ask for the reply by broadcast. A client that has no address yet cannot be
/// reached by a unicast IP packet.
const FLAG_BROADCAST: u16 = 0x8000;

const XID: Range<usize> = 4..8;
const FLAGS: Range<usize> = 10..12;
/// The address the server is offering us.
const YIADDR: Range<usize> = 16..20;
/// Client hardware address: 16 bytes wide, so a MAC leaves ten zero bytes.
const CHADDR: Range<usize> = 28..44;
const CHADDR_LEN: usize = 16;
const COOKIE: Range<usize> = 236..240;

const OPT_PAD: u8 = 0;
const OPT_SUBNET_MASK: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_MESSAGE_TYPE: u8 = 53;
const OPT_PARAMETER_REQUEST_LIST: u8 = 55;
const OPT_END: u8 = 255;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DHCPError {
    /// Shorter than the fixed header, so there is nothing to read.
    TooShort,
    /// Not a reply from a server to an ethernet client.
    NotAReply,
    BadMagicCookie,
    /// Answers some other exchange.
    XidMismatch,
    /// Answers some other client.
    ClientMacMismatch,
    /// An option is truncated, or carries a length its tag does not allow.
    InvalidTLV,
    /// Well formed, but not the kind of message that was expected.
    WrongMessageType,
    /// Well formed, but missing something the client needs to configure itself.
    MissingData,
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DHCPMessageType {
    Discover = 1,
    Offer = 2,
    Request = 3,
    Decline = 4,
    Ack = 5,
    Nak = 6,
    Release = 7,
    Inform = 8,
}

impl TryFrom<u8> for DHCPMessageType {
    type Error = (); // error means the value was not a message type we know

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Discover),
            2 => Ok(Self::Offer),
            3 => Ok(Self::Request),
            4 => Ok(Self::Decline),
            5 => Ok(Self::Ack),
            6 => Ok(Self::Nak),
            7 => Ok(Self::Release),
            8 => Ok(Self::Inform),
            _ => Err(()),
        }
    }
}

/// Builds the DHCPDISCOVER a client broadcasts to open an exchange.
///
/// `xid` identifies the exchange: it is opaque, and is only ever compared
/// against the copy a server echoes back.
pub fn build_discover(client_mac: MacAddress, xid: u32) -> [u8; DISCOVER_LEN] {
    let mut msg = [0u8; DISCOVER_LEN];

    msg[0] = OP_BOOT_REQUEST;
    msg[1] = HTYPE_ETHERNET;
    msg[2] = HLEN_ETHERNET;
    // [3] hops and [8..10] seconds elapsed stay zero: nothing is relaying this
    // and no earlier attempt has been made.
    msg[XID].copy_from_slice(&xid.to_be_bytes());
    msg[FLAGS].copy_from_slice(&FLAG_BROADCAST.to_be_bytes());
    // [12..28] are addresses we do not know yet, and [44..236] name a boot
    // server and file we do not want.
    msg[CHADDR.start..CHADDR.start + 6].copy_from_slice(&client_mac.addr);
    msg[COOKIE].copy_from_slice(&MAGIC_COOKIE);

    msg[FIXED_LEN..FIXED_LEN + 8].copy_from_slice(&[
        OPT_MESSAGE_TYPE,
        1,
        DHCPMessageType::Discover as u8,
        OPT_PARAMETER_REQUEST_LIST,
        2,
        OPT_SUBNET_MASK,
        OPT_ROUTER,
        OPT_END,
    ]);
    // The remainder is the padding that brings the message up to DISCOVER_LEN.

    msg
}

/// The parts of a DHCPOFFER a client needs to configure an interface.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DHCPOffer {
    pub offered_addr: Ipv4Addr,
    pub subnet_mask: Ipv4Addr,
    pub gateway: Ipv4Addr,
}

/// Options recognised while parsing. Absent is distinct from malformed: a
/// missing option is only an error once we know which message it belonged to.
#[derive(Default)]
struct Options {
    message_type: Option<DHCPMessageType>,
    subnet_mask: Option<Ipv4Addr>,
    router: Option<Ipv4Addr>,
}

impl DHCPOffer {
    /// Reads an offer out of a DHCP message.
    ///
    /// `xid` and `client_mac` are the ones the discover went out with. A server
    /// answering a different exchange, or a different client, is rejected here
    /// rather than being allowed to configure this interface: the reply arrives
    /// by broadcast, so other clients' offers reach us too.
    pub fn from_message(msg: &[u8], xid: u32, client_mac: MacAddress) -> Result<Self, DHCPError> {
        if msg.len() < FIXED_LEN {
            return Err(DHCPError::TooShort);
        }

        if msg[0] != OP_BOOT_REPLY || msg[1] != HTYPE_ETHERNET || msg[2] != HLEN_ETHERNET {
            return Err(DHCPError::NotAReply);
        }

        if msg[COOKIE] != MAGIC_COOKIE {
            return Err(DHCPError::BadMagicCookie);
        }

        if u32::from_be_slice(&msg[XID]) != xid {
            return Err(DHCPError::XidMismatch);
        }

        // Compared over the whole field: a six byte MAC that matches but leaves
        // junk in the padding is not this client's hardware address.
        let mut chaddr = [0u8; CHADDR_LEN];
        chaddr[..6].copy_from_slice(&client_mac.addr);

        if msg[CHADDR] != chaddr {
            return Err(DHCPError::ClientMacMismatch);
        }

        let options = parse_options(&msg[FIXED_LEN..])?;

        if options.message_type != Some(DHCPMessageType::Offer) {
            return Err(DHCPError::WrongMessageType);
        }

        let (Some(subnet_mask), Some(gateway)) = (options.subnet_mask, options.router) else {
            return Err(DHCPError::MissingData);
        };

        let offered_addr =
            Ipv4Addr::from_octets(msg[YIADDR].try_into().expect("yiaddr is a four byte field"));

        // An offer of the unspecified address offers nothing.
        if offered_addr.is_unspecified() {
            return Err(DHCPError::MissingData);
        }

        Ok(Self {
            offered_addr,
            subnet_mask,
            gateway,
        })
    }
}

/// Walks the options field, which is a run of tag-length-value triples closed by
/// an End option. Anything past End is padding and is not read.
fn parse_options(options: &[u8]) -> Result<Options, DHCPError> {
    let mut parsed = Options::default();

    let mut cur = 0;
    while cur < options.len() {
        let tag = options[cur];

        // Pad and End are the two tags that carry neither a length nor a value.
        if tag == OPT_PAD {
            cur += 1;
            continue;
        }

        if tag == OPT_END {
            return Ok(parsed);
        }

        let Some(&len) = options.get(cur + 1) else {
            return Err(DHCPError::InvalidTLV);
        };

        let len = len as usize;

        let Some(value) = options.get(cur + 2..cur + 2 + len) else {
            return Err(DHCPError::InvalidTLV);
        };

        match tag {
            OPT_SUBNET_MASK => parsed.subnet_mask = Some(parse_addr(value)?),
            OPT_ROUTER => {
                // A list of routers in order of preference. We take the first
                // and ignore the rest, having nowhere to put a second gateway.
                if value.is_empty() || value.len() % 4 != 0 {
                    return Err(DHCPError::InvalidTLV);
                }

                parsed.router = Some(parse_addr(&value[..4])?);
            }
            OPT_MESSAGE_TYPE => {
                parsed.message_type = Some(
                    value
                        .first()
                        .filter(|_| value.len() == 1)
                        .and_then(|ty| DHCPMessageType::try_from(*ty).ok())
                        .ok_or(DHCPError::InvalidTLV)?,
                );
            }
            _ => {}
        }

        cur += 2 + len;
    }

    // Options that simply run out have been truncated: End is what says the
    // server had nothing more to send.
    Err(DHCPError::InvalidTLV)
}

fn parse_addr(value: &[u8]) -> Result<Ipv4Addr, DHCPError> {
    let octets: [u8; 4] = value.try_into().map_err(|_| DHCPError::InvalidTLV)?;

    Ok(Ipv4Addr::from_octets(octets))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The client in the capture below.
    const CLIENT_MAC: MacAddress = MacAddress {
        addr: [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
    };

    /// The exchange id in the capture, read the way it sits on the wire.
    const CAPTURE_XID: u32 = 0xbcb0_ed7d;

    /// A discover this client sent to QEMU's built in server, captured off the
    /// wire. Building the same message from the same inputs has to reproduce it
    /// byte for byte.
    const DISCOVER: [u8; 300] = [
        0x01, 0x01, 0x06, 0x00, 0xbc, 0xb0, 0xed, 0x7d, 0x00, 0x00, 0x80, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x52, 0x54, 0x00, 0x12, 0x34, 0x56, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x63, 0x82, 0x53, 0x63, //
        0x35, 0x01, 0x01, 0x37, 0x02, 0x01, 0x03, 0xff, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
    ];

    /// The offer that answered it, from the same capture, truncated at the End
    /// option. On the wire the message ran to 548 bytes; the rest was the zero
    /// padding that [`test_offer_ignores_padding_after_end`] appends back.
    const OFFER: [u8; 274] = [
        0x02, 0x01, 0x06, 0x00, 0xbc, 0xb0, 0xed, 0x7d, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x02, 0x0f, 0x0a, 0x00, 0x02, 0x02, //
        0x00, 0x00, 0x00, 0x00, 0x52, 0x54, 0x00, 0x12, 0x34, 0x56, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x63, 0x82, 0x53, 0x63, //
        0x35, 0x01, 0x02, 0x36, 0x04, 0x0a, 0x00, 0x02, 0x02, 0x01, 0x04, 0xff, //
        0xff, 0xff, 0x00, 0x03, 0x04, 0x0a, 0x00, 0x02, 0x02, 0x06, 0x04, 0x0a, //
        0x00, 0x02, 0x03, 0x33, 0x04, 0x00, 0x01, 0x51, 0x80, 0xff, //
    ];

    /// The options the capture's server sent, as they appear from [`FIXED_LEN`].
    const OFFER_OPTIONS: &[u8] = &[
        53, 1, 2, // offer
        54, 4, 10, 0, 2, 2, // server identifier
        1, 4, 255, 255, 255, 0, // subnet mask
        3, 4, 10, 0, 2, 2, // router
        6, 4, 10, 0, 2, 3, // domain name server
        51, 4, 0, 1, 0x51, 0x80, // lease time
        255,
    ];

    /// Builds an offer around `options`, so that a test can vary the options
    /// without restating the header.
    fn offer_with(options: &[u8]) -> Vec<u8> {
        let mut msg = OFFER[..FIXED_LEN].to_vec();
        msg.extend_from_slice(options);
        msg
    }

    fn parse(msg: &[u8]) -> Result<DHCPOffer, DHCPError> {
        DHCPOffer::from_message(msg, CAPTURE_XID, CLIENT_MAC)
    }

    #[test]
    fn test_build_discover_matches_capture() {
        assert_eq!(build_discover(CLIENT_MAC, CAPTURE_XID), DISCOVER);
    }

    #[test]
    fn test_build_discover_is_padded_to_bootp_minimum() {
        let msg = build_discover(CLIENT_MAC, CAPTURE_XID);

        // Everything past the options is padding, and the options themselves
        // end well before the message does.
        assert_eq!(msg.len(), DISCOVER_LEN);
        assert_eq!(msg[FIXED_LEN + 7], OPT_END);
        assert!(msg[FIXED_LEN + 8..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn test_build_discover_carries_xid_and_mac() {
        // The two fields a server echoes back, and the only ones that tie the
        // reply to this client.
        let msg = build_discover(CLIENT_MAC, 0x0123_4567);

        assert_eq!(u32::from_be_slice(&msg[XID]), 0x0123_4567);
        assert_eq!(msg[28..34], CLIENT_MAC.addr);
        // The rest of the 16 byte hardware address field stays zero.
        assert_eq!(msg[34..44], [0u8; 10]);
    }

    #[test]
    fn test_build_discover_requests_broadcast_reply() {
        let msg = build_discover(CLIENT_MAC, CAPTURE_XID);

        assert_eq!(u16::from_be_slice(&msg[FLAGS]), FLAG_BROADCAST);
    }

    #[test]
    fn test_parse_offer_from_capture() {
        let offer = parse(&OFFER).expect("failed to parse a captured offer");

        assert_eq!(
            offer,
            DHCPOffer {
                offered_addr: Ipv4Addr::new(10, 0, 2, 15),
                subnet_mask: Ipv4Addr::new(255, 255, 255, 0),
                gateway: Ipv4Addr::new(10, 0, 2, 2),
            }
        );
    }

    #[test]
    fn test_offer_ignores_padding_after_end() {
        // The full 548 bytes as captured: End, then padding out to the length
        // the server chose to send.
        let mut msg = OFFER.to_vec();
        msg.resize(548, 0);

        assert_eq!(parse(&msg), parse(&OFFER));
    }

    #[test]
    fn test_offer_ignores_junk_after_end() {
        // Nothing past End is read, so it cannot make a valid offer invalid.
        let mut msg = OFFER.to_vec();
        msg.extend_from_slice(&[3, 4, 192, 168]); // a truncated router option

        assert_eq!(parse(&msg), parse(&OFFER));
    }

    #[test]
    fn test_offer_accepts_end_as_last_byte() {
        // The options need no padding at all: a message may stop at End.
        let msg = offer_with(OFFER_OPTIONS);

        assert_eq!(*msg.last().expect("the message is not empty"), OPT_END);
        assert_eq!(parse(&msg), parse(&OFFER));
    }

    #[test]
    fn test_offer_skips_pad_options() {
        // Pad carries no length byte, so a parser that reads one would consume
        // the tag of whatever follows.
        let mut options = vec![OPT_PAD, OPT_PAD];
        options.extend_from_slice(&[53, 1, 2, OPT_PAD]);
        options.extend_from_slice(&[1, 4, 255, 255, 255, 0, OPT_PAD]);
        options.extend_from_slice(&[3, 4, 10, 0, 2, 2, 255]);

        assert_eq!(parse(&offer_with(&options)), parse(&OFFER));
    }

    #[test]
    fn test_offer_skips_unknown_options() {
        // The server identifier, DNS and lease time options in the capture are
        // all stepped over by their length.
        let options = [53, 1, 2, 1, 4, 255, 255, 255, 0, 3, 4, 10, 0, 2, 2, 255];

        assert_eq!(parse(&offer_with(&options)), parse(&OFFER));
    }

    #[test]
    fn test_offer_skips_zero_length_option() {
        let mut options = vec![116, 0]; // auto configure, no value
        options.extend_from_slice(OFFER_OPTIONS);

        assert_eq!(parse(&offer_with(&options)), parse(&OFFER));
    }

    #[test]
    fn test_offer_takes_first_router() {
        let options = [
            53, 1, 2, //
            1, 4, 255, 255, 255, 0, //
            3, 8, 10, 0, 2, 2, 10, 0, 2, 9, // two routers, in preference order
            255,
        ];

        let offer = parse(&offer_with(&options)).expect("a router list is valid");

        assert_eq!(offer.gateway, Ipv4Addr::new(10, 0, 2, 2));
    }

    #[test]
    fn test_offer_rejects_short_message() {
        // One byte short of the fixed header: the magic cookie is incomplete.
        assert_eq!(parse(&OFFER[..FIXED_LEN - 1]), Err(DHCPError::TooShort));
        assert_eq!(parse(&[]), Err(DHCPError::TooShort));
    }

    #[test]
    fn test_offer_rejects_header_without_options() {
        // Long enough to read, but the server sent no options at all.
        assert_eq!(parse(&OFFER[..FIXED_LEN]), Err(DHCPError::InvalidTLV));
    }

    #[test]
    fn test_offer_rejects_request_op() {
        // Our own discover, looped back to us: same xid, same MAC, but it is a
        // request rather than a reply.
        let mut msg = OFFER;
        msg[0] = OP_BOOT_REQUEST;

        assert_eq!(parse(&msg), Err(DHCPError::NotAReply));
    }

    #[test]
    fn test_offer_rejects_non_ethernet_hardware() {
        for (index, byte) in [(1, 6), (2, 16)] {
            let mut msg = OFFER;
            msg[index] = byte;

            assert_eq!(parse(&msg), Err(DHCPError::NotAReply));
        }
    }

    #[test]
    fn test_offer_rejects_bad_magic_cookie() {
        let mut msg = OFFER;
        msg[COOKIE][3] ^= 0xff;

        assert_eq!(parse(&msg), Err(DHCPError::BadMagicCookie));
    }

    #[test]
    fn test_offer_rejects_other_exchange() {
        let mut msg = OFFER;
        msg[XID][0] ^= 0xff;

        assert_eq!(parse(&msg), Err(DHCPError::XidMismatch));
    }

    #[test]
    fn test_offer_rejects_other_client() {
        // Offers arrive by broadcast, so another client's reply reaches us.
        let mut msg = OFFER;
        msg[CHADDR][5] ^= 0xff;

        assert_eq!(parse(&msg), Err(DHCPError::ClientMacMismatch));
    }

    #[test]
    fn test_offer_rejects_dirty_hardware_address_padding() {
        // The MAC matches, but the field is not the 16 byte address we sent.
        let mut msg = OFFER;
        msg[CHADDR][6] = 0x01;

        assert_eq!(parse(&msg), Err(DHCPError::ClientMacMismatch));
    }

    #[test]
    fn test_offer_rejects_wrong_message_type() {
        for message_type in [
            DHCPMessageType::Discover,
            DHCPMessageType::Request,
            DHCPMessageType::Ack,
            DHCPMessageType::Nak,
        ] {
            let mut options = OFFER_OPTIONS.to_vec();
            options[2] = message_type as u8;

            assert_eq!(
                parse(&offer_with(&options)),
                Err(DHCPError::WrongMessageType),
                "{message_type:?} is not an offer"
            );
        }
    }

    #[test]
    fn test_offer_rejects_missing_message_type() {
        // Without option 53 this is a BOOTP reply, not a DHCP offer.
        let options = [1, 4, 255, 255, 255, 0, 3, 4, 10, 0, 2, 2, 255];

        assert_eq!(
            parse(&offer_with(&options)),
            Err(DHCPError::WrongMessageType)
        );
    }

    #[test]
    fn test_offer_rejects_unknown_message_type() {
        let mut options = OFFER_OPTIONS.to_vec();
        options[2] = 9; // past the message types RFC 2131 defines

        assert_eq!(parse(&offer_with(&options)), Err(DHCPError::InvalidTLV));
    }

    #[test]
    fn test_offer_rejects_missing_subnet_mask() {
        let options = [53, 1, 2, 3, 4, 10, 0, 2, 2, 255];

        assert_eq!(parse(&offer_with(&options)), Err(DHCPError::MissingData));
    }

    #[test]
    fn test_offer_rejects_missing_router() {
        let options = [53, 1, 2, 1, 4, 255, 255, 255, 0, 255];

        assert_eq!(parse(&offer_with(&options)), Err(DHCPError::MissingData));
    }

    #[test]
    fn test_offer_rejects_unspecified_address() {
        let mut msg = OFFER;
        msg[YIADDR].copy_from_slice(&Ipv4Addr::UNSPECIFIED.octets());

        assert_eq!(parse(&msg), Err(DHCPError::MissingData));
    }

    #[test]
    fn test_offer_rejects_missing_end() {
        let mut options = OFFER_OPTIONS.to_vec();
        options.pop(); // drop End, leaving the options to just run out

        assert_eq!(parse(&offer_with(&options)), Err(DHCPError::InvalidTLV));
    }

    #[test]
    fn test_offer_rejects_option_running_past_the_message() {
        // A length that reaches beyond the last byte must not be read as a
        // value that happens to be short.
        let options = [53, 1, 2, 1, 4, 255, 255, 255, 0, 3, 8, 10, 0, 2, 2, 255];

        assert_eq!(parse(&offer_with(&options)), Err(DHCPError::InvalidTLV));
    }

    #[test]
    fn test_offer_rejects_tag_without_length() {
        let mut options = OFFER_OPTIONS.to_vec();
        options.pop();
        options.push(3); // a tag as the final byte, with no length to follow

        assert_eq!(parse(&offer_with(&options)), Err(DHCPError::InvalidTLV));
    }

    #[test]
    fn test_offer_rejects_bad_option_lengths() {
        // Each of these is a well formed triple whose length is wrong for its
        // tag, so only the per-option check can reject it.
        let cases: [&[u8]; 6] = [
            &[53, 0, 1, 4, 255, 255, 255, 0, 3, 4, 10, 0, 2, 2, 255],
            &[53, 2, 2, 2, 1, 4, 255, 255, 255, 0, 3, 4, 10, 0, 2, 2, 255],
            &[53, 1, 2, 1, 0, 3, 4, 10, 0, 2, 2, 255],
            &[53, 1, 2, 1, 3, 255, 255, 255, 3, 4, 10, 0, 2, 2, 255],
            &[53, 1, 2, 1, 4, 255, 255, 255, 0, 3, 0, 255],
            &[
                53, 1, 2, 1, 4, 255, 255, 255, 0, 3, 6, 10, 0, 2, 2, 10, 0, 255,
            ],
        ];

        for options in cases {
            assert_eq!(
                parse(&offer_with(options)),
                Err(DHCPError::InvalidTLV),
                "{options:?}"
            );
        }
    }

    #[test]
    fn test_offer_checks_identity_before_contents() {
        // A malformed offer for another client is reported as another client's:
        // its contents were never ours to validate.
        let mut msg = OFFER;
        msg[XID][0] ^= 0xff;
        msg[FIXED_LEN] = 3; // truncate the first option

        assert_eq!(parse(&msg), Err(DHCPError::XidMismatch));
    }

    #[test]
    fn test_discover_parses_as_its_own_message_type() {
        // The discover we build is a request, so reading it back as an offer
        // fails on the op field rather than anywhere deeper.
        let msg = build_discover(CLIENT_MAC, CAPTURE_XID);

        assert_eq!(parse(&msg), Err(DHCPError::NotAReply));
    }

    #[test]
    fn test_message_type_round_trip() {
        for message_type in [
            DHCPMessageType::Discover,
            DHCPMessageType::Offer,
            DHCPMessageType::Request,
            DHCPMessageType::Decline,
            DHCPMessageType::Ack,
            DHCPMessageType::Nak,
            DHCPMessageType::Release,
            DHCPMessageType::Inform,
        ] {
            assert_eq!(
                DHCPMessageType::try_from(message_type as u8),
                Ok(message_type)
            );
        }

        assert_eq!(DHCPMessageType::try_from(0), Err(()));
        assert_eq!(DHCPMessageType::try_from(9), Err(()));
    }
}
