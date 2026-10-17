//! fff behind rust/bench/PROTOCOL.md. fff crawls a real directory, so this
//! takes the directory instead of a disk file.
//!
//!   fff-engine <dir>

use fff_search::{FilePicker, FilePickerOptions, FuzzySearchOptions, PaginationArgs, QueryParser};
use std::io::{BufRead, Write};

fn main() {
    let dir = std::env::args().nth(1).expect("usage: fff-engine <dir>");
    let dir = std::fs::canonicalize(dir).expect("directory");
    let mut picker = FilePicker::new(FilePickerOptions {
        base_path: dir.to_str().expect("UTF-8 path").into(),
        enable_mmap_cache: false,
        enable_content_indexing: false,
        watch: false,
        ..Default::default()
    })
    .expect("picker");
    picker.collect_files().expect("scan");
    // fff skips hidden and gitignored files and indexes no directories.
    eprintln!("fff-engine: {} files indexed", picker.live_file_count());
    let parser = QueryParser::default();
    let mut out = std::io::BufWriter::with_capacity(1 << 16, std::io::stdout().lock());
    out.write_all(b"ready\n").unwrap();
    out.flush().unwrap();
    for line in std::io::stdin().lock().lines() {
        let line = line.expect("stdin");
        let q = parser.parse(&line);
        let options = FuzzySearchOptions { pagination: PaginationArgs { offset: 0, limit: 50 }, ..Default::default() };
        let r = picker.fuzzy_search(&q, None, options);
        for (f, s) in r.items.iter().zip(&r.scores) {
            writeln!(out, "{}\t{}", s.total, f.relative_path(&picker)).unwrap();
        }
        out.write_all(b"\n").unwrap();
        out.flush().unwrap();
    }
}
