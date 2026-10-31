//! DNS-Nachrichtenhelfer (Parse, Block-/Fehler-Antworten).

use crate::config::BlockMode;
use hickory_proto::op::{Message, MessageType, ResponseCode};
use hickory_proto::rr::{rdata::A, Name, RData, Record, RecordType};
use hickory_proto::serialize::binary::BinDecodable;

/// Parst eine Roh-Anfrage.
pub fn parse_request(bytes: &[u8]) -> Option<Message> {
    Message::from_bytes(bytes).ok()
}

/// Kleingeschriebener QName der ersten Frage, inkl. trailing dot.
pub fn qname(msg: &Message) -> Option<String> {
    msg.queries()
        .first()
        .map(|q| q.name().to_lowercase().to_string())
}

/// Antwort für eine blockierte Anfrage.
pub fn blocked_response(req: &Message, mode: BlockMode) -> Vec<u8> {
    let mut resp = base_response(req);
    match mode {
        BlockMode::Zero => {
            resp.set_response_code(ResponseCode::NoError);
            let q = req.queries()[0].clone();
            if q.query_type() == RecordType::A {
                let name: Name = q.name().to_lowercase();
                let record = Record::from_rdata(
                    name,
                    // TTL 0: Clients sollen nicht cachen
                    0,
                    RData::A(A::new(0, 0, 0, 0)),
                );
                resp.add_answer(record);
            }
            // AAAA & Co.: NODATA (leere Antwort, NoError)
        }
        BlockMode::NxDomain => {
            resp.set_response_code(ResponseCode::NXDomain);
        }
        BlockMode::Refused => {
            resp.set_response_code(ResponseCode::Refused);
        }
    }
    resp.to_vec().unwrap_or_default()
}

/// SERVFAIL-Antwort (z. B. wenn der Upstream nicht antwortet).
pub fn servfail_response(req: &Message) -> Vec<u8> {
    let mut resp = base_response(req);
    resp.set_response_code(ResponseCode::ServFail);
    resp.to_vec().unwrap_or_default()
}

fn base_response(req: &Message) -> Message {
    let mut resp = Message::new();
    resp.set_id(req.id());
    resp.set_message_type(MessageType::Response);
    resp.set_op_code(req.op_code());
    resp.set_recursion_desired(req.recursion_desired());
    resp.set_recursion_available(true);
    for q in req.queries() {
        resp.add_query(q.clone());
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::op::Query;

    fn query_for(name: &str, rtype: RecordType) -> Message {
        let mut m = Message::new();
        m.set_id(0x1234);
        m.set_recursion_desired(true);
        let q = Query::query(name.parse().unwrap(), rtype);
        m.add_query(q);
        m
    }

    #[test]
    fn zero_mode_returns_0_0_0_0_for_a() {
        let req = query_for("ads.example.com.", RecordType::A);
        let bytes = blocked_response(&req, BlockMode::Zero);
        let resp = Message::from_bytes(&bytes).unwrap();
        assert_eq!(resp.id(), 0x1234);
        assert_eq!(resp.response_code(), ResponseCode::NoError);
        let ans = resp.answers();
        assert_eq!(ans.len(), 1);
        assert_eq!(ans[0].record_type(), RecordType::A);
        match ans[0].data().unwrap() {
            RData::A(a) => assert_eq!(a.0, std::net::Ipv4Addr::UNSPECIFIED),
            other => panic!("unerwartetes RData: {other:?}"),
        }
    }

    #[test]
    fn zero_mode_returns_nodata_for_aaaa() {
        let req = query_for("ads.example.com.", RecordType::AAAA);
        let bytes = blocked_response(&req, BlockMode::Zero);
        let resp = Message::from_bytes(&bytes).unwrap();
        assert_eq!(resp.response_code(), ResponseCode::NoError);
        assert!(resp.answers().is_empty());
    }

    #[test]
    fn nxdomain_and_refused_modes() {
        let req = query_for("x.example.", RecordType::A);
        assert_eq!(
            Message::from_bytes(&blocked_response(&req, BlockMode::NxDomain))
                .unwrap()
                .response_code(),
            ResponseCode::NXDomain
        );
        assert_eq!(
            Message::from_bytes(&blocked_response(&req, BlockMode::Refused))
                .unwrap()
                .response_code(),
            ResponseCode::Refused
        );
    }

    #[test]
    fn qname_is_lowercased() {
        let req = query_for("ADS.Example.COM.", RecordType::A);
        assert_eq!(qname(&req).as_deref(), Some("ads.example.com."));
    }
}
