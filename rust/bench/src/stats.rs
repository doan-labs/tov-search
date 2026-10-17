//! The shape of a disk file, as aggregates only (no names): compare a
//! synthetic disk against a real one to keep the generator honest.

use crate::disk::{Ent, FLAG_HIDDEN, KIND_DIR, KIND_FILE, KIND_LINK};
use std::collections::HashMap;

/// Folder names whose subtrees dominate real disks.
const MARKS: &[&str] =
    &["node_modules", ".git", "Library", "Caches", "site-packages", "target", "DerivedData", ".cache", "Containers", "Pods", "build", "dist", "Frameworks"];

/// Query fragments whose match rate drives search cost.
pub const PROBES: &[&str] = &["a", "s", "p", "m", "in", "pa", "ma", "re", "con", "src", "main", "test", "index", "png", "json", "lib", "readme"];

fn pct(v: &mut [u32], p: &[usize]) -> String {
    v.sort_unstable();
    p.iter().map(|&p| format!("p{p}={}", v[(v.len() - 1) * p / 100])).collect::<Vec<_>>().join(" ")
}

/// Case-insensitive subsequence: what a fuzzy token needs to match at all.
pub fn subseq(name: &[u8], q: &[u8]) -> bool {
    let mut j = 0;
    for &b in name {
        if j < q.len() && b.to_ascii_lowercase() == q[j] {
            j += 1;
        }
    }
    j == q.len()
}

pub fn print(ents: &[Ent]) {
    let n = ents.len();
    let f = |k: usize| format!("{:.1}%", 100.0 * k as f64 / n as f64);
    let count = |pred: &dyn Fn(&Ent) -> bool| ents.iter().filter(|e| pred(e)).count();
    let dirs = count(&|e| e.kind & 3 == KIND_DIR);
    println!("entries {n}  dirs {dirs} ({})  files {}  links {}  hidden-flag {}", f(dirs), f(count(&|e| e.kind & 3 == KIND_FILE)), f(count(&|e| e.kind & 3 == KIND_LINK)), f(count(&|e| e.kind & FLAG_HIDDEN != 0)));

    let mut uniq: HashMap<&[u8], u32> = HashMap::with_capacity(n / 3);
    for e in &ents[1..] {
        *uniq.entry(e.name).or_default() += 1;
    }
    let mut mult: Vec<u32> = uniq.values().copied().collect();
    let bucket = |lo: u32, hi: u32| mult.iter().filter(|&&c| c >= lo && c <= hi).count();
    let (b1, b2, b3, b4) = (bucket(1, 1), bucket(2, 9), bucket(10, 99), bucket(100, u32::MAX));
    let bytes: usize = uniq.keys().map(|k| k.len()).sum();
    println!("distinct names {} ({:.2} per entry, {} MB)  used once {} | 2-9 {} | 10-99 {} | 100+ {}", uniq.len(), uniq.len() as f64 / n as f64, bytes >> 20, b1, b2, b3, b4);
    println!("most repeated name counts: {}", pct(&mut mult, &[50, 90, 99, 100]));
    mult.sort_unstable_by(|a, b| b.cmp(a));
    println!("top 10 repeat counts: {:?}", &mult[..10.min(mult.len())]);

    let mut depth = vec![0u32; n];
    let mut kids = vec![0u32; n];
    let mut marks = vec![0u16; n];
    for i in 1..n {
        let p = ents[i].parent as usize;
        depth[i] = depth[p] + 1;
        kids[p] += 1;
        let own = MARKS.iter().position(|m| m.as_bytes() == ents[i].name).map_or(0, |k| 1 << k);
        marks[i] = marks[p] | if ents[i].kind & 3 == KIND_DIR { own } else { 0 };
    }
    println!("depth: {}", pct(&mut depth.clone(), &[10, 50, 90, 99, 100]));
    let mut dk: Vec<u32> = (0..n).filter(|&i| ents[i].kind & 3 == KIND_DIR || i == 0).map(|i| kids[i]).collect();
    let empty = dk.iter().filter(|&&k| k == 0).count();
    println!("children per dir: {}  empty dirs {:.1}%", pct(&mut dk, &[50, 90, 99, 100]), 100.0 * empty as f64 / dk.len() as f64);
    let mut lens: Vec<u32> = ents[1..].iter().map(|e| e.name.len() as u32).collect();
    println!("name length: {}", pct(&mut lens, &[10, 50, 90, 99, 100]));

    let has = |pred: &dyn Fn(&[u8]) -> bool| f(ents.iter().filter(|e| pred(e.name)).count());
    println!(
        "names with: upper {}  digit {}  space {}  -_ {}  leading dot {}  non-ascii {}  no ext {}",
        has(&|s| s.iter().any(u8::is_ascii_uppercase)),
        has(&|s| s.iter().any(u8::is_ascii_digit)),
        has(&|s| s.contains(&b' ')),
        has(&|s| s.iter().any(|&b| b == b'-' || b == b'_')),
        has(&|s| s.first() == Some(&b'.')),
        has(&|s| s.iter().any(|&b| b >= 0x80)),
        has(&|s| !s[1.min(s.len())..].contains(&b'.')),
    );

    let mut exts: HashMap<Vec<u8>, usize> = HashMap::new();
    for e in &ents[1..] {
        if let Some(d) = e.name.iter().rposition(|&b| b == b'.').filter(|&d| d > 0 && e.name.len() - d <= 12) {
            *exts.entry(e.name[d + 1..].to_ascii_lowercase()).or_default() += 1;
        }
    }
    let mut ev: Vec<_> = exts.into_iter().collect();
    ev.sort_by(|a, b| b.1.cmp(&a.1));
    println!("top extensions: {}", ev.iter().take(30).map(|(e, c)| format!("{} {}", String::from_utf8_lossy(e), f(*c))).collect::<Vec<_>>().join(", "));

    let mut tops: HashMap<&[u8], usize> = HashMap::new();
    let mut top_of = vec![0u32; n];
    for i in 1..n {
        top_of[i] = if depth[i] == 1 { i as u32 } else { top_of[ents[i].parent as usize] };
        *tops.entry(ents[top_of[i] as usize].name).or_default() += 1;
    }
    let mut tv: Vec<_> = tops.into_iter().collect();
    tv.sort_by(|a, b| b.1.cmp(&a.1));
    println!("under /: {}", tv.iter().take(12).map(|(t, c)| format!("{} {}", String::from_utf8_lossy(t), f(*c))).collect::<Vec<_>>().join(", "));
    println!(
        "inside folder named: {}",
        MARKS.iter().enumerate().map(|(k, m)| format!("{m} {}", f(marks.iter().filter(|&&x| x & (1 << k) != 0).count()))).collect::<Vec<_>>().join(", ")
    );

    let names: Vec<&[u8]> = uniq.keys().copied().collect();
    let line: Vec<String> = PROBES
        .iter()
        .map(|p| {
            let ents_hit = ents[1..].iter().filter(|e| subseq(e.name, p.as_bytes())).count();
            let names_hit = names.iter().filter(|nm| subseq(nm, p.as_bytes())).count();
            format!("{p} {}/{:.1}%", f(ents_hit), 100.0 * names_hit as f64 / names.len() as f64)
        })
        .collect();
    println!("fuzzy match rate (entries/distinct names): {}", line.join(", "));
}
