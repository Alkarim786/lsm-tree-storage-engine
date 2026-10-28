//! Write-ahead log. Record framing: `crc32(payload) u32 | len u32 | payload`,
//! payload = `kind u8 | klen u32 | key | vlen u32 | value` (kind 0 = delete).

use crate::crc::crc32;
use crate::Entry;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

const MAX_RECORD: usize = 64 << 20;

pub struct WalWriter {
    file: File,
    sync: bool,
}

impl WalWriter {
    pub fn create(path: &Path, sync: bool) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(WalWriter { file, sync })
    }

    /// Appends one record with a single `write` call; fsyncs when configured.
    /// Returns the number of bytes written.
    pub fn append(&mut self, key: &[u8], val: Option<&[u8]>) -> io::Result<usize> {
        let mut payload = Vec::with_capacity(9 + key.len() + val.map_or(0, |v| v.len()));
        payload.push(val.is_some() as u8);
        payload.extend_from_slice(&(key.len() as u32).to_le_bytes());
        payload.extend_from_slice(key);
        let v = val.unwrap_or(&[]);
        payload.extend_from_slice(&(v.len() as u32).to_le_bytes());
        payload.extend_from_slice(v);
        let mut rec = Vec::with_capacity(8 + payload.len());
        rec.extend_from_slice(&crc32(&payload).to_le_bytes());
        rec.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        rec.extend_from_slice(&payload);
        self.file.write_all(&rec)?;
        if self.sync {
            self.file.sync_data()?;
        }
        Ok(rec.len())
    }
}

/// Reads every intact record. Stops silently at the first truncated or corrupt
/// record: that is the torn tail of a crashed write and was never acknowledged.
pub fn read_wal(path: &Path) -> io::Result<Vec<Entry>> {
    let mut data = Vec::new();
    File::open(path)?.read_to_end(&mut data)?;
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let crc = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        let len = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        if len > MAX_RECORD || pos + 8 + len > data.len() {
            break;
        }
        let payload = &data[pos + 8..pos + 8 + len];
        if crc32(payload) != crc || payload.len() < 9 {
            break;
        }
        let kind = payload[0];
        let klen = u32::from_le_bytes(payload[1..5].try_into().unwrap()) as usize;
        if 5 + klen + 4 > payload.len() {
            break;
        }
        let key = payload[5..5 + klen].to_vec();
        let vlen = u32::from_le_bytes(payload[5 + klen..9 + klen].try_into().unwrap()) as usize;
        if 9 + klen + vlen != payload.len() {
            break;
        }
        let val = (kind == 1).then(|| payload[9 + klen..].to_vec());
        out.push((key, val));
        pos += 8 + len;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("lsm-wal-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn roundtrip_and_torn_tail() {
        let p = tmp("rt");
        {
            let mut w = WalWriter::create(&p, false).unwrap();
            w.append(b"a", Some(b"1")).unwrap();
            w.append(b"b", None).unwrap();
            w.append(b"c", Some(b"")).unwrap();
        }
        let all = read_wal(&p).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[1], (b"b".to_vec(), None));
        assert_eq!(all[2], (b"c".to_vec(), Some(vec![])));

        // Every possible truncation point yields a clean prefix of the log.
        let bytes = std::fs::read(&p).unwrap();
        for cut in 0..bytes.len() {
            std::fs::write(&p, &bytes[..cut]).unwrap();
            let got = read_wal(&p).unwrap();
            assert!(got.len() <= 3 && got[..] == all[..got.len()]);
        }
        // A flipped bit in the middle record stops replay there.
        let mut bad = bytes.clone();
        bad[bytes.len() / 2] ^= 0xff;
        std::fs::write(&p, &bad).unwrap();
        assert!(read_wal(&p).unwrap().len() < 3);
        let _ = std::fs::remove_file(&p);
    }
}
