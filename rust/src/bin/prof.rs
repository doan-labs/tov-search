//! Per-keystroke profiler: same protocol inputs as `engine`, but reports
//! where the time goes. Usage: prof <disk-file> <keys-file> [rounds]
//! Prints per-keystroke median µs, cold-cache (fresh score_names) µs, and
//! the total metric approximation — for profiling only, not the bench.

use tov_search::index::Index;
use tov_search::live::Live;
use tov_search::query::{NOW_OVERRIDE, Query, Searcher};
use tov_search::walk::{KIND_DIR, Listing, NONE, RawEnt};
use std::io::Read;
use std::sync::atomic::Ordering;
use std::time::Instant;

const HOME: &str = "/Users/bench";

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
    let mut a = std::env::args().skip(1);
    let disk = a.next().expect("usage: prof <disk> <keys> [rounds]");
    let keysf = a.next().expect("keys file");
    let rounds: usize = a.next().map_or(3, |r| r.parse().unwrap());
    let b = std::fs::read(&disk).expect("read disk file");
    let (ls, now) = listings(&b);
    drop(b);
    NOW_OVERRIDE.store(now, Ordering::Relaxed);
    let live = Live::new(Index::build(ls, 0, now, HOME.as_bytes()));
    let s = Searcher { live: &live };
    let mut kf = String::new();
    std::fs::File::open(&keysf).unwrap().read_to_string(&mut kf).unwrap();
    let keys: Vec<&str> = kf.lines().collect();

    let mut warm: Vec<Vec<u128>> = vec![Vec::with_capacity(rounds); keys.len()];
    let mut cold: Vec<Vec<u128>> = vec![Vec::with_capacity(rounds); keys.len()];
    let (mut names_us, mut scan_us, mut memo_us): (Vec<Vec<u64>>, Vec<Vec<u64>>, Vec<Vec<u64>>) =
        (vec![Vec::with_capacity(rounds); keys.len()], vec![Vec::with_capacity(rounds); keys.len()], vec![Vec::with_capacity(rounds); keys.len()]);
    let mut parse_us = 0u128;
    // Warmup round (like bench's untimed first round).
    for q in &keys {
        let Ok(q) = Query::parse(q, HOME) else { continue };
        s.search(&q);
    }
    for r in 0..rounds {
        for (k, qstr) in keys.iter().enumerate() {
            let t = Instant::now();
            let Ok(q) = Query::parse(qstr, HOME) else { continue };
            let tp = t.elapsed().as_micros();
            parse_us += tp;
            let t = Instant::now();
            let (n0, s0, m0) = (
                tov_search::query::PROF_NAMES_NS.load(Ordering::Relaxed),
                tov_search::query::PROF_SCAN_NS.load(Ordering::Relaxed),
                tov_search::query::PROF_MEMO_NS.load(Ordering::Relaxed),
            );
            s.search(&q);
            names_us[k].push(tov_search::query::PROF_NAMES_NS.load(Ordering::Relaxed) - n0);
            scan_us[k].push(tov_search::query::PROF_SCAN_NS.load(Ordering::Relaxed) - s0);
            memo_us[k].push(tov_search::query::PROF_MEMO_NS.load(Ordering::Relaxed) - m0);
            warm[k].push(t.elapsed().as_micros());
        }
        if r == 0 {
            // One cold-cache pass per keystroke on round 0 only (slow).
            for (k, qstr) in keys.iter().enumerate() {
                live.names_cache.clear();
                live.posneg_cache.clear();
                let Ok(q) = Query::parse(qstr, HOME) else { continue };
                let t = Instant::now();
                s.search(&q);
                cold[k].push(t.elapsed().as_micros());
            }
        }
    }
    let med = |v: &mut Vec<u128>| -> u128 {
        if v.is_empty() {
            return 0;
        }
        v.sort_unstable();
        v[v.len() / 2]
    };
    let med_u64 = |v: &mut Vec<u64>| -> u64 {
        if v.is_empty() {
            return 0;
        }
        v.sort_unstable();
        v[v.len() / 2]
    };
    let mut rows: Vec<(usize, u128, u128)> = Vec::new();
    let mut ph: Vec<(usize, u64, u64, u64)> = Vec::new();
    let (mut tot_w, mut tot_c) = (0u128, 0u128);
    for k in 0..keys.len() {
        let (w, c) = (med(&mut warm[k]), med(&mut cold[k]));
        tot_w += w;
        tot_c += c;
        rows.push((k, w, c));
        ph.push((k, med_u64(&mut names_us[k]), med_u64(&mut scan_us[k]), med_u64(&mut memo_us[k])));
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.1));
    ph.sort_by_key(|r| std::cmp::Reverse(r.1 + r.2));
    println!("keystrokes: {}  rounds: {}  warm-total: {:.1} ms  cold-total: {:.1} ms  parse-total: {:.1} ms", keys.len(), rounds, tot_w as f64 / 1e3, tot_c as f64 / 1e3, parse_us as f64 / 1e3);
    println!(
        "phases: names {:.1} ms  scan {:.1} ms (of which memo {:.1} ms)",
        tov_search::query::PROF_NAMES_NS.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1e6,
        tov_search::query::PROF_SCAN_NS.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1e6,
        tov_search::query::PROF_MEMO_NS.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1e6
    );
    println!("top warm keystrokes:");
    for (k, w, c) in rows.iter().take(60) {
        println!("  {:>7.1} us  cold {:>7.1}  k={:>3} {:?}", *w as f64, *c as f64, k, keys[*k]);
    }
    println!("top keystrokes by phase (ns, names+scan):");
    for (k, n, sc, m) in ph.iter().take(50) {
        println!("  k={:>3} {:<28} names {:>8.1} us  scan {:>8.1} us  memo {:>8.1} us", k, format!("{:?}", keys[*k]), *n as f64 / 1e3, *sc as f64 / 1e3, *m as f64 / 1e3);
    }
    // Every keystroke, for grouping by shape: k, warm us, names/scan/memo us, query.
    if let Some(path) = std::env::var_os("PROF_TSV") {
        let mut out = String::new();
        let warm: std::collections::HashMap<usize, u128> = rows.iter().map(|r| (r.0, r.1)).collect();
        for (k, n, sc, m) in &ph {
            out += &format!("{}\t{}\t{}\t{}\t{}\t{}\n", k, warm[k], n / 1000, sc / 1000, m / 1000, keys[*k]);
        }
        std::fs::write(path, out).unwrap();
    }
}
