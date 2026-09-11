//! Reachability diagnostics for "the service is up but the page won't load".
//!
//! That symptom has one dominant cause, and it is a nasty one to debug: a
//! packet filter somewhere is *dropping* the connection rather than refusing
//! it. A refusal is instant and produces an error message; a drop produces
//! nothing at all, so the browser spins until it times out and the operator is
//! left staring at a healthy `systemctl status` and an open port. Every check
//! here exists to turn that silence into a sentence.
//!
//! The trap duTime ships with itself: the system unit carries
//! `IPAddressAllow=localhost` / `IPAddressDeny=any`, so binding
//! `listen = "0.0.0.0:8471"` in the config produces a socket that is listening
//! and unreachable at the same time.

use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::time::Duration;

/// The address another machine would use to reach this host.
///
/// Found by asking the routing table which source address it would choose for
/// a distant destination. `connect` on a UDP socket transmits nothing — it
/// only resolves the route — so this touches the network not at all, which is
/// also why a packet filter cannot make it lie.
pub fn primary_address() -> Option<IpAddr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    // RFC 5737 documentation range: guaranteed never to be a real host.
    s.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

/// Where a remote client would actually connect, given what we bound to.
///
/// `None` means the question does not arise: we are bound to loopback, so
/// being unreachable from elsewhere is the configuration working as asked.
pub fn external_target(listen: SocketAddr) -> Option<SocketAddr> {
    if listen.ip().is_loopback() {
        return None;
    }
    if listen.ip().is_unspecified() {
        return primary_address().map(|ip| SocketAddr::new(ip, listen.port()));
    }
    Some(listen)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Reach {
    /// The handshake completed.
    Open,
    /// A RST came back: the packet arrived, nothing was listening.
    Refused,
    /// EPERM from `connect`: the kernel declined to emit the packet at all.
    /// A cgroup IP filter is the usual source. Note that TCP does not
    /// reliably surface this — `tcp_connect` discards the transmit error for
    /// the first SYN and falls back to retransmitting — so a filtered
    /// connection reports [`Reach::TimedOut`] more often than this.
    DeniedLocally,
    /// Silence. Something dropped the packet instead of refusing it, and this
    /// is precisely what a browser renders as a tab that spins forever.
    TimedOut,
    Error(String),
}

impl Reach {
    pub fn ok(&self) -> bool {
        matches!(self, Reach::Open)
    }

    /// What to actually do about it.
    pub fn advice(&self) -> &'static str {
        match self {
            Reach::Open => "reachable",
            Reach::DeniedLocally => {
                "blocked by a local policy before the packet left the host. \
                 On a systemd service this is IPAddressAllow/IPAddressDeny: the unit \
                 ships with 'IPAddressAllow=localhost', which silently drops every \
                 non-loopback connection no matter what you bind to. Fix with \
                 'dutime install --system --listen <addr>', or add the client subnet \
                 to IPAddressAllow= in a drop-in."
            }
            Reach::TimedOut => {
                "no response — the connection is being dropped, not refused, \
                 which is exactly what a browser shows as a page that spins forever. \
                 Check IPAddressAllow= in the unit first, then ufw/nftables."
            }
            Reach::Refused => {
                "connection refused: the packet arrived but nothing is listening there. \
                 Check that listen= names this address."
            }
            Reach::Error(_) => {
                "could not be tested; the error above is the reason"
            }
        }
    }
}

/// Connect to ourselves the way a remote client would.
///
/// **This proves less than it appears to, and the difference matters.** The
/// probe leaves from inside our own cgroup, so it sees the same egress filter
/// systemd applies to us — which is how it catches the `IPAddressDeny` trap.
/// But a packet addressed to one of this host's own IPs is routed over the
/// loopback device and never meets the external interface, so a host firewall
/// (ufw, nftables) that would drop a real client's packet lets this one
/// through. Success here means "not blocked locally", not "reachable".
pub fn probe(target: SocketAddr, timeout: Duration) -> Reach {
    match TcpStream::connect_timeout(&target, timeout) {
        Ok(_) => Reach::Open,
        Err(e) => match e.kind() {
            ErrorKind::ConnectionRefused => Reach::Refused,
            ErrorKind::PermissionDenied => Reach::DeniedLocally,
            ErrorKind::TimedOut | ErrorKind::WouldBlock => Reach::TimedOut,
            // EACCES and EPERM both land here on some kernels; so does
            // ENETUNREACH when a filter takes out the route.
            _ => Reach::Error(format!("{e} ({:?})", e.kind())),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_needs_no_external_check() {
        assert_eq!(external_target("127.0.0.1:8471".parse().unwrap()), None);
        assert_eq!(external_target("[::1]:8471".parse().unwrap()), None);
    }

    #[test]
    fn explicit_address_is_its_own_target() {
        let a: SocketAddr = "10.1.2.3:8471".parse().unwrap();
        assert_eq!(external_target(a), Some(a));
    }

    #[test]
    fn refusal_is_distinguished_from_a_drop() {
        // Port 1 on loopback: nothing listens, and loopback is never filtered,
        // so this must come back as a refusal rather than a timeout.
        let r = probe("127.0.0.1:1".parse().unwrap(), Duration::from_millis(500));
        assert_eq!(r, Reach::Refused, "expected RST, got {r:?}");
    }

    #[test]
    fn an_open_port_probes_open() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let a = l.local_addr().unwrap();
        assert_eq!(probe(a, Duration::from_millis(500)), Reach::Open);
    }
}
