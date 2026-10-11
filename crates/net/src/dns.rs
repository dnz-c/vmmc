//! The guest's DNS server: answers A and AAAA queries with the host's own resolver
//! (getaddrinfo), so whatever the host can resolve (VPN and hosts-file names too), the
//! guest can. Other record types get an empty answer.

use std::net::{IpAddr, ToSocketAddrs};

const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;
const TTL: u32 = 60;

/// A parsed query: its ID, flags, the question as raw bytes, and the name and type.
struct Query<'a> {
    id: [u8; 2],
    recursion_desired: bool,
    question: &'a [u8],
    name: String,
    kind: u16,
    class: u16,
}

fn parse(packet: &[u8]) -> Option<Query<'_>> {
    if packet.len() < 12 || packet[2] & 0x80 != 0 {
        return None; // too short, or a response
    }
    if u16::from_be_bytes([packet[4], packet[5]]) != 1 {
        return None; // exactly one question
    }
    let mut at = 12;
    let mut labels = Vec::new();
    loop {
        let len = usize::from(*packet.get(at)?);
        at += 1;
        if len == 0 {
            break;
        }
        if len > 63 {
            return None; // no compression in questions
        }
        labels.push(String::from_utf8_lossy(packet.get(at..at + len)?).into_owned());
        at += len;
    }
    let kind = u16::from_be_bytes([*packet.get(at)?, *packet.get(at + 1)?]);
    let class = u16::from_be_bytes([*packet.get(at + 2)?, *packet.get(at + 3)?]);
    Some(Query {
        id: [packet[0], packet[1]],
        recursion_desired: packet[2] & 0x01 != 0,
        question: &packet[12..at + 4],
        name: labels.join("."),
        kind,
        class,
    })
}

/// Resolves `name` with the host's resolver.
pub fn host_lookup(name: &str) -> Option<Vec<IpAddr>> {
    let addresses = (name, 0).to_socket_addrs().ok()?;
    Some(addresses.map(|address| address.ip()).collect())
}

/// The response to `packet`, with names resolved by `lookup` (None means the name
/// doesn't exist). Returns `None` for packets that aren't queries.
pub fn respond(packet: &[u8], lookup: impl Fn(&str) -> Option<Vec<IpAddr>>) -> Option<Vec<u8>> {
    let query = parse(packet)?;
    let mut answers: Vec<IpAddr> = Vec::new();
    let mut rcode = 0; // NOERROR
    if query.class == CLASS_IN && matches!(query.kind, TYPE_A | TYPE_AAAA) {
        match lookup(&query.name) {
            Some(addresses) => {
                answers = addresses
                    .into_iter()
                    .filter(|address| {
                        matches!(
                            (address, query.kind),
                            (IpAddr::V4(_), TYPE_A) | (IpAddr::V6(_), TYPE_AAAA)
                        )
                    })
                    .collect();
                answers.dedup();
            }
            None => rcode = 3, // NXDOMAIN
        }
    }
    let mut response = Vec::with_capacity(512);
    response.extend_from_slice(&query.id);
    // Response, recursion desired (echoed) and available.
    response.push(0x80 | u8::from(query.recursion_desired));
    response.push(0x80 | rcode);
    response.extend_from_slice(&1u16.to_be_bytes());
    response.extend_from_slice(&(answers.len() as u16).to_be_bytes());
    response.extend_from_slice(&[0, 0, 0, 0]);
    response.extend_from_slice(query.question);
    for address in answers {
        response.extend_from_slice(&[0xc0, 12]); // the name, by pointer to the question
        let (kind, data) = match address {
            IpAddr::V4(v4) => (TYPE_A, v4.octets().to_vec()),
            IpAddr::V6(v6) => (TYPE_AAAA, v6.octets().to_vec()),
        };
        response.extend_from_slice(&kind.to_be_bytes());
        response.extend_from_slice(&CLASS_IN.to_be_bytes());
        response.extend_from_slice(&TTL.to_be_bytes());
        response.extend_from_slice(&(data.len() as u16).to_be_bytes());
        response.extend_from_slice(&data);
    }
    Some(response)
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    fn query(name: &str, kind: u16) -> Vec<u8> {
        let mut packet = vec![0xab, 0xcd, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            packet.push(label.len() as u8);
            packet.extend_from_slice(label.as_bytes());
        }
        packet.push(0);
        packet.extend_from_slice(&kind.to_be_bytes());
        packet.extend_from_slice(&CLASS_IN.to_be_bytes());
        packet
    }

    fn fake(name: &str) -> Option<Vec<IpAddr>> {
        (name == "vmmc.test").then(|| {
            vec![
                IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7)),
                IpAddr::V6(Ipv6Addr::LOCALHOST),
            ]
        })
    }

    #[test]
    fn answers_a_and_aaaa_from_the_lookup() {
        let response = respond(&query("vmmc.test", TYPE_A), fake).unwrap();
        assert_eq!(&response[..2], &[0xab, 0xcd]);
        assert_eq!(response[2] & 0x80, 0x80, "a response");
        assert_eq!(response[3] & 0x0f, 0, "NOERROR");
        assert_eq!(
            u16::from_be_bytes([response[6], response[7]]),
            1,
            "one answer"
        );
        assert_eq!(&response[response.len() - 4..], &[192, 0, 2, 7]);

        let response = respond(&query("vmmc.test", TYPE_AAAA), fake).unwrap();
        assert_eq!(
            &response[response.len() - 16..],
            &Ipv6Addr::LOCALHOST.octets()
        );
    }

    #[test]
    fn unknown_names_and_types() {
        let response = respond(&query("nope.test", TYPE_A), fake).unwrap();
        assert_eq!(response[3] & 0x0f, 3, "NXDOMAIN");
        let mx = respond(&query("vmmc.test", 15), fake).unwrap();
        assert_eq!((mx[3] & 0x0f, mx[7]), (0, 0), "empty answer for MX");
        assert!(respond(&[0; 5], fake).is_none());
    }

    #[test]
    fn the_host_resolver_knows_localhost() {
        let addresses = host_lookup("localhost").unwrap();
        assert!(addresses.iter().any(|address| address.is_loopback()));
    }
}
