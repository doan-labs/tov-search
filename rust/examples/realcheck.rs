//! Checks the Tov app against this one on a real disk.
//!
//!   realcheck scan <root> <disk-file>          scan like the daemon, write a disk file
//!   realcheck engine <disk-file> <home> [linux] bench/PROTOCOL.md, ranked like the daemon
//!
//! A folder the scan did not enter (a mount point, or skipped) carries
//! FLAG_MOUNT in the disk file and gets no block, as in the daemon's index.

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

fn engine(path: &str, home: &str, linux: bool) {
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
    NOW_OVERRIDE.store(now, Ordering::Relaxed);
    let idx = if linux { Index::build_ranked(ls, 0, now, home.as_bytes(), linux_prior_adjust) } else { Index::build(ls, 0, now, home.as_bytes()) };
    let live = Live::new(idx);
    let s = Searcher { live: &live };
    let mut out = std::io::BufWriter::with_capacity(1 << 16, std::io::stdout().lock());
    out.write_all(b"ready\n").unwrap();
    out.flush().unwrap();
    let mut p = Vec::new();
    for line in std::io::stdin().lock().lines() {
        let line = line.expect("stdin");
        match Query::parse(&line, home) {
            Err(_) => out.write_all(b"error\n").unwrap(),
            Ok(q) => {
                for h in s.search(&q) {
                    live.base.path(h.idx as usize, &mut p);
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

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    match a.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["scan", root, out] => scan(root, out),
        ["engine", disk, home] => engine(disk, home, false),
        ["engine", disk, home, "linux"] => engine(disk, home, true),
        _ => eprintln!("usage: realcheck scan <root> <disk-file> | engine <disk-file> <home> [linux]"),
    }
}
