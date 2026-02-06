use anyhow::{bail, ensure, Context, Result};
use bytes::{Buf, BufMut, BytesMut};
use uuid::Uuid;

// Frame type tags
const FRAME_REQUEST: u8 = 0x01;
const FRAME_RESPONSE: u8 = 0x02;
const FRAME_ERROR: u8 = 0x03;

#[derive(Debug, Clone)]
pub struct TunnelRequest {
    pub method: String,
    pub uri: String,
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct TunnelResponse {
    pub status: u16,
    pub headers: Vec<(String, Vec<u8>)>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct TunnelError {
    pub message: String,
}

#[derive(Debug, Clone)]
pub enum TunnelFrame {
    Request(TunnelRequest),
    Response(TunnelResponse),
    Error(TunnelError),
}

/// Encode a `TunnelFrame` into a binary WebSocket message payload.
///
/// Wire format:
/// ```text
/// [16 bytes: stream_id UUID]
/// [1 byte: frame_type]
/// [payload]
/// ```
pub fn encode(stream_id: Uuid, frame: &TunnelFrame) -> Vec<u8> {
    let mut buf = BytesMut::new();

    // stream_id (16 bytes, big-endian)
    buf.put_slice(stream_id.as_bytes());

    match frame {
        TunnelFrame::Request(req) => {
            buf.put_u8(FRAME_REQUEST);
            encode_string(&mut buf, &req.method);
            encode_string(&mut buf, &req.uri);
            encode_headers(&mut buf, &req.headers);
            encode_body(&mut buf, &req.body);
        }
        TunnelFrame::Response(resp) => {
            buf.put_u8(FRAME_RESPONSE);
            buf.put_u16(resp.status);
            encode_headers(&mut buf, &resp.headers);
            encode_body(&mut buf, &resp.body);
        }
        TunnelFrame::Error(err) => {
            buf.put_u8(FRAME_ERROR);
            encode_string(&mut buf, &err.message);
        }
    }

    buf.to_vec()
}

/// Decode a binary WebSocket message payload into a `(stream_id, TunnelFrame)`.
pub fn decode_frame(data: &[u8]) -> Result<(Uuid, TunnelFrame)> {
    ensure!(data.len() >= 17, "frame too short: need at least 17 bytes");

    let mut buf = &data[..];

    // stream_id
    let mut uuid_bytes = [0u8; 16];
    uuid_bytes.copy_from_slice(&buf[..16]);
    buf.advance(16);
    let stream_id = Uuid::from_bytes(uuid_bytes);

    // frame_type
    let frame_type = buf.get_u8();

    let frame = match frame_type {
        FRAME_REQUEST => {
            let method = decode_string(&mut buf).context("method")?;
            let uri = decode_string(&mut buf).context("uri")?;
            let headers = decode_headers(&mut buf).context("headers")?;
            let body = decode_body(&mut buf).context("body")?;
            TunnelFrame::Request(TunnelRequest {
                method,
                uri,
                headers,
                body,
            })
        }
        FRAME_RESPONSE => {
            ensure!(buf.remaining() >= 2, "truncated response status");
            let status = buf.get_u16();
            let headers = decode_headers(&mut buf).context("headers")?;
            let body = decode_body(&mut buf).context("body")?;
            TunnelFrame::Response(TunnelResponse {
                status,
                headers,
                body,
            })
        }
        FRAME_ERROR => {
            let message = decode_string(&mut buf).context("error message")?;
            TunnelFrame::Error(TunnelError { message })
        }
        other => bail!("unknown frame type: 0x{other:02x}"),
    };

    Ok((stream_id, frame))
}

// --- encoding helpers ---

fn encode_string(buf: &mut BytesMut, s: &str) {
    buf.put_u32(s.len() as u32);
    buf.put_slice(s.as_bytes());
}

fn encode_headers(buf: &mut BytesMut, headers: &[(String, Vec<u8>)]) {
    buf.put_u32(headers.len() as u32);
    for (name, value) in headers {
        encode_string(buf, name);
        buf.put_u32(value.len() as u32);
        buf.put_slice(value);
    }
}

fn encode_body(buf: &mut BytesMut, body: &[u8]) {
    buf.put_u32(body.len() as u32);
    buf.put_slice(body);
}

// --- decoding helpers ---

fn decode_string(buf: &mut &[u8]) -> Result<String> {
    ensure!(buf.remaining() >= 4, "truncated string length");
    let len = buf.get_u32() as usize;
    ensure!(buf.remaining() >= len, "truncated string data");
    let s = std::str::from_utf8(&buf[..len])?.to_string();
    buf.advance(len);
    Ok(s)
}

fn decode_headers(buf: &mut &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    ensure!(buf.remaining() >= 4, "truncated header count");
    let count = buf.get_u32() as usize;
    let mut headers = Vec::with_capacity(count);
    for _ in 0..count {
        let name = decode_string(buf)?;
        ensure!(buf.remaining() >= 4, "truncated header value length");
        let vlen = buf.get_u32() as usize;
        ensure!(buf.remaining() >= vlen, "truncated header value");
        let value = buf[..vlen].to_vec();
        buf.advance(vlen);
        headers.push((name, value));
    }
    Ok(headers)
}

fn decode_body(buf: &mut &[u8]) -> Result<Vec<u8>> {
    ensure!(buf.remaining() >= 4, "truncated body length");
    let len = buf.get_u32() as usize;
    ensure!(buf.remaining() >= len, "truncated body data");
    let body = buf[..len].to_vec();
    buf.advance(len);
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_request() {
        let stream_id = Uuid::new_v4();
        let req = TunnelFrame::Request(TunnelRequest {
            method: "GET".into(),
            uri: "/hello?foo=bar".into(),
            headers: vec![
                ("host".into(), b"example.com".to_vec()),
                ("accept".into(), b"*/*".to_vec()),
            ],
            body: vec![],
        });

        let encoded = encode(stream_id, &req);
        let (decoded_id, decoded) = decode_frame(&encoded).unwrap();

        assert_eq!(decoded_id, stream_id);
        let TunnelFrame::Request(r) = decoded else {
            panic!("expected Request");
        };
        assert_eq!(r.method, "GET");
        assert_eq!(r.uri, "/hello?foo=bar");
        assert_eq!(r.headers.len(), 2);
        assert_eq!(r.body.len(), 0);
    }

    #[test]
    fn roundtrip_response() {
        let stream_id = Uuid::new_v4();
        let resp = TunnelFrame::Response(TunnelResponse {
            status: 200,
            headers: vec![("content-type".into(), b"text/plain".to_vec())],
            body: b"Hello, world!".to_vec(),
        });

        let encoded = encode(stream_id, &resp);
        let (decoded_id, decoded) = decode_frame(&encoded).unwrap();

        assert_eq!(decoded_id, stream_id);
        let TunnelFrame::Response(r) = decoded else {
            panic!("expected Response");
        };
        assert_eq!(r.status, 200);
        assert_eq!(r.headers.len(), 1);
        assert_eq!(r.body, b"Hello, world!");
    }

    #[test]
    fn roundtrip_error() {
        let stream_id = Uuid::new_v4();
        let err = TunnelFrame::Error(TunnelError {
            message: "connection refused".into(),
        });

        let encoded = encode(stream_id, &err);
        let (decoded_id, decoded) = decode_frame(&encoded).unwrap();

        assert_eq!(decoded_id, stream_id);
        let TunnelFrame::Error(e) = decoded else {
            panic!("expected Error");
        };
        assert_eq!(e.message, "connection refused");
    }
}
