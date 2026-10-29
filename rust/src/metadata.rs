//! Reader for shairport-sync's XML-ish metadata pipe.
//!
//! Each item looks like:
//!   <item><type>73736e63</type><code>636f706c</code><length>1234</length>
//!   <data encoding="base64">
//!   ...base64...</data></item>

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use std::io::{self, BufRead};

#[derive(Debug, PartialEq)]
pub struct Item {
    pub typ: String,
    pub code: String,
    pub data: Vec<u8>,
}

pub struct MetadataReader<R> {
    inner: R,
    line: Vec<u8>,
}

impl<R: BufRead> MetadataReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner, line: Vec::with_capacity(4096) }
    }

    fn read_line(&mut self) -> io::Result<bool> {
        self.line.clear();
        Ok(self.inner.read_until(b'\n', &mut self.line)? > 0)
    }

    /// Next complete item, or None on EOF (writer closed the pipe).
    pub fn next_item(&mut self) -> io::Result<Option<Item>> {
        loop {
            if !self.read_line()? {
                return Ok(None);
            }
            let Some((typ, code, length)) = parse_header(&self.line) else {
                continue;
            };

            let mut data = Vec::new();
            if length > 0 {
                if !self.read_line()? {
                    return Ok(None);
                }
                if !self.line.starts_with(b"<data") {
                    continue;
                }
                if !self.read_line()? {
                    return Ok(None);
                }
                data = decode_data(&self.line, length);
            }
            return Ok(Some(Item { typ, code, data }));
        }
    }
}

fn between<'a>(s: &'a [u8], open: &[u8], close: &[u8]) -> Option<&'a [u8]> {
    let start = find(s, open)? + open.len();
    let end = start + find(&s[start..], close)?;
    Some(&s[start..end])
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn hex_fourcc(hex: &[u8]) -> Option<String> {
    if hex.len() != 8 {
        return None;
    }
    let s = std::str::from_utf8(hex).ok()?;
    let bytes: Vec<u8> = (0..4)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16))
        .collect::<Result<_, _>>()
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn parse_header(line: &[u8]) -> Option<(String, String, usize)> {
    if !line.starts_with(b"<item>") {
        return None;
    }
    let typ = hex_fourcc(between(line, b"<type>", b"</type>")?)?;
    let code = hex_fourcc(between(line, b"<code>", b"</code>")?)?;
    let len = between(line, b"<length>", b"</length>")?;
    let length = if len.is_empty() {
        0
    } else {
        std::str::from_utf8(len).ok()?.parse().ok()?
    };
    Some((typ, code, length))
}

/// The base64 payload is followed by `</data></item>` on the same line;
/// take exactly the encoded length for `length` bytes, re-padding if short.
fn decode_data(line: &[u8], length: usize) -> Vec<u8> {
    let b64size = 4 * length.div_ceil(3);
    let mut b64 = line[..b64size.min(line.len())].to_vec();
    while b64.len() % 4 != 0 {
        b64.push(b'=');
    }
    STANDARD.decode(&b64).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item_text(typ: &str, code: &str, data: &[u8]) -> String {
        let hex = |s: &str| s.bytes().map(|b| format!("{b:02x}")).collect::<String>();
        let mut s = format!(
            "<item><type>{}</type><code>{}</code><length>{}</length>\n",
            hex(typ),
            hex(code),
            data.len()
        );
        if !data.is_empty() {
            s += "<data encoding=\"base64\">\n";
            s += &STANDARD.encode(data);
            s += "</data></item>\n";
        } else {
            s += "</item>\n";
        }
        s
    }

    #[test]
    fn parses_items_with_and_without_data() {
        let text = item_text("core", "asal", b"Rodeo") + &item_text("ssnc", "pend", b"");
        let mut r = MetadataReader::new(text.as_bytes());
        assert_eq!(
            r.next_item().unwrap(),
            Some(Item { typ: "core".into(), code: "asal".into(), data: b"Rodeo".to_vec() })
        );
        assert_eq!(
            r.next_item().unwrap(),
            Some(Item { typ: "ssnc".into(), code: "pend".into(), data: vec![] })
        );
        assert_eq!(r.next_item().unwrap(), None);
    }

    #[test]
    fn binary_payload_roundtrips() {
        let payload: Vec<u8> = (0..=255u8).cycle().take(10_001).collect();
        let text = item_text("ssnc", "PICT", &payload);
        let mut r = MetadataReader::new(text.as_bytes());
        assert_eq!(r.next_item().unwrap().unwrap().data, payload);
    }

    #[test]
    fn skips_garbage_lines() {
        let text = "junk\n</item>\n".to_string() + &item_text("ssnc", "prsm", b"");
        let mut r = MetadataReader::new(text.as_bytes());
        assert_eq!(r.next_item().unwrap().unwrap().code, "prsm");
    }
}
