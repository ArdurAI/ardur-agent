//! A minimal decoder for AWS's binary `application/vnd.amazon.eventstream`
//! framing — the wire format `InvokeModelWithResponseStream` answers with.
//! Distinct from Server-Sent Events: each message is a length-prefixed
//! binary frame, not a text line.
//!
//! Frame layout (all integers big-endian):
//!
//! ```text
//! +------------------+-------------------+-------------+---------+-------------+-------------+
//! | total length (4) | headers length (4)| prelude crc | headers | payload     | message crc |
//! +------------------+-------------------+-------------+---------+-------------+-------------+
//! ```
//!
//! `total length` counts the whole frame including both CRCs. `prelude crc`
//! is the CRC32 of the first 8 bytes; `message crc` is the CRC32 of every
//! byte before it. Each header is `name_len(1) name(name_len) value_type(1)
//! value...` — this crate only needs the string value type (`7`), the only
//! one Bedrock's chunk/exception headers use.
//!
//! See <https://docs.aws.amazon.com/AmazonS3/latest/API/RESTSelectObjectAppendix.html#RESTSelectObjectAppendixEventStream>
//! (the format is shared across AWS services, documented alongside S3
//! Select).

use std::collections::HashMap;

/// One decoded event-stream message: its headers (name → string value) and
/// raw payload bytes.
#[derive(Debug)]
pub(crate) struct Message {
    pub(crate) headers: HashMap<String, String>,
    pub(crate) payload: Vec<u8>,
}

/// An event-stream frame failed to decode: a length that overruns the
/// buffer, a truncated header, or a CRC mismatch.
#[derive(Debug)]
pub(crate) struct FrameError(pub(crate) String);

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "event-stream frame error: {}", self.0)
    }
}

/// Try to decode one complete frame from the front of `buf`.
///
/// Returns `Ok(None)` when `buf` does not yet hold a whole frame (the caller
/// should wait for more bytes). Returns `Ok(Some((message, consumed)))` on
/// success, where `consumed` is the number of bytes to drain from the front
/// of `buf`. Returns `Err` on a malformed frame (bad length, CRC mismatch) —
/// the connection should be treated as broken at that point.
pub(crate) fn decode_frame(buf: &[u8]) -> Result<Option<(Message, usize)>, FrameError> {
    if buf.len() < 12 {
        return Ok(None);
    }
    let total_length = u32::from_be_bytes(buf[0..4].try_into().unwrap()) as usize;
    let headers_length = u32::from_be_bytes(buf[4..8].try_into().unwrap()) as usize;
    let prelude_crc = u32::from_be_bytes(buf[8..12].try_into().unwrap());

    if crc32(&buf[0..8]) != prelude_crc {
        return Err(FrameError("prelude CRC mismatch".to_string()));
    }
    if total_length < 16 || headers_length > total_length {
        return Err(FrameError(format!(
            "invalid lengths: total={total_length} headers={headers_length}"
        )));
    }
    if buf.len() < total_length {
        return Ok(None);
    }

    let message_crc = u32::from_be_bytes(buf[total_length - 4..total_length].try_into().unwrap());
    if crc32(&buf[0..total_length - 4]) != message_crc {
        return Err(FrameError("message CRC mismatch".to_string()));
    }

    let headers_start = 12;
    let headers_end = headers_start + headers_length;
    let payload_end = total_length - 4;
    if headers_end > payload_end {
        return Err(FrameError("headers overrun payload".to_string()));
    }

    let headers = decode_headers(&buf[headers_start..headers_end])?;
    let payload = buf[headers_end..payload_end].to_vec();

    Ok(Some((Message { headers, payload }, total_length)))
}

/// Decode the header block: a sequence of `name_len(1) name value_type(1)
/// value...` entries. Only the string value type (`7`) is handled — the only
/// type Bedrock's `:message-type`/`:event-type`/`:content-type`/`:exception-type`
/// headers use; any other type is skipped by its declared length where
/// possible, or treated as a decode error if the length cannot be determined.
fn decode_headers(mut buf: &[u8]) -> Result<HashMap<String, String>, FrameError> {
    let mut headers = HashMap::new();
    while !buf.is_empty() {
        if buf.len() < 2 {
            return Err(FrameError("truncated header name".to_string()));
        }
        let name_len = buf[0] as usize;
        buf = &buf[1..];
        if buf.len() < name_len + 1 {
            return Err(FrameError("truncated header name/type".to_string()));
        }
        let name = String::from_utf8_lossy(&buf[..name_len]).to_string();
        buf = &buf[name_len..];
        let value_type = buf[0];
        buf = &buf[1..];

        match value_type {
            7 => {
                // string: 2-byte length + UTF-8 bytes
                if buf.len() < 2 {
                    return Err(FrameError("truncated string header length".to_string()));
                }
                let value_len = u16::from_be_bytes(buf[0..2].try_into().unwrap()) as usize;
                buf = &buf[2..];
                if buf.len() < value_len {
                    return Err(FrameError("truncated string header value".to_string()));
                }
                let value = String::from_utf8_lossy(&buf[..value_len]).to_string();
                buf = &buf[value_len..];
                headers.insert(name, value);
            }
            0 | 1 => {
                // bool_true / bool_false: no value bytes.
            }
            2 => buf = &buf[1..],  // byte
            3 => buf = &buf[2..],  // short
            4 => buf = &buf[4..],  // integer
            5 => buf = &buf[8..],  // long
            8 => buf = &buf[8..],  // timestamp (int64 ms)
            9 => buf = &buf[16..], // uuid
            6 => {
                // byte_array: 2-byte length + raw bytes
                if buf.len() < 2 {
                    return Err(FrameError("truncated byte_array header length".to_string()));
                }
                let value_len = u16::from_be_bytes(buf[0..2].try_into().unwrap()) as usize;
                buf = &buf[2..];
                if buf.len() < value_len {
                    return Err(FrameError("truncated byte_array header value".to_string()));
                }
                buf = &buf[value_len..];
            }
            other => return Err(FrameError(format!("unknown header value type {other}"))),
        }
    }
    Ok(headers)
}

/// The standard CRC-32 (IEEE 802.3 / zlib / gzip) checksum: bit-by-bit
/// reflected algorithm, polynomial `0xEDB88320`, initial value and final XOR
/// both `0xFFFFFFFF`. Hand-rolled (no `crc32fast`/`crc` crate pinned in the
/// workspace) since this crate's only need is validating two small CRCs per
/// frame — see the `crc32_matches_known_test_vector` test for the standard
/// "123456789" → `0xCBF43926` vector this implementation is checked against.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn crc32_matches_known_test_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    /// Build a well-formed frame carrying `headers` (name, string-value
    /// pairs) and `payload`, matching exactly what a real Bedrock stream
    /// sends — used by every other test in this module and by
    /// `crate::streaming`'s tests.
    pub(crate) fn build_frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        let mut header_bytes = Vec::new();
        for (name, value) in headers {
            header_bytes.push(name.len() as u8);
            header_bytes.extend_from_slice(name.as_bytes());
            header_bytes.push(7); // string type
            header_bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
            header_bytes.extend_from_slice(value.as_bytes());
        }

        let total_length = 12 + header_bytes.len() + payload.len() + 4;
        let mut frame = Vec::with_capacity(total_length);
        frame.extend_from_slice(&(total_length as u32).to_be_bytes());
        frame.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
        let prelude_crc = crc32(&frame[0..8]);
        frame.extend_from_slice(&prelude_crc.to_be_bytes());
        frame.extend_from_slice(&header_bytes);
        frame.extend_from_slice(payload);
        let message_crc = crc32(&frame);
        frame.extend_from_slice(&message_crc.to_be_bytes());
        frame
    }

    #[test]
    fn decodes_a_well_formed_frame() {
        let frame = build_frame(
            &[(":event-type", "chunk"), (":message-type", "event")],
            br#"{"bytes":"eyJoaSI6MX0="}"#,
        );
        let (msg, consumed) = decode_frame(&frame).unwrap().unwrap();
        assert_eq!(consumed, frame.len());
        assert_eq!(msg.headers.get(":event-type").unwrap(), "chunk");
        assert_eq!(msg.headers.get(":message-type").unwrap(), "event");
        assert_eq!(msg.payload, br#"{"bytes":"eyJoaSI6MX0="}"#);
    }

    #[test]
    fn incomplete_buffer_returns_none() {
        let frame = build_frame(&[(":event-type", "chunk")], b"{}");
        assert!(decode_frame(&frame[..frame.len() - 3]).unwrap().is_none());
        assert!(decode_frame(&frame[..5]).unwrap().is_none());
    }

    #[test]
    fn corrupted_prelude_crc_is_an_error() {
        let mut frame = build_frame(&[(":event-type", "chunk")], b"{}");
        frame[9] ^= 0xFF; // flip a bit inside the prelude CRC
        assert!(decode_frame(&frame).is_err());
    }

    #[test]
    fn corrupted_payload_fails_message_crc() {
        let mut frame = build_frame(&[(":event-type", "chunk")], br#"{"bytes":"AA=="}"#);
        let last_payload_byte = frame.len() - 4 - 1;
        frame[last_payload_byte] ^= 0xFF;
        assert!(decode_frame(&frame).is_err());
    }

    #[test]
    fn two_frames_back_to_back_decode_independently() {
        let f1 = build_frame(&[("a", "1")], b"one");
        let f2 = build_frame(&[("b", "2")], b"two");
        let mut buf = f1.clone();
        buf.extend_from_slice(&f2);

        let (m1, c1) = decode_frame(&buf).unwrap().unwrap();
        assert_eq!(c1, f1.len());
        assert_eq!(m1.payload, b"one");

        let (m2, c2) = decode_frame(&buf[c1..]).unwrap().unwrap();
        assert_eq!(c2, f2.len());
        assert_eq!(m2.payload, b"two");
    }
}
