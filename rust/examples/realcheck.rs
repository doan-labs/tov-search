//! Checks the Tov app against this one on a real disk.
//!
//!   realcheck scan <root> <disk-file>          scan like the daemon, write a disk file
//!   realcheck engine <disk-file> <home> [linux] bench/PROTOCOL.md, ranked like the daemon
//!   realcheck serve <disk-file> <home> [linux]  the daemon's JSON replies, on stdin/stdout
//!
//! A folder the scan did not enter (a mount point, or skipped) carries
//! FLAG_MOUNT in the disk file and gets no block, as in the daemon's index.

use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::sync::atomic::Ordering;
use tov_search::index::{Index, linux_prior_adjust};
use tov_search::live::Live;
use tov_search::query::{NOW_OVERRIDE, Query, Searcher};
use tov_search::walk::{self, FLAG_MOUNT, KIND_DIR, Listing, NONE, RawEnt};

fn rd<const N: usize>(b: &[u8], at: usize) -> [u8; N] {
    b[at..at + N].try_into().unwrap()
}

fn scan(root: &str, out: &str) {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    // As the daemon: Linux always skips its pseudo filesystems.
    if cfg!(target_os = "linux") || !tov_search::has_full_disk_access() || std::env::var_os("TOV_SEARCH_RESTRICT").is_some() {
        let _ = walk::SKIP.set(tov_search::gated(&home));
    }
    let now = tov_search::query::now_secs();
    let ls = walk::scan(root.as_bytes(), 8);
    let mut by_id = vec![NONE; ls.iter().map(|l| l.id as usize + 1).max().unwrap_or(1)];
    for (i, l) in ls.iter().enumerate() {
        by_id[l.id as usize] = i as u32;
    }
    let mut b = b"FSDISK01".to_vec();
    b.extend_from_slice(&[0; 8]);
    b.extend_from_slice(&now.to_le_bytes());
    b.extend_from_slice(&[0, 0, 0, 0, KIND_DIR, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let mut n = 1u64;
    // Breadth first, so parents come before their children.
    let mut todo = std::collections::VecDeque::from([(0u32, by_id[0])]);
    while let Some((rec, li)) = todo.pop_front() {
        let l = &ls[li as usize];
        for e in &l.ents {
            let mut kind = e.kind;
            let child = by_id.get(e.child as usize).copied().unwrap_or(NONE);
            if kind & 3 == KIND_DIR && child == NONE {
                kind |= FLAG_MOUNT;
            }
            if child != NONE {
                todo.push_back((n as u32, child));
            }
            b.extend_from_slice(&rec.to_le_bytes());
            b.push(kind);
            b.extend_from_slice(&e.size.to_le_bytes());
            b.extend_from_slice(&e.mtime.to_le_bytes());
            b.extend_from_slice(&e.name_len.to_le_bytes());
            b.extend_from_slice(&l.names[e.name_off as usize..][..e.name_len as usize]);
            n += 1;
        }
    }
    b[8..16].copy_from_slice(&n.to_le_bytes());
    std::fs::write(out, b).unwrap();
}

/// The index of a disk file, built like the daemon builds its own. pin: the
/// disk file's clock, else the real one (as the daemon).
fn load(path: &str, home: &str, linux: bool, pin: bool) -> Live {
    let b = std::fs::read(path).expect("read disk file");
    assert!(b.len() >= 20 && &b[..8] == b"FSDISK01", "not a disk file");
    let n = u64::from_le_bytes(rd(&b, 8)) as usize;
    let now = u32::from_le_bytes(rd(&b, 16));
    let mut slot = vec![NONE; n];
    let mut ls: Vec<Listing> = vec![Listing { id: 0, names: Vec::new(), ents: Vec::new() }];
    slot[0] = 0;
    let mut p = 20 + 19;
    for i in 1..n {
        let parent = u32::from_le_bytes(rd(&b, p)) as usize;
        let kind = b[p + 4];
        let len = u16::from_le_bytes(rd(&b, p + 17)) as usize;
        let child = if kind & 3 == KIND_DIR && kind & FLAG_MOUNT == 0 {
            slot[i] = ls.len() as u32;
            ls.push(Listing { id: i as u32, names: Vec::new(), ents: Vec::new() });
            i as u32
        } else {
            NONE
        };
        let l = &mut ls[slot[parent] as usize];
        let (size, mtime) = (u64::from_le_bytes(rd(&b, p + 5)), u32::from_le_bytes(rd(&b, p + 13)));
        l.ents.push(RawEnt { name_off: l.names.len() as u32, name_len: len as u16, kind, size, mtime, child });
        l.names.extend_from_slice(&b[p + 19..p + 19 + len]);
        p += 19 + len;
    }
    drop(b);
    if pin {
        NOW_OVERRIDE.store(now, Ordering::Relaxed);
    }
    let idx = if linux { Index::build_ranked(ls, 0, now, home.as_bytes(), linux_prior_adjust) } else { Index::build(ls, 0, now, home.as_bytes()) };
    Live::new(idx)
}

fn engine(path: &str, home: &str, linux: bool) {
    let mut live = load(path, home, linux, true);
    let mut out = std::io::BufWriter::with_capacity(1 << 16, std::io::stdout().lock());
    out.write_all(b"ready\n").unwrap();
    out.flush().unwrap();
    let mut p = Vec::new();
    for line in std::io::stdin().lock().lines() {
        let line = line.expect("stdin");
        // Not a query: bring a folder in line with the disk, as the daemon does on an event.
        if let Some(dir) = line.strip_prefix("!apply ") {
            live.apply_dir(dir.as_bytes(), false);
            out.write_all(b"\n").unwrap();
            out.flush().unwrap();
            continue;
        }
        match Query::parse(&line, home) {
            Err(_) => out.write_all(b"error\n").unwrap(),
            Ok(q) => {
                for h in (Searcher { live: &live }).search(&q) {
                    match &h.over {
                        Some(o) => p.clone_from(o),
                        None => live.base.path(h.idx as usize, &mut p),
                    }
                    write!(out, "{}\t", h.score).unwrap();
                    out.write_all(&p).unwrap();
                    out.write_all(b"\n").unwrap();
                }
            }
        }
        out.write_all(b"\n").unwrap();
        out.flush().unwrap();
    }
}

/// One serialized hit, as rust/src/server.rs writes it.
#[derive(serde::Serialize)]
struct HitOut<'a> {
    kind: &'a str,
    mtime: u32,
    path: std::borrow::Cow<'a, str>,
    score: i32,
    size: u64,
}

#[derive(serde::Serialize)]
struct HitsOut<'a> {
    hits: Vec<HitOut<'a>>,
    id: &'a Value,
    ok: bool,
    took_us: u64,
}

/// rust/src/server.rs's answers to name searches, pings and unknown ops,
/// one JSON request per stdin line, took_us 0.
fn serve(path: &str, home: &str, linux: bool) {
    let live = load(path, home, linux, false);
    let s = Searcher { live: &live };
    let mut out = std::io::stdout().lock();
    let mut p = Vec::new();
    for line in std::io::stdin().lock().lines() {
        let line = line.expect("stdin");
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                writeln!(out, "{}", json!({"ok": false, "error": format!("bad json: {e}")})).unwrap();
                continue;
            }
        };
        let id = v.get("id").unwrap_or(&Value::Null).clone();
        let op = v.get("op").and_then(Value::as_str).unwrap_or("search");
        let reply = match op {
            "ping" => json!({"ok": true, "id": id}).to_string(),
            "search" => match request(&v, home) {
                Ok(q) => {
                    let hits = s.search(&q);
                    let paths: Vec<Vec<u8>> = hits
                        .iter()
                        .map(|h| {
                            live.base.path(h.idx as usize, &mut p);
                            p.clone()
                        })
                        .collect();
                    let hits = hits
                        .iter()
                        .zip(&paths)
                        .map(|(h, path)| {
                            let i = h.idx as usize;
                            let kind = match live.base.kind()[i] & 3 {
                                0 => "file",
                                1 => "dir",
                                2 => "link",
                                _ => "other",
                            };
                            HitOut { kind, mtime: live.base.mtime()[i], path: String::from_utf8_lossy(path), score: h.score, size: live.base.size_of(i) }
                        })
                        .collect();
                    serde_json::to_string(&HitsOut { hits, id: &id, ok: true, took_us: 0 }).unwrap()
                }
                Err(e) => json!({"ok": false, "error": e, "id": id}).to_string(),
            },
            _ => json!({"ok": false, "error": format!("unknown op {op}"), "id": id}).to_string(),
        };
        writeln!(out, "{reply}").unwrap();
        out.flush().unwrap();
    }
}

/// rust/src/server.rs's parse_request.
fn request(v: &Value, home: &str) -> Result<Query, String> {
    let mut q = Query::parse(v.get("q").and_then(Value::as_str).unwrap_or(""), home)?;
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            if k == "limit" {
                q.limit = val.as_u64().ok_or("limit must be a number")? as usize;
            } else {
                let s = match val {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                q.filter(k, &s, home)?;
            }
        }
    }
    Ok(q)
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    match a.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["scan", root, out] => scan(root, out),
        ["engine", disk, home] => engine(disk, home, false),
        ["engine", disk, home, "linux"] => engine(disk, home, true),
        ["serve", disk, home] => serve(disk, home, false),
        ["serve", disk, home, "linux"] => serve(disk, home, true),
        _ => eprintln!("usage: realcheck scan <root> <disk-file> | engine|serve <disk-file> <home> [linux]"),
    }
}
