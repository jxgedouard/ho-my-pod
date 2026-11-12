//! TLV8 encoding used by HomeKit-style pairing. Values longer than 255 bytes
//! are split in consecutive items of the same type.

use std::collections::HashMap;

pub const METHOD: u8 = 0;
pub const SALT: u8 = 2;
pub const PUBLIC_KEY: u8 = 3;
pub const PROOF: u8 = 4;
pub const STATE: u8 = 6;
pub const ERROR: u8 = 7;
pub const FLAGS: u8 = 19;

pub fn encode(items: &[(u8, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for &(ty, value) in items {
        if value.is_empty() {
            out.push(ty);
            out.push(0);
            continue;
        }
        for chunk in value.chunks(255) {
            out.push(ty);
            out.push(chunk.len() as u8);
            out.extend_from_slice(chunk);
        }
    }
    out
}

pub fn decode(data: &[u8]) -> HashMap<u8, Vec<u8>> {
    let mut map: HashMap<u8, Vec<u8>> = HashMap::new();
    let mut i = 0;
    let mut last_type: Option<u8> = None;
    while i + 2 <= data.len() {
        let ty = data[i];
        let len = data[i + 1] as usize;
        let end = (i + 2 + len).min(data.len());
        let value = &data[i + 2..end];
        if last_type == Some(ty) {
            // Continuation of a fragmented value
            map.entry(ty).or_default().extend_from_slice(value);
        } else {
            map.insert(ty, value.to_vec());
        }
        last_type = Some(ty);
        i = end;
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_long_value() {
        let long = vec![7u8; 600];
        let enc = encode(&[(STATE, &[3]), (PUBLIC_KEY, &long), (PROOF, &[1, 2])]);
        let dec = decode(&enc);
        assert_eq!(dec[&STATE], vec![3]);
        assert_eq!(dec[&PUBLIC_KEY], long);
        assert_eq!(dec[&PROOF], vec![1, 2]);
    }
}
