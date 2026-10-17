//! Away from home: the signed list of this machine's addresses (kind 18, Sill's `SignedMacInfo`)
//! that a paired device keeps, so that later, off this network (WireGuard into home, Tailscale),
//! its Remote row dials them with the same TLS handshake as at home, on the same port.
//!
//! - every session gets it after the window list and the settings; it goes again to every session
//!   when the addresses change;
//! - addresses: Tailscale's, other tunnels' (WireGuard, tun), then this network's IPv4; never
//!   loopback, link-local or container bridges; at most 12, as Sill for Mac;
//! - `--no-remote`: `remoteAccess` false and no addresses (a device keeps what it had).

use serde::Serialize;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};

static REMOTE: AtomicBool = AtomicBool::new(true);

/// `--no-remote`: devices are not told this machine's addresses.
pub fn set_enabled(on: bool) {
    REMOTE.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    REMOTE.load(Ordering::Relaxed)
}

/// Sill's `MacAddress`; no port: the door's own (`remotePort`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Address {
    pub host: String,
    pub kind: &'static str,
    pub via: String,
}

/// One interface address: its interface's name, the address, and whether the interface is a
/// point-to-point tunnel.
pub struct Iface {
    pub name: String,
    pub ip: IpAddr,
    pub point_to_point: bool,
}

fn tailscale(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.octets()[0] == 100 && (v.octets()[1] & 0xC0) == 64,
        IpAddr::V6(v) => v.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

fn container(name: &str) -> bool {
    ["docker", "br-", "virbr", "veth", "lxc", "lxd", "podman", "cni", "flannel", "vnet"].iter().any(|p| name.starts_with(p))
}

fn tunnel(i: &Iface) -> bool {
    i.point_to_point || ["wg", "tun", "tap", "tailscale", "zt", "ppp", "nordlynx", "proton"].iter().any(|p| i.name.starts_with(p))
}

/// In Sill for Mac's order: Tailscale's IPv4, other tunnels' IPv4, tunnels' IPv6, this network's
/// IPv4. Pure.
pub fn pick(ifaces: &[Iface]) -> Vec<Address> {
    let usable = |i: &&Iface| match i.ip {
        IpAddr::V4(v) => !v.is_loopback() && !v.is_link_local() && !v.is_unspecified(),
        IpAddr::V6(v) => !v.is_loopback() && (v.segments()[0] & 0xFFC0) != 0xFE80 && !v.is_unspecified(),
    } && !container(&i.name);
    let ifaces: Vec<&Iface> = ifaces.iter().filter(usable).collect();
    let vpn = |i: &Iface| tunnel(i) || tailscale(i.ip);
    let via = |i: &Iface| if tailscale(i.ip) { "Tailscale".to_string() } else { format!("VPN ({})", i.name) };
    let mut out = Vec::new();
    let mut add = |i: &Iface, kind: &'static str, via: String| {
        let host = i.ip.to_string();
        if !out.iter().any(|a: &Address| a.host == host) {
            out.push(Address { host, kind, via });
        }
    };
    for i in ifaces.iter().filter(|i| i.ip.is_ipv4() && tailscale(i.ip)) {
        add(i, "vpn", "Tailscale".into());
    }
    for i in ifaces.iter().filter(|i| i.ip.is_ipv4() && vpn(i)) {
        add(i, "vpn", via(i));
    }
    for i in ifaces.iter().filter(|i| i.ip.is_ipv6() && vpn(i)) {
        add(i, "vpn", via(i));
    }
    for i in ifaces.iter().filter(|i| i.ip.is_ipv4() && !vpn(i)) {
        let via = if i.name.starts_with("wl") { "Wi-Fi".to_string() } else if i.name.starts_with("en") || i.name.starts_with("eth") { "Ethernet".to_string() } else { i.name.clone() };
        add(i, "lan", via);
    }
    out.truncate(12);
    out
}

/// This machine's interface addresses.
pub fn interfaces() -> Vec<Iface> {
    let mut out = Vec::new();
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return out;
        }
        let mut cur = ifap;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || ifa.ifa_flags & libc::IFF_UP as u32 == 0 {
                continue;
            }
            let ip: IpAddr = match (*ifa.ifa_addr).sa_family as i32 {
                libc::AF_INET => std::net::Ipv4Addr::from(u32::from_be((*(ifa.ifa_addr as *const libc::sockaddr_in)).sin_addr.s_addr)).into(),
                libc::AF_INET6 => std::net::Ipv6Addr::from((*(ifa.ifa_addr as *const libc::sockaddr_in6)).sin6_addr.s6_addr).into(),
                _ => continue,
            };
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
            out.push(Iface { name, ip, point_to_point: ifa.ifa_flags & libc::IFF_POINTOPOINT as u32 != 0 });
        }
        libc::freeifaddrs(ifap);
    }
    out
}

/// The addresses devices are told now (none with `--no-remote`).
pub fn addresses() -> Vec<Address> {
    if enabled() { pick(&interfaces()) } else { Vec::new() }
}

/// Sill's `MacInfo` as JSON bytes: what the signature covers.
pub fn mac_info(mac_id: &str, name: &str, port: u16, addresses: &[Address], issued_at: f64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "v": 1,
        "macID": mac_id,
        "name": name,
        "issuedAt": issued_at,
        "remoteAccess": enabled(),
        "remotePort": port,
        "internet": false,
        "addresses": addresses,
    }))
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn i(name: &str, ip: &str, p2p: bool) -> Iface {
        Iface { name: name.into(), ip: ip.parse().unwrap(), point_to_point: p2p }
    }

    #[test]
    fn order_and_filters() {
        let got = pick(&[
            i("lo", "127.0.0.1", false),
            i("wlp2s0", "192.168.1.20", false),
            i("wlp2s0", "fe80::1", false),
            i("docker0", "172.17.0.1", false),
            i("wg0", "10.8.0.2", true),
            i("wg0", "fd00:8::2", true),
            i("tailscale0", "100.101.102.103", true),
            i("tailscale0", "fd7a:115c:a1e0::5", true),
            i("enp3s0", "192.168.1.21", false),
            i("wlp2s0", "2a02:1::20", false),
        ]);
        let hosts: Vec<(&str, &str, &str)> = got.iter().map(|a| (a.host.as_str(), a.kind, a.via.as_str())).collect();
        assert_eq!(
            hosts,
            [
                ("100.101.102.103", "vpn", "Tailscale"),
                ("10.8.0.2", "vpn", "VPN (wg0)"),
                ("fd00:8::2", "vpn", "VPN (wg0)"),
                ("fd7a:115c:a1e0::5", "vpn", "Tailscale"),
                ("192.168.1.20", "lan", "Wi-Fi"),
                ("192.168.1.21", "lan", "Ethernet"),
            ]
        );
        let many: Vec<Iface> = (0..20).map(|n| i("eth0", &format!("10.0.0.{n}"), false)).collect();
        assert_eq!(pick(&many).len(), 12);
    }

    #[test]
    fn mac_info_json() {
        let a = [Address { host: "10.8.0.2".into(), kind: "vpn", via: "VPN (wg0)".into() }];
        let v: serde_json::Value = serde_json::from_slice(&mac_info("ABCDEFGHJKMNPQRS", "plexy", 47460, &a, 1.5)).unwrap();
        assert_eq!(v["macID"], "ABCDEFGHJKMNPQRS");
        assert_eq!(v["remotePort"], 47460);
        assert_eq!(v["issuedAt"], 1.5);
        assert_eq!(v["addresses"][0], serde_json::json!({"host": "10.8.0.2", "kind": "vpn", "via": "VPN (wg0)"}));
        assert!(v["addresses"][0].get("port").is_none(), "the door's own port");
    }
}
