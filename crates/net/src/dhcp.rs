//! A DHCP server with one lease: the guest always gets the same address (RFC 2131).

use std::net::Ipv4Addr;

use crate::NatConfig;

const BOOTREQUEST: u8 = 1;
const BOOTREPLY: u8 = 2;
const MAGIC_COOKIE: [u8; 4] = [99, 130, 83, 99];
/// Fixed BOOTP header up to the options.
const HEADER: usize = 236;

// Message types (option 53).
const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const ACK: u8 = 5;
const NAK: u8 = 6;
const INFORM: u8 = 8;

const LEASE_SECONDS: u32 = 24 * 3600;

/// The reply to a DHCP message from the guest, if it needs one.
pub fn reply(config: &NatConfig, request: &[u8]) -> Option<Vec<u8>> {
    if request.len() < HEADER + 4 || request[0] != BOOTREQUEST || request[1] != 1 || request[2] != 6
    {
        return None;
    }
    if request[HEADER..HEADER + 4] != MAGIC_COOKIE {
        return None;
    }
    let options = parse_options(&request[HEADER + 4..]);
    let message_type = options
        .iter()
        .find(|(code, _)| *code == 53)?
        .1
        .first()
        .copied()?;
    let requested = options
        .iter()
        .find(|(code, _)| *code == 50)
        .and_then(|(_, value)| <[u8; 4]>::try_from(value.as_slice()).ok())
        .map(Ipv4Addr::from);
    let (reply_type, address) = match message_type {
        DISCOVER => (OFFER, config.guest),
        REQUEST => match requested {
            Some(address) if address != config.guest => (NAK, Ipv4Addr::UNSPECIFIED),
            _ => (ACK, config.guest),
        },
        // INFORM: configuration only, no address.
        INFORM => (ACK, Ipv4Addr::UNSPECIFIED),
        _ => return None,
    };

    let mut reply = vec![0u8; HEADER];
    reply[0] = BOOTREPLY;
    reply[1] = 1; // Ethernet
    reply[2] = 6;
    reply[4..8].copy_from_slice(&request[4..8]); // transaction ID
    reply[10..12].copy_from_slice(&request[10..12]); // flags
    reply[12..16].copy_from_slice(&request[12..16]); // client's current address
    reply[16..20].copy_from_slice(&address.octets()); // "your" address
    reply[20..24].copy_from_slice(&config.gateway.octets()); // next server
    reply[28..44].copy_from_slice(&request[28..44]); // client hardware address
    reply.extend_from_slice(&MAGIC_COOKIE);
    let mut option = |code: u8, value: &[u8]| {
        reply.push(code);
        reply.push(value.len() as u8);
        reply.extend_from_slice(value);
    };
    option(53, &[reply_type]);
    option(54, &config.gateway.octets());
    if reply_type != NAK {
        if message_type != INFORM {
            option(51, &LEASE_SECONDS.to_be_bytes());
            option(58, &(LEASE_SECONDS / 2).to_be_bytes()); // renewal
            option(59, &(LEASE_SECONDS / 8 * 7).to_be_bytes()); // rebinding
        }
        option(1, &config.netmask().octets());
        option(3, &config.gateway.octets());
        option(6, &config.dns.octets());
        option(28, &config.broadcast().octets());
        option(15, config.domain.as_bytes());
    }
    reply.push(255);
    // Some clients want at least a BOOTP-sized message.
    if reply.len() < 300 {
        reply.resize(300, 0);
    }
    Some(reply)
}

fn parse_options(mut bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut options = Vec::new();
    while let Some((&code, rest)) = bytes.split_first() {
        match code {
            0 => bytes = rest,
            255 => break,
            _ => {
                let Some((&len, rest)) = rest.split_first() else {
                    break;
                };
                let Some(value) = rest.get(..usize::from(len)) else {
                    break;
                };
                options.push((code, value.to_vec()));
                bytes = &rest[usize::from(len)..];
            }
        }
    }
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(kind: u8, requested: Option<Ipv4Addr>) -> Vec<u8> {
        let mut message = vec![0u8; HEADER];
        message[0] = BOOTREQUEST;
        message[1] = 1;
        message[2] = 6;
        message[4..8].copy_from_slice(&0x1234_5678u32.to_be_bytes());
        message[28..34].copy_from_slice(&[0x52, 0x54, 0, 1, 2, 3]);
        message.extend_from_slice(&MAGIC_COOKIE);
        message.extend_from_slice(&[53, 1, kind]);
        if let Some(address) = requested {
            message.extend_from_slice(&[50, 4]);
            message.extend_from_slice(&address.octets());
        }
        message.push(255);
        message
    }

    fn reply_options(reply: &[u8]) -> Vec<(u8, Vec<u8>)> {
        parse_options(&reply[HEADER + 4..])
    }

    fn option(options: &[(u8, Vec<u8>)], code: u8) -> Vec<u8> {
        options.iter().find(|(c, _)| *c == code).unwrap().1.clone()
    }

    #[test]
    fn discover_then_request_gets_the_lease() {
        let config = NatConfig::default();
        let offer = reply(&config, &message(DISCOVER, None)).unwrap();
        assert_eq!(offer[0], BOOTREPLY);
        assert_eq!(&offer[4..8], &0x1234_5678u32.to_be_bytes());
        assert_eq!(&offer[16..20], &config.guest.octets());
        let options = reply_options(&offer);
        assert_eq!(option(&options, 53), [OFFER]);
        assert_eq!(option(&options, 3), config.gateway.octets());
        assert_eq!(option(&options, 6), config.dns.octets());
        assert_eq!(option(&options, 1), [255, 255, 255, 0]);

        let ack = reply(&config, &message(REQUEST, Some(config.guest))).unwrap();
        assert_eq!(option(&reply_options(&ack), 53), [ACK]);
        let nak = reply(
            &config,
            &message(REQUEST, Some(Ipv4Addr::new(10, 0, 2, 99))),
        )
        .unwrap();
        assert_eq!(option(&reply_options(&nak), 53), [NAK]);
    }

    #[test]
    fn ignores_what_isnt_a_dhcp_request() {
        let config = NatConfig::default();
        assert!(reply(&config, &[0; 10]).is_none());
        let mut release = message(7, None);
        assert!(
            reply(&config, &release).is_none(),
            "RELEASE needs no answer"
        );
        release[0] = BOOTREPLY;
        assert!(reply(&config, &release).is_none());
    }
}
