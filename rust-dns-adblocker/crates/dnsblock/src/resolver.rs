//! Upstream-Resolver: leitet Anfragen roh (byte-identisch) an den Upstream
//! weiter – Standard: lokales Unbound auf 127.0.0.1:5353.
//!
//! Optional (Anforderung 7 / AdGuard-Funktionsumfang): **DoH-Upstream**
//! (DNS-over-HTTPS, RFC 8484, `application/dns-message`). Wenn konfiguriert,
//! wird er bevorzugt; UDP/TCP fungieren als Failover, damit ein DoH-Ausfall
//! den Dienst nicht lahmlegt.

use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::time::timeout;

const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_DNS_MSG: usize = 65_535;

#[derive(Debug, Clone)]
pub struct Resolver {
    upstream: SocketAddr,
    /// DoH-URL (RFC 8484) oder None.
    doh_url: Option<String>,
}

impl Resolver {
    pub fn new(upstream: SocketAddr) -> Self {
        Self {
            upstream,
            doh_url: None,
        }
    }

    /// Resolver mit bevorzugtem DoH-Upstream (UDP/TCP als Failover).
    pub fn with_doh(mut self, doh_url: Option<String>) -> Self {
        self.doh_url = doh_url.filter(|u| !u.trim().is_empty());
        self
    }

    /// Bevorzugter Transport + Failover-Kette:
    /// DoH (falls konfiguriert) → UDP (oder TCP bei `prefer_tcp`).
    pub async fn resolve(&self, query: &[u8], prefer_tcp: bool) -> Option<Vec<u8>> {
        if let Some(url) = &self.doh_url {
            match self.resolve_doh(url, query).await {
                Some(bytes) => return Some(bytes),
                None => tracing::debug!(url, "DoH fehlgeschlagen – Failover auf UDP/TCP"),
            }
        }
        if prefer_tcp {
            self.resolve_tcp(query).await
        } else {
            self.resolve_udp(query).await
        }
    }

    /// DoH-Fetch (RFC 8484): POST mit Wireformat-Anfrage.
    async fn resolve_doh(&self, url: &str, query: &[u8]) -> Option<Vec<u8>> {
        static CLIENT: tokio::sync::OnceCell<reqwest::Client> = tokio::sync::OnceCell::const_new();
        let client = CLIENT
            .get_or_init(|| async {
                reqwest::Client::builder()
                    .user_agent("dnsblock/0.2")
                    .build()
                    .unwrap_or_default()
            })
            .await;
        let fut = async {
            let resp = client
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, "application/dns-message")
                .header(reqwest::header::ACCEPT, "application/dns-message")
                .body(query.to_vec())
                .send()
                .await
                .ok()?;
            if !resp.status().is_success() {
                return None;
            }
            let bytes = resp.bytes().await.ok()?;
            if bytes.len() > MAX_DNS_MSG || bytes.is_empty() {
                return None;
            }
            Some(bytes.to_vec())
        };
        timeout(UPSTREAM_TIMEOUT, fut).await.ok().flatten()
    }

    /// UDP-Forward. Liefert `None` bei Timeout/Fehler (Aufrufer sendet SERVFAIL).
    pub async fn resolve_udp(&self, query: &[u8]) -> Option<Vec<u8>> {
        let sock = UdpSocket::bind(("0.0.0.0", 0)).await.ok()?;
        sock.connect(self.upstream).await.ok()?;
        let fut = async {
            sock.send(query).await.ok()?;
            let mut buf = vec![0u8; MAX_DNS_MSG];
            let n = sock.recv(&mut buf).await.ok()?;
            buf.truncate(n);
            Some(buf)
        };
        timeout(UPSTREAM_TIMEOUT, fut).await.ok().flatten()
    }

    /// TCP-Forward (2-Byte-Längen-Framing, für truncierte Antworten).
    pub async fn resolve_tcp(&self, query: &[u8]) -> Option<Vec<u8>> {
        let mut stream = TcpStream::connect(self.upstream).await.ok()?;
        let fut = async {
            let len = u16::try_from(query.len()).ok()?;
            stream.write_all(&len.to_be_bytes()).await.ok()?;
            stream.write_all(query).await.ok()?;
            let mut lenbuf = [0u8; 2];
            stream.read_exact(&mut lenbuf).await.ok()?;
            let len = u16::from_be_bytes(lenbuf) as usize;
            if len > MAX_DNS_MSG {
                return None;
            }
            let mut resp = vec![0u8; len];
            stream.read_exact(&mut resp).await.ok()?;
            Some(resp)
        };
        timeout(UPSTREAM_TIMEOUT, fut).await.ok().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_doh_keeps_empty_as_none() {
        let r = Resolver::new("127.0.0.1:53".parse().unwrap()).with_doh(Some("  ".into()));
        assert!(r.doh_url.is_none());
        let r = Resolver::new("127.0.0.1:53".parse().unwrap())
            .with_doh(Some("https://dns.cloudflare.com/dns-query".into()));
        assert!(r.doh_url.is_some());
    }
}
