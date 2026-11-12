//! TLV8, the encoding of HomeKit pairing messages: one byte of type, one of length, the value.
//! A value longer than 255 bytes continues in the next item of the same type.

pub const METHOD: u8 = 0x00;
pub const IDENTIFIER: u8 = 0x01;
pub const SALT: u8 = 0x02;
pub const PUBLIC_KEY: u8 = 0x03;
pub const PROOF: u8 = 0x04;
pub const ENCRYPTED_DATA: u8 = 0x05;
pub const SEQUENCE: u8 = 0x06;
pub const ERROR: u8 = 0x07;
pub const SIGNATURE: u8 = 0x0A;

pub fn encode(items: &[(u8, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (kind, value) in items {
        if value.is_empty() {
            out.extend_from_slice(&[*kind, 0]);
        }
        for chunk in value.chunks(255) {
            out.push(*kind);
            out.push(chunk.len() as u8);
            out.extend_from_slice(chunk);
        }
    }
    out
}

/// Reads items, joining the pieces of long values; a truncated tail is dropped.
pub fn decode(data: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut items: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut rest = data;
    let mut previous_full = false;
    while let [kind, length, tail @ ..] = rest {
        let Some(value) = tail.get(..usize::from(*length)) else {
            break;
        };
        match items.last_mut() {
            // Only a full item can continue: two short items of one type are two values.
            Some((last, joined)) if *last == *kind && previous_full => {
                joined.extend_from_slice(value)
            }
            _ => items.push((*kind, value.to_vec())),
        }
        previous_full = *length == 255;
        rest = &tail[usize::from(*length)..];
    }
    items
}

pub fn get(items: &[(u8, Vec<u8>)], kind: u8) -> Option<&[u8]> {
    items
        .iter()
        .find(|(found, _)| *found == kind)
        .map(|(_, value)| value.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_short_and_long_values() {
        let long = vec![7u8; 600];
        let encoded = encode(&[(SEQUENCE, &[1]), (PUBLIC_KEY, &long), (METHOD, &[])]);
        assert_eq!(&encoded[..3], &[SEQUENCE, 1, 1]);
        assert_eq!(encoded.len(), 3 + 600 + 3 * 2 + 2);
        let decoded = decode(&encoded);
        assert_eq!(get(&decoded, SEQUENCE), Some([1u8].as_slice()));
        assert_eq!(get(&decoded, PUBLIC_KEY), Some(long.as_slice()));
        assert_eq!(get(&decoded, METHOD), Some([].as_slice()));
        assert_eq!(get(&decoded, PROOF), None);
    }

    #[test]
    fn short_items_of_one_type_stay_apart() {
        let decoded = decode(&[IDENTIFIER, 1, b'a', IDENTIFIER, 1, b'b']);
        assert_eq!(
            decoded,
            vec![(IDENTIFIER, vec![b'a']), (IDENTIFIER, vec![b'b'])]
        );
    }

    #[test]
    fn drops_a_truncated_tail() {
        assert_eq!(
            decode(&[SEQUENCE, 1, 2, ERROR, 5, 1]),
            vec![(SEQUENCE, vec![2])]
        );
        assert!(decode(&[SEQUENCE]).is_empty());
        assert!(decode(&[]).is_empty());
    }
}
