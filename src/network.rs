//! Which of this machine's addresses a device on the network can reach.

use std::net::{IpAddr, Ipv4Addr, UdpSocket};

/// The local address the system would use to talk to `target` (no packet is sent).
pub fn local_ip_for(target: IpAddr) -> Option<IpAddr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((target, 9)).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

/// The address of the interface carrying the default route: discovery is sent from it, so that
/// multicast leaves on the real network rather than on a virtual adapter.
pub fn primary_ipv4() -> Ipv4Addr {
    match local_ip_for(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))) {
        Some(IpAddr::V4(ip)) => ip,
        _ => Ipv4Addr::UNSPECIFIED,
    }
}

/// Whether this program runs in a container (Docker and Podman leave a marker file).
pub fn in_container() -> bool {
    ["/.dockerenv", "/run/.containerenv"]
        .iter()
        .any(|marker| std::path::Path::new(marker).exists())
}

/// The gateway of the default route: for a container, the machine hosting it.
pub fn default_gateway() -> Option<Ipv4Addr> {
    gateway_in(&std::fs::read_to_string("/proc/net/route").ok()?)
}

/// Reads the default gateway from the kernel's routing table (`/proc/net/route`), where
/// addresses are hexadecimal in the machine's byte order.
pub fn gateway_in(table: &str) -> Option<Ipv4Addr> {
    table.lines().skip(1).find_map(|line| {
        let mut fields = line.split_whitespace().skip(1);
        let destination = fields.next()?;
        let gateway = u32::from_str_radix(fields.next()?, 16).ok()?;
        (destination == "00000000" && gateway != 0).then(|| Ipv4Addr::from(gateway.to_le_bytes()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_default_gateway() {
        let table =
            "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                     eth0\t0000FAAC\t00000000\t0001\t0\t0\t0\tF8FFFFFF\t0\t0\t0\n\
                     eth0\t00000000\t01FA1FAC\t0003\t0\t0\t0\t00000000\t0\t0\t0\n";
        assert_eq!(gateway_in(table), Some(Ipv4Addr::new(172, 31, 250, 1)));
        assert_eq!(
            gateway_in("Iface\tDestination\tGateway\neth0\t0000FAAC\t00000000\n"),
            None
        );
        assert_eq!(gateway_in("Iface\neth0\t00000000\tzz\n"), None);
        assert_eq!(gateway_in(""), None);
    }
}
