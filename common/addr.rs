use std::net::{IpAddr, SocketAddr};

pub fn any_addr(ip_version: &IpAddr) -> SocketAddr {
    let any_ip = match ip_version {
        IpAddr::V4(_) => IpAddr::V4("0.0.0.0".parse().unwrap()),
        IpAddr::V6(_) => IpAddr::V6("::".parse().unwrap()),
    };
    SocketAddr::new(any_ip, 0)
}
