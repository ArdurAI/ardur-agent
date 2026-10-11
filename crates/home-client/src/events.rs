//! Bounded, resumable event windows over the existing signed device transport.
use crate::{Error, HomeClient, PinnedTransport, redact, safe_output};
use home_protocol::{DeviceRunDetail, RunStatus, string_len};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::sleep;

const MAX_FRAME_BYTES: usize = 64 * 1024;
const MAX_BYTES: usize = 1024 * 1024;
const MAX_FRAMES: usize = 128;
const MAX_CURSOR: i64 = i32::MAX as i64;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EventWindow {
    pub next_cursor: i64,
    pub reason: String,
}
impl EventWindow {
    pub fn message(&self) -> &'static str {
        match self.reason.as_str() {
            "access_lost" => {
                "Access to this run was lost. Following stopped; check permissions at home."
            }
            "payload_too_large" => {
                "An event is too large. Following stopped; inspect the run at home."
            }
            "error" => "Home could not finish the event window. Following stopped.",
            "shutdown" => "Home shut down the event stream. Following stopped.",
            "completed" => "The run ended. Following stopped.",
            "interrupted" => "Following stopped.",
            _ => "Event window ended.",
        }
    }
    pub fn exit_code(&self) -> i32 {
        match self.reason.as_str() {
            "access_lost" => 4,
            "payload_too_large" | "error" | "shutdown" => 2,
            _ => 0,
        }
    }
}

#[derive(Debug)]
pub enum EventRecord {
    Event { cursor: i64, data: Value },
    Window(EventWindow),
}
impl EventRecord {
    pub fn cursor(&self) -> i64 {
        match self {
            Self::Event { cursor, .. } => *cursor,
            Self::Window(window) => window.next_cursor,
        }
    }
    /// Redact decoded strings before serialization, so JSON escapes cannot hide secrets.
    pub fn json(&self) -> Value {
        redact(match self {
            Self::Event { cursor, data } => {
                json!({"version":1,"command":"runs events","event":"event","cursor":cursor,"data":data})
            }
            Self::Window(window) => {
                json!({"version":1,"command":"runs events","event":"window","data":window,"message":window.message()})
            }
        })
    }
    pub fn human(&self) -> String {
        let value = self.json();
        safe_output(&match self {
            Self::Event { cursor, .. } => format!(
                "{cursor}\t{}\t{}",
                value["data"]["type"].as_str().unwrap_or("event"),
                value["data"]["payload"]
            ),
            Self::Window(_) => value["message"].as_str().unwrap_or("").to_owned(),
        })
    }
}

/// Byte-oriented framing keeps incomplete UTF-8 private until a complete frame arrives.
/// Only one frame is retained; heartbeats count toward both window limits.
struct Reader {
    frame: Vec<u8>,
    bytes: usize,
    frames: usize,
    cursor: i64,
    window: Option<EventWindow>,
    line_start: usize,
}
impl Reader {
    fn new(cursor: i64) -> Self {
        Self {
            frame: Vec::new(),
            bytes: 0,
            frames: 0,
            cursor,
            window: None,
            line_start: 0,
        }
    }
    fn byte(&mut self, byte: u8) -> Result<Option<EventRecord>, Error> {
        if self.window.is_some() {
            return Err(Error::Protocol);
        }
        self.bytes += 1;
        if self.bytes > MAX_BYTES || self.frame.len() >= MAX_FRAME_BYTES {
            return Err(Error::PayloadTooLarge);
        }
        self.frame.push(byte);
        if byte != b'\n' {
            return Ok(None);
        }
        let line = &self.frame[self.line_start..self.frame.len() - 1];
        let blank = line.is_empty() || line == b"\r";
        self.line_start = self.frame.len();
        if !blank {
            return Ok(None);
        }
        self.frames += 1;
        if self.frames > MAX_FRAMES {
            return Err(Error::Protocol);
        }
        let frame = std::mem::take(&mut self.frame);
        self.line_start = 0;
        self.parse(&frame)
    }
    fn parse(&mut self, frame: &[u8]) -> Result<Option<EventRecord>, Error> {
        let text = std::str::from_utf8(frame).map_err(|_| Error::Protocol)?;
        let (mut kind, mut id, mut data) = (None, None, String::new());
        for line in text.lines() {
            if line.starts_with(':') || line.is_empty() {
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => kind = Some(value),
                "id" => id = Some(value),
                "data" => {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(value);
                }
                _ => {}
            }
        }
        if kind.is_none() && data.is_empty() && id.is_none() {
            return Ok(None);
        }
        let value: Value = serde_json::from_str(&data).map_err(|_| Error::Protocol)?;
        match kind {
            Some("event") => {
                let raw = id.ok_or(Error::Protocol)?;
                if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(Error::Protocol);
                }
                let seq: i64 = raw.parse().map_err(|_| Error::Protocol)?;
                if seq > MAX_CURSOR || value["seq"].as_i64() != Some(seq) || !value.is_object() {
                    return Err(Error::Protocol);
                }
                if seq <= self.cursor {
                    return Ok(None);
                }
                // The caller validates correlation before publishing the new cursor.
                Ok(Some(EventRecord::Event {
                    cursor: seq,
                    data: value,
                }))
            }
            Some("window") => {
                let window: EventWindow =
                    serde_json::from_value(value).map_err(|_| Error::Protocol)?;
                if id.is_some()
                    || !(self.cursor..=MAX_CURSOR).contains(&window.next_cursor)
                    || !matches!(
                        window.reason.as_str(),
                        "timeout"
                            | "limit"
                            | "access_lost"
                            | "payload_too_large"
                            | "error"
                            | "shutdown"
                    )
                {
                    return Err(Error::Protocol);
                }
                self.cursor = window.next_cursor;
                self.window = Some(window.clone());
                Ok(Some(EventRecord::Window(window)))
            }
            _ => Err(Error::Protocol),
        }
    }
}

fn valid_id(id: &str) -> bool {
    (1..=128).contains(&string_len(id))
}
async fn run_detail(
    client: &HomeClient,
    run_id: &str,
) -> Result<(DeviceRunDetail, Option<String>), Error> {
    let value = client.request("runs/get", &json!({"runId":run_id})).await?;
    let run: DeviceRunDetail =
        serde_json::from_value(value["run"].clone()).map_err(|_| Error::Protocol)?;
    if run.run_id != run_id || !valid_id(&run.thread_id) || !valid_id(&run.bot_id) {
        return Err(Error::Protocol);
    }
    let group = match &value["run"]["groupId"] {
        Value::Null => None,
        Value::String(id) if valid_id(id) => Some(id.clone()),
        _ => return Err(Error::Protocol),
    };
    Ok((run, group))
}
fn ended(run: &DeviceRunDetail) -> bool {
    matches!(
        run.status,
        RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
    )
}

/// Stream one window, or follow with fresh signed requests. Dropping this future
/// cancels reads and backoff, which lets the CLI stop cleanly on Ctrl-C.
pub async fn execute_run_events(
    client: &HomeClient,
    run_id: &str,
    mut cursor: i64,
    follow: bool,
    mut emit: impl FnMut(EventRecord) -> Result<(), Error>,
) -> Result<EventWindow, Error> {
    if !valid_id(run_id) || !(-1..=MAX_CURSOR).contains(&cursor) {
        return Err(Error::Input);
    }
    let (run, group) = run_detail(client, run_id).await?;
    let mut retries = 0u32;
    loop {
        let mut body = json!({"runId":run_id,"threadId":run.thread_id,"cursor":cursor});
        if let Some(group) = &group {
            body["groupId"] = json!(group);
        } else {
            body["botId"] = json!(run.bot_id);
        }
        let mut reader = Reader::new(cursor);
        let result = async {
            // signed_envelope obtains a fresh pinned home proof and nonce every time.
            let (url, envelope) = client.signed_envelope("events", &body).await?;
            let transport =
                PinnedTransport::new(&client.home.profile.pins.certificate_fingerprint)?;
            let mut response = transport.post_events(&url, &envelope).await?;
            while let Some(chunk) = response.chunk().await.map_err(|_| Error::Unreachable)? {
                for byte in chunk {
                    if let Some(record) = reader.byte(byte)? {
                        if let EventRecord::Event { cursor: seq, data } = &record {
                            if data["runId"] != run_id
                                || data["threadId"] != run.thread_id
                                || data["botId"] != run.bot_id
                            {
                                return Err(Error::Protocol);
                            }
                            let seq = *seq;
                            emit(record)?;
                            reader.cursor = seq;
                        } else {
                            emit(record)?;
                        }
                    }
                }
                if reader.window.is_some() {
                    return Ok(());
                }
            }
            // Even a clean EOF without a final frame is an interrupted window.
            Err(Error::Unreachable)
        }
        .await;
        cursor = reader.cursor;
        match result {
            Ok(()) => {
                let window = reader
                    .window
                    .as_ref()
                    .expect("successful window has final frame");
                if !follow || !matches!(window.reason.as_str(), "timeout" | "limit") {
                    return Ok(window.clone());
                }
                retries = 0;
            }
            Err(Error::Unreachable) if follow && retries < 5 => {
                retries += 1;
            }
            Err(error) => return Err(error),
        }
        // A limit window may still have unread history even if the run ended.
        // Interrupted windows must replay the unfinished frame before testing completion.
        if reader
            .window
            .as_ref()
            .is_some_and(|w| w.reason == "timeout")
        {
            let (current, current_group) = run_detail(client, run_id).await?;
            if current.thread_id != run.thread_id
                || current.bot_id != run.bot_id
                || current_group != group
            {
                return Err(Error::Protocol);
            }
            if ended(&current) {
                let window = EventWindow {
                    next_cursor: cursor,
                    reason: "completed".into(),
                };
                emit(EventRecord::Window(window.clone()))?;
                return Ok(window);
            }
        }
        // Brief pauses also bound fast empty windows; transport retries back off to 2 s.
        sleep(Duration::from_millis((100u64 << retries).min(2000))).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Fixture {
        event_window: Value,
        event_streams: Vec<Value>,
    }
    fn fixture() -> Fixture {
        // Rejected surrogate bodies elsewhere in the fixture stay outside Value.
        serde_json::from_str(include_str!(
            "../../home-protocol/tests/fixtures/device-events.json"
        ))
        .unwrap()
    }
    #[test]
    fn all_home_sse_vectors_match_including_split_utf8_and_partial_frames() {
        let f = fixture();
        assert_eq!(f.event_streams.len(), 9);
        assert_eq!(f.event_window["maxFrameBytes"], MAX_FRAME_BYTES);
        assert_eq!(f.event_window["maxBytes"], MAX_BYTES);
        assert_eq!(f.event_window["maxFrames"], MAX_FRAMES);
        for case in &f.event_streams {
            let mut reader = Reader::new(case["startCursor"].as_i64().unwrap());
            let mut events = Vec::new();
            for chunk in case["utf8HexChunks"].as_array().unwrap() {
                let hex = chunk.as_str().unwrap();
                for offset in (0..hex.len()).step_by(2) {
                    let byte = u8::from_str_radix(&hex[offset..offset + 2], 16).unwrap();
                    if let Some(EventRecord::Event { cursor, data }) = reader.byte(byte).unwrap() {
                        reader.cursor = cursor;
                        events.push(data);
                    }
                }
            }
            assert_eq!(json!(events), case["expectedEvents"], "{}", case["name"]);
            assert_eq!(
                json!(reader.window),
                case["expectedWindow"],
                "{}",
                case["name"]
            );
            assert_eq!(reader.cursor, case["expectedCursor"], "{}", case["name"]);
        }
    }
    #[test]
    fn frame_payload_and_frame_count_limits_fail_closed() {
        let mut reader = Reader::new(-1);
        for _ in 0..MAX_FRAME_BYTES {
            reader.byte(b'a').unwrap();
        }
        assert_eq!(reader.byte(b'a').unwrap_err(), Error::PayloadTooLarge);
        assert_eq!(reader.cursor, -1);
        let mut reader = Reader::new(-1);
        let heartbeat = format!(":{}\n\n", "a".repeat(MAX_FRAME_BYTES - 3));
        for _ in 0..16 {
            for byte in heartbeat.bytes() {
                reader.byte(byte).unwrap();
            }
        }
        assert_eq!(reader.byte(b':').unwrap_err(), Error::PayloadTooLarge);
        let mut reader = Reader::new(-1);
        for _ in 0..MAX_FRAMES {
            for byte in b": heartbeat\n\n" {
                reader.byte(*byte).unwrap();
            }
        }
        let result = b": heartbeat\n\n"
            .iter()
            .try_for_each(|b| reader.byte(*b).map(|_| ()));
        assert_eq!(result, Err(Error::Protocol));
    }
    #[test]
    fn malformed_frames_and_cursor_regressions_are_refused() {
        for frame in [
            "id: 2\nevent: event\ndata: {\"seq\":3}\n\n",
            "id: 2147483648\nevent: event\ndata: {\"seq\":2147483648}\n\n",
            "event: window\ndata: {\"nextCursor\":0,\"reason\":\"timeout\"}\n\n",
            "event: window\ndata: {\"nextCursor\":2,\"reason\":\"private diagnostic\"}\n\n",
            "id: 3\nevent: window\ndata: {\"nextCursor\":3,\"reason\":\"timeout\"}\n\n",
        ] {
            let mut reader = Reader::new(1);
            assert!(
                frame
                    .bytes()
                    .try_for_each(|b| reader.byte(b).map(|_| ()))
                    .is_err()
            );
            assert_eq!(reader.cursor, 1);
        }
        let mut reader = Reader::new(-1);
        assert_eq!(
            b": \xff\n\n"
                .iter()
                .try_for_each(|b| reader.byte(*b).map(|_| ())),
            Err(Error::Protocol)
        );
        let mut reader = Reader::new(-1);
        for byte in b": heartbeat\r\n\r\n" {
            reader.byte(*byte).unwrap();
        }
        assert_eq!(reader.frames, 1);
    }
    #[test]
    fn event_output_redacts_decoded_credentials_and_terminal_controls() {
        let record = EventRecord::Event {
            cursor: 1,
            data: json!({"type":"progress\u{1b}[2J", "payload":{"text":"token=secret-value\u{1b}]0;bad\u{7}","apiKey":"secret-value"}}),
        };
        for text in [record.json().to_string(), record.human()] {
            assert!(!text.contains("secret-value"));
            assert!(!text.contains('\u{1b}'));
            assert!(!text.contains('\u{7}'));
        }
    }
}
