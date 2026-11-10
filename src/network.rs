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
