//! Which peers may name the address of the visitor they speak for.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;

use axum::http::HeaderMap;
use thiserror::Error;

/// The networks of the reverse proxies in front of the server.
///
/// `X-Real-IP` is believed from these peers only: from anyone else it would let
/// a visitor choose the address that limits are counted against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedProxies(Arc<[Network]>);

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TrustedProxiesError {
    #[error("`{0}` is not an IP address or a network such as 172.18.0.0/16")]
    Network(String),
    #[error("the prefix of `{0}` is longer than its address")]
    Prefix(String),
    #[error("`{0}` has address bits outside its prefix")]
    HostBits(String),
}

/// The default is the loopback addresses.
impl Default for TrustedProxies {
    fn default() -> Self {
        Self(Arc::new([
            Network {
                base: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 0)),
                prefix: 8,
            },
            Network {
                base: IpAddr::V6(Ipv6Addr::LOCALHOST),
                prefix: 128,
            },
        ]))
    }
}

impl FromStr for TrustedProxies {
    type Err = TrustedProxiesError;

    /// Reads networks separated by commas; an address alone is a network of one.
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        raw.split(',')
            .map(|entry| entry.trim().parse())
            .collect::<Result<Vec<Network>, _>>()
            .map(|networks| Self(networks.into()))
    }
}

impl fmt::Display for TrustedProxies {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (at, network) in self.0.iter().enumerate() {
            let separator = if at == 0 { "" } else { "," };
            write!(f, "{separator}{}/{}", network.base, network.prefix)?;
        }
        Ok(())
    }
}

impl TrustedProxies {
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        self.0.iter().any(|network| network.contains(ip))
    }

    /// The address of the visitor: the peer, or whom a trusted peer names.
    ///
    /// A request without a peer did not come through a socket and counts as
    /// local.
    pub fn client_ip(&self, peer: Option<SocketAddr>, headers: &HeaderMap) -> IpAddr {
        let peer = peer.map_or(IpAddr::V4(Ipv4Addr::LOCALHOST), |peer| {
            peer.ip().to_canonical()
        });
        if !self.contains(peer) {
            return peer;
        }
        headers
            .get("x-real-ip")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<IpAddr>().ok())
            .map_or(peer, |ip| ip.to_canonical())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Network {
    base: IpAddr,
    prefix: u8,
}

impl Network {
    fn contains(self, ip: IpAddr) -> bool {
        match (self.base, ip) {
            (IpAddr::V4(base), IpAddr::V4(ip)) => {
                (base.to_bits() ^ ip.to_bits()) & mask_v4(self.prefix) == 0
            }
            (IpAddr::V6(base), IpAddr::V6(ip)) => {
                (base.to_bits() ^ ip.to_bits()) & mask_v6(self.prefix) == 0
            }
            _ => false,
        }
    }
}

fn mask_v4(prefix: u8) -> u32 {
    u32::MAX
        .checked_shl(32_u32.saturating_sub(u32::from(prefix)))
        .unwrap_or(0)
}

fn mask_v6(prefix: u8) -> u128 {
    u128::MAX
        .checked_shl(128_u32.saturating_sub(u32::from(prefix)))
        .unwrap_or(0)
}

impl FromStr for Network {
    type Err = TrustedProxiesError;

    fn from_str(entry: &str) -> Result<Self, Self::Err> {
        let invalid = || TrustedProxiesError::Network(entry.to_owned());
        let (address, prefix) = match entry.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (entry, None),
        };
        let base: IpAddr = address.parse().map_err(|_| invalid())?;
        let bits = if base.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => bits,
            Some(digits) if digits.bytes().all(|byte| byte.is_ascii_digit()) => {
                digits.parse::<u8>().map_err(|_| invalid())?
            }
            Some(_) => return Err(invalid()),
        };
        if prefix > bits {
            return Err(TrustedProxiesError::Prefix(entry.to_owned()));
        }
        let network = match base {
            // Peers are compared as the IPv4 addresses they are.
            IpAddr::V6(mapped) if prefix >= 96 => match mapped.to_ipv4_mapped() {
                Some(v4) => Self {
                    base: IpAddr::V4(v4),
                    prefix: prefix - 96,
                },
                None => Self { base, prefix },
            },
            _ => Self { base, prefix },
        };
        let stray = match network.base {
            IpAddr::V4(base) => base.to_bits() & !mask_v4(network.prefix) != 0,
            IpAddr::V6(base) => base.to_bits() & !mask_v6(network.prefix) != 0,
        };
        if stray {
            return Err(TrustedProxiesError::HostBits(entry.to_owned()));
        }
        Ok(network)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxies(raw: &str) -> TrustedProxies {
        raw.parse().unwrap()
    }

    fn ip(raw: &str) -> IpAddr {
        raw.parse().unwrap()
    }

    fn named(proxies: &TrustedProxies, peer: &str, real_ip: &str) -> IpAddr {
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", real_ip.parse().unwrap());
        proxies.client_ip(Some(SocketAddr::new(ip(peer), 4000)), &headers)
    }

    #[test]
    fn only_loopback_is_trusted_unless_told_otherwise() {
        let default = TrustedProxies::default();
        for peer in ["127.0.0.1", "127.3.2.1", "::1", "::ffff:127.0.0.1"] {
            assert!(default.contains(ip(peer)), "{peer}");
        }
        for peer in ["172.18.0.1", "10.0.0.1", "::2", "::ffff:172.18.0.1"] {
            assert!(!default.contains(ip(peer)), "{peer}");
        }
    }

    #[test]
    fn networks_of_both_families_and_mapped_addresses() {
        let set = proxies("172.16.0.0/12, 192.168.65.1 ,fd00:1::/64,::ffff:10.1.0.0/112");
        for peer in [
            "172.16.0.0",
            "172.31.255.255",
            "::ffff:172.18.0.1",
            "192.168.65.1",
            "fd00:1::9",
            "10.1.255.255",
            "::ffff:10.1.2.3",
        ] {
            assert!(set.contains(ip(peer)), "{peer}");
        }
        for peer in [
            "172.15.255.255",
            "172.32.0.0",
            "192.168.65.2",
            "fd00:1:0:1::",
            "10.2.0.0",
            "127.0.0.1",
            "::1",
        ] {
            assert!(!set.contains(ip(peer)), "{peer}");
        }
        assert!(proxies("0.0.0.0/0").contains(ip("203.0.113.9")));
        assert!(!proxies("0.0.0.0/0").contains(ip("2001:db8::1")));
        assert!(proxies("::/0").contains(ip("2001:db8::1")));
        assert!(!proxies("::/0").contains(ip("203.0.113.9")));
    }

    #[test]
    fn anything_else_is_refused() {
        for raw in [
            "",
            "127.0.0.1,",
            "nginx",
            "172.18.0.0/",
            "172.18.0.0/+8",
            "172.18.0.0/8/8",
            "172.18/16",
            "fe80::1%eth0",
            "172.18.0.0/300",
        ] {
            assert!(
                matches!(
                    raw.parse::<TrustedProxies>(),
                    Err(TrustedProxiesError::Network(_))
                ),
                "{raw}"
            );
        }
        for raw in ["10.0.0.0/33", "fd00::/129"] {
            assert!(
                matches!(
                    raw.parse::<TrustedProxies>(),
                    Err(TrustedProxiesError::Prefix(_))
                ),
                "{raw}"
            );
        }
        for raw in [
            "172.18.0.1/16",
            "fd00::1/64",
            "::ffff:10.1.0.1/112",
            "::ffff:0:0/95",
        ] {
            assert!(
                matches!(
                    raw.parse::<TrustedProxies>(),
                    Err(TrustedProxiesError::HostBits(_))
                ),
                "{raw}"
            );
        }
    }

    #[test]
    fn the_header_counts_from_a_trusted_peer_only() {
        let default = TrustedProxies::default();
        assert_eq!(
            named(&default, "127.0.0.1", "203.0.113.7"),
            ip("203.0.113.7")
        );
        assert_eq!(
            named(&default, "::ffff:127.0.0.1", " 2001:db8::7 "),
            ip("2001:db8::7")
        );
        assert_eq!(
            named(&default, "172.18.0.1", "203.0.113.7"),
            ip("172.18.0.1")
        );
        assert_eq!(
            named(&default, "198.51.100.4", "203.0.113.7"),
            ip("198.51.100.4")
        );

        let bridge = proxies("172.16.0.0/12");
        assert_eq!(
            named(&bridge, "172.18.0.1", "203.0.113.7"),
            ip("203.0.113.7")
        );
        assert_eq!(
            named(&bridge, "::ffff:172.18.0.1", "::ffff:203.0.113.7"),
            ip("203.0.113.7")
        );
        // The list replaces the default.
        assert_eq!(named(&bridge, "127.0.0.1", "203.0.113.7"), ip("127.0.0.1"));
    }

    #[test]
    fn a_header_that_names_no_address_leaves_the_peer() {
        let default = TrustedProxies::default();
        for real_ip in ["unknown", "", "203.0.113.7, 10.0.0.1", "203.0.113.7:80"] {
            assert_eq!(named(&default, "127.0.0.1", real_ip), ip("127.0.0.1"));
        }
        assert_eq!(default.client_ip(None, &HeaderMap::new()), ip("127.0.0.1"));
    }

    #[test]
    fn prints_as_it_is_written() {
        assert_eq!(TrustedProxies::default().to_string(), "127.0.0.0/8,::1/128");
        assert_eq!(
            proxies("10.0.0.0/8,fd00::/8").to_string(),
            "10.0.0.0/8,fd00::/8"
        );
    }
}
