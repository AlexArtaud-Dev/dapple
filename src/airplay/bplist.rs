//! Apple's binary property lists, the format of every AirPlay 2 message body.
//!
//! Only what AirPlay exchanges: dictionaries, arrays, strings, integers, reals, booleans and
//! raw data. Dictionaries keep their order, which some receivers rely on.

use crate::error::Result;
use crate::fail;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Str(String),
    Data(Vec<u8>),
    Array(Vec<Value>),
    Dict(Vec<(String, Value)>),
}

impl Value {
    /// The value under a key, for dictionaries.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Dict(entries) => entries
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_data(&self) -> Option<&[u8]> {
        match self {
            Value::Data(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// Numbers arrive as integers or reals depending on the sender's mood.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(number) => Some(*number as f64),
            Value::Real(number) => Some(*number),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(number) => Some(*number),
            Value::Real(number) => Some(*number as i64),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }
}

/// Builds a dictionary from `(key, value)` pairs.
pub fn dict(entries: Vec<(&str, Value)>) -> Value {
    Value::Dict(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    )
}

pub fn text(value: &str) -> Value {
    Value::Str(value.to_string())
}

const HEADER: &[u8] = b"bplist00";
const TRAILER: usize = 32;

/// Flattens the tree: every object gets a number, containers refer to their children by it.
fn flatten(value: &Value, objects: &mut Vec<Flat>) -> usize {
    let index = objects.len();
    match value {
        Value::Array(items) => {
            objects.push(Flat::Array(Vec::new()));
            let children = items.iter().map(|item| flatten(item, objects)).collect();
            objects[index] = Flat::Array(children);
        }
        Value::Dict(entries) => {
            objects.push(Flat::Dict(Vec::new(), Vec::new()));
            let keys = entries
                .iter()
                .map(|(key, _)| flatten(&Value::Str(key.clone()), objects))
                .collect();
            let values = entries
                .iter()
                .map(|(_, value)| flatten(value, objects))
                .collect();
            objects[index] = Flat::Dict(keys, values);
        }
        leaf => objects.push(Flat::Leaf(leaf.clone())),
    }
    index
}

enum Flat {
    Leaf(Value),
    Array(Vec<usize>),
    Dict(Vec<usize>, Vec<usize>),
}

/// The marker byte and, for 15 items or more, the count that follows it.
fn marker(out: &mut Vec<u8>, kind: u8, count: usize) {
    if count < 15 {
        out.push(kind | count as u8);
    } else {
        out.push(kind | 0x0F);
        integer(out, count as i64);
    }
}

fn integer(out: &mut Vec<u8>, number: i64) {
    if (0..=0xFF).contains(&number) {
        out.push(0x10);
        out.push(number as u8);
    } else if (0..=0xFFFF).contains(&number) {
        out.push(0x11);
        out.extend_from_slice(&(number as u16).to_be_bytes());
    } else if (0..=0xFFFF_FFFF).contains(&number) {
        out.push(0x12);
        out.extend_from_slice(&(number as u32).to_be_bytes());
    } else {
        out.push(0x13);
        out.extend_from_slice(&number.to_be_bytes());
    }
}

fn reference(out: &mut Vec<u8>, index: usize, size: usize) {
    out.extend_from_slice(&(index as u64).to_be_bytes()[8 - size..]);
}

/// Writes a value as a binary property list.
pub fn encode(value: &Value) -> Vec<u8> {
    let mut objects = Vec::new();
    flatten(value, &mut objects);
    let ref_size = if objects.len() < 256 { 1 } else { 2 };
    let mut out = HEADER.to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for object in &objects {
        offsets.push(out.len());
        match object {
            Flat::Leaf(Value::Null) => out.push(0x00),
            Flat::Leaf(Value::Bool(flag)) => out.push(if *flag { 0x09 } else { 0x08 }),
            Flat::Leaf(Value::Int(number)) => integer(&mut out, *number),
            Flat::Leaf(Value::Real(number)) => {
                out.push(0x23);
                out.extend_from_slice(&number.to_be_bytes());
            }
            Flat::Leaf(Value::Str(string)) if string.is_ascii() => {
                marker(&mut out, 0x50, string.len());
                out.extend_from_slice(string.as_bytes());
            }
            Flat::Leaf(Value::Str(string)) => {
                let units: Vec<u16> = string.encode_utf16().collect();
                marker(&mut out, 0x60, units.len());
                for unit in units {
                    out.extend_from_slice(&unit.to_be_bytes());
                }
            }
            Flat::Leaf(Value::Data(bytes)) => {
                marker(&mut out, 0x40, bytes.len());
                out.extend_from_slice(bytes);
            }
            // Containers never reach here as leaves: flatten replaces them.
            Flat::Leaf(Value::Array(_)) | Flat::Leaf(Value::Dict(_)) => out.push(0x00),
            Flat::Array(children) => {
                marker(&mut out, 0xA0, children.len());
                for child in children {
                    reference(&mut out, *child, ref_size);
                }
            }
            Flat::Dict(keys, values) => {
                marker(&mut out, 0xD0, keys.len());
                for index in keys.iter().chain(values) {
                    reference(&mut out, *index, ref_size);
                }
            }
        }
    }
    let table = out.len();
    let offset_size = match table {
        0..=0xFF => 1,
        0x100..=0xFFFF => 2,
        _ => 4,
    };
    for offset in &offsets {
        reference(&mut out, *offset, offset_size);
    }
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0, offset_size as u8, ref_size as u8]);
    out.extend_from_slice(&(objects.len() as u64).to_be_bytes());
    out.extend_from_slice(&0u64.to_be_bytes());
    out.extend_from_slice(&(table as u64).to_be_bytes());
    out
}

struct Reader<'a> {
    data: &'a [u8],
    offsets: Vec<usize>,
    ref_size: usize,
}

fn number(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .fold(0usize, |total, byte| (total << 8) | usize::from(*byte))
}

impl Reader<'_> {
    fn bytes(&self, at: usize, length: usize) -> Result<&[u8]> {
        self.data
            .get(
                at..at
                    .checked_add(length)
                    .ok_or_else(|| fail!("property list too large"))?,
            )
            .ok_or_else(|| fail!("truncated property list"))
    }

    /// The count of a marker, and where the content starts.
    fn count(&self, at: usize) -> Result<(usize, usize)> {
        let low = usize::from(self.bytes(at, 1)?[0] & 0x0F);
        if low < 15 {
            return Ok((low, at + 1));
        }
        let size = 1usize << (self.bytes(at + 1, 1)?[0] & 0x0F);
        Ok((number(self.bytes(at + 2, size)?), at + 2 + size))
    }

    fn object(&self, index: usize, depth: usize) -> Result<Value> {
        if depth > 32 {
            return Err(fail!("property list nested too deep"));
        }
        let at = *self
            .offsets
            .get(index)
            .ok_or_else(|| fail!("bad property list reference"))?;
        let kind = self.bytes(at, 1)?[0];
        let children = |start: usize, count: usize| -> Result<Vec<Value>> {
            (0..count)
                .map(|slot| {
                    let child = number(self.bytes(start + slot * self.ref_size, self.ref_size)?);
                    self.object(child, depth + 1)
                })
                .collect()
        };
        Ok(match kind >> 4 {
            0x0 => match kind {
                0x08 => Value::Bool(false),
                0x09 => Value::Bool(true),
                _ => Value::Null,
            },
            0x1 => {
                let size = 1usize << (kind & 0x0F);
                let raw = self.bytes(at + 1, size)?;
                match size {
                    // Eight bytes (and more) are signed; shorter integers are not.
                    8 => Value::Int(i64::from_be_bytes(raw.try_into().unwrap_or([0; 8]))),
                    16 => Value::Int(i64::from_be_bytes(raw[8..].try_into().unwrap_or([0; 8]))),
                    _ => Value::Int(number(raw) as i64),
                }
            }
            0x2 => match kind & 0x0F {
                2 => Value::Real(f64::from(f32::from_be_bytes(
                    self.bytes(at + 1, 4)?.try_into().unwrap_or([0; 4]),
                ))),
                _ => Value::Real(f64::from_be_bytes(
                    self.bytes(at + 1, 8)?.try_into().unwrap_or([0; 8]),
                )),
            },
            // A date: seconds since 2001, kept as a number.
            0x3 => Value::Real(f64::from_be_bytes(
                self.bytes(at + 1, 8)?.try_into().unwrap_or([0; 8]),
            )),
            0x4 => {
                let (count, start) = self.count(at)?;
                Value::Data(self.bytes(start, count)?.to_vec())
            }
            0x5 => {
                let (count, start) = self.count(at)?;
                Value::Str(String::from_utf8_lossy(self.bytes(start, count)?).into_owned())
            }
            0x6 => {
                let (count, start) = self.count(at)?;
                let units: Vec<u16> = self
                    .bytes(start, count * 2)?
                    .chunks(2)
                    .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                    .collect();
                Value::Str(String::from_utf16_lossy(&units))
            }
            // A UID (keyed archives): its number is enough here.
            0x8 => Value::Int(number(self.bytes(at + 1, usize::from(kind & 0x0F) + 1)?) as i64),
            0xA | 0xC => {
                let (count, start) = self.count(at)?;
                Value::Array(children(start, count)?)
            }
            0xD => {
                let (count, start) = self.count(at)?;
                let keys = children(start, count)?;
                let values = children(start + count * self.ref_size, count)?;
                Value::Dict(
                    keys.into_iter()
                        .zip(values)
                        .filter_map(|(key, value)| match key {
                            Value::Str(name) => Some((name, value)),
                            _ => None,
                        })
                        .collect(),
                )
            }
            _ => return Err(fail!("unsupported property list object")),
        })
    }
}

/// Reads a binary property list.
pub fn decode(data: &[u8]) -> Result<Value> {
    if !data.starts_with(HEADER) || data.len() < HEADER.len() + TRAILER {
        return Err(fail!("not a binary property list"));
    }
    let trailer = &data[data.len() - TRAILER..];
    let offset_size = usize::from(trailer[6]);
    let ref_size = usize::from(trailer[7]);
    let count = number(&trailer[8..16]);
    let top = number(&trailer[16..24]);
    let table = number(&trailer[24..32]);
    if !(1..=8).contains(&offset_size) || !(1..=8).contains(&ref_size) || count > data.len() {
        return Err(fail!("malformed property list"));
    }
    let mut offsets = Vec::with_capacity(count);
    for index in 0..count {
        let at = table + index * offset_size;
        let raw = data
            .get(at..at + offset_size)
            .ok_or_else(|| fail!("truncated property list"))?;
        offsets.push(number(raw));
    }
    Reader {
        data,
        offsets,
        ref_size,
    }
    .object(top, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let value = dict(vec![
            ("type", text("insertPlayQueueItem")),
            ("rate", Value::Real(1.0)),
            ("flag", Value::Bool(true)),
            ("off", Value::Bool(false)),
            ("nothing", Value::Null),
            ("small", Value::Int(130)),
            ("medium", Value::Int(40_000)),
            ("large", Value::Int(3_000_000_000)),
            ("huge", Value::Int(-5)),
            ("data", Value::Data(vec![1, 2, 3])),
            ("long", Value::Data(vec![7; 300])),
            ("accent", text("écran")),
            (
                "list",
                Value::Array(vec![
                    Value::Int(1),
                    text("two"),
                    dict(vec![("k", text("v"))]),
                ]),
            ),
            ("sixteen", Value::Array((0..16).map(Value::Int).collect())),
        ]);
        assert_eq!(decode(&encode(&value)).unwrap(), value);
    }

    #[test]
    fn matches_a_known_encoding() {
        // plistlib.dumps({"a": 1}, fmt=FMT_BINARY)
        let known = [
            b"bplist00".as_slice(),
            &[0xD1, 0x01, 0x02, 0x51, b'a', 0x10, 0x01],
            &[0x08, 0x0B, 0x0D],
            &[0, 0, 0, 0, 0, 0, 1, 1],
            &3u64.to_be_bytes(),
            &0u64.to_be_bytes(),
            &15u64.to_be_bytes(),
        ]
        .concat();
        assert_eq!(encode(&dict(vec![("a", Value::Int(1))])), known);
        assert_eq!(decode(&known).unwrap(), dict(vec![("a", Value::Int(1))]));
    }

    #[test]
    fn many_objects_use_wider_references() {
        let value = Value::Array((0..400).map(Value::Int).collect());
        assert_eq!(decode(&encode(&value)).unwrap(), value);
    }

    #[test]
    fn reads_values() {
        let value = dict(vec![
            ("n", Value::Int(4)),
            ("r", Value::Real(2.5)),
            ("s", text("x")),
            ("d", Value::Data(vec![9])),
            ("a", Value::Array(vec![])),
        ]);
        assert_eq!(value.get("n").and_then(Value::as_i64), Some(4));
        assert_eq!(value.get("n").and_then(Value::as_f64), Some(4.0));
        assert_eq!(value.get("r").and_then(Value::as_f64), Some(2.5));
        assert_eq!(value.get("r").and_then(Value::as_i64), Some(2));
        assert_eq!(value.get("s").and_then(Value::as_str), Some("x"));
        assert_eq!(
            value.get("d").and_then(Value::as_data),
            Some([9u8].as_slice())
        );
        assert_eq!(
            value.get("a").and_then(Value::as_array).map(<[Value]>::len),
            Some(0)
        );
        assert_eq!(value.get("missing"), None);
        assert_eq!(Value::Int(1).get("n"), None);
        assert_eq!(
            (
                Value::Null.as_str(),
                Value::Null.as_data(),
                Value::Null.as_f64(),
                Value::Null.as_i64()
            ),
            (None, None, None, None)
        );
        assert!(Value::Null.as_array().is_none());
    }

    #[test]
    fn refuses_garbage() {
        assert!(decode(b"").is_err());
        assert!(decode(b"bplist00").is_err());
        assert!(decode(&[b'x'; 64]).is_err());
        let good = encode(&dict(vec![("a", text("b"))]));
        for cut in 8..good.len() {
            let _ = decode(&good[..cut]);
        }
        let mut bad_reference = good.clone();
        let length = bad_reference.len();
        bad_reference[length - 9] = 200;
        assert!(decode(&bad_reference).is_err());
    }
}
