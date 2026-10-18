mod server;

use tov_search::{index, live, query};

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Instant;

/// Big buffers (index builds, content batches) come straight from mmap and
/// go straight back with munmap. macOS's malloc keeps freed large blocks
/// mapped and dirty, which left the daemon at ~1 GB footprint after a build
/// while only ~2 MB was live.
struct Alloc;
const BIG: usize = 1 << 20;

unsafe impl GlobalAlloc for Alloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if l.size() < BIG || l.align() > 16384 {
            return unsafe { System.alloc(l) };
        }
        let p = unsafe { libc::mmap(std::ptr::null_mut(), l.size(), libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANON, -1, 0) };
        if p == libc::MAP_FAILED { std::ptr::null_mut() } else { p as *mut u8 }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        // Fresh anonymous pages are already zero.
        if l.size() < BIG || l.align() > 16384 { unsafe { System.alloc_zeroed(l) } } else { unsafe { self.alloc(l) } }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if l.size() < BIG || l.align() > 16384 {
            unsafe { System.dealloc(p, l) }
        } else {
            unsafe { libc::munmap(p as *mut libc::c_void, l.size()) };
        }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        if (l.size() < BIG && new_size < BIG) || l.align() > 16384 {
            return unsafe { System.realloc(p, l, new_size) };
        }
        let q = unsafe { self.alloc(Layout::from_size_align_unchecked(new_size, l.align())) };
        if !q.is_null() {
            unsafe {
                std::ptr::copy_nonoverlapping(p, q, l.size().min(new_size));
                self.dealloc(p, l);
            }
        }
        q
    }
}

#[global_allocator]
static GLOBAL: Alloc = Alloc;

#[cfg(target_os = "macos")]
const USAGE: &str = "usage:
  tov-search <query...> [--json]   search (starts the daemon if needed)
  tov-search stdio                 JSON lines on stdin/stdout
  tov-search serve                 run the daemon in the foreground
  tov-search status
  tov-search install [--login]     copy to ~/.local/bin; --login also starts the daemon at login
                                   (needs Full Disk Access granted to ~/.local/bin/tov-search)
  tov-search uninstall             remove the login agent (keeps the index)
  tov-search bench <query...>      time a query in-process against the saved index";

#[cfg(target_os = "linux")]
const USAGE: &str = "usage:
  tov-search <query...> [--json]   search (starts the daemon if needed)
  tov-search stdio                 JSON lines on stdin/stdout
  tov-search serve                 run the daemon in the foreground
  tov-search status
  tov-search install [--login]     copy to ~/.local/bin; --login also starts the daemon at login
                                   (a systemd user service)
  tov-search uninstall             remove the service (keeps the index)
  tov-search bench <query...>      time a query in-process against the saved index";

#[cfg(target_os = "macos")]
const LABEL: &str = "mt.nd.tov-search";

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".into())
}

fn data_dir() -> PathBuf {
    let d = tov_search::default_dir(&home());
    std::fs::create_dir_all(&d).ok();
    d
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn setiopolicy_np(iotype: i32, scope: i32, policy: i32) -> i32;
}

fn main() {
    // Never let a search download iCloud placeholders: opening or listing a
    // dataless file/dir fails fast instead of materializing it.
    // (IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES, IOPOL_SCOPE_PROCESS, OFF)
    #[cfg(target_os = "macos")]
    unsafe {
        setiopolicy_np(3, 0, 1)
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("-h" | "--help") => eprintln!("{USAGE}"),
        Some("serve") => server::serve(data_dir(), home()),
        Some("stdio") => stdio(),
        Some("status") => print_one(&serde_json::json!({"op": "status"}), true),
        Some("bench") => bench(&args[1..].join(" ")),
        Some("install") => install(args.iter().any(|a| a == "--login")),
        Some("uninstall") => uninstall(),
        Some(_) => {
            let json = args.iter().any(|a| a == "--json");
            let q: Vec<&str> = args.iter().map(String::as_str).filter(|a| *a != "--json").collect();
            print_one(&serde_json::json!({"q": q.join(" ")}), json);
        }
    }
}

fn print_one(req: &serde_json::Value, raw: bool) {
    let mut s = server::connect(&data_dir()).unwrap_or_else(|e| die(&format!("cannot reach daemon: {e}")));
    writeln!(s, "{req}").unwrap();
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line).unwrap();
    if raw {
        print!("{line}");
        return;
    }
    let v: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
    if v["ok"] != true {
        die(v["error"].as_str().unwrap_or("error"));
    }
    let mut out = std::io::stdout().lock();
    for h in v["hits"].as_array().into_iter().flatten() {
        let _ = writeln!(out, "{}", h["path"].as_str().unwrap_or(""));
    }
    for f in v["files"].as_array().into_iter().flatten() {
        for m in f["matches"].as_array().into_iter().flatten() {
            let _ = writeln!(out, "{}:{}: {}", f["path"].as_str().unwrap_or(""), m["line"], m["text"].as_str().unwrap_or("").trim());
        }
    }
}

fn stdio() {
    let s = server::connect(&data_dir()).unwrap_or_else(|e| die(&format!("cannot reach daemon: {e}")));
    let mut up = s.try_clone().unwrap();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if writeln!(up, "{line}").is_err() {
                break;
            }
        }
        let _ = up.shutdown(std::net::Shutdown::Write);
    });
    let mut out = std::io::stdout().lock();
    for line in BufReader::new(s).lines() {
        let Ok(line) = line else { break };
        if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
            break;
        }
    }
}

fn bench(qs: &str) {
    let dir = data_dir();
    let idx = index::Index::load(&dir.join("index.bin")).unwrap_or_else(|| die("no index yet; run tov-search serve"));
    let live = live::Live::new(idx);
    let home = home();
    let mut q = query::Query::parse(qs, &home).unwrap_or_else(|e| die(&e));
    // Like the daemon: a content search when the query has a pattern, on
    // user-interactive threads.
    let grep = q.grep.take().map(|p| tov_search::Grep::new(&p, q.grep_mode).unwrap_or_else(|e| die(&e)));
    let content = grep.as_ref().map(|_| tov_search::content::Content::open_shared(dir.join("content")));
    let pool = rayon::ThreadPoolBuilder::new()
        .start_handler(|_| {
            #[cfg(target_os = "macos")]
            unsafe {
                libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
            }
        })
        .build()
        .unwrap();
    let run = || -> Vec<String> {
        let (Some(g), Some(c)) = (&grep, &content) else {
            let mut p = Vec::new();
            let hits = pool.install(|| query::Searcher { live: &live }.search(&q));
            return hits
                .iter()
                .map(|h| {
                    live.base.path(h.idx as usize, &mut p);
                    format!("{:5} {}", h.score, String::from_utf8_lossy(&p))
                })
                .collect();
        };
        let r = if q.scope.as_ref().is_none_or(|s| tov_search::content::in_scope(s, home.as_bytes())) {
            pool.install(|| c.search(g, &q))
        } else {
            let paths = pool.install(|| tov_search::content::scan_paths(&live, q.clone_for_scan()));
            tov_search::content::verify(g, &paths, q.limit)
        };
        let first = |f: &tov_search::FileMatches| f.lines.first().map_or(String::new(), |(n, t)| format!("{n}: {}", t.trim()));
        r.files.iter().map(|f| format!("{}:{}", String::from_utf8_lossy(&f.path), first(f))).collect()
    };
    // Between runs, another name search: a repeat would be answered from
    // the names cache, which only a repeat of the last query hits.
    let other = query::Query::parse("zzzzzz", &home).unwrap();
    if std::env::var_os("TOV_BENCH_TYPE").is_some() {
        // Typing: each prefix of the query in turn, median per keystroke.
        let keys: Vec<query::Query> = (1..=qs.len()).map(|i| query::Query::parse(&qs[..i], &home).unwrap()).collect();
        use std::sync::atomic::Ordering::Relaxed;
        let mut t = vec![Vec::new(); keys.len()];
        for _ in 0..20 {
            pool.install(|| query::Searcher { live: &live }.search(&other));
            for (i, k) in keys.iter().enumerate() {
                let (n0, s0) = (query::PROF_NAMES_NS.load(Relaxed), query::PROF_SCAN_NS.load(Relaxed));
                let s = Instant::now();
                pool.install(|| query::Searcher { live: &live }.search(k));
                let us = s.elapsed().as_micros() as u64;
                t[i].push((us, (query::PROF_NAMES_NS.load(Relaxed) - n0) / 1000, (query::PROF_SCAN_NS.load(Relaxed) - s0) / 1000));
            }
        }
        for (i, mut v) in t.into_iter().enumerate() {
            v.sort();
            let (us, names, scan) = v[v.len() / 2];
            eprintln!("{:>40} {us:6}us  names {names:5}  scan {scan:5}", &qs[..=i]);
        }
        return;
    }
    let mut times = Vec::new();
    let mut out = Vec::new();
    for _ in 0..20 {
        pool.install(|| query::Searcher { live: &live }.search(&other));
        let t = Instant::now();
        out = run();
        times.push(t.elapsed());
    }
    for l in out.iter().take(10) {
        println!("{l}");
    }
    let first = times[0];
    times.sort();
    eprintln!("first {first:.2?}  median {:.2?}  min {:.2?}", times[times.len() / 2], times[0]);
}

/// Copy this binary to ~/.local/bin; the installed path.
fn copy_to_bin() -> PathBuf {
    let bin = PathBuf::from(home()).join(".local/bin/tov-search");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    // Replace, never overwrite in place: a rewritten signed binary at the same
    // path can be SIGKILLed by the code-signing cache (and a running one is
    // busy on Linux).
    let _ = std::fs::remove_file(&bin);
    std::fs::copy(std::env::current_exe().unwrap(), &bin).unwrap_or_else(|e| die(&format!("copy: {e}")));
    bin
}

#[cfg(target_os = "linux")]
const UNIT: &str = "tov-search.service";

#[cfg(target_os = "linux")]
fn unit_path() -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|d| d.is_absolute());
    config.unwrap_or_else(|| PathBuf::from(home()).join(".config")).join("systemd/user").join(UNIT)
}

#[cfg(target_os = "linux")]
fn systemctl(args: &[&str]) -> bool {
    std::process::Command::new("systemctl").arg("--user").args(args).status().is_ok_and(|s| s.success())
}

#[cfg(target_os = "linux")]
fn install(login: bool) {
    let bin = copy_to_bin();
    if !login {
        println!("installed {}; the daemon starts on first use", bin.display());
        return;
    }
    let unit = format!(
        "[Unit]
Description=tov-search daemon (whole-disk file search)

[Service]
ExecStart={} serve
Restart=always
# Like launchd's throttle: a daemon started on first use may hold the
# socket; this one takes over once it is gone.
RestartSec=10
StandardOutput=append:{log}
StandardError=append:{log}

[Install]
WantedBy=default.target
",
        bin.display(),
        log = data_dir().join("daemon.log").display()
    );
    let path = unit_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, unit).unwrap();
    if !(systemctl(&["daemon-reload"]) && systemctl(&["enable", UNIT]) && systemctl(&["restart", UNIT])) {
        die("systemctl --user failed (is there a user session? see `loginctl enable-linger`)");
    }
    println!("installed {} (systemd user service {UNIT})", bin.display());
    println!("it runs while you are logged in; to keep it running after logout: loginctl enable-linger");
}

#[cfg(target_os = "linux")]
fn uninstall() {
    systemctl(&["disable", "--now", UNIT]);
    let _ = std::fs::remove_file(unit_path());
    systemctl(&["daemon-reload"]);
    println!("removed systemd user service {UNIT}; index kept in {}", data_dir().display());
}

#[cfg(target_os = "macos")]
fn plist_path() -> PathBuf {
    PathBuf::from(home()).join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

#[cfg(target_os = "macos")]
fn launchctl(args: &[&str]) -> bool {
    std::process::Command::new("launchctl").args(args).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
}

#[cfg(target_os = "macos")]
fn domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

#[cfg(target_os = "macos")]
fn install(login: bool) {
    let bin = copy_to_bin();
    if !login {
        println!("installed {}; the daemon starts on first use", bin.display());
        return;
    }
    let log = data_dir().join("daemon.log");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>serve</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        bin.display(),
        log.display(),
        log.display()
    );
    std::fs::write(plist_path(), plist).unwrap();
    let target = format!("{}/{LABEL}", domain());
    launchctl(&["bootout", &target]);
    // bootout returns before the old job is fully gone; bootstrap fails
    // until it is.
    let bootstrap = || launchctl(&["bootstrap", &domain(), plist_path().to_str().unwrap()]);
    let retry = || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        false
    };
    if !(0..50).any(|_| bootstrap() || retry()) {
        die("launchctl bootstrap failed");
    }
    println!("installed {} (LaunchAgent {LABEL})", bin.display());
}

#[cfg(target_os = "macos")]
fn uninstall() {
    launchctl(&["bootout", &format!("{}/{LABEL}", domain())]);
    let _ = std::fs::remove_file(plist_path());
    println!("removed LaunchAgent {LABEL}; index kept in {}", data_dir().display());
}

fn die(msg: &str) -> ! {
    eprintln!("tov-search: {msg}");
    std::process::exit(1)
}
