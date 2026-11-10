//! The server binds only to a Tailscale address, or loopback for tests.
//! `0.0.0.0`, LAN, and public addresses are refused by construction.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Tailscale's IPv4 range, `100.64.0.0/10` (the CGNAT block).
pub fn is_tailscale_v4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 100 && (64..128).contains(&b)
}

/// Tailscale's IPv6 range, `fd7a:115c:a1e0::/48`.
pub fn is_tailscale_v6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    segments[0] == 0xfd7a && segments[1] == 0x115c && segments[2] == 0xa1e0
}

pub fn is_tailscale(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_tailscale_v4(ip),
        IpAddr::V6(ip) => is_tailscale_v6(ip),
    }
}

/// Whether the server may bind this address: a Tailscale address, or
/// loopback for tests and local checks. Never unspecified.
pub fn is_allowed(ip: IpAddr) -> bool {
    is_tailscale(ip) || ip.is_loopback()
}

/// A readable refusal for addresses outside the allowed set.
pub fn refusal(ip: IpAddr) -> Option<String> {
    if is_allowed(ip) {
        return None;
    }
    Some(if ip.is_unspecified() {
        "Flick serves only on its Tailscale address, never on every interface. Leave --bind unset, or pass the Tailscale address.".into()
    } else {
        format!(
            "{ip} is not a Tailscale address. Flick serves only on the tailnet (100.64.0.0/10 or fd7a:115c:a1e0::/48), or 127.0.0.1 for local checks."
        )
    })
}

// TODO(server agent): `pub async fn tailscale_ipv4() -> anyhow::Result<Ipv4Addr>`
// that runs `tailscale ip -4` from PATH, then
// `/Applications/Tailscale.app/Contents/MacOS/Tailscale`, then
// `/usr/local/bin/tailscale`, and validates the result with `is_tailscale_v4`.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_tailscale_and_loopback_are_allowed() {
        let allowed: [IpAddr; 4] = [
            "100.98.200.69".parse().unwrap(),
            "100.64.0.1".parse().unwrap(),
            "fd7a:115c:a1e0::6c2e:c846".parse().unwrap(),
            "127.0.0.1".parse().unwrap(),
        ];
        for ip in allowed {
            assert!(is_allowed(ip), "{ip}");
            assert_eq!(refusal(ip), None);
        }
        let refused: [IpAddr; 7] = [
            "0.0.0.0".parse().unwrap(),
            "::".parse().unwrap(),
            "192.168.1.10".parse().unwrap(),
            "10.0.0.5".parse().unwrap(),
            "100.63.255.255".parse().unwrap(),
            "100.128.0.0".parse().unwrap(),
            "8.8.8.8".parse().unwrap(),
        ];
        for ip in refused {
            assert!(!is_allowed(ip), "{ip}");
            assert!(refusal(ip).is_some());
        }
        assert!(
            refusal("0.0.0.0".parse().unwrap())
                .unwrap()
                .contains("every interface")
        );
    }
}
