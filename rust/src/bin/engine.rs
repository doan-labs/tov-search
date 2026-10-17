//! The benchmark engine: speaks bench/PROTOCOL.md over stdin/stdout.
//!
//!   engine <disk-file>   index the disk file, print "ready", then answer one
//!                        query per stdin line until EOF.

use tov_search::index::Index;
use tov_search::live::Live;
use tov_search::query::{NOW_OVERRIDE, Query, Searcher};
use tov_search::walk::{KIND_DIR, Listing, NONE, RawEnt};
use std::io::{BufRead, Write};
use std::sync::atomic::Ordering;

/// What `~` means in queries, and the home folder ranking favors.
const HOME: &str = "/Users/bench";

fn rd<const N: usize>(b: &[u8], at: usize) -> [u8; N] {
    b[at..at + N].try_into().unwrap()
}

/// Disk file -> one listing per directory (keyed by its entry index), and
/// the pinned clock.
fn listings(b: &[u8]) -> (Vec<Listing>, u32) {
    assert!(b.len() >= 20 && &b[..8] == b"FSDISK01", "not a disk file");
    let n = u64::from_le_bytes(rd(b, 8)) as usize;
    let now = u32::from_le_bytes(rd(b, 16));
    let mut slot = vec![NONE; n];
    let mut ls: Vec<Listing> = vec![Listing { id: 0, names: Vec::new(), ents: Vec::new() }];
    slot[0] = 0;
    let mut p = 20 + 19; // past the root record (empty name)
    for i in 1..n {
        let parent = u32::from_le_bytes(rd(b, p)) as usize;
        let kind = b[p + 4];
        let len = u16::from_le_bytes(rd(b, p + 17)) as usize;
        let name = &b[p + 19..p + 19 + len];
        let child = if kind & 3 == KIND_DIR {
            slot[i] = ls.len() as u32;
            ls.push(Listing { id: i as u32, names: Vec::new(), ents: Vec::new() });
            i as u32
        } else {
            NONE
        };
        let l = &mut ls[slot[parent] as usize];
        let (size, mtime) = (u64::from_le_bytes(rd(b, p + 5)), u32::from_le_bytes(rd(b, p + 13)));
        l.ents.push(RawEnt { name_off: l.names.len() as u32, name_len: len as u16, kind, size, mtime, child });
        l.names.extend_from_slice(name);
        p += 19 + len;
    }
    (ls, now)
}

/// A dedicated pool for searches, equivalent to the daemon's
/// `engine::search_pool` (its own threads, USER_INTERACTIVE on macOS), for
/// measuring adapter scheduling against the daemon's. Opt-in only:
/// `BF_SEARCH_POOL=daemon` selects it, anything else keeps the historical
/// behavior of searching on the calling thread's rayon context. The pool
/// only decides where the query's work runs, never what it answers.
fn daemon_pool() -> Option<rayon::ThreadPool> {
    std::env::var("BF_SEARCH_POOL").ok().filter(|v| v == "daemon")?;
    rayon::ThreadPoolBuilder::new()
        .thread_name(|i| format!("tov-search-search-{i}"))
        .start_handler(|_| {
            #[cfg(target_os = "macos")]
            unsafe {
                libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
            };
        })
        .build()
        .ok()
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: engine <disk-file>");
    let b = std::fs::read(&path).expect("read disk file");
    let (ls, now) = listings(&b);
    drop(b);
    NOW_OVERRIDE.store(now, Ordering::Relaxed);
    let live = Live::new(Index::build(ls, 0, now, HOME.as_bytes()));
    let s = Searcher { live: &live };
    let pool = daemon_pool();

    let mut out = std::io::BufWriter::with_capacity(1 << 16, std::io::stdout().lock());
    out.write_all(b"ready\n").unwrap();
    out.flush().unwrap();
    let mut p = Vec::new();
    for line in std::io::stdin().lock().lines() {
        let line = line.expect("stdin");
        match Query::parse(&line, HOME) {
            Err(_) => out.write_all(b"error\n").unwrap(),
            Ok(q) => {
                let hits = match &pool {
                    Some(pool) => pool.install(|| s.search(&q)),
                    None => s.search(&q),
                };
                for h in hits {
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
