//! End-to-End-Tests: UDP/TCP-Requests gegen den echten Server,
//! mit gemocktem Upstream (Unbound-Stellvertreter).

use std::sync::Arc;
use std::time::Duration;

use dnsblock::config::ResolvedConfig;
use dnsblock::server;
use filter_engine::FilterEngine;
use hickory_proto::op::{Message, MessageType, Query, ResponseCode};
use hickory_proto::rr::{rdata::A, RData, Record, RecordType};
use hickory_proto::serialize::binary::BinDecodable;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

const MOCK_ANSWER_IP: &str = "192.0.2.42";

/// UDP/TCP-Upstream, der jede Anfrage mit einem A-Record beantwortet.
async fn spawn_mock_upstream() -> std::io::Result<std::net::SocketAddr> {
    let udp = UdpSocket::bind(("127.0.0.1", 0)).await?;
    let addr = udp.local_addr()?;
    let tcp = TcpListener::bind(addr).await?;

    // UDP-Schleife
    let udp = Arc::new(udp);
    tokio::spawn(async move {
        loop {
            let sock = udp.clone();
            let mut buf = vec![0u8; 4096];
            if let Ok((n, peer)) = sock.recv_from(&mut buf).await {
                let req = Message::from_bytes(&buf[..n]).unwrap();
                let bytes = mock_response_bytes(&req);
                let _ = sock.send_to(&bytes, peer).await;
            }
        }
    });

    // TCP-Schleife (2-Byte-Framing)
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = match tcp.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            tokio::spawn(async move {
                loop {
                    let mut lenbuf = [0u8; 2];
                    if stream.read_exact(&mut lenbuf).await.is_err() {
                        return;
                    }
                    let n = u16::from_be_bytes(lenbuf) as usize;
                    let mut req = vec![0u8; n];
                    if stream.read_exact(&mut req).await.is_err() {
                        return;
                    }
                    let parsed = Message::from_bytes(&req).unwrap();
                    let bytes = mock_response_bytes(&parsed);
                    let Some(len) = u16::try_from(bytes.len()).ok() else {
                        return;
                    };
                    if stream.write_all(&len.to_be_bytes()).await.is_err()
                        || stream.write_all(&bytes).await.is_err()
                    {
                        return;
                    }
                }
            });
        }
    });
    Ok(addr)
}

fn mock_response_bytes(req: &Message) -> Vec<u8> {
    let mut resp = Message::new();
    resp.set_id(req.id());
    resp.set_message_type(MessageType::Response);
    resp.set_op_code(req.op_code());
    resp.set_recursion_desired(req.recursion_desired());
    resp.set_recursion_available(true);
    resp.set_response_code(ResponseCode::NoError);
    let q = req.queries()[0].clone();
    resp.add_query(q.clone());
    if q.query_type() == RecordType::A {
        resp.add_answer(Record::from_rdata(
            q.name().clone(),
            300,
            RData::A(A::new(192, 0, 2, 42)),
        ));
    }
    resp.to_vec().unwrap()
}

fn query_bytes(name: &str, rtype: RecordType) -> Vec<u8> {
    let mut m = Message::new();
    m.set_id(0x0042);
    m.set_recursion_desired(true);
    let q = Query::query(name.parse().unwrap(), rtype);
    m.add_query(q);
    m.to_vec().unwrap()
}

async fn udp_roundtrip(server: std::net::SocketAddr, query: &[u8]) -> Message {
    let sock = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
    sock.connect(server).await.unwrap();
    sock.send(query).await.unwrap();
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(3), sock.recv(&mut buf))
        .await
        .expect("Timeout: der Server hat nicht geantwortet")
        .unwrap();
    Message::from_bytes(&buf[..n]).unwrap()
}

async fn tcp_roundtrip(server: std::net::SocketAddr, query: &[u8]) -> Message {
    let mut stream = TcpStream::connect(server).await.unwrap();
    let len = u16::try_from(query.len()).unwrap();
    stream.write_all(&len.to_be_bytes()).await.unwrap();
    stream.write_all(query).await.unwrap();
    let mut lenbuf = [0u8; 2];
    stream.read_exact(&mut lenbuf).await.unwrap();
    let n = u16::from_be_bytes(lenbuf) as usize;
    let mut resp = vec![0u8; n];
    stream.read_exact(&mut resp).await.unwrap();
    Message::from_bytes(&resp).unwrap()
}

fn engine_with_rules() -> FilterEngine {
    let mut e = FilterEngine::new();
    e.load_str("||ads.example.com^\n0.0.0.0 tracker.example.net\n@@||ok.ads.example.com^\n");
    e
}

async fn start_test_server() -> (server::ServerHandle, FilterEngine) {
    let upstream = spawn_mock_upstream().await.unwrap();
    let engine = engine_with_rules();
    let cfg = ResolvedConfig::minimal("127.0.0.1:0".parse().unwrap(), upstream);
    // minimal(): block_mode = zero, Listen leer – exakt wie vorher explizit gesetzt
    let handle = server::start(
        dnsblock::state::shared_engine(Arc::new(engine.clone())),
        cfg,
    )
    .await
    .unwrap();
    (handle, engine)
}

#[tokio::test]
async fn blocked_domain_returns_sinkhole_answer() {
    let (handle, _engine) = start_test_server().await;
    let resp = udp_roundtrip(handle.udp, &query_bytes("ads.example.com.", RecordType::A)).await;
    assert_eq!(resp.response_code(), ResponseCode::NoError);
    assert_eq!(resp.answers().len(), 1);
    match resp.answers()[0].data().unwrap() {
        RData::A(a) => assert_eq!(a.0, std::net::Ipv4Addr::UNSPECIFIED),
        other => panic!("unerwartet: {other:?}"),
    }
}

#[tokio::test]
async fn subdomain_of_blocked_entry_is_blocked() {
    let (handle, _engine) = start_test_server().await;
    let resp = udp_roundtrip(
        handle.udp,
        &query_bytes("video.tracker.example.net.", RecordType::A),
    )
    .await;
    assert!(!resp.answers().is_empty());
    match resp.answers()[0].data().unwrap() {
        RData::A(a) => assert_eq!(a.0, std::net::Ipv4Addr::UNSPECIFIED),
        other => panic!("unerwartet: {other:?}"),
    }
}

#[tokio::test]
async fn allowlisted_domain_is_forwarded() {
    let (handle, _engine) = start_test_server().await;
    let resp = udp_roundtrip(
        handle.udp,
        &query_bytes("ok.ads.example.com.", RecordType::A),
    )
    .await;
    match resp.answers()[0].data().unwrap() {
        RData::A(a) => assert_eq!(a.0.to_string(), MOCK_ANSWER_IP),
        other => panic!("unerwartet: {other:?}"),
    }
}

#[tokio::test]
async fn unknown_domain_is_forwarded_to_upstream() {
    let (handle, _engine) = start_test_server().await;
    let resp = udp_roundtrip(handle.udp, &query_bytes("www.example.org.", RecordType::A)).await;
    assert_eq!(resp.response_code(), ResponseCode::NoError);
    match resp.answers()[0].data().unwrap() {
        RData::A(a) => assert_eq!(a.0.to_string(), MOCK_ANSWER_IP),
        other => panic!("unerwartet: {other:?}"),
    }
}

#[tokio::test]
async fn unreachable_upstream_yields_servfail() {
    let engine = engine_with_rules();
    let cfg = ResolvedConfig::minimal(
        "127.0.0.1:0".parse().unwrap(),
        // Port 1 auf localhost: nichts hört dort (hoffentlich)
        "127.0.0.1:1".parse().unwrap(),
    );
    let handle = server::start(dnsblock::state::shared_engine(Arc::new(engine)), cfg)
        .await
        .unwrap();
    let resp = udp_roundtrip(handle.udp, &query_bytes("www.example.org.", RecordType::A)).await;
    assert_eq!(resp.response_code(), ResponseCode::ServFail);
}

#[tokio::test]
async fn tcp_requests_are_served() {
    let (handle, _engine) = start_test_server().await;
    let resp = tcp_roundtrip(handle.tcp, &query_bytes("ads.example.com.", RecordType::A)).await;
    match resp.answers()[0].data().unwrap() {
        RData::A(a) => assert_eq!(a.0, std::net::Ipv4Addr::UNSPECIFIED),
        other => panic!("unerwartet: {other:?}"),
    }
    let resp = tcp_roundtrip(handle.tcp, &query_bytes("www.example.org.", RecordType::A)).await;
    match resp.answers()[0].data().unwrap() {
        RData::A(a) => assert_eq!(a.0.to_string(), MOCK_ANSWER_IP),
        other => panic!("unerwartet: {other:?}"),
    }
}

#[tokio::test]
async fn stats_are_counted() {
    let (handle, _engine) = start_test_server().await;
    udp_roundtrip(handle.udp, &query_bytes("ads.example.com.", RecordType::A)).await;
    udp_roundtrip(handle.udp, &query_bytes("www.example.org.", RecordType::A)).await;
    // kurz warten, bis die Zähler hochgezählt sind (Handler laufen gespawnt)
    tokio::time::sleep(Duration::from_millis(50)).await;
    let s = &handle.stats;
    use std::sync::atomic::Ordering;
    assert_eq!(s.total.load(Ordering::Relaxed), 2);
    assert_eq!(s.blocked.load(Ordering::Relaxed), 1);
    assert_eq!(s.forwarded.load(Ordering::Relaxed), 1);
}
