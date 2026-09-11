//! Asking the router to let viewers in.
//!
//! Most viewers reach this machine on their own: both ends find their public
//! address through STUN, each sends towards the other, and the routers in
//! between let the replies through. The one kind that cannot is a phone on
//! mobile data. Carriers put every subscriber behind one shared address that
//! hands out a different outside port for every destination, so the port this
//! end is told to expect them on is not the one they arrive from, and a home
//! router drops traffic from a port it has never sent to.
//!
//! The fix is a door that does not care where the knock comes from: a port
//! forwarded on the router to this machine. Most home routers let a program
//! ask for exactly that, through UPnP, so nobody has to log into anything.
//! One port per viewer, the one their connection is already listening on,
//! for as long as they are connected, and taken down again when they go.
//!
//! What is behind the door is a WebRTC connection, which answers nothing that
//! does not carry the credentials in the offer and the certificate that goes
//! with it. An open port with nobody holding those is a port that ignores you.
//!
//! None of this can help when the router itself is behind a shared address,
//! which some internet providers do to home connections too. That is detected
//! and said, rather than an opening being made that leads nowhere.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use igd_next::{AddPortError, PortMappingProtocol, SearchOptions};

/// How long the router is asked to keep a mapping. Short, so one left behind
/// by a crash closes itself, and renewed well before it runs out while the
/// viewer it belongs to is still watching.
const LEASE: Duration = Duration::from_secs(3600);
const RENEW_EVERY: Duration = Duration::from_secs(20 * 60);

/// How long to look for a router before deciding none is going to answer.
/// A router that supports this answers in milliseconds.
const SEARCH_FOR: Duration = Duration::from_secs(3);

/// What the mapping is called in the router's own list of them, so anybody
/// looking there can tell what it is for.
const DESCRIPTION: &str = "Sideband";

/// A router that has agreed to talk about port mappings.
pub struct Router {
    gateway: igd_next::Gateway,
    external: Ipv4Addr,
    local: Ipv4Addr,
}

impl Router {
    /// Looks for the router on the network `local` is on. Blocking, for up to
    /// a few seconds.
    pub fn find(local: Ipv4Addr) -> Result<Router, String> {
        // Searched from the one interface that leads anywhere, for the same
        // reason ICE is bound to it: a virtual adapter's network has no
        // router on it to answer.
        let gateway = igd_next::search_gateway(SearchOptions {
            bind_addr: SocketAddr::new(IpAddr::V4(local), 0),
            timeout: Some(SEARCH_FOR),
            single_search_timeout: Some(SEARCH_FOR),
            ..Default::default()
        })
        .map_err(|_| "the router did not answer a request to open ports (UPnP is off, or not supported)".to_owned())?;

        let external = match gateway.get_external_ip() {
            Ok(IpAddr::V4(ip)) => ip,
            Ok(IpAddr::V6(_)) => return Err("the router only reported an IPv6 address".into()),
            Err(e) => return Err(format!("the router would not say its own address ({e})")),
        };

        if !is_public(external) {
            return Err(format!(
                "the router's own address, {external}, is not a public one. Your internet \
                 provider shares one address between customers, so opening a port on the \
                 router cannot make this machine reachable"
            ));
        }

        Ok(Router { gateway, external, local })
    }

    pub fn external(&self) -> Ipv4Addr {
        self.external
    }
}

/// A port the router is forwarding here. Closed when this is dropped.
pub struct Opening {
    router: Arc<Router>,
    /// The port on this machine the viewer's connection is listening on.
    port: u16,
    /// The port on the router that leads to it. The same one when the router
    /// allows, which is nearly always.
    outside: u16,
    stop: Arc<AtomicBool>,
}

impl Opening {
    /// Where a viewer outside should knock.
    pub fn public(&self) -> SocketAddrV4 {
        SocketAddrV4::new(self.router.external, self.outside)
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

/// Asks the router to forward `port` to this machine. Blocking.
pub fn open(router: &Arc<Router>, port: u16) -> Result<Opening, String> {
    let inside = SocketAddr::new(IpAddr::V4(router.local), port);

    let (outside, lease) = match map(&router.gateway, port, inside, LEASE) {
        Ok(()) => (port, LEASE),
        // Only mappings with no expiry. Those are removed by hand when the
        // viewer goes, which is the same as happens to any other.
        Err(AddPortError::OnlyPermanentLeasesSupported) => {
            map(&router.gateway, port, inside, Duration::ZERO).map_err(describe)?;
            (port, Duration::ZERO)
        }
        // That port is already forwarded to another machine. Any free one
        // does just as well, it only has to be the one that is advertised.
        Err(AddPortError::PortInUse) => {
            let any = router
                .gateway
                .add_any_port(PortMappingProtocol::UDP, inside, LEASE.as_secs() as u32, DESCRIPTION)
                .map_err(|e| format!("the router refused to open a port ({e})"))?;
            (any, LEASE)
        }
        Err(e) => return Err(describe(e)),
    };

    let stop = Arc::new(AtomicBool::new(false));
    if !lease.is_zero() {
        // Renewed on a thread of its own, checking often whether it is still
        // wanted, so a viewer leaving is not followed by twenty minutes of a
        // thread asleep holding a port open.
        let router = Arc::clone(router);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut renewed = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
                if renewed.elapsed() >= RENEW_EVERY {
                    let _ = router.gateway.add_port(
                        PortMappingProtocol::UDP,
                        outside,
                        inside,
                        LEASE.as_secs() as u32,
                        DESCRIPTION,
                    );
                    renewed = Instant::now();
                }
            }
        });
    }

    Ok(Opening { router: Arc::clone(router), port, outside, stop })
}

impl Drop for Opening {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Its own thread, because this can be dropped inside the async
        // runtime, and a router can take a moment to answer.
        let router = Arc::clone(&self.router);
        let outside = self.outside;
        std::thread::spawn(move || {
            let _ = router.gateway.remove_port(PortMappingProtocol::UDP, outside);
        });
    }
}

fn map(
    gateway: &igd_next::Gateway,
    port: u16,
    inside: SocketAddr,
    lease: Duration,
) -> Result<(), AddPortError> {
    gateway.add_port(PortMappingProtocol::UDP, port, inside, lease.as_secs() as u32, DESCRIPTION)
}

fn describe(e: AddPortError) -> String {
    format!("the router refused to open a port ({e})")
}

/// Whether an address is reachable from the internet at large.
///
/// Private ranges, and the shared range carriers put whole neighbourhoods
/// behind (100.64.0.0/10), are the ones that matter: a router reporting one
/// of those is itself behind somebody else's router, and a port opened on it
/// leads only as far as that.
fn is_public(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    let shared = o[0] == 100 && (o[1] & 0xc0) == 64;
    !(ip.is_private()
        || shared
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || o[0] == 0
        || o[0] >= 240)
}

/// The UDP port this end's own candidate is listening on, from its offer.
///
/// ICE is bound to exactly one interface, see `net::local_bind`, so there is
/// exactly one such port to find.
pub fn host_port(sdp: &str) -> Option<u16> {
    sdp.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("a=candidate:")?;
        let f: Vec<&str> = rest.split_whitespace().collect();
        // foundation component transport priority address port typ kind
        (f.len() >= 8 && f[2].eq_ignore_ascii_case("udp") && f[6] == "typ" && f[7] == "host")
            .then(|| f[5].parse().ok())
            .flatten()
    })
}

/// Adds the opening to an offer, as one more address the viewer may try.
///
/// Written into the text rather than handed to the WebRTC stack, because the
/// offer is complete before it leaves here, every candidate included, and the
/// viewer only ever reads it as text. The stack does not need to have gathered
/// this one itself: a viewer's check arriving through the router lands on
/// the socket that is already listening, and is answered like any other.
///
/// Placed directly after the candidate it leads to, in every section that
/// lists it, and typed as server reflexive, which is what it is: this
/// machine's own port as the outside world sees it.
pub fn with_public_candidate(sdp: &str, local_port: u16, public: SocketAddrV4) -> String {
    let already = sdp.lines().any(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        f.len() >= 6 && f[4] == public.ip().to_string() && f[5] == public.port().to_string()
    });
    if already {
        return sdp.to_owned();
    }

    let mut out = String::with_capacity(sdp.len() + 256);
    for line in sdp.split_inclusive('\n') {
        out.push_str(line);

        let Some(rest) = line.trim().strip_prefix("a=candidate:") else { continue };
        let f: Vec<&str> = rest.split_whitespace().collect();
        if f.len() < 8
            || !f[2].eq_ignore_ascii_case("udp")
            || f[7] != "host"
            || f[5] != local_port.to_string()
        {
            continue;
        }

        if !out.ends_with('\n') {
            out.push_str("\r\n");
        }
        // The usual priority for a server reflexive candidate: below this
        // machine's own address, so a viewer on the same network still goes
        // direct, and above anything relayed.
        out.push_str(&format!(
            "a=candidate:upnp{component} {component} udp 1694498815 {ip} {port} typ srflx raddr {raddr} rport {rport}\r\n",
            component = f[1],
            ip = public.ip(),
            port = public.port(),
            raddr = f[4],
            rport = local_port,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer() -> String {
        [
            "v=0",
            "m=video 9 UDP/TLS/RTP/SAVPF 102",
            "a=candidate:1 1 udp 2130706431 192.168.1.20 51234 typ host",
            "a=candidate:2 1 udp 1694498815 203.0.113.5 40000 typ srflx raddr 192.168.1.20 rport 51234",
            "m=audio 9 UDP/TLS/RTP/SAVPF 111",
            "a=candidate:1 1 udp 2130706431 192.168.1.20 51234 typ host",
            "",
        ]
        .join("\r\n")
    }

    #[test]
    fn the_host_port_is_found() {
        assert_eq!(host_port(&offer()), Some(51234));
        assert_eq!(host_port("v=0\r\n"), None);
    }

    #[test]
    fn the_opening_is_offered_after_every_candidate_it_leads_to() {
        let public = SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 7), 51234);
        let out = with_public_candidate(&offer(), 51234, public);

        let added: Vec<&str> = out.lines().filter(|l| l.contains("198.51.100.7")).collect();
        assert_eq!(added.len(), 2, "one per section:\n{out}");
        assert!(added[0].ends_with("typ srflx raddr 192.168.1.20 rport 51234"));

        // Straight after the host candidate, and nothing else disturbed.
        let lines: Vec<&str> = out.lines().collect();
        let host = lines.iter().position(|l| l.contains("typ host")).unwrap();
        assert!(lines[host + 1].contains("198.51.100.7"));
        assert_eq!(out.lines().count(), offer().lines().count() + 2);
        assert!(out.lines().all(|l| !l.ends_with('\r')), "line endings stay CRLF pairs");
    }

    #[test]
    fn an_address_already_offered_is_not_offered_twice() {
        // A router that keeps the same port on the way out makes STUN find
        // exactly this address already.
        let public = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 5), 40000);
        assert_eq!(with_public_candidate(&offer(), 51234, public), offer());
    }

    #[test]
    fn shared_and_private_addresses_are_not_public() {
        for private in ["10.0.0.1", "192.168.1.1", "172.16.5.4", "100.64.0.1", "100.127.255.254", "127.0.0.1", "169.254.1.1"] {
            assert!(!is_public(private.parse().unwrap()), "{private}");
        }
        for public in ["8.8.8.8", "100.128.0.1", "100.63.255.255", "81.2.69.160"] {
            assert!(is_public(public.parse().unwrap()), "{public}");
        }
    }
}
