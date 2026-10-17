//! Linux file watching: inotify, one watch per folder, reported at folder
//! granularity like FSEvents so the same apply path handles both. inotify
//! has no history: a start, a queue overflow or a folder it could not watch
//! is caught up by relisting folders whose mtime moved (`relist_changed`).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;

// Same meanings and values as the FSEvents flags the engine reads.
pub const MUST_SCAN_SUBDIRS: u32 = 0x1;
/// Some folders are not watched (the watch limit): a periodic catch-up.
pub const USER_DROPPED: u32 = 0x2;
/// The kernel queue overflowed.
pub const KERNEL_DROPPED: u32 = 0x4;
pub const HISTORY_DONE: u32 = 0x10;

pub struct Event {
    pub path: Vec<u8>,
    pub flags: u32,
    pub id: u64,
}

const MASK: u32 = libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_MODIFY
    | libc::IN_CLOSE_WRITE
    | libc::IN_ATTRIB
    | libc::IN_DELETE_SELF
    | libc::IN_ONLYDIR
    | libc::IN_DONT_FOLLOW
    | libc::IN_EXCL_UNLINK;

/// Wall-clock nanoseconds: event ids only need to grow.
pub fn now_id() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

fn max_watches() -> u64 {
    std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches").ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

pub struct Stream {
    fd: i32,
    paths: Mutex<HashMap<i32, Vec<u8>>>,
    /// Folders that could not be watched since the last `watch_all`.
    missed: AtomicUsize,
    /// The watch limit when watches last ran out (0: never).
    full_at: Mutex<u64>,
    warned: AtomicBool,
    ran: AtomicBool,
    /// Watches ever placed on a folder that had none.
    placed: AtomicUsize,
}

impl Stream {
    /// Start reading events; batches go to `tx`. The stream lives as long
    /// as the engine (its reader thread holds it).
    pub fn start(tx: Sender<Vec<Event>>) -> std::sync::Arc<Stream> {
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd < 0 {
            eprintln!(
                "{} inotify unavailable ({}): changes are picked up by a periodic catch-up. Raise fs.inotify.max_user_instances if it is 0 or used up.",
                crate::query::now_secs(),
                std::io::Error::last_os_error()
            );
        }
        let s = std::sync::Arc::new(Stream {
            fd,
            paths: Mutex::default(),
            missed: AtomicUsize::new(0),
            full_at: Mutex::new(0),
            warned: AtomicBool::new(false),
            ran: AtomicBool::new(false),
            placed: AtomicUsize::new(0),
        });
        if fd >= 0 {
            let r = s.clone();
            std::thread::Builder::new().name("tov-search-inotify".into()).spawn(move || r.read_loop(tx)).expect("spawn");
        }
        s
    }

    /// Some folders are unwatched (or inotify is unavailable).
    pub fn degraded(&self) -> bool {
        self.fd < 0 || self.missed.load(Ordering::Relaxed) > 0
    }

    /// Whether `watch_all` would add anything: never ran, or the limit was
    /// raised since watches ran out.
    pub fn wants_rewatch(&self) -> bool {
        let full = *self.full_at.lock().unwrap();
        self.fd >= 0 && (!self.ran.load(Ordering::Relaxed) || (full != 0 && max_watches() > full))
    }

    /// Watch `/` and every folder `dirs_where` visits (it gets the add as its
    /// filter, and returns the folders that failed).
    pub fn watch_all(&self, dirs_where: impl FnOnce(&(dyn Fn(&[u8]) -> bool + Sync)) -> Vec<Vec<u8>>) {
        self.ran.store(true, Ordering::Relaxed);
        self.missed.store(0, Ordering::Relaxed);
        self.warned.store(false, Ordering::Relaxed);
        *self.full_at.lock().unwrap() = 0;
        self.add(b"/");
        let missed = dirs_where(&|p| !self.add(p)).len();
        self.note_missed(missed);
    }

    /// Says, once per `watch_all`, how to raise the limit when watches run out.
    fn note_missed(&self, n: usize) {
        if n == 0 {
            return;
        }
        let missed = self.missed.fetch_add(n, Ordering::Relaxed) + n;
        if self.warned.swap(true, Ordering::Relaxed) {
            return;
        }
        let (watched, limit) = (self.paths.lock().unwrap().len(), max_watches());
        let want = (limit + missed as u64 * 2).next_power_of_two();
        eprintln!(
            "{} inotify watch limit reached (fs.inotify.max_user_watches = {limit}): {watched} folders watched, {missed} not. \
             Changes in those are caught up every {}s instead of at once.\n  \
             To watch everything: sudo sysctl fs.inotify.max_user_watches={want}\n  \
             To keep it after a reboot: echo fs.inotify.max_user_watches={want} | sudo tee /etc/sysctl.d/60-tov-search.conf\n  \
             tov-search picks up the new limit by itself.",
            crate::query::now_secs(),
            CATCH_UP_EVERY.as_secs()
        );
    }

    /// Watch one folder. False only when the watch limit is used up (other
    /// failures, like a folder we may not read, don't count).
    fn add(&self, path: &[u8]) -> bool {
        if self.fd < 0 {
            return false;
        }
        let Ok(c) = std::ffi::CString::new(path) else { return true };
        let wd = unsafe { libc::inotify_add_watch(self.fd, c.as_ptr(), MASK) };
        if wd >= 0 {
            if self.paths.lock().unwrap().insert(wd, path.to_vec()).is_none() {
                self.placed.fetch_add(1, Ordering::Relaxed);
            }
            return true;
        }
        let e = std::io::Error::last_os_error().raw_os_error();
        if e != Some(libc::ENOSPC) {
            return true;
        }
        let mut full = self.full_at.lock().unwrap();
        if *full == 0 {
            *full = max_watches();
        }
        false
    }

    /// Watch a folder that just appeared (created or moved in) and every
    /// folder under it. Done before its parent's event goes out, so the
    /// engine's scan of it starts after the watches exist and nothing in
    /// between is lost. A moved folder keeps its watches; adding them again
    /// updates their paths. True if a folder got its first watch.
    pub fn add_tree(&self, path: Vec<u8>) -> bool {
        let before = self.placed.load(Ordering::Relaxed);
        let mut stack = vec![path];
        while let Some(p) = stack.pop() {
            if crate::walk::blocked(&p) {
                continue;
            }
            if !self.add(&p) {
                self.note_missed(1);
                continue;
            }
            let Some(l) = crate::walk::list_one(&p) else { continue };
            for r in &l.ents {
                if r.kind & 3 == crate::walk::KIND_DIR && r.kind & crate::walk::FLAG_MOUNT == 0 {
                    stack.push(crate::live::join(&p, &l.names[r.name_off as usize..][..r.name_len as usize]));
                }
            }
        }
        self.placed.load(Ordering::Relaxed) != before
    }

    fn read_loop(&self, tx: Sender<Vec<Event>>) {
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return;
            }
            let id = now_id();
            let mut batch = Vec::new();
            let mut p = 0usize;
            while p + 16 <= n as usize {
                // struct inotify_event: wd i32, mask u32, cookie u32, len u32, name[len]
                let rd = |at: usize| u32::from_ne_bytes(buf[p + at..p + at + 4].try_into().unwrap());
                let (wd, mask, len) = (rd(0) as i32, rd(4), rd(12) as usize);
                let name = &buf[p + 16..p + 16 + len];
                let name = &name[..memchr::memchr(0, name).unwrap_or(len)];
                p += 16 + len;
                if mask & libc::IN_Q_OVERFLOW != 0 {
                    batch.push(Event { path: b"/".to_vec(), flags: MUST_SCAN_SUBDIRS | KERNEL_DROPPED, id });
                    continue;
                }
                if mask & libc::IN_IGNORED != 0 {
                    self.paths.lock().unwrap().remove(&wd);
                    continue;
                }
                let Some(dir) = self.paths.lock().unwrap().get(&wd).cloned() else { continue };
                if mask & libc::IN_DELETE_SELF != 0 {
                    // Its parent reports the removal.
                    continue;
                }
                if mask & libc::IN_ISDIR != 0 && mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0 {
                    self.add_tree(crate::live::join(&dir, name));
                }
                batch.push(Event { path: dir, flags: 0, id });
            }
            if !batch.is_empty() && tx.send(batch).is_err() {
                return;
            }
        }
    }
}

/// How often unwatched folders are caught up by mtime.
pub const CATCH_UP_EVERY: std::time::Duration = std::time::Duration::from_secs(30);
