//! This machine's own MAC and LAN IPv4 addresses, so a paired Flick on the
//! same LAN can send a Wake-on-LAN packet. Only this machine's interfaces
//! are read; nothing about other machines on the network.
//!
//! IPv4 addresses come from `getifaddrs` on macOS and Linux. MAC addresses
//! come from places that also name ports without an address right now,
//! such as an unplugged Ethernet port, because that is the one a magic
//! packet usually wakes:
//!
//! - macOS: `networksetup -listallhardwareports`, which gives each port's
//!   burned-in address. `getifaddrs` is no use here: macOS gives
//!   unprivileged processes `02:00:00:00:00:00` for Wi-Fi. When
//!   `networksetup` fails or names no Ethernet or Wi-Fi port, the `ether`
//!   lines of `ifconfig` for `en*` devices are used instead.
//! - Linux: `/sys/class/net/<name>/address` of every interface with a
//!   `device` link, whether it is up or down.
//!
//! Other systems report nothing.

use crate::remote::protocol::Hardware;
use std::net::Ipv4Addr;

/// At most this many MAC addresses are listed.
const MAX_MACS: usize = 8;
/// At most this many LAN addresses are listed.
const MAX_LAN_IPS: usize = 8;

/// What macOS reports for Wi-Fi to processes it does not trust with the
/// real address. It wakes nothing.
const PLACEHOLDER_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0];

/// Name prefixes of virtual interfaces that a magic packet cannot wake:
/// tunnels (including Tailscale's), bridges, Apple's peer-to-peer links,
/// containers, and virtual machines.
const VIRTUAL_PREFIXES: [&str; 26] = [
    "lo",
    "utun",
    "tun",
    "tap",
    "gif",
    "stf",
    "awdl",
    "llw",
    "nan",
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

/// What sort of interface the system says this is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    /// The system does not say.
    #[default]
    Unknown,
    /// An Ethernet port, built in or on an adapter.
    Wired,
    /// Wi-Fi.
    Wireless,
    /// Not a real device: a bridge, tunnel, container, or virtual machine.
    Virtual,
}

/// One network interface as the system reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub mac: Option<[u8; 6]>,
    pub ipv4: Vec<Ipv4Addr>,
    pub kind: Kind,
}

/// This machine's addresses for Wake-on-LAN, read at most every
/// [`CACHE_FOR`]. Empty when the system cannot say, and always on Windows.
pub fn local() -> Hardware {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    /// How long a reading is reused; the listing asks on every request.
    const CACHE_FOR: Duration = Duration::from_secs(30);
    static CACHE: Mutex<Option<(Instant, Hardware)>> = Mutex::new(None);

    let mut cache = CACHE.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some((read, hardware)) = cache.as_ref()
        && read.elapsed() < CACHE_FOR
    {
        return hardware.clone();
    }
    let hardware = summarize(&interfaces());
    *cache = Some((Instant::now(), hardware.clone()));
    hardware
}

/// The MACs of real interfaces, Ethernet first and then those with a LAN
/// address, and the private IPv4 addresses on them, at most eight of each.
/// Loopback, link-local, Tailscale (`100.64.0.0/10`), public, broadcast,
/// and multicast addresses are left out, and so are all-zero, group, and
/// placeholder MACs.
pub fn summarize(interfaces: &[Interface]) -> Hardware {
    let mut real: Vec<&Interface> = interfaces
        .iter()
        .filter(|interface| {
            interface.kind != Kind::Virtual
                && !VIRTUAL_PREFIXES
                    .iter()
                    .any(|prefix| interface.name.starts_with(prefix))
        })
        .collect();
    let on_lan = |interface: &Interface| interface.ipv4.iter().any(|ip| is_lan(*ip));
    let rank = |interface: &Interface| match interface.kind {
        Kind::Wired => 0,
        Kind::Unknown | Kind::Virtual => 1,
        Kind::Wireless => 2,
    };
    // Stable, so the system's order breaks ties.
    real.sort_by_key(|interface| (rank(interface), !on_lan(interface)));
    let mut hardware = Hardware::default();
    for interface in real {
        if let Some(mac) = interface.mac.filter(is_wakeable) {
            let text = format_mac(&mac);
            if !hardware.mac.contains(&text) && hardware.mac.len() < MAX_MACS {
                hardware.mac.push(text);
            }
        }
        for ip in interface.ipv4.iter().filter(|ip| is_lan(**ip)) {
            let text = ip.to_string();
            if !hardware.lan_ips.contains(&text) && hardware.lan_ips.len() < MAX_LAN_IPS {
                hardware.lan_ips.push(text);
            }
        }
    }
    hardware
}

/// A private LAN address (`10/8`, `172.16/12`, `192.168/16`). Tailscale's
/// `100.64/10` is not private in this sense, so it never appears.
fn is_lan(ip: Ipv4Addr) -> bool {
    ip.is_private() && !ip.is_broadcast() && !ip.is_multicast()
}

/// Not all zeros, not macOS's placeholder, and not a group (multicast or
/// broadcast) address.
fn is_wakeable(mac: &[u8; 6]) -> bool {
    mac.iter().any(|&b| b != 0) && *mac != PLACEHOLDER_MAC && mac[0] & 1 == 0
}

/// Set by whoever assigned it rather than the manufacturer: macOS's private
/// Wi-Fi addresses, Thunderbolt bridge members, and virtual ports.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn is_locally_administered(mac: &[u8; 6]) -> bool {
    mac[0] & 2 != 0
}

pub fn format_mac(mac: &[u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg_attr(
    not(any(target_os = "linux", target_os = "macos", test)),
    allow(dead_code)
)]
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

/// What a macOS hardware port's name says it is, or None for ports a
/// magic packet cannot reach: Thunderbolt Bridge and its members
/// ("Thunderbolt 1"), Apple's virtual "Ethernet Adapter (enN)" ports,
/// Bluetooth PAN, iPhone USB, and the like.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn port_kind(port: &str) -> Option<Kind> {
    let port = port.trim();
    let wired = (port.starts_with("Ethernet") && !port.starts_with("Ethernet Adapter"))
        || port.starts_with("Thunderbolt Ethernet")
        || (port.starts_with("USB") && port.contains("LAN"));
    if wired {
        Some(Kind::Wired)
    } else if port == "Wi-Fi" || port == "AirPort" {
        Some(Kind::Wireless)
    } else {
        None
    }
}

/// The Ethernet and Wi-Fi ports in `networksetup -listallhardwareports`
/// output, with their burned-in addresses, in the order listed.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn parse_hardware_ports(text: &str) -> Vec<Interface> {
    fn finish(
        found: &mut Vec<Interface>,
        port: &mut Option<String>,
        device: &mut Option<String>,
        mac: &mut Option<[u8; 6]>,
    ) {
        if let (Some(port), Some(device), Some(mac)) = (port.take(), device.take(), mac.take())
            && let Some(kind) = port_kind(&port)
            && !found.iter().any(|known| known.name == device)
        {
            found.push(Interface {
                name: device,
                mac: Some(mac),
                ipv4: Vec::new(),
                kind,
            });
        }
    }
    let mut found = Vec::new();
    let (mut port, mut device, mut mac) = (None, None, None);
    for line in text.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("Hardware Port:") {
            finish(&mut found, &mut port, &mut device, &mut mac);
            port = Some(name.trim().to_owned());
        } else if let Some(name) = line.strip_prefix("Device:") {
            device = Some(name.trim().to_owned());
        } else if let Some(address) = line.strip_prefix("Ethernet Address:") {
            mac = parse_mac(address);
        } else if line.is_empty() {
            finish(&mut found, &mut port, &mut device, &mut mac);
        }
    }
    finish(&mut found, &mut port, &mut device, &mut mac);
    found
}

/// The `en*` devices in `ifconfig` output with a manufacturer's `ether`
/// address. Locally administered addresses are left out: on macOS they are
/// private Wi-Fi addresses, Thunderbolt bridge members, and virtual ports.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn parse_ifconfig(text: &str) -> Vec<Interface> {
    let mut found: Vec<Interface> = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if !line.starts_with(char::is_whitespace) {
            current = line
                .split_once(':')
                .map(|(name, _)| name.trim().to_owned())
                .filter(|name| name.starts_with("en"));
            continue;
        }
        let Some(name) = &current else { continue };
        let mut words = line.split_whitespace();
        if words.next() == Some("ether")
            && let Some(mac) = words.next().and_then(parse_mac)
            && !is_locally_administered(&mac)
            && !found.iter().any(|known| &known.name == name)
        {
            found.push(Interface {
                name: name.clone(),
                mac: Some(mac),
                ..Interface::default()
            });
        }
    }
    found
}

/// Fills in MACs and kinds from `ports` by name, adding ports that have
/// no address yet, in their order after those already known.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn merge_ports(found: &mut Vec<Interface>, ports: Vec<Interface>) {
    for port in ports {
        match found.iter_mut().find(|known| known.name == port.name) {
            Some(known) => {
                known.mac = port.mac;
                known.kind = port.kind;
            }
            None => found.push(port),
        }
    }
}

/// Runs a system tool without a shell and returns its output, or None when
/// it fails, prints too much, or takes longer than a few seconds.
#[cfg(target_os = "macos")]
fn run(program: &str, args: &[&str]) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    const TIMEOUT: Duration = Duration::from_secs(3);
    const MAX_OUTPUT: u64 = 1 << 20;

    let mut child = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    // Read on a thread so a full pipe never stalls the tool while waiting.
    let reader = std::thread::spawn(move || {
        let mut text = Vec::new();
        stdout.take(MAX_OUTPUT).read_to_end(&mut text).map(|_| text)
    });
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::debug!(program, "Gave up reading hardware addresses");
                return None;
            }
        }
    };
    let text = reader.join().ok()?.ok()?;
    status
        .success()
        .then(|| String::from_utf8_lossy(&text).into_owned())
}

/// The interfaces with IPv4 addresses, by name, from `getifaddrs`.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn ipv4_interfaces() -> Vec<Interface> {
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
        // SAFETY: a non-null address starts with a sockaddr header.
        let family = i32::from(unsafe { (*node.ifa_addr).sa_family });
        if family != libc::AF_INET {
            continue;
        }
        // SAFETY: a non-null name is a NUL-terminated string in the list.
        let name = unsafe { std::ffi::CStr::from_ptr(node.ifa_name) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: an AF_INET address is a whole sockaddr_in.
        let address = unsafe { &*node.ifa_addr.cast::<libc::sockaddr_in>() };
        let ip = Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr));
        match found.iter_mut().find(|known| known.name == name) {
            Some(known) => known.ipv4.push(ip),
            None => found.push(Interface {
                name,
                ipv4: vec![ip],
                ..Interface::default()
            }),
        }
    }
    // SAFETY: `list` came from getifaddrs and is freed once.
    unsafe { libc::freeifaddrs(list) };
    found
}

#[cfg(target_os = "macos")]
fn interfaces() -> Vec<Interface> {
    let mut found = ipv4_interfaces();
    let ports = run("/usr/sbin/networksetup", &["-listallhardwareports"])
        .map(|text| parse_hardware_ports(&text))
        .filter(|ports| !ports.is_empty())
        .or_else(|| run("/sbin/ifconfig", &[]).map(|text| parse_ifconfig(&text)))
        .unwrap_or_default();
    merge_ports(&mut found, ports);
    found
}

#[cfg(target_os = "linux")]
fn interfaces() -> Vec<Interface> {
    let mut found = ipv4_interfaces();
    if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
        for dir in entries.flatten() {
            let path = dir.path();
            let name = dir.file_name().to_string_lossy().into_owned();
            let mac = std::fs::read_to_string(path.join("address"))
                .ok()
                .and_then(|text| parse_mac(&text));
            // A `device` link means real hardware, up or down; `wireless`
            // marks Wi-Fi among those.
            let kind = if !path.join("device").exists() {
                Kind::Virtual
            } else if path.join("wireless").exists() || path.join("phy80211").exists() {
                Kind::Wireless
            } else {
                Kind::Wired
            };
            match found.iter_mut().find(|known| known.name == name) {
                Some(known) => {
                    known.mac = mac;
                    known.kind = kind;
                }
                None => found.push(Interface {
                    name,
                    mac,
                    ipv4: Vec::new(),
                    kind,
                }),
            }
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

    /// `networksetup -listallhardwareports` on a Mac mini whose Ethernet
    /// cable is unplugged and whose Wi-Fi has the LAN address.
    const NETWORKSETUP: &str = "
Hardware Port: Ethernet
Device: en0
Ethernet Address: c8:80:6d:56:7f:b1

Hardware Port: Ethernet Adapter (en4)
Device: en4
Ethernet Address: be:39:fd:38:33:a6

Hardware Port: Thunderbolt Bridge
Device: bridge0
Ethernet Address: 36:15:b1:29:70:40

Hardware Port: Wi-Fi
Device: en1
Ethernet Address: c8:80:6d:4d:f2:c8

Hardware Port: Thunderbolt 1
Device: en2
Ethernet Address: 36:15:b1:29:70:40

Hardware Port: USB 10/100/1G/2.5G LAN
Device: en8
Ethernet Address: 00:e0:4c:68:01:02

Hardware Port: Thunderbolt Ethernet Slot 1
Device: en9
Ethernet Address: 00:e0:4c:68:01:03

Hardware Port: Bluetooth PAN
Device: en10
Ethernet Address: c8:80:6d:4d:f2:c9

VLAN Configurations
===================
";

    /// `ifconfig` on the same machine, trimmed.
    const IFCONFIG: &str = "\
lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST> mtu 16384
\tinet 127.0.0.1 netmask 0xff000000
anpi0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
\tether be:39:fd:38:33:c6
en0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
\tether c8:80:6d:56:7f:b1
en4: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
\tether be:39:fd:38:33:a6
en2: flags=8963<UP,BROADCAST,SMART,RUNNING,PROMISC,SIMPLEX,MULTICAST> mtu 1500
\tether 36:15:b1:29:70:40
bridge0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
\tether 36:15:b1:29:70:40
en1: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
\tether 22:67:3d:5a:52:71
\tinet 192.168.0.47 netmask 0xffffff00 broadcast 192.168.0.255
awdl0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
\tether 02:36:10:31:cb:94
en3: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500
\tether 02:00:00:00:00:00
utun0: flags=8051<UP,POINTOPOINT,RUNNING,MULTICAST> mtu 1280
\tinet 100.98.200.69 --> 100.98.200.69 netmask 0xffffffff
";

    /// What `getifaddrs` gives an unprivileged process on that Mac.
    fn mac_mini_ipv4() -> Vec<Interface> {
        vec![
            interface("lo0", "", &["127.0.0.1"], Kind::Unknown),
            interface("en1", "02:00:00:00:00:00", &["192.168.0.47"], Kind::Unknown),
            interface("utun0", "", &["100.98.200.69"], Kind::Unknown),
        ]
    }

    fn interface(name: &str, mac: &str, ips: &[&str], kind: Kind) -> Interface {
        Interface {
            name: name.into(),
            mac: parse_mac(mac),
            ipv4: ips.iter().map(|ip| ip.parse().unwrap()).collect(),
            kind,
        }
    }

    #[test]
    fn networksetup_keeps_ethernet_and_wifi_ports() {
        let ports = parse_hardware_ports(NETWORKSETUP);
        let named: Vec<(&str, String, Kind)> = ports
            .iter()
            .map(|port| {
                (
                    port.name.as_str(),
                    format_mac(&port.mac.unwrap()),
                    port.kind,
                )
            })
            .collect();
        assert_eq!(
            named,
            [
                ("en0", "c8:80:6d:56:7f:b1".into(), Kind::Wired),
                ("en1", "c8:80:6d:4d:f2:c8".into(), Kind::Wireless),
                ("en8", "00:e0:4c:68:01:02".into(), Kind::Wired),
                ("en9", "00:e0:4c:68:01:03".into(), Kind::Wired),
            ]
        );
    }

    /// The Mac mini's unplugged Ethernet port comes first even though only
    /// Wi-Fi has an address, and the placeholder never appears.
    #[test]
    fn the_mac_mini_advertises_its_ethernet_port_first() {
        let mut found = mac_mini_ipv4();
        merge_ports(&mut found, parse_hardware_ports(NETWORKSETUP));
        let hardware = summarize(&found);
        assert_eq!(
            hardware.mac,
            [
                "c8:80:6d:56:7f:b1",
                "00:e0:4c:68:01:02",
                "00:e0:4c:68:01:03",
                "c8:80:6d:4d:f2:c8",
            ]
        );
        assert_eq!(hardware.lan_ips, ["192.168.0.47"]);
    }

    #[test]
    fn ifconfig_keeps_manufacturer_addresses_of_en_devices() {
        let ports = parse_ifconfig(IFCONFIG);
        let named: Vec<(&str, String)> = ports
            .iter()
            .map(|port| (port.name.as_str(), format_mac(&port.mac.unwrap())))
            .collect();
        assert_eq!(named, [("en0", "c8:80:6d:56:7f:b1".into())]);

        let mut found = mac_mini_ipv4();
        merge_ports(&mut found, ports);
        let hardware = summarize(&found);
        assert_eq!(hardware.mac, ["c8:80:6d:56:7f:b1"]);
        assert_eq!(hardware.lan_ips, ["192.168.0.47"]);
    }

    #[test]
    fn port_names() {
        for wired in [
            "Ethernet",
            "Ethernet 1",
            "Thunderbolt Ethernet",
            "Thunderbolt Ethernet Slot 2",
            "USB 10/100/1000 LAN",
        ] {
            assert_eq!(port_kind(wired), Some(Kind::Wired), "{wired}");
        }
        assert_eq!(port_kind("Wi-Fi"), Some(Kind::Wireless));
        for other in [
            "Ethernet Adapter (en4)",
            "Thunderbolt Bridge",
            "Thunderbolt 1",
            "Bluetooth PAN",
            "iPhone USB",
        ] {
            assert_eq!(port_kind(other), None, "{other}");
        }
    }

    #[test]
    fn ethernet_first_then_interfaces_with_a_lan_address() {
        let hardware = summarize(&[
            interface("lo0", "00:00:00:00:00:00", &["127.0.0.1"], Kind::Unknown),
            interface(
                "wlan0",
                "aa:bb:cc:00:00:0b",
                &["192.168.1.30"],
                Kind::Wireless,
            ),
            interface("en1", "aa:bb:cc:00:00:01", &[], Kind::Unknown),
            interface(
                "en0",
                "AA:BB:CC:00:00:00",
                &["192.168.1.20", "169.254.3.4"],
                Kind::Unknown,
            ),
            interface("eth0", "aa:bb:cc:00:00:0e", &[], Kind::Wired),
            interface("utun4", "", &["100.98.200.69"], Kind::Unknown),
            interface("tailscale0", "", &["100.98.200.70"], Kind::Unknown),
            interface("awdl0", "aa:bb:cc:00:00:09", &[], Kind::Unknown),
            interface("bridge0", "aa:bb:cc:00:00:0a", &["10.0.0.1"], Kind::Unknown),
            interface("eth1", "02:42:ac:11:00:02", &["172.17.0.2"], Kind::Virtual),
            interface("en5", "01:00:5e:00:00:01", &["10.1.2.3"], Kind::Unknown),
            interface("en6", "aa:bb:cc:00:00:06", &["8.8.8.8"], Kind::Unknown),
            interface("en7", "02:00:00:00:00:00", &[], Kind::Unknown),
        ]);
        assert_eq!(
            hardware.mac,
            [
                "aa:bb:cc:00:00:0e",
                "aa:bb:cc:00:00:00",
                "aa:bb:cc:00:00:01",
                "aa:bb:cc:00:00:06",
                "aa:bb:cc:00:00:0b",
            ]
        );
        assert_eq!(
            hardware.lan_ips,
            ["192.168.1.20", "10.1.2.3", "192.168.1.30"]
        );
    }

    #[test]
    fn lists_are_capped_at_eight() {
        let many: Vec<Interface> = (0..20u8)
            .map(|i| Interface {
                name: format!("eth{i}"),
                mac: Some([0xaa, 0xbb, 0xcc, 0, 0, i]),
                ipv4: vec![
                    Ipv4Addr::new(10, 0, 0, i + 1),
                    Ipv4Addr::new(10, 0, 1, i + 1),
                ],
                kind: Kind::Wired,
            })
            .collect();
        let hardware = summarize(&many);
        assert_eq!(hardware.mac.len(), MAX_MACS);
        assert_eq!(hardware.lan_ips.len(), MAX_LAN_IPS);
        assert_eq!(hardware.mac[0], "aa:bb:cc:00:00:00");
        assert_eq!(hardware.lan_ips[..2], ["10.0.0.1", "10.0.1.1"]);
    }

    #[test]
    fn only_private_unicast_addresses_are_lan() {
        for lan in ["10.0.0.1", "172.16.5.4", "192.168.0.47"] {
            assert!(is_lan(lan.parse().unwrap()), "{lan}");
        }
        for other in [
            "127.0.0.1",
            "169.254.3.4",
            "100.98.200.69",
            "8.8.8.8",
            "255.255.255.255",
            "224.0.0.251",
            "0.0.0.0",
        ] {
            assert!(!is_lan(other.parse().unwrap()), "{other}");
        }
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
        assert!(!is_wakeable(&[0; 6]));
        assert!(!is_wakeable(&PLACEHOLDER_MAC));
        assert!(!is_wakeable(&[0xff; 6]));
        assert!(is_wakeable(&[0xc8, 0x80, 0x6d, 0x56, 0x7f, 0xb1]));
    }

    /// Whatever this machine has, the summary never names loopback or
    /// Tailscale addresses, or a MAC that wakes nothing.
    #[test]
    fn this_machine_lists_only_lan_addresses() {
        let hardware = local();
        assert!(hardware.lan_ips.len() <= MAX_LAN_IPS);
        assert!(hardware.mac.len() <= MAX_MACS);
        for ip in &hardware.lan_ips {
            let ip: Ipv4Addr = ip.parse().unwrap();
            assert!(is_lan(ip), "{ip}");
        }
        for mac in &hardware.mac {
            assert!(parse_mac(mac).is_some_and(|mac| is_wakeable(&mac)), "{mac}");
        }
    }
}
