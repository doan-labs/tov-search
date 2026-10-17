//! Scratch analysis: stat histogram + scope sizes. Not part of the engine.
use tov_search::index::Index;
use tov_search::live::Live;
use tov_search::walk::{KIND_DIR, Listing, NONE, RawEnt};

fn rd<const N: usize>(b: &[u8], at: usize) -> [u8; N] {
    b[at..at + N].try_into().unwrap()
}

fn listings(b: &[u8]) -> (Vec<Listing>, u32) {
    assert!(b.len() >= 20 && &b[..8] == b"FSDISK01", "not a disk file");
    let n = u64::from_le_bytes(rd(b, 8)) as usize;
    let now = u32::from_le_bytes(rd(b, 16));
    let mut slot = vec![NONE; n];
    let mut ls: Vec<Listing> = vec![Listing { id: 0, names: Vec::new(), ents: Vec::new() }];
    slot[0] = 0;
    let mut p = 20 + 19;
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

fn main() {
    let b = std::fs::read(std::env::args().nth(1).unwrap()).unwrap();
    let (ls, now) = listings(&b);
    drop(b);
    tov_search::query::NOW_OVERRIDE.store(now, std::sync::atomic::Ordering::Relaxed);
    let live = Live::new(Index::build(ls, 0, now, b"/Users/bench"));
    let idx = &live.base;
    println!("n={} u={} d={}", idx.n, idx.u, idx.d);
    // stat histogram, buckets of 5
    let mut hist = std::collections::BTreeMap::new();
    for &s in &live.stat {
        *hist.entry(s / 5 * 5).or_insert(0usize) += 1;
    }
    let mut cum = 0usize;
    println!("stat_hi_bucket\tcount\tcum_from_top");
    let keys: Vec<i32> = hist.keys().copied().collect();
    for k in keys.iter().rev() {
        cum += hist[&k];
        println!("{k}\t{}\t{cum}", hist[&k]);
    }
    // scope sizes
    for scope in ["/Users/bench", "/Users/bench/Developer"] {
        if let Some(e) = idx.lookup(scope.as_bytes()) {
            if let Some(d) = idx.dir_of(e) {
                let (a, b) = (idx.dir_start()[d as usize], idx.dir_end()[d as usize]);
                println!("scope {scope}: entries {}..{} ({})", a, b, b - a);
            }
        }
    }
    // stat range + quantiles
    let mut v = live.stat.clone();
    v.sort_unstable();
    for q in [50.0, 90.0, 99.0, 99.9, 100.0] {
        let i = ((v.len() - 1) as f64 * q / 100.0) as usize;
        println!("p{q} stat = {}", v[i]);
    }
}
