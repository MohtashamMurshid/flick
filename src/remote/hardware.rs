//! This machine's own MAC and LAN IPv4 addresses, so a paired Flick on the
//! same LAN can send a Wake-on-LAN packet. Only this machine's interfaces
//! are read; nothing about other machines on the network.
//!
//! macOS and Linux read the interface list with `getifaddrs`; macOS takes
//! MAC addresses from its link-layer entries, Linux from
//! `/sys/class/net/<name>/address`. Other systems report nothing.

use crate::remote::protocol::Hardware;
use std::net::Ipv4Addr;

/// At most this many MAC addresses are listed.
const MAX_MACS: usize = 8;

/// Name prefixes of virtual interfaces that a magic packet cannot wake:
/// tunnels (including Tailscale's), bridges, Apple's peer-to-peer links,
/// containers, and virtual machines.
const VIRTUAL_PREFIXES: [&str; 25] = [
    "lo",
    "utun",
    "tun",
    "tap",
    "gif",
    "stf",
    "awdl",
    "llw",
    "bridge",
    "ap",
    "anpi",
    "ipsec",
    "ppp",
    "docker",
    "veth",
    "br-",
    "virbr",
    "vmnet",
    "vboxnet",
    "tailscale",
    "zt",
    "wg",
    "vnic",
    "dummy",
    "ifb",
];

/// One network interface as the system reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub mac: Option<[u8; 6]>,
    pub ipv4: Vec<Ipv4Addr>,
    /// The system says this is a real device (Linux: it has a `device`
    /// link); None when the system does not say.
    pub physical: Option<bool>,
}

/// This machine's addresses for Wake-on-LAN, read now. Empty when the
/// system cannot say, and always on Windows.
pub fn local() -> Hardware {
    summarize(&interfaces())
}

/// The MACs of real interfaces, those with a LAN address first, and the
/// private IPv4 addresses on them. Loopback, link-local, Tailscale
/// (`100.64.0.0/10`), and public addresses are left out.
pub fn summarize(interfaces: &[Interface]) -> Hardware {
    let real: Vec<&Interface> = interfaces
        .iter()
        .filter(|interface| {
            interface.physical != Some(false)
                && !VIRTUAL_PREFIXES
                    .iter()
                    .any(|prefix| interface.name.starts_with(prefix))
        })
        .collect();
    let on_lan = |interface: &Interface| interface.ipv4.iter().any(|ip| is_lan(*ip));
    let mut hardware = Hardware::default();
    let ordered = real
        .iter()
        .filter(|interface| on_lan(interface))
        .chain(real.iter().filter(|interface| !on_lan(interface)));
    for interface in ordered {
        if let Some(mac) = interface.mac.filter(is_unicast) {
            let text = format_mac(&mac);
            if !hardware.mac.contains(&text) && hardware.mac.len() < MAX_MACS {
                hardware.mac.push(text);
            }
        }
        for ip in interface.ipv4.iter().filter(|ip| is_lan(**ip)) {
            let text = ip.to_string();
            if !hardware.lan_ips.contains(&text) {
                hardware.lan_ips.push(text);
            }
        }
    }
    hardware
}

/// A private LAN address (`10/8`, `172.16/12`, `192.168/16`). Tailscale's
/// `100.64/10` is not private in this sense, so it never appears.
fn is_lan(ip: Ipv4Addr) -> bool {
    ip.is_private()
}

/// Not all zeros, and not a group (multicast or broadcast) address.
fn is_unicast(mac: &[u8; 6]) -> bool {
    mac.iter().any(|&b| b != 0) && mac[0] & 1 == 0
}

pub fn format_mac(mac: &[u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = text.trim().split(':');
    for byte in &mut mac {
        let part = parts.next()?;
        if part.len() != 2 {
            return None;
        }
        *byte = u8::from_str_radix(part, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn interfaces() -> Vec<Interface> {
    fn entry(found: &mut Vec<Interface>, name: String) -> usize {
        if let Some(at) = found.iter().position(|i| i.name == name) {
            return at;
        }
        found.push(Interface {
            name,
            ..Interface::default()
        });
        found.len() - 1
    }
    let mut found: Vec<Interface> = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list` with a linked list that stays valid
    // until freeifaddrs, which is called below on every path.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return Vec::new();
    }
    let mut cursor = list;
    while !cursor.is_null() {
        // SAFETY: `cursor` is a node of the list from getifaddrs.
        let node = unsafe { &*cursor };
        cursor = node.ifa_next;
        if node.ifa_name.is_null() || node.ifa_addr.is_null() {
            continue;
        }
        // SAFETY: a non-null name is a NUL-terminated string in the list.
        let name = unsafe { std::ffi::CStr::from_ptr(node.ifa_name) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: a non-null address starts with a sockaddr header.
        let family = i32::from(unsafe { (*node.ifa_addr).sa_family });
        if family == libc::AF_INET {
            // SAFETY: an AF_INET address is a sockaddr_in.
            let address = unsafe { &*node.ifa_addr.cast::<libc::sockaddr_in>() };
            let ip = Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr));
            let at = entry(&mut found, name);
            found[at].ipv4.push(ip);
        } else {
            #[cfg(target_os = "macos")]
            if family == libc::AF_LINK {
                // SAFETY: an AF_LINK address is a sockaddr_dl of `sdl_len`
                // bytes, whose data holds the name and then the address.
                let link = unsafe { &*node.ifa_addr.cast::<libc::sockaddr_dl>() };
                let data_offset = std::mem::offset_of!(libc::sockaddr_dl, sdl_data);
                let (nlen, alen) = (usize::from(link.sdl_nlen), usize::from(link.sdl_alen));
                if alen == 6 && data_offset + nlen + alen <= usize::from(link.sdl_len) {
                    let mut mac = [0u8; 6];
                    // SAFETY: the bytes are inside the sdl_len-long address.
                    let start = unsafe { link.sdl_data.as_ptr().add(nlen).cast::<u8>() };
                    for (i, byte) in mac.iter_mut().enumerate() {
                        *byte = unsafe { *start.add(i) };
                    }
                    let at = entry(&mut found, name);
                    found[at].mac = Some(mac);
                }
            }
        }
    }
    // SAFETY: `list` came from getifaddrs and is freed once.
    unsafe { libc::freeifaddrs(list) };

    #[cfg(target_os = "linux")]
    if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
        for dir in entries.flatten() {
            let name = dir.file_name().to_string_lossy().into_owned();
            let mac = std::fs::read_to_string(dir.path().join("address"))
                .ok()
                .and_then(|text| parse_mac(&text));
            let physical = dir.path().join("device").exists();
            let at = entry(&mut found, name);
            found[at].mac = mac;
            found[at].physical = Some(physical);
        }
    }
    found
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn interfaces() -> Vec<Interface> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interface(name: &str, mac: &str, ips: &[&str], physical: Option<bool>) -> Interface {
        Interface {
            name: name.into(),
            mac: parse_mac(mac),
            ipv4: ips.iter().map(|ip| ip.parse().unwrap()).collect(),
            physical,
        }
    }

    #[test]
    fn real_interfaces_with_a_lan_address_come_first() {
        let hardware = summarize(&[
            interface("lo0", "00:00:00:00:00:00", &["127.0.0.1"], None),
            interface("en1", "aa:bb:cc:00:00:01", &[], None),
            interface(
                "en0",
                "AA:BB:CC:00:00:00",
                &["192.168.1.20", "169.254.3.4"],
                None,
            ),
            interface("utun4", "", &["100.98.200.69"], None),
            interface("tailscale0", "", &["100.98.200.70"], None),
            interface("awdl0", "aa:bb:cc:00:00:09", &[], None),
            interface("bridge0", "aa:bb:cc:00:00:0a", &["10.0.0.1"], None),
            interface("eth1", "02:42:ac:11:00:02", &["172.17.0.2"], Some(false)),
            interface("en5", "01:00:5e:00:00:01", &["10.1.2.3"], None),
            interface("en6", "aa:bb:cc:00:00:06", &["8.8.8.8"], None),
        ]);
        assert_eq!(
            hardware.mac,
            [
                "aa:bb:cc:00:00:00",
                "aa:bb:cc:00:00:01",
                "aa:bb:cc:00:00:06"
            ]
        );
        assert_eq!(hardware.lan_ips, ["192.168.1.20", "10.1.2.3"]);
    }

    #[test]
    fn nothing_known_is_empty() {
        assert_eq!(summarize(&[]), Hardware::default());
    }

    #[test]
    fn macs_parse_and_format() {
        assert_eq!(
            parse_mac("aa:BB:0c:00:01:ff\n"),
            Some([0xaa, 0xbb, 0x0c, 0, 1, 0xff])
        );
        for bad in [
            "",
            "aa:bb",
            "aa:bb:cc:dd:ee:ff:00",
            "a:bb:cc:dd:ee:ff",
            "zz:bb:cc:dd:ee:ff",
        ] {
            assert_eq!(parse_mac(bad), None, "{bad}");
        }
        assert_eq!(
            format_mac(&[0xaa, 0xbb, 0x0c, 0, 1, 0xff]),
            "aa:bb:0c:00:01:ff"
        );
    }

    /// Whatever this machine has, the summary never names loopback or
    /// Tailscale addresses.
    #[test]
    fn this_machine_lists_only_lan_addresses() {
        let hardware = local();
        for ip in &hardware.lan_ips {
            let ip: Ipv4Addr = ip.parse().unwrap();
            assert!(ip.is_private(), "{ip}");
        }
        for mac in &hardware.mac {
            assert!(parse_mac(mac).is_some_and(|mac| is_unicast(&mac)), "{mac}");
        }
    }
}
