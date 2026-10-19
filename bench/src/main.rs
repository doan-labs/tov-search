//! The tov-search benchmark harness. FROZEN: the optimization loop may not edit
//! bench/. It drives any engine binary through bench/PROTOCOL.md, so the
//! engine can be written in any language.
//!
//!   bench gen                  write the synthetic disk (bench/data/disk.bin)
//!   bench run <trace>          time typing every query of bench/traces/<trace>.txt
//!                              and check every keystroke against bench/oracle/<trace>.tsv
//!   bench ab <trace> <A> <B>   alternate runs of two engine binaries; who is faster
//!   bench oracle <trace>       (re)write the oracle from the current engine
//!   bench stats                shape of the disk (compare with a real one)
//!
//! Options: --engine <bin> (default bin/engine), --disk <file> (default
//! bench/data/disk.bin, or $BENCH_DATA/disk.bin), --rounds <n> (timed rounds
//! after one warmup, default 5), --pairs <n> (ab, default 5), --no-oracle.

mod disk;
mod generate;
mod stats;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const BENCH_DIR: &str = env!("CARGO_MANIFEST_DIR");
/// A whole run (startup + every round) must finish within this.
const RUN_TIMEOUT: Duration = Duration::from_secs(20 * 60);

fn die(msg: &str) -> ! {
    eprintln!("bench: {msg}");
    std::process::exit(2)
}

fn bench_dir() -> &'static Path {
    Path::new(BENCH_DIR)
}

fn data_dir() -> PathBuf {
    std::env::var_os("BENCH_DATA").map_or_else(|| bench_dir().join("data"), PathBuf::from)
}

struct Args {
    cmd: String,
    pos: Vec<String>,
    engine: PathBuf,
    disk: PathBuf,
    rounds: usize,
    pairs: usize,
    oracle: bool,
}

fn args() -> Args {
    let mut a = Args {
        cmd: String::new(),
        pos: Vec::new(),
        engine: bench_dir().join("../bin/engine"),
        disk: data_dir().join("disk.bin"),
        rounds: 5,
        pairs: 5,
        oracle: true,
    };
    let mut it = std::env::args().skip(1);
    let num = |v: Option<String>, what: &str| {
        v.and_then(|r| r.parse().ok()).filter(|&r| r > 0).unwrap_or_else(|| die(&format!("{what} needs a number")))
    };
    while let Some(x) = it.next() {
        match x.as_str() {
            "--engine" => a.engine = it.next().unwrap_or_else(|| die("--engine needs a path")).into(),
            "--disk" => a.disk = it.next().unwrap_or_else(|| die("--disk needs a path")).into(),
            "--rounds" => a.rounds = num(it.next(), "--rounds"),
            "--pairs" => a.pairs = num(it.next(), "--pairs"),
            "--no-oracle" => a.oracle = false,
            _ if a.cmd.is_empty() => a.cmd = x,
            _ => a.pos.push(x),
        }
    }
    a
}

fn main() {
    let a = args();
    match a.cmd.as_str() {
        "gen" => cmd_gen(&a.disk),
        "run" => cmd_run(&a),
        "ab" => cmd_ab(&a),
        "oracle" => cmd_oracle(&a),
        "stats" => stats::print(&disk::parse(&read(&a.disk)).unwrap_or_else(|e| die(&e)).0),
        _ => die(
            "usage: bench gen | run <trace> | ab <trace> <engineA> <engineB> | oracle <trace> | stats   [--engine bin] [--disk f] [--rounds n] [--pairs n] [--no-oracle]",
        ),
    }
}

fn read(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap_or_else(|e| die(&format!("{}: {e} (run `bench gen` first)", p.display())))
}

/// The committed fingerprint of the synthetic disk: `<digest hex> <entries>`.
fn expected() -> Option<String> {
    std::fs::read_to_string(bench_dir().join("DATASET")).ok().map(|s| s.trim().to_string())
}

fn fingerprint(b: &[u8]) -> String {
    format!("{:016x} {}", disk::digest(b), u64::from_le_bytes(b[8..16].try_into().unwrap()))
}

fn cmd_gen(out: &Path) {
    let t = Instant::now();
    let b = generate::disk();
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    std::fs::write(out, &b).unwrap();
    let fp = fingerprint(&b);
    println!("wrote {} ({} MB) in {:.1}s, fingerprint {fp}", out.display(), b.len() >> 20, t.elapsed().as_secs_f64());
    match expected() {
        Some(e) if e != fp => {
            die(&format!("fingerprint differs from bench/DATASET ({e}): the generator is not deterministic here"))
        }
        Some(_) => println!("matches bench/DATASET"),
        None => println!("no bench/DATASET yet"),
    }
}

/// Check the default disk is the committed one (other --disk files are the
/// caller's business).
fn check_disk(a: &Args) {
    if !a.disk.exists() {
        die(&format!("{} missing; run `bench gen`", a.disk.display()));
    }
    if a.disk == data_dir().join("disk.bin")
        && let Some(e) = expected()
    {
        let fp = fingerprint(&read(&a.disk));
        if fp != e {
            die(&format!("{} fingerprint {fp} != bench/DATASET {e}; rerun `bench gen`", a.disk.display()));
        }
    }
}

/// Every keystroke of typing each query: "ab c" -> "a", "ab", "ab ", "ab c".
fn keystrokes(trace: &str) -> Vec<String> {
    let path = bench_dir().join("traces").join(format!("{trace}.txt"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| die(&format!("{}: {e}", path.display())));
    let mut out = Vec::new();
    for q in text.lines().map(str::trim_end).filter(|l| !l.trim().is_empty() && !l.starts_with('#')) {
        for (i, c) in q.char_indices() {
            out.push(q[..i + c.len_utf8()].to_string());
        }
    }
    out
}

struct Run {
    /// Median µs per keystroke over the timed rounds.
    med: Vec<f64>,
    /// Every keystroke's response (from the warmup round).
    seen: Vec<String>,
    /// Keystrokes whose response changed between rounds.
    unstable: usize,
    startup_s: f64,
    peak_rss_mb: f64,
}

impl Run {
    fn metric(&self) -> f64 {
        self.med.iter().sum::<f64>() / 1e3
    }
}

/// Start the engine, wait for "ready", type every keystroke `rounds + 1`
/// times (the first round is warmup), shut it down.
fn run_engine(engine: &Path, disk: &Path, keys: &[String], rounds: usize) -> Run {
    let t = Instant::now();
    let mut child = Command::new(engine)
        .arg(disk)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| die(&format!("cannot start {}: {e} (run ./build.sh)", engine.display())));
    let pid = child.id() as i32;
    let done = Arc::new(AtomicBool::new(false));
    let d = done.clone();
    std::thread::spawn(move || {
        let start = Instant::now();
        while start.elapsed() < RUN_TIMEOUT {
            std::thread::sleep(Duration::from_millis(200));
            if d.load(Ordering::Relaxed) {
                return;
            }
        }
        eprintln!("bench: engine took longer than {RUN_TIMEOUT:?}, killing it");
        unsafe { libc::kill(pid, libc::SIGKILL) };
    });

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::with_capacity(1 << 16, child.stdout.take().unwrap());
    let mut line = Vec::new();
    let mut next_line = |line: &mut Vec<u8>| {
        line.clear();
        if stdout.read_until(b'\n', line).unwrap_or(0) == 0 {
            die("engine exited early (crash? see its stderr above)");
        }
    };
    next_line(&mut line);
    if line != b"ready\n" {
        die(&format!("engine said {:?} instead of \"ready\"", String::from_utf8_lossy(&line)));
    }
    let startup_s = t.elapsed().as_secs_f64();

    let mut us = vec![Vec::with_capacity(rounds); keys.len()];
    let mut seen = vec![String::new(); keys.len()];
    let mut unstable = 0;
    let mut resp = Vec::new();
    for round in 0..=rounds {
        for (k, q) in keys.iter().enumerate() {
            resp.clear();
            let t = Instant::now();
            stdin
                .write_all(q.as_bytes())
                .and_then(|_| stdin.write_all(b"\n"))
                .and_then(|_| stdin.flush())
                .unwrap_or_else(|_| die("engine closed stdin"));
            loop {
                next_line(&mut line);
                if line == b"\n" {
                    break;
                }
                resp.extend_from_slice(&line);
            }
            let el = t.elapsed().as_secs_f64() * 1e6;
            let text = String::from_utf8_lossy(&resp).into_owned();
            if round == 0 {
                seen[k] = text;
            } else {
                us[k].push(el);
                if seen[k] != text {
                    unstable += 1;
                }
            }
        }
    }
    drop(stdin);
    let mut status = 0;
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::wait4(pid, &mut status, 0, &mut ru) };
    done.store(true, Ordering::Relaxed);
    // ru_maxrss: bytes on macOS, KiB on Linux.
    let unit = if cfg!(target_os = "macos") { 1.0 } else { 1024.0 };
    let med = us
        .into_iter()
        .map(|mut v| {
            v.sort_by(f64::total_cmp);
            v.get(v.len() / 2).copied().unwrap_or(0.0)
        })
        .collect();
    Run { med, seen, unstable, startup_s, peak_rss_mb: ru.ru_maxrss as f64 * unit / (1 << 20) as f64 }
}

fn oracle_path(trace: &str) -> PathBuf {
    bench_dir().join("oracle").join(format!("{trace}.tsv"))
}

/// Oracle file: "@@ <query>" then that keystroke's response lines.
fn write_oracle(trace: &str, keys: &[String], seen: &[String]) {
    let mut out = String::new();
    for (q, text) in keys.iter().zip(seen) {
        out += &format!("@@ {q}\n{text}");
    }
    let p = oracle_path(trace);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, out).unwrap();
    println!("wrote {} ({} keystrokes)", p.display(), keys.len());
}

/// Keystrokes whose response differs from the oracle.
fn check_oracle(trace: &str, keys: &[String], seen: &[String]) -> Vec<usize> {
    let p = oracle_path(trace);
    let o = std::fs::read_to_string(&p).unwrap_or_else(|e| die(&format!("{}: {e}", p.display())));
    let mut blocks: Vec<(&str, String)> = Vec::new();
    for l in o.split_inclusive('\n') {
        match l.strip_prefix("@@ ") {
            Some(q) => blocks.push((q.trim_end_matches('\n'), String::new())),
            None => blocks.last_mut().unwrap_or_else(|| die("bad oracle file")).1.push_str(l),
        }
    }
    if blocks.len() != keys.len() || blocks.iter().zip(keys).any(|(b, q)| b.0 != q) {
        die("oracle does not match this trace's keystrokes");
    }
    (0..keys.len()).filter(|&k| blocks[k].1 != seen[k]).collect()
}

fn pct(med: &[f64], q: usize) -> f64 {
    let mut s = med.to_vec();
    s.sort_by(f64::total_cmp);
    s[(s.len() - 1) * q / 100] / 1e3
}

fn trace_arg(a: &Args) -> &str {
    a.pos.first().map(String::as_str).unwrap_or_else(|| die("which trace? (dev, holdout)"))
}

fn cmd_oracle(a: &Args) {
    check_disk(a);
    let trace = trace_arg(a);
    let keys = keystrokes(trace);
    let r = run_engine(&a.engine, &a.disk, &keys, 1);
    if r.unstable > 0 {
        die("engine answers differ between rounds; not writing an oracle");
    }
    write_oracle(trace, &keys, &r.seen);
}

fn cmd_run(a: &Args) {
    check_disk(a);
    let trace = trace_arg(a);
    let keys = keystrokes(trace);
    let r = run_engine(&a.engine, &a.disk, &keys, a.rounds);
    let bad = if a.oracle { check_oracle(trace, &keys, &r.seen) } else { Vec::new() };

    let out_dir = bench_dir().join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let mut tsv = String::from("keystroke\tmedian_us\thits\toracle\n");
    for (k, q) in keys.iter().enumerate() {
        let ok = if !a.oracle {
            "-"
        } else if bad.contains(&k) {
            "FAIL"
        } else {
            "ok"
        };
        tsv += &format!("{q}\t{:.0}\t{}\t{ok}\n", r.med[k], r.seen[k].lines().count());
    }
    std::fs::write(out_dir.join(format!("{trace}.tsv")), tsv).unwrap();

    println!("metric: {:.3}", r.metric());
    println!(
        "p50_ms: {:.3}  p90_ms: {:.3}  p99_ms: {:.3}  max_ms: {:.3}",
        pct(&r.med, 50),
        pct(&r.med, 90),
        pct(&r.med, 99),
        pct(&r.med, 100)
    );
    println!("peak_rss_mb: {:.0}  startup_s: {:.2}", r.peak_rss_mb, r.startup_s);
    println!(
        "keystrokes: {}  rounds: {}  cpus: {}",
        keys.len(),
        a.rounds,
        std::thread::available_parallelism().map_or(0, |n| n.get())
    );
    if !a.oracle {
        println!("oracle: SKIPPED");
    } else if bad.is_empty() && r.unstable == 0 {
        println!("oracle: PASS");
    } else {
        println!("oracle: FAIL {} keystrokes differ, {} unstable across rounds", bad.len(), r.unstable);
        for &k in bad.iter().take(5) {
            eprintln!("  differs: {:?}  (bench/out/{trace}.tsv has every keystroke)", keys[k]);
        }
        std::process::exit(1);
    }
}

/// Alternate whole runs A, B, A, B, ... so machine drift hits both alike.
fn cmd_ab(a: &Args) {
    check_disk(a);
    let [trace, ea, eb] =
        [0, 1, 2].map(|i| a.pos.get(i).cloned().unwrap_or_else(|| die("usage: bench ab <trace> <engineA> <engineB>")));
    let keys = keystrokes(&trace);
    let (mut ma, mut mb, mut wins) = (Vec::new(), Vec::new(), 0);
    for i in 0..a.pairs {
        let ra = run_engine(Path::new(&ea), &a.disk, &keys, a.rounds);
        let rb = run_engine(Path::new(&eb), &a.disk, &keys, a.rounds);
        for (name, r) in [("A", &ra), ("B", &rb)] {
            if a.oracle && (!check_oracle(&trace, &keys, &r.seen).is_empty() || r.unstable > 0) {
                println!("oracle: FAIL ({name})");
                std::process::exit(1);
            }
        }
        wins += (rb.metric() < ra.metric()) as usize;
        println!(
            "pair {}: A {:.3}  B {:.3}  rss A {:.0} B {:.0} MB  startup A {:.2} B {:.2} s",
            i + 1,
            ra.metric(),
            rb.metric(),
            ra.peak_rss_mb,
            rb.peak_rss_mb,
            ra.startup_s,
            rb.startup_s
        );
        ma.push(ra.metric());
        mb.push(rb.metric());
    }
    let median = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let (a_med, b_med) = (median(&mut ma), median(&mut mb));
    println!("A median: {a_med:.3}");
    println!("B median: {b_med:.3}");
    println!(
        "change: {:+.2}%  (negative = B faster)  B faster in {wins}/{} pairs",
        (b_med / a_med - 1.0) * 100.0,
        a.pairs
    );
    println!("oracle: {}", if a.oracle { "PASS" } else { "SKIPPED" });
}
