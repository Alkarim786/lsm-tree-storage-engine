//! Interactive shell: `cargo run --release --example kv_cli -- ./data`
//! Commands: put K V | get K | del K | scan LO HI | flush | compact | stats | quit

use lsm_engine::{Db, Options};
use std::io::{self, BufRead, Write};

fn main() -> io::Result<()> {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "./lsm-data".to_string());
    let db = Db::open(&dir, Options::default())?;
    println!("lsm-tree-storage-engine shell, data dir: {dir}");
    let stdin = io::stdin();
    loop {
        print!("> ");
        io::stdout().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts.as_slice() {
            ["put", k, v] => db.put(k.as_bytes(), v.as_bytes())?,
            ["get", k] => match db.get(k.as_bytes())? {
                Some(v) => println!("{}", String::from_utf8_lossy(&v)),
                None => println!("(not found)"),
            },
            ["del", k] => db.delete(k.as_bytes())?,
            ["scan", lo, hi] => {
                for (k, v) in db.scan(lo.as_bytes(), hi.as_bytes())? {
                    println!(
                        "{} = {}",
                        String::from_utf8_lossy(&k),
                        String::from_utf8_lossy(&v)
                    );
                }
            }
            ["flush"] => db.flush()?,
            ["compact"] => db.compact_all()?,
            ["stats"] => {
                println!("{:#?}", db.stats());
                println!("files per level: {:?}", db.level_file_counts());
            }
            ["quit"] | ["exit"] => break,
            [] => {}
            _ => println!(
                "commands: put K V | get K | del K | scan LO HI | flush | compact | stats | quit"
            ),
        }
    }
    Ok(())
}
