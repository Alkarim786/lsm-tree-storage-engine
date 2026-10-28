//! Data block encoding with prefix-compressed (delta) keys.

use crate::crc::crc32;
use crate::{invalid, Entry};
use std::io;

pub fn put_varint(buf: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

pub fn get_varint(data: &[u8], pos: &mut usize) -> io::Result<u32> {
    let mut result = 0u32;
    let mut shift = 0;
    loop {
        let b = *data.get(*pos).ok_or_else(|| invalid("truncated varint"))?;
        *pos += 1;
        result |= ((b & 0x7f) as u32) << shift;
        if b & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift > 28 {
            return Err(invalid("varint too long"));
        }
    }
}

/// Appends a crc32 trailer.
pub fn with_crc(mut data: Vec<u8>) -> Vec<u8> {
    let c = crc32(&data);
    data.extend_from_slice(&c.to_le_bytes());
    data
}

/// Verifies and strips the crc32 trailer.
pub fn strip_crc(data: &[u8]) -> io::Result<&[u8]> {
    if data.len() < 4 {
        return Err(invalid("block too short"));
    }
    let (body, crc) = data.split_at(data.len() - 4);
    if crc32(body) != u32::from_le_bytes(crc.try_into().unwrap()) {
        return Err(invalid("block checksum mismatch"));
    }
    Ok(body)
}

#[derive(Default)]
pub struct BlockBuilder {
    buf: Vec<u8>,
    last_key: Vec<u8>,
    count: usize,
}

impl BlockBuilder {
    pub fn add(&mut self, key: &[u8], val: Option<&[u8]>) {
        let shared = self
            .last_key
            .iter()
            .zip(key)
            .take_while(|(a, b)| a == b)
            .count();
        put_varint(&mut self.buf, shared as u32);
        put_varint(&mut self.buf, (key.len() - shared) as u32);
        put_varint(&mut self.buf, val.map_or(0, |v| v.len() as u32 + 1));
        self.buf.extend_from_slice(&key[shared..]);
        if let Some(v) = val {
            self.buf.extend_from_slice(v);
        }
        self.last_key.clear();
        self.last_key.extend_from_slice(key);
        self.count += 1;
    }

    pub fn size(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Returns the finished block (with checksum) and resets the builder.
    pub fn finish(&mut self) -> Vec<u8> {
        self.last_key.clear();
        self.count = 0;
        with_crc(std::mem::take(&mut self.buf))
    }
}

pub fn decode_block(data: &[u8]) -> io::Result<Vec<Entry>> {
    let body = strip_crc(data)?;
    let mut pos = 0;
    let mut last: Vec<u8> = Vec::new();
    let mut out = Vec::new();
    while pos < body.len() {
        let shared = get_varint(body, &mut pos)? as usize;
        let unshared = get_varint(body, &mut pos)? as usize;
        let vl = get_varint(body, &mut pos)? as usize;
        if shared > last.len() || pos + unshared > body.len() {
            return Err(invalid("corrupt block entry"));
        }
        let mut key = last[..shared].to_vec();
        key.extend_from_slice(&body[pos..pos + unshared]);
        pos += unshared;
        let val = if vl == 0 {
            None
        } else {
            let n = vl - 1;
            if pos + n > body.len() {
                return Err(invalid("corrupt block value"));
            }
            let v = body[pos..pos + n].to_vec();
            pos += n;
            Some(v)
        };
        last.clone_from(&key);
        out.push((key, val));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_roundtrip_with_prefix_compression() {
        let mut b = BlockBuilder::default();
        let entries: Vec<Entry> = (0..200)
            .map(|i| {
                let k = format!("user:{:06}", i).into_bytes();
                let v = if i % 7 == 0 {
                    None
                } else {
                    Some(vec![i as u8; i % 13])
                };
                (k, v)
            })
            .collect();
        let mut raw = 0;
        for (k, v) in &entries {
            raw += k.len() + v.as_ref().map_or(0, |v| v.len());
            b.add(k, v.as_deref());
        }
        assert!(
            b.size() < raw + entries.len() * 3,
            "prefix compression should shrink keys"
        );
        let data = b.finish();
        assert!(b.is_empty());
        assert_eq!(decode_block(&data).unwrap(), entries);
    }

    #[test]
    fn detects_corruption() {
        let mut b = BlockBuilder::default();
        b.add(b"k", Some(b"v"));
        let mut data = b.finish();
        data[0] ^= 1;
        assert!(decode_block(&data).is_err());
    }

    #[test]
    fn varint_roundtrip() {
        for v in [0u32, 1, 127, 128, 300, 16384, u32::MAX] {
            let mut buf = Vec::new();
            put_varint(&mut buf, v);
            let mut pos = 0;
            assert_eq!(get_varint(&buf, &mut pos).unwrap(), v);
            assert_eq!(pos, buf.len());
        }
    }
}
