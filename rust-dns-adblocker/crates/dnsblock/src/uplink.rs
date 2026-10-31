//! DNS-Uplink-Test (Anforderung 7).
//!
//! Schickt eine echte A-Query nach `example.com` an jeden getesteten Server,
//! misst die RTT und liest den DNS-Statuscode der Antwort. Läuft parallel
//! für alle Server und wird beim Start + auf Knopfdruck (Web-UI) gefahren.

use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
use hickory_proto::rr::RecordType;
use hickory_proto::serialize::binary::BinDecodable;

use crate::state::UplinkTestResult;

const UPTEST_TIMEOUT: Duration = Duration::from_secs(3);
const UPTEST_DOMAIN: &str = "example.com.";

/// Streu-ID ohne Extra-Crate (Nanosekunden als u16).
fn nonce() -> u16 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos ^ (nanos >> 8)) as u16
}

/// Bau viele minimale A-Query als Wireformat.
fn probe_query() -> Vec<u8> {
    let mut m = Message::new();
    m.set_id(nonce());
    m.set_message_type(MessageType::Query);
    m.set_op_code(OpCode::Query);
    m.set_recursion_desired(true);
    m.add_query(Query::query(UPTEST_DOMAIN.parse().unwrap(), RecordType::A));
    m.to_vec().expect("Query immer serialisierbar")
}

/// Sendet eine Probe an einen DNS-Server und misst RTT/Statuscode.
pub async fn test_one(server: SocketAddr) -> UplinkTestResult {
    let started = Instant::now();
    let query = probe_query();

    let fut = async {
        let sock = tokio::net::UdpSocket::bind(("0.0.0.0", 0))
            .await
            .map_err(|e| format!("Socket: {e}"))?;
        sock.connect(server)
            .await
            .map_err(|e| format!("Connect: {e}"))?;
        sock.send(&query).await.map_err(|e| format!("Send: {e}"))?;
        let mut buf = vec![0u8; 4096];
        let n = sock
            .recv(&mut buf)
            .await
            .map_err(|e| format!("Empfangen: {e}"))?;
        let resp = Message::from_bytes(&buf[..n]).map_err(|e| format!("Parse: {e}"))?;
        let rcode = match resp.response_code() {
            ResponseCode::NoError => "NOERROR",
            ResponseCode::NXDomain => "NXDOMAIN",
            ResponseCode::ServFail => "SERVFAIL",
            ResponseCode::Refused => "REFUSED",
            other => {
                return Ok::<_, String>((format!("{other:?}"), false));
            }
        };
        let ok = rcode == "NOERROR" && !resp.answers().is_empty();
        Ok((rcode.to_string(), ok))
    };

    match tokio::time::timeout(UPTEST_TIMEOUT, fut).await {
        Ok(Ok((rcode, ok))) => UplinkTestResult {
            server: server.to_string(),
            ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(-1),
            rcode,
            ok,
            error: None,
        },
        Ok(Err(e)) => UplinkTestResult {
            server: server.to_string(),
            ms: -1,
            rcode: "-".into(),
            ok: false,
            error: Some(e),
        },
        Err(_) => UplinkTestResult {
            server: server.to_string(),
            ms: -1,
            rcode: "-".into(),
            ok: false,
            error: Some("Timeout (keine Antwort in 3 s)".into()),
        },
    }
}

/// Prüft alle Server konkurrierend; Reihenfolge = Eingabereihenfolge.
pub async fn test_many(servers: &[SocketAddr]) -> Vec<UplinkTestResult> {
    let mut order: Vec<Option<UplinkTestResult>> = vec![None; servers.len()];
    let mut set = tokio::task::JoinSet::new();
    for (idx, server) in servers.iter().copied().enumerate() {
        set.spawn(async move { (idx, test_one(server).await) });
    }
    while let Some(joined) = set.join_next().await {
        let Ok((idx, result)) = joined else {
            continue; // Task-Panic: Slot bleibt None
        };
        order[idx] = Some(result);
    }
    order.into_iter().flatten().collect::<Vec<_>>()
}

/// Kurz-/Preis-Zeile fürs Terminal-Log.
pub fn format_result(r: &UplinkTestResult) -> String {
    if r.ok {
        format!("{} → OK ({} ms, {})", r.server, r.ms, r.rcode)
    } else {
        let err = r.error.as_deref().unwrap_or("");
        format!("{} → FEHLGESCHLAGEN ({}, {})", r.server, r.rcode, err)
    }
}
