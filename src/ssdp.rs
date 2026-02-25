use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::watch;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const SSDP_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const SSDP_PORT: u16 = 1900;
const DIAL_ST: &str = "urn:dial-multiscreen-org:service:dial:1";
const DIAL_DEVICE_ST: &str = "urn:dial-multiscreen-org:device:dial:1";
const NOTIFY_INTERVAL: Duration = Duration::from_secs(60);
const CACHE_MAX_AGE: u32 = 1800;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Discover the first non-loopback IPv4 address on this machine.
pub fn get_local_ip() -> Result<Ipv4Addr> {
    for iface in nix_net_interfaces()? {
        if !iface.is_loopback && iface.ip != Ipv4Addr::UNSPECIFIED {
            return Ok(iface.ip);
        }
    }
    Err("no non-loopback IPv4 address found".into())
}

/// Run the SSDP responder + periodic alive announcer.
///
/// Listens on `239.255.255.250:1900` for M-SEARCH requests from DIAL
/// controllers and responds with the device description URL.
pub async fn run_ssdp(
    local_ip: Ipv4Addr,
    http_port: u16,
    uuid: &str,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let socket = bind_multicast(local_ip)?;
    let socket = UdpSocket::from_std(socket)?;

    // Separate socket for sending unicast responses and multicast NOTIFYs.
    // We bind to a random ephemeral port on our local IP so responses come
    // from the right source address (not 0.0.0.0).
    let send_socket = Arc::new(UdpSocket::bind(SocketAddrV4::new(local_ip, 0)).await?);

    let location = format!("http://{}:{}/ssdp/device-desc.xml", local_ip, http_port);
    let usn = format!("uuid:{}::urn:dial-multiscreen-org:service:dial:1", uuid);

    tracing::info!("SSDP listening on 239.255.255.250:1900 (local_ip={local_ip})");

    // Send an initial NOTIFY right away
    send_alive(&send_socket, &location, &usn).await;

    let mut notify_interval = tokio::time::interval(NOTIFY_INTERVAL);
    notify_interval.tick().await; // consume the immediate first tick

    let mut buf = [0u8; 2048];

    loop {
        tokio::select! {
            biased;

            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("SSDP shutting down, sending byebye");
                    send_byebye(&send_socket, &usn).await;
                    return Ok(());
                }
            }

            _ = notify_interval.tick() => {
                send_alive(&send_socket, &location, &usn).await;
            }

            result = socket.recv_from(&mut buf) => {
                let (len, src) = result?;
                let msg = match std::str::from_utf8(&buf[..len]) {
                    Ok(s) => s,
                    Err(_) => continue,
                };

                if let Some(mx) = parse_msearch(msg) {
                    let delay = rand_delay(mx);
                    let response = build_search_response(&location, &usn);
                    let send = Arc::clone(&send_socket);

                    // The spec says wait a random time in [0, MX] before replying.
                    // We do this per-request so we don't block the main loop.
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        if let Err(e) = send.send_to(response.as_bytes(), src).await {
                            tracing::warn!("SSDP failed to send response to {src}: {e}");
                        } else {
                            tracing::debug!("SSDP responded to M-SEARCH from {src}");
                        }
                    });
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// M-SEARCH parsing
// ---------------------------------------------------------------------------

/// If the packet is a valid DIAL M-SEARCH, return the MX value (clamped to 1..5).
fn parse_msearch(msg: &str) -> Option<u64> {
    let first_line = msg.lines().next()?;
    if !first_line.starts_with("M-SEARCH * HTTP/1.1") {
        return None;
    }

    let headers = parse_headers(msg);

    // Must match either DIAL search target or ssdp:all
    let st = headers.iter().find(|(k, _)| k == "st").map(|(_, v)| *v)?;
    if st != DIAL_ST && st != DIAL_DEVICE_ST && st != "ssdp:all" && st != "upnp:rootdevice" {
        return None;
    }

    let mx = headers
        .iter()
        .find(|(k, _)| k == "mx")
        .and_then(|(_, v)| v.parse::<u64>().ok())
        .unwrap_or(3)
        .clamp(1, 5);

    Some(mx)
}

/// Parse HTTP-style headers from an SSDP message.
/// Keys are lowercased for case-insensitive matching.
fn parse_headers(msg: &str) -> Vec<(String, &str)> {
    msg.lines()
        .skip(1) // skip request line
        .filter_map(|line| {
            let (key, val) = line.split_once(':')?;
            Some((key.trim().to_ascii_lowercase(), val.trim()))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Response builders
// ---------------------------------------------------------------------------

fn build_search_response(location: &str, usn: &str) -> String {
    let date = httpdate::fmt_http_date(std::time::SystemTime::now());
    format!(
"HTTP/1.1 200 OK\r\n\
CACHE-CONTROL: max-age={CACHE_MAX_AGE}\r\n\
DATE: {date}\r\n\
EXT: \r\n\
LOCATION: {location}\r\n\
ST: {DIAL_ST}\r\n\
USN: {usn}\r\n\
SERVER: Linux/4.14 UPnP/1.1 ytcast-lite/0.1\r\n\
CONFIGID.UPNP.ORG: 7337\r\n\
BOOTID.UPNP.ORG: 7337\r\n\
\r\n"
    )
}

fn build_alive_notify(location: &str, usn: &str) -> String {
    format!(
"NOTIFY * HTTP/1.1\r\n\
HOST: {SSDP_ADDR}:{SSDP_PORT}\r\n\
CACHE-CONTROL: max-age={CACHE_MAX_AGE}\r\n\
LOCATION: {location}\r\n\
NT: {DIAL_ST}\r\n\
NTS: ssdp:alive\r\n\
USN: {usn}\r\n\
SERVER: Linux/4.14 UPnP/1.1 ytcast-lite/0.1\r\n\
\r\n"
    )
}

fn build_byebye_notify(usn: &str) -> String {
    format!(
"NOTIFY * HTTP/1.1\r\n\
HOST: {SSDP_ADDR}:{SSDP_PORT}\r\n\
NT: {DIAL_ST}\r\n\
NTS: ssdp:byebye\r\n\
USN: {usn}\r\n\
\r\n"
    )
}

// ---------------------------------------------------------------------------
// Network helpers
// ---------------------------------------------------------------------------

async fn send_alive(socket: &UdpSocket, location: &str, usn: &str) {
    let msg = build_alive_notify(location, usn);
    let dest = SocketAddr::from(SocketAddrV4::new(SSDP_ADDR, SSDP_PORT));
    if let Err(e) = socket.send_to(msg.as_bytes(), dest).await {
        tracing::warn!("SSDP alive notify failed: {e}");
    } else {
        tracing::debug!("SSDP alive notify sent");
    }
}

async fn send_byebye(socket: &UdpSocket, usn: &str) {
    let msg = build_byebye_notify(usn);
    let dest = SocketAddr::from(SocketAddrV4::new(SSDP_ADDR, SSDP_PORT));
    // Send twice for reliability (UDP is unreliable).
    for _ in 0..2 {
        let _ = socket.send_to(msg.as_bytes(), dest).await;
    }
}

fn rand_delay(mx: u64) -> Duration {
    if mx == 0 {
        return Duration::ZERO;
    }
    let millis = rand::thread_rng().gen_range(0..mx * 1000);
    Duration::from_millis(millis)
}

/// Create a UDP socket bound to 0.0.0.0:1900 joined to the SSDP multicast group.
fn bind_multicast(local_ip: Ipv4Addr) -> Result<std::net::UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

    socket.set_reuse_address(true)?;

    // SO_REUSEPORT is not available on all platforms (notably absent on Windows)
    // but on Linux (the Pi's OS) it is needed when another process also binds 1900.
    #[cfg(not(target_os = "windows"))]
    socket.set_reuse_port(true)?;

    socket.set_nonblocking(true)?;

    let bind_addr = SockAddr::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, SSDP_PORT));
    socket.bind(&bind_addr)?;

    socket.join_multicast_v4(&SSDP_ADDR, &local_ip)?;

    // Set multicast TTL to 2 (standard for SSDP on a LAN).
    socket.set_multicast_ttl_v4(2)?;

    Ok(socket.into())
}

// ---------------------------------------------------------------------------
// Local IP discovery
// ---------------------------------------------------------------------------

struct IfaceInfo {
    ip: Ipv4Addr,
    is_loopback: bool,
}

/// Cross-platform helper to enumerate IPv4 interfaces.
///
/// On Linux we use a simple UDP connect trick (no actual traffic) to let the
/// kernel tell us which source IP it would pick for an external destination.
/// This avoids depending on `nix` or `libc` directly.
fn nix_net_interfaces() -> Result<Vec<IfaceInfo>> {
    // Trick: connect a UDP socket to a public address. The OS will bind it to
    // the correct local interface. We never send anything.
    let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
    socket.connect("8.8.8.8:80")?;
    let local_addr = socket.local_addr()?;

    if let SocketAddr::V4(v4) = local_addr {
        Ok(vec![IfaceInfo {
            ip: *v4.ip(),
            is_loopback: v4.ip().is_loopback(),
        }])
    } else {
        Err("no IPv4 address found".into())
    }
}
