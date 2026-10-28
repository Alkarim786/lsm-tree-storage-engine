//! MANIFEST: the durable list of live SSTables per level. Rewritten in full to a
//! temp file, fsynced, then atomically renamed over the old one.

use crate::invalid;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestData {
    pub next_id: u64,
    /// WAL files with an id below this are fully flushed and may be ignored.
    pub log_number: u64,
    /// `(level, file id)`
    pub files: Vec<(usize, u64)>,
}

impl Default for ManifestData {
    fn default() -> Self {
        ManifestData {
            next_id: 1,
            log_number: 0,
            files: Vec::new(),
        }
    }
}

pub fn save(dir: &Path, data: &ManifestData) -> io::Result<()> {
    let mut text = format!("next_id {}\nlog_number {}\n", data.next_id, data.log_number);
    for (level, id) in &data.files {
        text.push_str(&format!("file {level} {id}\n"));
    }
    let tmp = dir.join("MANIFEST.tmp");
    let mut f = File::create(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, dir.join("MANIFEST"))?;
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    Ok(())
}

pub fn load(dir: &Path) -> io::Result<Option<ManifestData>> {
    let text = match fs::read_to_string(dir.join("MANIFEST")) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut d = ManifestData::default();
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let num = |s: &str| {
            s.parse::<u64>()
                .map_err(|_| invalid(format!("bad manifest line: {line}")))
        };
        match parts.as_slice() {
            ["next_id", n] => d.next_id = num(n)?,
            ["log_number", n] => d.log_number = num(n)?,
            ["file", l, id] => d.files.push((num(l)? as usize, num(id)?)),
            [] => {}
            _ => return Err(invalid(format!("bad manifest line: {line}"))),
        }
    }
    Ok(Some(d))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join(format!("lsm-manifest-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert!(load(&dir).unwrap().is_none());
        let d = ManifestData {
            next_id: 42,
            log_number: 7,
            files: vec![(0, 3), (1, 5), (1, 6)],
        };
        save(&dir, &d).unwrap();
        assert_eq!(load(&dir).unwrap().unwrap(), d);
        fs::remove_dir_all(&dir).unwrap();
    }
}
