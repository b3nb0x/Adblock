//! UDP/TCP-DNS-Server mit Sinkhole-Logik, Statistik und Query-Log.
//!
//! Fix-Notizen (Review Phase 2):
//! - **UDP wird pro Datagramm gespawnt** statt sequenziell abgearbeitet –
//!   sonst blockt ein hängender Upstream (bis 3 s) die komplette LAN-DNS.
//! - **Empfangspuffer 64 KiB** statt 4 KiB, damit EDNS-Antworten > 4 KiB
//!   nicht stumm beschnitten werden.
//! - Begrenzung auf 256 parallele Handler via Semaphore (Task-Explosion bei
//!   Flood bleibt aus).
//! - Engine hinter `RwLock<Arc<FilterEngine>>` für Hot-Reload.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;

use crate::config::BlockMode;
use crate::dns;
use crate::resolver::Resolver;
use crate::state::{Action, QueryLogEntry, SharedEngine, Stats};

/// Maximale UDP-Datagramm-Größe (DNS-over-UDP-theoretisch 65399).
const UDP_BUF: usize = 65_535;
/// Maximal paralleler UDP-Handler.
const MAX_PARALLEL_UDP: usize = 256;

/// Startet UDP- und TCP-Listener. Beide binden sofort (Port 0 = ephemeral,
/// nützlich für Tests) und laufen als Hintergrund-Tasks.
pub async fn start(
    engine: SharedEngine,
    cfg: crate::config::ResolvedConfig,
) -> std::io::Result<ServerHandle> {
    let udp = UdpSocket::bind(cfg.listen_udp).await?;
    let udp_addr = udp.local_addr()?;
    let tcp = TcpListener::bind(cfg.listen_tcp).await?;
    let tcp_addr = tcp.local_addr()?;

    let resolver = Arc::new(Resolver::new(cfg.upstream).with_doh(cfg.upstream_doh.clone()));
    let stats = Arc::new(Stats::default());
    let mode = cfg.block_mode;

    let udp_task = tokio::spawn(udp_loop(
        udp,
        engine.clone(),
        resolver.clone(),
        mode,
        stats.clone(),
    ));
    let tcp_task = tokio::spawn(tcp_loop(tcp, engine, resolver, mode, stats.clone()));

    Ok(ServerHandle {
        udp: udp_addr,
        tcp: tcp_addr,
        stats,
        tasks: vec![udp_task, tcp_task],
    })
}

/// Handle auf den laufenden Server (Adressen + Statistik-Zugriff).
#[derive(Debug)]
pub struct ServerHandle {
    pub udp: SocketAddr,
    pub tcp: SocketAddr,
    pub stats: Arc<Stats>,
    tasks: Vec<JoinHandle<()>>,
}

impl ServerHandle {
    /// Stoppt alle Server-Tasks.
    pub fn shutdown(&self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

async fn udp_loop(
    sock: UdpSocket,
    engine: SharedEngine,
    resolver: Arc<Resolver>,
    mode: BlockMode,
    stats: Arc<Stats>,
) {
    let sock = Arc::new(sock);
    let semaphore = Arc::new(Semaphore::new(MAX_PARALLEL_UDP));

    loop {
        let mut buf = vec![0u8; UDP_BUF];
        let Ok((n_bytes, peer)) = sock.recv_from(&mut buf).await else {
            tracing::error!("UDP recv fehlgeschlagen – Loop pausiert 1 s");
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            continue;
        };
        let raw = buf[..n_bytes].to_vec();

        // Pro Anfrage ein eigener Handler: ein hängender Upstream (3 s
        // Timeout) blockiert die übrige LAN-DNS nicht mehr.
        let permit: OwnedSemaphorePermit = semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("Semaphore geschlossen nie");
        let sock = sock.clone();
        let engine = engine.clone();
        let resolver = resolver.clone();
        let stats = stats.clone();
        tokio::spawn(async move {
            let resp = handle_datagram(&raw, engine, resolver, mode, &stats, false).await;
            if let Some(bytes) = resp {
                let _ = sock.send_to(&bytes, peer).await;
            }
            drop(permit);
        });
    }
}

async fn tcp_loop(
    listener: TcpListener,
    engine: SharedEngine,
    resolver: Arc<Resolver>,
    mode: BlockMode,
    stats: Arc<Stats>,
) {
    loop {
        if let Ok((stream, _peer)) = listener.accept().await {
            let engine = engine.clone();
            let resolver = resolver.clone();
            let stats = stats.clone();
            tokio::spawn(async move {
                handle_tcp_conn(stream, engine, resolver, mode, stats).await;
            });
        }
    }
}

async fn handle_tcp_conn(
    mut stream: TcpStream,
    engine: SharedEngine,
    resolver: Arc<Resolver>,
    mode: BlockMode,
    stats: Arc<Stats>,
) {
    loop {
        let mut lenbuf = [0u8; 2];
        if stream.read_exact(&mut lenbuf).await.is_err() {
            return; // EOF oder Fehler: Verbindung beenden
        }
        let len = u16::from_be_bytes(lenbuf) as usize;
        if len == 0 {
            return;
        }
        let mut req = vec![0u8; len];
        if stream.read_exact(&mut req).await.is_err() {
            return;
        }
        // TCP-Transport auch Richtung Upstream verwenden (korrekt für
        // Antworten > 512 Bytes, TC-Flag wird so nie nötig)
        let resp =
            handle_datagram(&req, engine.clone(), resolver.clone(), mode, &stats, true).await;
        let Some(resp) = resp else { return };
        let out_len = match u16::try_from(resp.len()) {
            Ok(l) => l,
            Err(_) => return,
        };
        if stream.write_all(&out_len.to_be_bytes()).await.is_err()
            || stream.write_all(&resp).await.is_err()
        {
            return;
        }
    }
}

/// Kernlogik: parst die Anfrage, entscheidet block/forward, baut die Antwort
/// und schreibt den Query-Log-Eintrag.
async fn handle_datagram(
    raw: &[u8],
    engine: SharedEngine,
    resolver: Arc<Resolver>,
    mode: BlockMode,
    stats: &Stats,
    prefer_tcp: bool,
) -> Option<Vec<u8>> {
    let req = match dns::parse_request(raw) {
        Some(m) => m,
        None => {
            stats.failed.fetch_add(1, Ordering::Relaxed);
            return None;
        }
    };
    stats.total.fetch_add(1, Ordering::Relaxed);

    let name = dns::qname(&req);
    let snapshot = engine.read().await.clone(); // Arc-Klon, kein Lock im Weiteren
    let blocked = name
        .as_deref()
        .map(|n| snapshot.is_blocked(n))
        .unwrap_or(false);

    if blocked {
        stats.blocked.fetch_add(1, Ordering::Relaxed);
        if let Some(q) = name.as_deref() {
            tracing::debug!(qname = q, "blockiert");
            stats.log_push(QueryLogEntry {
                ts: now_millis(),
                qname: q.trim_end_matches('.').to_string(),
                action: Action::Blocked,
            });
        }
        return Some(dns::blocked_response(&req, mode));
    }

    let forwarded = resolver.resolve(raw, prefer_tcp).await;

    match forwarded {
        Some(bytes) => {
            stats.forwarded.fetch_add(1, Ordering::Relaxed);
            if let Some(q) = name.as_deref() {
                stats.log_push(QueryLogEntry {
                    ts: now_millis(),
                    qname: q.trim_end_matches('.').to_string(),
                    action: Action::Forwarded,
                });
            }
            Some(bytes)
        }
        None => {
            stats.failed.fetch_add(1, Ordering::Relaxed);
            if let Some(q) = name.as_deref() {
                tracing::warn!(qname = q, "Upstream-Timeout");
                stats.log_push(QueryLogEntry {
                    ts: now_millis(),
                    qname: q.trim_end_matches('.').to_string(),
                    action: Action::Failed,
                });
            }
            Some(dns::servfail_response(&req))
        }
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
