use std::collections::HashMap;

use rand::Rng;

pub type Result<T> = std::result::Result<T, MessageError>;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum MessageError {
    /// Malformed wire-format data (bad length prefix, invalid JSON, etc.)
    Parse(String),
    /// JSON structure doesn't match expected message layout
    Schema(String),
}

impl std::fmt::Display for MessageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MessageError::Parse(msg) => write!(f, "lounge parse: {msg}"),
            MessageError::Schema(msg) => write!(f, "lounge schema: {msg}"),
        }
    }
}

impl std::error::Error for MessageError {}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A single message received from the YouTube Lounge long-poll.
///
/// Wire format: `[index, ["command", { "key": "val", ... }]]`
/// The args map may be absent, in which case `args` is empty.
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub index: i32,
    pub command: String,
    pub args: HashMap<String, String>,
}

/// A message to send to the YouTube Lounge endpoint.
///
/// `args` is ordered because the form-encoded body preserves insertion order.
#[derive(Debug, Clone)]
pub struct OutgoingMessage {
    pub command: String,
    pub args: Vec<(String, String)>,
}

impl OutgoingMessage {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
        }
    }

    pub fn with_arg(mut self, key: impl Into<String>, val: impl Into<String>) -> Self {
        self.args.push((key.into(), val.into()));
        self
    }
}

/// Player state codes used by the YouTube Lounge protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i8)]
pub enum PlayerState {
    Idle = -1,
    Stopped = 0,
    Playing = 1,
    Paused = 2,
    Loading = 3,
}

impl PlayerState {
    pub fn as_str(self) -> &'static str {
        match self {
            PlayerState::Idle => "-1",
            PlayerState::Stopped => "0",
            PlayerState::Playing => "1",
            PlayerState::Paused => "2",
            PlayerState::Loading => "3",
        }
    }
}

/// Generate a stable content playback nonce (CPN) — 16 hex chars.
/// This should be generated once per session and reused across all state reports.
pub fn generate_cpn() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..16)
        .map(|_| format!("{:x}", rng.gen_range(0u8..16)))
        .collect()
}

/// Format a time/duration value for the Lounge API.
///
/// Matches JavaScript's `Number.toString()` behavior:
/// - Whole numbers: `"191"` not `"191.000"`
/// - Fractional: `"42.567"` not `"42.567000"`
pub fn fmt_time(val: f64) -> String {
    if val.fract() == 0.0 {
        format!("{}", val as i64)
    } else {
        format!("{}", val)
    }
}

impl std::fmt::Display for PlayerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Wire-format parsing: length-prefixed JSON chunks
// ---------------------------------------------------------------------------

/// Read all length-prefixed JSON chunks from a response body and return the
/// flattened list of incoming messages.
///
/// The wire format is:
/// ```text
/// <decimal_length>\n
/// <JSON array of that many bytes>
/// <decimal_length>\n
/// <JSON array of that many bytes>
/// ...
/// ```
///
/// Each JSON chunk is an array of message tuples:
/// ```json
/// [[0, ["c", "SESSION_ID"]], [1, ["S", "GSESSION_ID"]]]
/// ```
pub fn parse_chunks(data: &[u8]) -> Result<Vec<IncomingMessage>> {
    let mut messages = Vec::new();
    let mut pos = 0;
    let len = data.len();

    while pos < len {
        // Skip leading whitespace / newlines between chunks
        while pos < len && (data[pos] == b'\n' || data[pos] == b'\r' || data[pos] == b' ') {
            pos += 1;
        }
        if pos >= len {
            break;
        }

        // Read decimal length until newline
        let line_end = memchr_newline(data, pos).ok_or_else(|| {
            MessageError::Parse(format!(
                "expected length line at offset {pos}, got: {:?}",
                String::from_utf8_lossy(&data[pos..std::cmp::min(pos + 40, len)])
            ))
        })?;

        let length_str = std::str::from_utf8(&data[pos..line_end])
            .map_err(|e| MessageError::Parse(format!("invalid UTF-8 in length line: {e}")))?
            .trim();

        let chunk_len: usize = length_str
            .parse()
            .map_err(|e| MessageError::Parse(format!("bad length value \"{length_str}\": {e}")))?;

        // Advance past the newline
        let chunk_start = line_end + 1;
        let chunk_end = chunk_start + chunk_len;

        if chunk_end > len {
            return Err(MessageError::Parse(format!(
                "chunk claims {chunk_len} bytes at offset {chunk_start}, but only {} remain",
                len - chunk_start
            )));
        }

        let chunk = &data[chunk_start..chunk_end];
        let mut chunk_msgs = parse_messages(chunk)?;
        messages.append(&mut chunk_msgs);

        pos = chunk_end;
    }

    Ok(messages)
}

/// Parse a single JSON chunk (one array of message tuples) into messages.
///
/// Expected JSON: `[[index, ["command", args?]], ...]`
pub fn parse_messages(data: &[u8]) -> Result<Vec<IncomingMessage>> {
    let json_str = std::str::from_utf8(data)
        .map_err(|e| MessageError::Parse(format!("invalid UTF-8 in chunk: {e}")))?;

    let value: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| MessageError::Parse(format!("invalid JSON: {e}")))?;

    let outer = value
        .as_array()
        .ok_or_else(|| MessageError::Schema("top-level JSON is not an array".into()))?;

    let mut messages = Vec::with_capacity(outer.len());

    for entry in outer {
        let tuple = entry
            .as_array()
            .ok_or_else(|| MessageError::Schema(format!("message entry is not an array: {entry}")))?;

        if tuple.len() < 2 {
            return Err(MessageError::Schema(format!(
                "message tuple has {} elements, expected >= 2",
                tuple.len()
            )));
        }

        let index = tuple[0]
            .as_i64()
            .ok_or_else(|| MessageError::Schema(format!("message index is not a number: {}", tuple[0])))?
            as i32;

        let inner = tuple[1]
            .as_array()
            .ok_or_else(|| MessageError::Schema(format!("message payload is not an array: {}", tuple[1])))?;

        if inner.is_empty() {
            return Err(MessageError::Schema("message payload array is empty".into()));
        }

        let command = inner[0]
            .as_str()
            .ok_or_else(|| MessageError::Schema(format!("command is not a string: {}", inner[0])))?
            .to_owned();

        // Args can be a JSON object (most commands), a plain string value
        // (e.g., "c" and "S" commands), or absent entirely.
        let args = if inner.len() > 1 {
            match &inner[1] {
                serde_json::Value::Object(map) => {
                    map.iter()
                        .map(|(k, v)| {
                            let val = match v {
                                serde_json::Value::String(s) => s.clone(),
                                other => other.to_string(),
                            };
                            (k.clone(), val)
                        })
                        .collect()
                }
                serde_json::Value::String(s) => {
                    // Commands like ["c", "SESSION_ID"] — store the value under ""
                    let mut m = HashMap::new();
                    m.insert(String::new(), s.clone());
                    m
                }
                other => {
                    // Numeric or other scalar — store stringified under ""
                    let mut m = HashMap::new();
                    m.insert(String::new(), other.to_string());
                    m
                }
            }
        } else {
            HashMap::new()
        };

        messages.push(IncomingMessage {
            index,
            command,
            args,
        });
    }

    Ok(messages)
}

/// Find the next `\n` starting at `pos`.
fn memchr_newline(data: &[u8], pos: usize) -> Option<usize> {
    data[pos..].iter().position(|&b| b == b'\n').map(|i| pos + i)
}

// ---------------------------------------------------------------------------
// Incremental streaming parser
// ---------------------------------------------------------------------------

/// Incremental parser for streaming HTTP response bodies.
///
/// YouTube's long-poll sends length-prefixed JSON chunks over a streaming
/// HTTP connection. An HTTP chunk boundary can split a message at any point.
/// `ChunkParser` buffers partial data and yields complete messages as they
/// become available.
pub struct ChunkParser {
    buf: Vec<u8>,
}

impl ChunkParser {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(4096),
        }
    }

    /// Feed raw bytes from the HTTP stream. Returns any complete messages
    /// that can be parsed from the accumulated buffer.
    pub fn feed(&mut self, data: &[u8]) -> Vec<IncomingMessage> {
        self.buf.extend_from_slice(data);
        let mut messages = Vec::new();

        loop {
            // Skip leading whitespace/newlines
            let start = self
                .buf
                .iter()
                .position(|b| !matches!(b, b'\n' | b'\r' | b' '));
            match start {
                Some(s) if s > 0 => {
                    self.buf.drain(..s);
                }
                None => {
                    self.buf.clear();
                    break;
                }
                _ => {}
            }

            if self.buf.is_empty() {
                break;
            }

            // Find newline that terminates the length prefix
            let newline_pos = match self.buf.iter().position(|&b| b == b'\n') {
                Some(p) => p,
                None => break, // incomplete length line, wait for more data
            };

            // Parse the decimal length
            let length_str = match std::str::from_utf8(&self.buf[..newline_pos]) {
                Ok(s) => s.trim(),
                Err(_) => {
                    self.buf.drain(..=newline_pos);
                    continue;
                }
            };

            let chunk_len: usize = match length_str.parse() {
                Ok(n) => n,
                Err(_) => {
                    self.buf.drain(..=newline_pos);
                    continue;
                }
            };

            let chunk_start = newline_pos + 1;
            let chunk_end = chunk_start + chunk_len;

            if chunk_end > self.buf.len() {
                break; // not enough data for this chunk yet
            }

            // Parse the JSON message array
            match parse_messages(&self.buf[chunk_start..chunk_end]) {
                Ok(mut msgs) => messages.append(&mut msgs),
                Err(_) => {} // skip malformed chunks
            }

            self.buf.drain(..chunk_end);
        }

        messages
    }
}

// ---------------------------------------------------------------------------
// Outgoing message encoding
// ---------------------------------------------------------------------------

/// Serialize outgoing messages into a form-urlencoded POST body.
///
/// Format:
/// ```text
/// count=N&ofs=O&req0__sc=command&req0_key1=val1&req1__sc=command2&...
/// ```
///
/// `offset` is the cumulative number of messages already sent (the `ofs`
/// parameter used by the Lounge protocol to track sequencing).
pub fn encode_outgoing(messages: &[OutgoingMessage], offset: u32) -> String {
    let mut parts: Vec<String> = Vec::new();

    parts.push(format!("count={}", messages.len()));
    parts.push(format!("ofs={offset}"));

    for (i, msg) in messages.iter().enumerate() {
        parts.push(format!(
            "req{i}__sc={}",
            percent_encode(&msg.command)
        ));
        for (key, val) in &msg.args {
            parts.push(format!(
                "req{i}_{key}={}",
                percent_encode(val)
            ));
        }
    }

    parts.join("&")
}

/// Manually percent-encode a value for use in form-urlencoded bodies.
///
/// We do this inline rather than pulling in yet another crate — the Lounge
/// protocol only sends ASCII command names and simple values.
pub fn percent_encode_public(input: &str) -> String {
    percent_encode(input)
}

fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(hex_digit(byte >> 4));
                out.push(hex_digit(byte & 0x0F));
            }
        }
    }
    out
}

fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'A' + nibble - 10) as char,
        _ => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Generate a random 12-character lowercase string, used as the `zx`
/// cache-buster query parameter on Lounge API requests.
pub fn zx() -> String {
    let mut rng = rand::thread_rng();
    (0..12)
        .map(|_| (b'a' + rng.gen_range(0..26)) as char)
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_chunk() {
        // Simulates a wire-format response with one chunk
        let json = r#"[[0,["c","SID_VALUE"]],[1,["S","GSID_VALUE"]]]"#;
        let wire = format!("{}\n{}", json.len(), json);

        let msgs = parse_chunks(wire.as_bytes()).expect("parse should succeed");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].index, 0);
        assert_eq!(msgs[0].command, "c");
        assert_eq!(msgs[0].args.get(""), Some(&"SID_VALUE".to_string()));
        assert_eq!(msgs[1].index, 1);
        assert_eq!(msgs[1].command, "S");
        assert_eq!(msgs[1].args.get(""), Some(&"GSID_VALUE".to_string()));
    }

    #[test]
    fn test_parse_object_args() {
        let json = r#"[[2,["remoteConnected",{"name":"My Phone","device":"REMOTE_CONTROL"}]]]"#;
        let wire = format!("{}\n{}", json.len(), json);

        let msgs = parse_chunks(wire.as_bytes()).expect("parse should succeed");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].command, "remoteConnected");
        assert_eq!(msgs[0].args.get("name"), Some(&"My Phone".to_string()));
        assert_eq!(
            msgs[0].args.get("device"),
            Some(&"REMOTE_CONTROL".to_string())
        );
    }

    #[test]
    fn test_parse_multiple_chunks() {
        let chunk1 = r#"[[0,["noop"]]]"#;
        let chunk2 = r#"[[1,["c","ABC"]]]"#;
        let wire = format!(
            "{}\n{}{}\n{}",
            chunk1.len(),
            chunk1,
            chunk2.len(),
            chunk2
        );

        let msgs = parse_chunks(wire.as_bytes()).expect("parse should succeed");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].command, "noop");
        assert_eq!(msgs[1].command, "c");
    }

    #[test]
    fn test_encode_outgoing_empty() {
        let body = encode_outgoing(&[], 0);
        assert_eq!(body, "count=0&ofs=0");
    }

    #[test]
    fn test_encode_outgoing_single() {
        let msg = OutgoingMessage::new("setPlayback")
            .with_arg("state", "1")
            .with_arg("currentTime", "42.5");

        let body = encode_outgoing(&[msg], 3);
        assert!(body.starts_with("count=1&ofs=3"));
        assert!(body.contains("req0__sc=setPlayback"));
        assert!(body.contains("req0_state=1"));
        assert!(body.contains("req0_currentTime=42.5"));
    }

    #[test]
    fn test_zx_length_and_chars() {
        let val = zx();
        assert_eq!(val.len(), 12);
        assert!(val.chars().all(|c| c.is_ascii_lowercase()));
    }
}
