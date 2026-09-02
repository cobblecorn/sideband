//! Local signalling, no cloud involved.
//!
//! Because the offer is non-trickle and self-contained, the whole handshake is
//! two plain HTTP requests. That is small enough to hand-roll, which is why
//! LAN and Tailscale need no Worker, no account and no infrastructure.
//!
//! It is still not open house. Being on the same network is not consent to
//! watch someone's screen, a flatmate, a guest, or anything already running
//! on the machine could otherwise just open the port. So the link carries a
//! secret, every route checks it, and the session can be claimed exactly once,
//! matching what the relay enforces for internet pairing.
//!
//! Deliberately not a general web server: it answers four routes and closes.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

const PAGE: &str = include_str!("client.html");

/// 128 bits, hex. Never spoken aloud, it travels inside the link, so there
/// is no reason to make it short enough to read out.
pub fn make_secret() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Shared with the caller so the session can be marked claimed.
#[derive(Clone)]
pub struct LocalSession {
    pub secret: Arc<String>,
    pub offer: Arc<String>,
    claimed: Arc<AtomicBool>,
}

impl LocalSession {
    pub fn new(secret: String, offer: String) -> Self {
        Self {
            secret: Arc::new(secret),
            offer: Arc::new(offer),
            claimed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Marks the session claimed, returning whether this caller was first.
    /// A second viewer racing the first gets `false` and is turned away.
    fn claim(&self) -> bool {
        !self.claimed.swap(true, Ordering::SeqCst)
    }

    fn is_claimed(&self) -> bool {
        self.claimed.load(Ordering::Relaxed)
    }
}

/// Serves the viewer page and the SDP exchange.
pub async fn serve(
    addr: SocketAddr,
    session: LocalSession,
    answers: mpsc::Sender<String>,
) -> Result<(), String> {
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| format!("could not bind {addr}: {e}"))?;

    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let session = session.clone();
        let answers = answers.clone();
        tokio::spawn(async move {
            let _ = handle(stream, session, answers).await;
        });
    }
}

async fn handle(
    mut stream: TcpStream,
    session: LocalSession,
    answers: mpsc::Sender<String>,
) -> Result<(), String> {
    let mut buf = Vec::with_capacity(8192);
    let mut chunk = [0u8; 4096];

    let header_end = loop {
        let n = stream.read(&mut chunk).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = find_double_crlf(&buf) {
            break i;
        }
        if buf.len() > 1 << 20 {
            return Ok(()); // absurd headers; drop it
        }
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();

    // Query strings end up in browser history and server logs, so the secret
    // travels as a path segment instead.
    let path = target.split('?').next().unwrap_or_default();
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    match (method, segments.as_slice()) {
        // The page itself. Handing it out without the secret would confirm
        // that something is being shared here, which is not anyone's business.
        ("GET", ["v", secret]) if constant_time_eq(secret, &session.secret) => {
            respond(&mut stream, "200 OK", "text/html; charset=utf-8", PAGE.as_bytes()).await
        }

        ("GET", ["v", secret, "offer"]) if constant_time_eq(secret, &session.secret) => {
            if session.is_claimed() {
                return respond(&mut stream, "410 Gone", "text/plain", b"already claimed").await;
            }
            respond(&mut stream, "200 OK", "application/sdp", session.offer.as_bytes()).await
        }

        ("POST", ["v", secret, "answer"]) if constant_time_eq(secret, &session.secret) => {
            if !session.claim() {
                return respond(&mut stream, "409 Conflict", "text/plain", b"already claimed")
                    .await;
            }

            let want: usize = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse().ok())?
                })
                .unwrap_or(0);

            // An SDP with a full candidate set is a few kilobytes.
            if want > 64 * 1024 {
                return respond(&mut stream, "413 Payload Too Large", "text/plain", b"too large")
                    .await;
            }

            let mut body = buf[header_end + 4..].to_vec();
            while body.len() < want {
                let n = stream.read(&mut chunk).await.map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..n]);
            }

            let sdp = String::from_utf8_lossy(&body).to_string();
            let _ = answers.send(sdp).await;
            respond(&mut stream, "200 OK", "text/plain", b"ok").await
        }

        // Everything else, including a wrong secret, looks identical. There is
        // nothing to learn from probing this port.
        _ => respond(&mut stream, "404 Not Found", "text/plain", b"not found").await,
    }
}

/// Compares without an early exit, so a wrong guess reveals nothing about how
/// much of it was right.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

async fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> Result<(), String> {
    let head = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Referrer-Policy: no-referrer\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stream.write_all(body).await.map_err(|e| e.to_string())?;
    stream.flush().await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::{constant_time_eq, make_secret, LocalSession};

    #[test]
    fn secrets_are_long_and_distinct() {
        let a = make_secret();
        let b = make_secret();
        assert_eq!(a.len(), 32, "128 bits of hex");
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn comparison_rejects_wrong_and_short_values() {
        let s = "abcdef0123456789";
        assert!(constant_time_eq(s, s));
        assert!(!constant_time_eq(s, "abcdef012345678"));
        assert!(!constant_time_eq(s, "abcdef0123456780"));
        assert!(!constant_time_eq(s, ""));
    }

    #[test]
    fn a_session_can_be_claimed_exactly_once() {
        let session = LocalSession::new(make_secret(), "v=0".into());
        assert!(!session.is_claimed());
        assert!(session.claim(), "first claim wins");
        assert!(!session.claim(), "a second viewer is turned away");
        assert!(session.is_claimed());
    }
}
