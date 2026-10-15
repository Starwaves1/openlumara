//! SSRF protection: `_BLOCKED_NETWORKS` + `_ip_is_blocked` from modules/http/module.py,
//! including python `ipaddress` `is_private` / `is_reserved` / ... semantics (python 3.14 tables).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

fn v4_in(ip: Ipv4Addr, net: &str) -> bool {
    let (a, p) = net.split_once('/').unwrap();
    let base: Ipv4Addr = a.parse().unwrap();
    let p: u32 = p.parse().unwrap();
    let mask = if p == 0 { 0 } else { u32::MAX << (32 - p) };
    (u32::from(ip) & mask) == (u32::from(base) & mask)
}

fn v6_in(ip: Ipv6Addr, net: &str) -> bool {
    let (a, p) = net.split_once('/').unwrap();
    let base: Ipv6Addr = a.parse().unwrap();
    let p: u32 = p.parse().unwrap();
    let mask = if p == 0 { 0 } else { u128::MAX << (128 - p) };
    (u128::from(ip) & mask) == (u128::from(base) & mask)
}

const BLOCKED_V4: &[&str] = &[
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.0.0.0/24",
    "192.0.2.0/24",
    "192.88.99.0/24",
    "192.168.0.0/16",
    "198.18.0.0/15",
    "198.51.100.0/24",
    "203.0.113.0/24",
    "224.0.0.0/4",
    "240.0.0.0/4",
    "255.255.255.255/32",
];

const BLOCKED_V6: &[&str] = &["::/128", "::1/128", "::ffff:0:0/96", "64:ff9b::/96", "fc00::/7", "fe80::/10", "ff00::/8", "2001:db8::/32"];

// python ipaddress._IPv6Constants (3.14)
const V6_PRIVATE: &[&str] = &[
    "::1/128",
    "::/128",
    "::ffff:0.0.0.0/96",
    "64:ff9b:1::/48",
    "100::/64",
    "2001::/23",
    "2001:db8::/32",
    "2002::/16",
    "3fff::/20",
    "fc00::/7",
    "fe80::/10",
];
const V6_PRIVATE_EXCEPTIONS: &[&str] = &["2001:1::1/128", "2001:1::2/128", "2001:3::/32", "2001:4:112::/48", "2001:20::/28", "2001:30::/28"];
const V6_RESERVED: &[&str] = &[
    "::/8", "100::/8", "200::/7", "400::/6", "800::/5", "1000::/4", "4000::/3", "6000::/3", "8000::/3", "a000::/3", "c000::/3", "e000::/4",
    "f000::/5", "f800::/6", "fe00::/9",
];

// python ipaddress._IPv4Constants (3.14); every one of these lies inside BLOCKED_V4 but is
// kept for clarity
const V4_PRIVATE: &[&str] = &[
    "0.0.0.0/8",
    "10.0.0.0/8",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.0.0.0/24",
    "192.0.0.170/31",
    "192.0.2.0/24",
    "192.168.0.0/16",
    "198.18.0.0/15",
    "198.51.100.0/24",
    "203.0.113.0/24",
    "240.0.0.0/4",
    "255.255.255.255/32",
];

fn v4_blocked(ip: Ipv4Addr) -> bool {
    let private = V4_PRIVATE.iter().any(|n| v4_in(ip, n)) && !["192.0.0.9/32", "192.0.0.10/32"].iter().any(|n| v4_in(ip, n));
    private
        || v4_in(ip, "127.0.0.0/8")
        || v4_in(ip, "169.254.0.0/16")
        || v4_in(ip, "240.0.0.0/4")
        || v4_in(ip, "224.0.0.0/4")
        || ip == Ipv4Addr::UNSPECIFIED
        || BLOCKED_V4.iter().any(|n| v4_in(ip, n))
}

fn v6_blocked(ip: Ipv6Addr) -> bool {
    let private = V6_PRIVATE.iter().any(|n| v6_in(ip, n)) && !V6_PRIVATE_EXCEPTIONS.iter().any(|n| v6_in(ip, n));
    private
        || ip == Ipv6Addr::LOCALHOST
        || v6_in(ip, "fe80::/10")
        || V6_RESERVED.iter().any(|n| v6_in(ip, n))
        || v6_in(ip, "ff00::/8")
        || ip == Ipv6Addr::UNSPECIFIED
        || BLOCKED_V6.iter().any(|n| v6_in(ip, n))
}

/// Return true if the address is private/loopback/link-local/reserved/etc.
pub fn ip_is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_blocked(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4_blocked(v4),
            None => v6_blocked(v6),
        },
    }
}

/// `_ip_is_blocked(ip_str)`: unparseable -> blocked
pub fn ip_str_is_blocked(s: &str) -> bool {
    let bare = s.split('%').next().unwrap_or("");
    match bare.parse::<IpAddr>() {
        Ok(ip) => ip_is_blocked(ip),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_matches_python() {
        // expected values from python's _ip_is_blocked
        for (ip, exp) in [
            ("127.0.0.1", true),
            ("10.1.2.3", true),
            ("100.64.1.1", true),
            ("169.254.169.254", true),
            ("192.168.1.5", true),
            ("8.8.8.8", false),
            ("93.184.215.14", false),
            ("::1", true),
            ("::ffff:127.0.0.1", true),
            ("::ffff:8.8.8.8", false),
            ("2606:4700::6810:84e5", false),
            ("2001:db8::1", true),
            ("2001:3::1", false),
            ("2002::1", true),
            ("fd00::1", true),
            ("fe80::1", true),
            ("ff02::1", true),
            ("::", true),
            ("64:ff9b::1.2.3.4", true),
            ("not-an-ip", true),
        ] {
            assert_eq!(ip_str_is_blocked(ip), exp, "{}", ip);
        }
    }
}
