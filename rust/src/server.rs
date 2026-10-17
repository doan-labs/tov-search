//! The daemon: one `Engine`, answering JSON lines over a unix socket.
//! `tov-search stdio` and the CLI are thin clients.

use tov_search::walk::{KIND_DIR, KIND_FILE, KIND_LINK};
use tov_search::{Engine, Found, GrepMode, Options, Query};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join("tov-search.sock")
}

pub fn serve(dir: PathBuf, home: String) {
    // One daemon per socket. (The engine's own lock decides who writes the
    // index: an app embedding tov-search may own it while the daemon follows.)
    std::fs::create_dir_all(&dir).ok();
    let Ok(lock) = std::fs::File::create(dir.join("socket.lock")) else { return };
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        eprintln!("{} another tov-search daemon is running", tov_search::query::now_secs());
        return;
    }
    let engine = match Engine::start(Options { dir: dir.clone(), home, skip: None }) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("{} {e}", tov_search::query::now_secs());
            return;
        }
    };
    let sock = socket_path(&dir);
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).expect("bind socket");
    for conn in listener.incoming().flatten() {
        let e = engine.clone();
        std::thread::spawn(move || handle(conn, &e));
    }
}

/// A still-connected client gets the cores kept clocked up for this long
/// after each answer — up to ~one busy core during the window — so the next
/// keystroke's search starts fast (macOS slows idle cores after ~20 ms).
/// `TOV_SEARCH_KEEP_WARM_MS` on the daemon overrides (0 disables).
fn keep_warm() -> Option<Duration> {
    static MS: OnceLock<Option<Duration>> = OnceLock::new();
    *MS.get_or_init(|| {
        std::env::var("TOV_SEARCH_KEEP_WARM_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(Some(Duration::from_millis(250)), |ms| (ms > 0).then_some(Duration::from_millis(ms)))
    })
}

fn handle(conn: UnixStream, engine: &Engine) {
    let Ok(r) = conn.try_clone() else { return };
    let Ok(mut w) = conn.try_clone() else { return };
    let mut out = Vec::new();
    for line in BufReader::new(r).lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        out.clear();
        respond(&line, engine, &mut out);
        if w.write_all(&out).is_err() {
            return;
        }
        if let Some(d) = keep_warm() {
            if waiting(&conn) {
                engine.keep_warm(d);
            }
        }
    }
}

/// The client is still connected with nothing more to ask yet: a session
/// whose next request is a keystroke away. (One-shot clients like the CLI
/// close their side as soon as the request is out.)
fn waiting(conn: &UnixStream) -> bool {
    use std::os::unix::io::AsRawFd;
    let mut buf = [0u8; 1];
    let n = unsafe { libc::recv(conn.as_raw_fd(), buf.as_mut_ptr().cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
    n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock
}

/// The answer to one request line, as one line of JSON.
fn respond(line: &str, engine: &Engine, out: &mut Vec<u8>) {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return put(out, &json!({"ok": false, "error": format!("bad json: {e}")})),
    };
    let id = v.get("id").unwrap_or(&Value::Null).clone();
    match run(&v, engine) {
        Ok(Reply::Hits(found, took_us)) => {
            let hits = found
                .iter()
                .map(|f| HitOut { path: f.path.to_string_lossy(), kind: kind_name(f.kind), size: f.size, mtime: f.mtime, score: f.score })
                .collect();
            put(out, &HitsOut { hits, id: &id, ok: true, took_us })
        }
        Ok(Reply::Grep(r, took_us, indexed, indexing)) => {
            let files = r
                .files
                .iter()
                .map(|f| GrepFile {
                    path: String::from_utf8_lossy(&f.path),
                    matches: f.lines.iter().map(|(n, t)| GrepMatch { line: *n, text: t }).collect(),
                })
                .collect();
            put(
                out,
                &GrepOut {
                    candidates: r.candidates,
                    complete: r.complete,
                    files,
                    id: &id,
                    indexing,
                    ok: true,
                    read: r.read,
                    source: if indexed { "index" } else { "scan" },
                    took_us,
                },
            )
        }
        Ok(Reply::Value(mut r)) => {
            r["id"] = id;
            put(out, &r)
        }
        Err(e) => put(out, &json!({"ok": false, "error": e, "id": id})),
    }
}

fn put(out: &mut Vec<u8>, v: &impl serde::Serialize) {
    serde_json::to_writer(&mut *out, v).expect("json");
    out.push(b'\n');
}

/// One serialized hit: written straight from the result, no `Value` tree.
/// Fields alphabetical — `json!` maps sort keys, and the bench digests the
/// raw reply line.
#[derive(serde::Serialize)]
struct HitOut<'a> {
    kind: &'a str,
    mtime: u32,
    path: std::borrow::Cow<'a, str>,
    score: i32,
    size: u64,
}

/// The search reply: `hits` first, the request's `id`, `took_us` µs.
#[derive(serde::Serialize)]
struct HitsOut<'a> {
    hits: Vec<HitOut<'a>>,
    id: &'a Value,
    ok: bool,
    took_us: u64,
}

/// One matched line: `line` number, `text` as shown.
#[derive(serde::Serialize)]
struct GrepMatch<'a> {
    line: usize,
    text: &'a str,
}

/// One file's matches: `matches` then `path`, alphabetical like `json!`.
#[derive(serde::Serialize)]
struct GrepFile<'a> {
    matches: Vec<GrepMatch<'a>>,
    path: std::borrow::Cow<'a, str>,
}

/// The grep reply: fields alphabetical, like the `json!` map they replace.
#[derive(serde::Serialize)]
struct GrepOut<'a> {
    candidates: usize,
    complete: bool,
    files: Vec<GrepFile<'a>>,
    id: &'a Value,
    indexing: usize,
    ok: bool,
    read: usize,
    source: &'a str,
    took_us: u64,
}

enum Reply {
    Value(Value),
    /// Name-search results and the search's own time (µs): the hot path,
    /// written straight from the results.
    Hits(Vec<Found>, u64),
    /// Grep results: the search's own time (µs), whether it was served
    /// from the index, and the content-indexing flag — no `Value` tree.
    Grep(tov_search::GrepResult, u64, bool, usize),
}

fn run(v: &Value, engine: &Engine) -> Result<Reply, String> {
    let op = v.get("op").and_then(Value::as_str).unwrap_or("search");
    let is_grep = op == "grep"
        || (op == "search"
            && v.get("q").and_then(Value::as_str).is_some_and(|q| ["grep:", "regex:", "sym:", "content:", "symbol:"].iter().any(|k| q.contains(k))));
    match op {
        "ping" => Ok(Reply::Value(json!({"ok": true}))),
        "save" => {
            engine.save();
            Ok(Reply::Value(json!({"ok": true, "scheduled": true})))
        }
        _ if is_grep => grep(v, engine),
        "status" => {
            let s = engine.status();
            if !s.ready {
                return Err("indexing (first run scans the whole disk, ~20s)".into());
            }
            let mut v = serde_json::to_value(s).map_err(|e| e.to_string())?;
            v["ok"] = true.into();
            Ok(Reply::Value(v))
        }
        "search" => {
            let q = parse_request(v, engine.home())?;
            let t = Instant::now();
            let found = engine.search(&q)?;
            Ok(Reply::Hits(found, t.elapsed().as_micros() as u64))
        }
        _ => Err(format!("unknown op {op}")),
    }
}

/// Content search. The pattern comes from `pattern` (+ `mode`) or from a
/// `grep:`/`regex:`/`sym:` filter in `q`; the rest of the query narrows
/// which files are read.
fn grep(v: &Value, engine: &Engine) -> Result<Reply, String> {
    let mut q = parse_request(v, engine.home())?;
    let mode = match v.get("mode").and_then(Value::as_str) {
        Some("regex") => GrepMode::Regex,
        Some("symbol") => GrepMode::Symbol,
        Some("literal") => GrepMode::Literal,
        Some(m) => return Err(format!("unknown mode {m}")),
        None => q.grep_mode,
    };
    let pattern = v.get("pattern").and_then(Value::as_str).map(str::to_string).or(q.grep.take()).ok_or("grep needs a pattern")?;
    let mut g = tov_search::Grep::new(&pattern, mode)?;
    if let Some(n) = v.get("per_file").and_then(Value::as_u64) {
        g.max_per_file = n as usize;
    }
    if let Some(ms) = v.get("budget_ms").and_then(Value::as_u64) {
        g.budget = (ms > 0).then(|| Duration::from_millis(ms));
    }
    let t = Instant::now();
    let (r, indexed) = engine.grep(&q, &g)?;
    Ok(Reply::Grep(r, t.elapsed().as_micros() as u64, indexed, engine.status().content_pending))
}

/// `q` is the query language; any filter key may also be given as its own
/// JSON field (`{"q": "main", "ext": "rs", "in": "~/Developer"}`).
fn parse_request(v: &Value, home: &str) -> Result<Query, String> {
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

fn kind_name(k: u8) -> &'static str {
    match k & 3 {
        KIND_FILE => "file",
        KIND_DIR => "dir",
        KIND_LINK => "link",
        _ => "other",
    }
}

/// Connect to the daemon, starting it if it isn't running.
pub fn connect(dir: &Path) -> std::io::Result<UnixStream> {
    let sock = socket_path(dir);
    if let Ok(s) = UnixStream::connect(&sock) {
        return Ok(s);
    }
    let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("daemon.log"))?;
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    // Own session: closing the terminal that started it doesn't kill it.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.arg("serve").stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log).spawn()?;
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(30));
        if let Ok(s) = UnixStream::connect(&sock) {
            return Ok(s);
        }
    }
    UnixStream::connect(&sock)
}
