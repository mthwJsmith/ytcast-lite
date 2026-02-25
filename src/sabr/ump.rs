/// UMP (Universal Media Protocol) parser for YouTube's SABR streaming protocol.
///
/// UMP frames arrive as a chunked HTTP stream. Each frame (called a "part") is:
///   [varint: part_type] [varint: part_size] [raw bytes: part_data]
///
/// The varint encoding is YouTube's own variable-length integer format, NOT
/// standard protobuf varint.

// ---------------------------------------------------------------------------
// Part type constants
// ---------------------------------------------------------------------------

pub const MEDIA_HEADER: u32 = 20;
pub const MEDIA: u32 = 21;
pub const MEDIA_END: u32 = 22;
pub const NEXT_REQUEST_POLICY: u32 = 35;
pub const FORMAT_INITIALIZATION_METADATA: u32 = 42;
pub const SABR_REDIRECT: u32 = 43;
pub const SABR_ERROR: u32 = 44;
pub const SABR_CONTEXT_UPDATE: u32 = 57;
pub const STREAM_PROTECTION_STATUS: u32 = 58;
pub const SABR_CONTEXT_SENDING_POLICY: u32 = 59;

// ---------------------------------------------------------------------------
// Variable-length integer codec
// ---------------------------------------------------------------------------

/// Read a YouTube-style varint from `buf`.
///
/// Returns `Some((value, bytes_consumed))` or `None` if `buf` is too short.
///
/// Encoding:
///   1 byte:  first < 128      -> value = byte
///   2 bytes: first 128..192   -> value = (b0 & 0x3F) + 64 * b1
///   3 bytes: first 192..224   -> value = (b0 & 0x1F) + 32 * (b1 + 256 * b2)
///   4 bytes: first 224..240   -> value = (b0 & 0x0F) + 16 * (b1 + 256 * (b2 + 256 * b3))
///   5 bytes: first >= 240     -> value = u32::from_le_bytes(buf[1..5])
pub fn read_varint(buf: &[u8]) -> Option<(u32, usize)> {
    let first = *buf.first()?;

    if first < 128 {
        // 1 byte
        Some((first as u32, 1))
    } else if first < 192 {
        // 2 bytes
        let b1 = *buf.get(1)?;
        let value = (first as u32 & 0x3F) + 64 * b1 as u32;
        Some((value, 2))
    } else if first < 224 {
        // 3 bytes
        let b1 = *buf.get(1)?;
        let b2 = *buf.get(2)?;
        let value = (first as u32 & 0x1F) + 32 * (b1 as u32 + 256 * b2 as u32);
        Some((value, 3))
    } else if first < 240 {
        // 4 bytes
        let b1 = *buf.get(1)?;
        let b2 = *buf.get(2)?;
        let b3 = *buf.get(3)?;
        let value =
            (first as u32 & 0x0F) + 16 * (b1 as u32 + 256 * (b2 as u32 + 256 * b3 as u32));
        Some((value, 4))
    } else {
        // 5 bytes: first >= 240, next 4 bytes are a little-endian u32
        if buf.len() < 5 {
            return None;
        }
        let value = u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]);
        Some((value, 5))
    }
}

/// Encode a value as a YouTube-style varint and append it to `out`.
pub fn write_varint(out: &mut Vec<u8>, value: u32) {
    if value < 128 {
        // 1 byte
        out.push(value as u8);
    } else if value < 128 + 64 * 256 {
        // 2 bytes: value = (b0 & 0x3F) + 64 * b1
        // b0 high bits = 10xx_xxxx (128..192)
        let lo = value % 64;
        let hi = value / 64;
        out.push(128 | lo as u8);
        out.push(hi as u8);
    } else if value < 32 + 32 * 256 * 256 {
        // 3 bytes: value = (b0 & 0x1F) + 32 * (b1 + 256 * b2)
        // b0 high bits = 110x_xxxx (192..224)
        let lo = value % 32;
        let rest = value / 32;
        let b1 = rest % 256;
        let b2 = rest / 256;
        out.push(192 | lo as u8);
        out.push(b1 as u8);
        out.push(b2 as u8);
    } else if value < 16 + 16 * 256 * 256 * 256 {
        // 4 bytes: value = (b0 & 0x0F) + 16 * (b1 + 256 * (b2 + 256 * b3))
        // b0 high bits = 1110_xxxx (224..240)
        let lo = value % 16;
        let rest = value / 16;
        let b1 = rest % 256;
        let rest = rest / 256;
        let b2 = rest % 256;
        let b3 = rest / 256;
        out.push(224 | lo as u8);
        out.push(b1 as u8);
        out.push(b2 as u8);
        out.push(b3 as u8);
    } else {
        // 5 bytes: prefix byte >= 240, then raw LE u32
        out.push(240);
        out.extend_from_slice(&value.to_le_bytes());
    }
}

// ---------------------------------------------------------------------------
// UMP part
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct UmpPart {
    pub part_type: u32,
    pub data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Streaming parser
// ---------------------------------------------------------------------------

/// Accumulates chunked HTTP response data and yields complete UMP parts.
pub struct UmpParser {
    buffer: Vec<u8>,
}

impl UmpParser {
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Append raw bytes from the latest HTTP response chunk.
    pub fn push(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }

    /// Try to extract the next complete UMP part from the internal buffer.
    ///
    /// Returns `None` if there is not enough data yet -- call `push` with more
    /// bytes and try again.
    pub fn next_part(&mut self) -> Option<UmpPart> {
        let buf = &self.buffer;

        // 1. Read part_type varint.
        let (part_type, type_len) = read_varint(buf)?;

        // 2. Read part_size varint (starts right after the type varint).
        let (part_size, size_len) = read_varint(&buf[type_len..])?;

        let header_len = type_len + size_len;
        let total_len = header_len + part_size as usize;

        // 3. Check if the full part body is available.
        if buf.len() < total_len {
            return None;
        }

        // 4. Extract the part data.
        let data = buf[header_len..total_len].to_vec();

        // 5. Remove the consumed bytes from the front of the buffer.
        self.buffer.drain(..total_len);

        Some(UmpPart { part_type, data })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- varint round-trip ---------------------------------------------------

    fn roundtrip(value: u32) {
        let mut buf = Vec::new();
        write_varint(&mut buf, value);
        let (decoded, consumed) = read_varint(&buf).expect("should decode");
        assert_eq!(decoded, value, "value mismatch for {value}");
        assert_eq!(consumed, buf.len(), "consumed length mismatch for {value}");
    }

    #[test]
    fn varint_1_byte() {
        roundtrip(0);
        roundtrip(1);
        roundtrip(127);
    }

    #[test]
    fn varint_2_bytes() {
        roundtrip(128);
        roundtrip(1000);
        // Max 2-byte: (0x3F) + 64 * 255 = 63 + 16320 = 16383
        roundtrip(16383);
    }

    #[test]
    fn varint_3_bytes() {
        roundtrip(16384);
        roundtrip(100_000);
        // Max 3-byte: 31 + 32 * (255 + 256 * 255) = 31 + 32 * 65535 = 2097151
        roundtrip(2_097_151);
    }

    #[test]
    fn varint_4_bytes() {
        roundtrip(2_097_152);
        roundtrip(10_000_000);
        // Max 4-byte: 15 + 16 * (255 + 256 * (255 + 256 * 255))
        //           = 15 + 16 * 16_777_215 = 268_435_455
        roundtrip(268_435_455);
    }

    #[test]
    fn varint_5_bytes() {
        roundtrip(268_435_456);
        roundtrip(u32::MAX);
    }

    // -- read_varint edge cases ----------------------------------------------

    #[test]
    fn read_varint_empty() {
        assert!(read_varint(&[]).is_none());
    }

    #[test]
    fn read_varint_short_buffer() {
        // 2-byte varint but only 1 byte present
        assert!(read_varint(&[0x80]).is_none());
        // 5-byte varint but only 3 bytes present
        assert!(read_varint(&[0xF0, 0x01, 0x02]).is_none());
    }

    // -- parser --------------------------------------------------------------

    #[test]
    fn parser_single_part() {
        let mut out = Vec::new();
        write_varint(&mut out, MEDIA_HEADER); // part type = 20
        write_varint(&mut out, 4); // part size = 4
        out.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // body

        let mut parser = UmpParser::new();
        parser.push(&out);

        let part = parser.next_part().expect("should yield a part");
        assert_eq!(part.part_type, MEDIA_HEADER);
        assert_eq!(part.data, vec![0xDE, 0xAD, 0xBE, 0xEF]);

        assert!(parser.next_part().is_none());
    }

    #[test]
    fn parser_multiple_parts() {
        let mut out = Vec::new();
        for i in 0..3 {
            write_varint(&mut out, MEDIA);
            write_varint(&mut out, 2);
            out.extend_from_slice(&[i, i + 10]);
        }

        let mut parser = UmpParser::new();
        parser.push(&out);

        for i in 0..3u8 {
            let part = parser.next_part().expect("should yield a part");
            assert_eq!(part.part_type, MEDIA);
            assert_eq!(part.data, vec![i, i + 10]);
        }
        assert!(parser.next_part().is_none());
    }

    #[test]
    fn parser_chunked_delivery() {
        // Build a part with a 4-byte body.
        let mut out = Vec::new();
        write_varint(&mut out, SABR_ERROR);
        write_varint(&mut out, 4);
        out.extend_from_slice(&[1, 2, 3, 4]);

        let mut parser = UmpParser::new();

        // Feed one byte at a time -- the parser must wait until it has everything.
        for (i, &byte) in out.iter().enumerate() {
            assert!(
                parser.next_part().is_none(),
                "should not yield before all bytes are pushed (byte {i})"
            );
            parser.push(&[byte]);
        }

        let part = parser.next_part().expect("should yield after all bytes");
        assert_eq!(part.part_type, SABR_ERROR);
        assert_eq!(part.data, vec![1, 2, 3, 4]);
    }

    #[test]
    fn parser_empty_body() {
        let mut out = Vec::new();
        write_varint(&mut out, MEDIA_END);
        write_varint(&mut out, 0);

        let mut parser = UmpParser::new();
        parser.push(&out);

        let part = parser.next_part().expect("should yield a part");
        assert_eq!(part.part_type, MEDIA_END);
        assert!(part.data.is_empty());
    }
}
