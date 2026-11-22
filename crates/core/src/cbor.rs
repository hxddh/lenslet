//! Bounded, zero-unsafe RFC 8949 CBOR decoder for AWS Smithy RPCv2 (`rpc-v2-cbor`).
//!
//! Modern AWS SDKs (`boto3`, `aws-sdk-go-v2`, `aws-sdk-js-v3`, `aws-sdk-rust`)
//! transmit CloudWatch, SQS, SSM, DynamoDB and custom Smithy services using
//! `Content-Type: application/cbor` (`smithy-protocol: rpc-v2-cbor`). Decoding
//! CBOR into [`serde_json::Value`] lets IAM resource derivation read every
//! field the way it reads JSON protocols.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::{DateTime, Utc};
use serde_json::{Map, Number, Value};

const MAX_DEPTH: usize = 64;
const MAX_ITEMS: usize = 250_000;

/// Decode a complete CBOR value into [`serde_json::Value`].
pub fn decode_cbor(input: &[u8]) -> Option<Value> {
    if input.is_empty() {
        return None;
    }
    let mut reader = CborReader {
        buf: input,
        pos: 0,
        items: 0,
    };
    let val = reader.read_value(0)?;
    // Allow trailing padding whitespace only if entire frame was consumed;
    // CBOR frames are self-delimiting and must consume the payload cleanly.
    (reader.pos == input.len()).then_some(val)
}

/// Decode a CBOR payload into compact JSON text: what the run file stores
/// and access derivation reads.
pub fn decode_cbor_json(input: &[u8]) -> Option<String> {
    serde_json::to_string(&decode_cbor(input)?).ok()
}

struct CborReader<'a> {
    buf: &'a [u8],
    pos: usize,
    items: usize,
}

impl<'a> CborReader<'a> {
    fn bump_items(&mut self) -> Option<()> {
        self.items = self.items.checked_add(1)?;
        (self.items <= MAX_ITEMS).then_some(())
    }

    fn read_byte(&mut self) -> Option<u8> {
        let b = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    fn read_slice(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(len)?;
        let slice = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    fn read_arg(&mut self, info: u8) -> Option<u64> {
        match info {
            0..=23 => Some(info as u64),
            24 => Some(self.read_byte()? as u64),
            25 => {
                let s = self.read_slice(2)?;
                Some(u16::from_be_bytes([s[0], s[1]]) as u64)
            }
            26 => {
                let s = self.read_slice(4)?;
                Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]) as u64)
            }
            27 => {
                let s = self.read_slice(8)?;
                Some(u64::from_be_bytes([
                    s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
                ]))
            }
            _ => None,
        }
    }

    fn read_value(&mut self, depth: usize) -> Option<Value> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.bump_items()?;
        let head = self.read_byte()?;
        let major = head >> 5;
        let info = head & 0x1f;

        match major {
            // 0: Unsigned integer
            0 => {
                let n = self.read_arg(info)?;
                Some(Value::Number(Number::from(n)))
            }
            // 1: Negative integer (-1 - n)
            1 => {
                let n = self.read_arg(info)?;
                if let Some(neg) = (-1i128).checked_sub(n as i128) {
                    if let Ok(i) = i64::try_from(neg) {
                        return Some(Value::Number(Number::from(i)));
                    }
                    return Some(Value::String(neg.to_string()));
                }
                None
            }
            // 2: Byte string (definite or indefinite)
            2 => {
                if info == 31 {
                    let mut acc = Vec::new();
                    while self.buf.get(self.pos).copied()? != 0xff {
                        self.bump_items()?;
                        let ch_head = self.read_byte()?;
                        if (ch_head >> 5) != 2 || (ch_head & 0x1f) == 31 {
                            return None;
                        }
                        let len = usize::try_from(self.read_arg(ch_head & 0x1f)?).ok()?;
                        acc.extend_from_slice(self.read_slice(len)?);
                    }
                    self.pos += 1; // consume 0xff break
                    Some(Value::String(BASE64.encode(&acc)))
                } else {
                    let len = usize::try_from(self.read_arg(info)?).ok()?;
                    let bytes = self.read_slice(len)?;
                    Some(Value::String(BASE64.encode(bytes)))
                }
            }
            // 3: UTF-8 text string (definite or indefinite)
            3 => {
                if info == 31 {
                    let mut acc = String::new();
                    while self.buf.get(self.pos).copied()? != 0xff {
                        self.bump_items()?;
                        let ch_head = self.read_byte()?;
                        if (ch_head >> 5) != 3 || (ch_head & 0x1f) == 31 {
                            return None;
                        }
                        let len = usize::try_from(self.read_arg(ch_head & 0x1f)?).ok()?;
                        let piece = std::str::from_utf8(self.read_slice(len)?).ok()?;
                        acc.push_str(piece);
                    }
                    self.pos += 1;
                    Some(Value::String(acc))
                } else {
                    let len = usize::try_from(self.read_arg(info)?).ok()?;
                    let s = std::str::from_utf8(self.read_slice(len)?).ok()?;
                    Some(Value::String(s.to_string()))
                }
            }
            // 4: Array
            4 => {
                let mut items = Vec::new();
                if info == 31 {
                    while self.buf.get(self.pos).copied()? != 0xff {
                        items.push(self.read_value(depth + 1)?);
                    }
                    self.pos += 1;
                } else {
                    let len = usize::try_from(self.read_arg(info)?).ok()?;
                    items.reserve(len.min(1024));
                    for _ in 0..len {
                        items.push(self.read_value(depth + 1)?);
                    }
                }
                Some(Value::Array(items))
            }
            // 5: Map
            5 => {
                let mut map = Map::new();
                if info == 31 {
                    while self.buf.get(self.pos).copied()? != 0xff {
                        let k = value_to_key(self.read_value(depth + 1)?);
                        let v = self.read_value(depth + 1)?;
                        map.insert(k, v);
                    }
                    self.pos += 1;
                } else {
                    let len = usize::try_from(self.read_arg(info)?).ok()?;
                    for _ in 0..len {
                        let k = value_to_key(self.read_value(depth + 1)?);
                        let v = self.read_value(depth + 1)?;
                        map.insert(k, v);
                    }
                }
                Some(Value::Object(map))
            }
            // 6: Semantic Tag (e.g. Tag 1 epoch timestamp in Smithy RPCv2 CBOR)
            6 => {
                let tag = self.read_arg(info)?;
                let inner = self.read_value(depth + 1)?;
                if tag == 1
                    && let Some(time) = inner.as_f64().and_then(epoch_to_rfc3339)
                {
                    return Some(Value::String(time));
                }
                Some(inner)
            }
            // 7: Simple values & floats
            7 => self.read_simple(info),
            _ => None,
        }
    }

    /// Major type 7: simple values and floats.
    fn read_simple(&mut self, info: u8) -> Option<Value> {
        match info {
            20 => Some(Value::Bool(false)),
            21 => Some(Value::Bool(true)),
            22 | 23 => Some(Value::Null),
            25 => {
                let s = self.read_slice(2)?;
                Some(float_to_json(f16_to_f64(u16::from_be_bytes([s[0], s[1]]))))
            }
            26 => {
                let s = self.read_slice(4)?;
                Some(float_to_json(
                    f32::from_be_bytes([s[0], s[1], s[2], s[3]]) as f64
                ))
            }
            27 => {
                let s = self.read_slice(8)?;
                Some(float_to_json(f64::from_be_bytes([
                    s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
                ])))
            }
            0..=19 => Some(Value::Number(Number::from(info as u64))),
            24 => Some(Value::Number(Number::from(self.read_byte()? as u64))),
            _ => None,
        }
    }
}

/// RFC 3339 for epoch seconds (CBOR tag 1). A fraction counts forward from
/// the whole second below: `-1.5` is half a second after `-2`
/// (1969-12-31T23:59:58.5Z), not after `-1`.
fn epoch_to_rfc3339(secs: f64) -> Option<String> {
    if !secs.is_finite() {
        return None;
    }
    let mut whole = secs.floor();
    let mut nanos = ((secs - whole) * 1e9).round();
    if nanos >= 1e9 {
        whole += 1.0;
        nanos = 0.0;
    }
    if whole < i64::MIN as f64 || whole > i64::MAX as f64 {
        return None;
    }
    DateTime::<Utc>::from_timestamp(whole as i64, nanos as u32).map(|dt| dt.to_rfc3339())
}

fn value_to_key(val: Value) -> String {
    match val {
        Value::String(s) => s,
        other => other.to_string(),
    }
}

fn float_to_json(f: f64) -> Value {
    if let Some(num) = Number::from_f64(f) {
        Value::Number(num)
    } else if f.is_nan() {
        Value::String("NaN".into())
    } else if f.is_sign_positive() {
        Value::String("Infinity".into())
    } else {
        Value::String("-Infinity".into())
    }
}

/// Exact IEEE-754 binary16 → binary64 conversion.
fn f16_to_f64(bits: u16) -> f64 {
    let sign = if (bits & 0x8000) != 0 { -1.0 } else { 1.0 };
    let exp = ((bits >> 10) & 0x1f) as i32;
    let mant = (bits & 0x03ff) as u32;
    if exp == 0 {
        if mant == 0 {
            sign * 0.0
        } else {
            sign * (mant as f64 / 1024.0) * 2.0_f64.powi(-14)
        }
    } else if exp == 31 {
        if mant == 0 {
            sign * f64::INFINITY
        } else {
            f64::NAN
        }
    } else {
        sign * (1.0 + mant as f64 / 1024.0) * 2.0_f64.powi(exp - 15)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_smithy_rpcv2_cbor_maps_tags_and_indefinite_collections() {
        // CBOR map: {"QueueName": "orders", "DelaySeconds": 5, "Tags": {"env": "dev"}, "At": 1(1700000000)}
        let mut frame = Vec::new();
        frame.push(0xa4); // map(4)
        // "QueueName": "orders"
        frame.push(0x69);
        frame.extend_from_slice(b"QueueName");
        frame.push(0x66);
        frame.extend_from_slice(b"orders");
        // "DelaySeconds": 5
        frame.push(0x6c);
        frame.extend_from_slice(b"DelaySeconds");
        frame.push(0x05);
        // "Tags": indefinite map {"env": "dev"}
        frame.push(0x64);
        frame.extend_from_slice(b"Tags");
        frame.push(0xbf);
        frame.push(0x63);
        frame.extend_from_slice(b"env");
        frame.push(0x63);
        frame.extend_from_slice(b"dev");
        frame.push(0xff);
        // "At": tag(1) + uint32(1700000000)
        frame.push(0x62);
        frame.extend_from_slice(b"At");
        frame.push(0xc1);
        frame.push(0x1a);
        frame.extend_from_slice(&1_700_000_000u32.to_be_bytes());

        let json = decode_cbor(&frame).expect("valid CBOR");
        assert_eq!(json["QueueName"], "orders");
        assert_eq!(json["DelaySeconds"], 5);
        assert_eq!(json["Tags"]["env"], "dev");
        assert!(json["At"].as_str().unwrap().starts_with("2023-11-14T"));
    }

    fn tagged_epoch(secs: f64) -> Value {
        let mut frame = vec![0xc1, 0xfb];
        frame.extend_from_slice(&secs.to_be_bytes());
        decode_cbor(&frame).expect("valid CBOR")
    }

    #[test]
    fn fractional_epochs_before_1970_count_from_the_second_below() {
        assert_eq!(tagged_epoch(-1.5), "1969-12-31T23:59:58.500+00:00");
        assert_eq!(tagged_epoch(-0.25), "1969-12-31T23:59:59.750+00:00");
        assert_eq!(tagged_epoch(1.5), "1970-01-01T00:00:01.500+00:00");
        assert_eq!(tagged_epoch(-2.0), "1969-12-31T23:59:58+00:00");
        // A fraction that rounds up to a whole second carries over.
        assert_eq!(tagged_epoch(-0.000_000_000_1), "1970-01-01T00:00:00+00:00");
        // Out of range or not finite: the value itself.
        assert_eq!(tagged_epoch(1e300), serde_json::json!(1e300));
        let mut nan = vec![0xc1, 0xfb];
        nan.extend_from_slice(&f64::NAN.to_be_bytes());
        assert_eq!(decode_cbor(&nan), Some(Value::String("NaN".into())));
    }

    #[test]
    fn hostile_frames_are_rejected_without_recursion_or_allocation_blowups() {
        // Nesting deeper than MAX_DEPTH: arrays of one array each.
        let mut deep = vec![0x81; MAX_DEPTH + 2];
        deep.push(0x00);
        assert_eq!(decode_cbor(&deep), None);
        let mut ok = vec![0x81; MAX_DEPTH];
        ok.push(0x00);
        assert!(decode_cbor(&ok).is_some());
        // Indefinite-length nesting is bounded the same way.
        let mut indefinite = vec![0x9f; 10_000];
        indefinite.extend(vec![0xff; 10_000]);
        assert_eq!(decode_cbor(&indefinite), None);
        // Huge declared lengths with nothing behind them.
        for frame in [
            vec![0x5b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff], // bytes(2^64-1)
            vec![0x7b, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff], // text(2^63-1)
            vec![0x9b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff], // array(2^64-1)
            vec![0xbb, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00], // map(2^32)
            vec![0x5f, 0x5b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff], // chunk
        ] {
            assert_eq!(decode_cbor(&frame), None, "{frame:02x?}");
        }
        // An array with more items than the item budget fails fast.
        let mut wide = vec![0x9a];
        wide.extend_from_slice(&(MAX_ITEMS as u32 + 1).to_be_bytes());
        wide.extend(vec![0x00; MAX_ITEMS + 1]);
        assert_eq!(decode_cbor(&wide), None);
        // Truncated heads and reserved additional-information values.
        for frame in [
            vec![0x19, 0x01],
            vec![0x1c],
            vec![0xf8],
            vec![0xfc],
            vec![0xa1, 0x00],
        ] {
            assert_eq!(decode_cbor(&frame), None, "{frame:02x?}");
        }
        // Trailing bytes after a complete value.
        assert_eq!(decode_cbor(&[0x00, 0x00]), None);
    }
}
