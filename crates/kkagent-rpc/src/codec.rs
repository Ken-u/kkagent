use bytes::{Buf, BytesMut};
use kkagent_protocol::Frame;
use serde_json;

pub struct NdjsonCodec;

impl NdjsonCodec {
    pub fn encode(frame: &Frame) -> Vec<u8> {
        let mut data = serde_json::to_vec(frame).expect("frame serialization");
        data.push(b'\n');
        data
    }

    pub fn decode(buf: &mut BytesMut) -> Option<Frame> {
        let newline_pos = buf.iter().position(|&b| b == b'\n')?;
        let line = buf.split_to(newline_pos + 1);
        let json_bytes = &line[..line.len() - 1];
        if json_bytes.is_empty() {
            return None;
        }
        match serde_json::from_slice(json_bytes) {
            Ok(frame) => Some(frame),
            Err(e) => {
                tracing::warn!("Failed to decode frame: {}", e);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip() {
        let frame = Frame::Ready;
        let encoded = NdjsonCodec::encode(&frame);
        let mut buf = BytesMut::from(&encoded[..]);
        let decoded = NdjsonCodec::decode(&mut buf).unwrap();
        assert!(matches!(decoded, Frame::Ready));
    }

    #[test]
    fn test_call_roundtrip() {
        let frame = Frame::Call {
            id: "test-1".into(),
            method: "sessions.create".into(),
            params: Some(serde_json::json!({"workspace": "/tmp"})),
        };
        let encoded = NdjsonCodec::encode(&frame);
        let mut buf = BytesMut::from(&encoded[..]);
        let decoded = NdjsonCodec::decode(&mut buf).unwrap();
        match decoded {
            Frame::Call { id, method, .. } => {
                assert_eq!(id, "test-1");
                assert_eq!(method, "sessions.create");
            }
            _ => panic!("expected Call frame"),
        }
    }
}
