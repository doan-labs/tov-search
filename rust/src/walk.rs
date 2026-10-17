//! Parallel whole-disk enumeration with getattrlistbulk(2) on macOS,
//! getdents64 + statx on Linux.
//!
//! One syscall returns hundreds of entries with name, type, size, mtime and
//! flags already attached, so there is no per-file stat. Directories fan out
//! over a rayon pool; mount points are not crossed, firmlinks are (that is
//! how /Users etc. on the data volume appear under / exactly once).

use rayon::Scope;
use std::cell::RefCell;
use std::ffi::CString;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

pub const NONE: u32 = u32::MAX;

/// Folders never to open: set when the daemon runs without Full Disk
/// Access, where opening a consent-gated folder (Downloads, Desktop, ...)
/// pops a privacy prompt and blocks the call until someone answers it.
pub static SKIP: std::sync::OnceLock<Vec<Vec<u8>>> = std::sync::OnceLock::new();

pub fn blocked(path: &[u8]) -> bool {
    SKIP.get().is_some_and(|v| v.iter().any(|s| path.starts_with(s) && (path.len() == s.len() || path[s.len()] == b'/')))
}

#[cfg(target_os = "macos")]
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
#[cfg(target_os = "macos")]
const DIR_MNTSTATUS_TRIGGER: u32 = 0x2;

pub const KIND_FILE: u8 = 0;
pub const KIND_DIR: u8 = 1;
pub const KIND_LINK: u8 = 2;
pub const KIND_OTHER: u8 = 3;

/// Entry flag bit: UF_HIDDEN set by the Finder.
pub const FLAG_HIDDEN: u8 = 1 << 2;
/// Directory that is a mount point we did not descend into.
pub const FLAG_MOUNT: u8 = 1 << 3;

#[derive(Clone, Copy)]
pub struct RawEnt {
    pub name_off: u32,
    pub name_len: u16,
    /// kind in the low 2 bits, FLAG_* above.
    pub kind: u8,
    pub size: u64,
    pub mtime: u32,
    /// Temp id of the listing for this directory, or NONE.
    pub child: u32,
}

pub struct Listing {
    pub id: u32,
    pub names: Vec<u8>,
    pub ents: Vec<RawEnt>,
}

struct Ctx {
    next_id: AtomicU32,
    out: Vec<Mutex<Vec<Listing>>>,
}

#[cfg(target_os = "macos")]
thread_local! {
    static BUF: RefCell<Vec<u8>> = RefCell::new(vec![0u8; 256 * 1024]);
}

/// Scan `root` recursively. Listing id 0 is `root` itself.
pub fn scan(root: &[u8], threads: usize) -> Vec<Listing> {
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).start_handler(|_| crate::no_materialize()).build().unwrap();
    let ctx = Ctx { next_id: AtomicU32::new(1), out: (0..threads + 1).map(|_| Mutex::new(Vec::new())).collect() };
    raise_fd_limit();
    let fd = if blocked(root) { -1 } else { CString::new(root).map_or(-1, |c| unsafe { libc::open(c.as_ptr(), OPEN_DIR) }) };
    // Paths are only tracked when there is something to skip.
    let path = SKIP.get().is_some_and(|v| !v.is_empty()).then(|| root.to_vec());
    pool.scope(|s| finish_dir(s, fd, path, 0, &ctx));
    ctx.out.into_iter().flat_map(|m| m.into_inner().unwrap()).collect()
}

fn raise_fd_limit() {
    let mut r: libc::rlimit = unsafe { std::mem::zeroed() };
    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) };
    r.rlim_cur = r.rlim_max.min(65536);
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &r) };
}

/// List a single directory (no recursion). Subdirectories come back with
/// `child == NONE`. Used by the live updater.
pub fn list_one(path: &[u8]) -> Option<Listing> {
    if blocked(path) {
        return None;
    }
    let mut l = Listing { id: 0, names: Vec::new(), ents: Vec::new() };
    list_into(path, &mut l).then_some(l)
}

const OPEN_DIR: i32 = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// Directory fd shared by the tasks that still need to openat() a child.
struct Fd(i32);
impl Drop for Fd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

// Children are opened with openat() relative to the parent's fd, so paths
// never get rebuilt and PATH_MAX never bites. Measured on this Mac: open() +
// close() is ~19us per directory (two Endpoint Security clients tax every
// open), getattrlistbulk ~14us; past ~8 threads the kernel side stops scaling.
fn finish_dir<'s>(s: &Scope<'s>, fd: i32, path: Option<Vec<u8>>, id: u32, ctx: &'s Ctx) {
    let mut l = Listing { id, names: Vec::new(), ents: Vec::new() };
    if fd < 0 {
        push(l, ctx);
        return;
    }
    list_fd(fd, &mut l);
    let me = std::sync::Arc::new(Fd(fd));
    let mut kids = Vec::new();
    for e in l.ents.iter_mut() {
        if e.kind & 3 == KIND_DIR && e.kind & FLAG_MOUNT == 0 {
            let name = &l.names[e.name_off as usize..e.name_off as usize + e.name_len as usize];
            let child_path = path.as_ref().map(|p| crate::live::join(p, name));
            if child_path.as_deref().is_some_and(blocked) {
                continue;
            }
            e.child = ctx.next_id.fetch_add(1, Ordering::Relaxed);
            kids.push((CString::new(name).unwrap_or_default(), e.child, child_path));
        }
    }
    push(l, ctx);
    for (name, cid, child_path) in kids {
        let parent = me.clone();
        s.spawn(move |s| {
            let fd = unsafe { libc::openat(parent.0, name.as_ptr(), OPEN_DIR) };
            drop(parent);
            finish_dir(s, fd, child_path, cid, ctx);
        });
    }
}

fn push(l: Listing, ctx: &Ctx) {
    let slot = rayon::current_thread_index().unwrap_or(ctx.out.len() - 1);
    ctx.out[slot].lock().unwrap().push(l);
}

#[cfg(target_os = "macos")]
fn rd32(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
}

#[cfg(target_os = "macos")]
fn rd64(b: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(b[at..at + 8].try_into().unwrap())
}

/// Returns false if the directory could not be opened.
fn list_into(path: &[u8], l: &mut Listing) -> bool {
    let Ok(cpath) = CString::new(path) else { return false };
    let fd = unsafe { libc::open(cpath.as_ptr(), OPEN_DIR) };
    if fd < 0 {
        return false;
    }
    list_fd(fd, l);
    unsafe { libc::close(fd) };
    true
}

#[cfg(target_os = "linux")]
thread_local! {
    static BUF: RefCell<Vec<u8>> = RefCell::new(vec![0u8; 64 * 1024]);
}

/// Mounts never crossed: kernel pseudo filesystems, other mounts' views
/// (overlay, squashfs snaps) and network or FUSE drives, like the Mac skips
/// /Volumes. `statfs` f_type values.
#[cfg(target_os = "linux")]
#[rustfmt::skip]
const SKIP_FS: &[u32] = &[
    0x9fa0 /* proc */, 0x6265_6572 /* sysfs */, 0x1cd1 /* devpts */, 0x0027_e0eb /* cgroup */, 0x6367_7270 /* cgroup2 */,
    0x7363_6673 /* securityfs */, 0x6462_6720 /* debugfs */, 0x7472_6163 /* tracefs */, 0x6165_676c /* pstore */,
    0xcafe_4a11 /* bpf */, 0x6265_6570 /* configfs */, 0x1980_0202 /* mqueue */, 0x9584_58f6 /* hugetlbfs */,
    0xde5e_81e4 /* efivarfs */, 0x6e73_6673 /* nsfs */, 0x4249_4e4d /* binfmt_misc */, 0x6573_5543 /* fusectl */,
    0x0187 /* autofs */, 0xf97c_ff8c /* selinuxfs */, 0x794c_7630 /* overlay */, 0x7371_7368 /* squashfs */,
    0x6969 /* nfs */, 0x517b /* smb */, 0xff53_4d42 /* cifs */, 0xfe53_4d42 /* smb2 */, 0x6573_5546 /* fuse */,
];

/// getdents64 plus one statx per entry. Mount points of real filesystems
/// are crossed (a Linux disk is often /, /home, /boot...); the rest get
/// FLAG_MOUNT and are not descended.
#[cfg(target_os = "linux")]
fn list_fd(fd: i32, l: &mut Listing) {
    let mut me: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut me) } != 0 {
        return;
    }
    BUF.with_borrow_mut(|buf| {
        loop {
            let n = unsafe { libc::syscall(libc::SYS_getdents64, fd, buf.as_mut_ptr(), buf.len()) };
            if n <= 0 {
                break;
            }
            let mut p = 0usize;
            while p < n as usize {
                // struct linux_dirent64: ino u64, off i64, reclen u16, type u8, name (NUL-terminated)
                let reclen = u16::from_ne_bytes([buf[p + 16], buf[p + 17]]) as usize;
                let rec = &buf[p..p + reclen];
                p += reclen;
                let name = &rec[19..];
                let name = &name[..memchr::memchr(0, name).unwrap_or(name.len())];
                if name == b"." || name == b".." || name.is_empty() || name.len() > u16::MAX as usize {
                    continue;
                }
                stat_into(fd, rec[19..].as_ptr().cast(), name, rec[18], &me, l);
            }
        }
    });
}

#[cfg(target_os = "linux")]
fn stat_into(fd: i32, cname: *const libc::c_char, name: &[u8], d_type: u8, me: &libc::stat, l: &mut Listing) {
    let mut st: libc::statx = unsafe { std::mem::zeroed() };
    let flags = libc::AT_SYMLINK_NOFOLLOW | libc::AT_NO_AUTOMOUNT | libc::AT_STATX_DONT_SYNC;
    let mask = libc::STATX_TYPE | libc::STATX_SIZE | libc::STATX_MTIME;
    let (mut kind, size, mtime);
    if unsafe { libc::statx(fd, cname, flags, mask, &mut st) } == 0 {
        kind = match st.stx_mode as u32 & libc::S_IFMT {
            libc::S_IFREG => KIND_FILE,
            libc::S_IFDIR => KIND_DIR,
            libc::S_IFLNK => KIND_LINK,
            _ => KIND_OTHER,
        };
        // Like getattrlistbulk: a link's size is its target's length.
        size = if kind == KIND_FILE || kind == KIND_LINK { st.stx_size } else { 0 };
        mtime = st.stx_mtime.tv_sec.clamp(0, u32::MAX as i64) as u32;
        let root = st.stx_attributes_mask & st.stx_attributes & libc::STATX_ATTR_MOUNT_ROOT as u64 != 0;
        if kind == KIND_DIR && (root || libc::makedev(st.stx_dev_major, st.stx_dev_minor) != me.st_dev) && !crossable(fd, cname) {
            kind |= FLAG_MOUNT;
        }
    } else if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
        return;
    } else {
        // Listable but not searchable (no x on the folder): name only.
        kind = match d_type {
            libc::DT_REG => KIND_FILE,
            libc::DT_LNK => KIND_LINK,
            libc::DT_DIR => KIND_DIR,
            _ => KIND_OTHER,
        };
        (size, mtime) = (0, 0);
    }
    l.ents.push(RawEnt { name_off: l.names.len() as u32, name_len: name.len() as u16, kind, size, mtime, child: NONE });
    l.names.extend_from_slice(name);
}

/// A mount point worth indexing: a local filesystem.
#[cfg(target_os = "linux")]
fn crossable(fd: i32, cname: *const libc::c_char) -> bool {
    let m = unsafe { libc::openat(fd, cname, libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if m < 0 {
        return false;
    }
    let mut sf: libc::statfs = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::fstatfs(m, &mut sf) } == 0 && !SKIP_FS.contains(&(sf.f_type as u32));
    unsafe { libc::close(m) };
    ok
}

#[cfg(target_os = "macos")]
fn list_fd(fd: i32, l: &mut Listing) {
    let mut al: libc::attrlist = unsafe { std::mem::zeroed() };
    al.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    al.commonattr =
        libc::ATTR_CMN_RETURNED_ATTRS | libc::ATTR_CMN_NAME | ATTR_CMN_ERROR | libc::ATTR_CMN_OBJTYPE | libc::ATTR_CMN_MODTIME | libc::ATTR_CMN_FLAGS;
    al.dirattr = libc::ATTR_DIR_MOUNTSTATUS;
    al.fileattr = libc::ATTR_FILE_DATALENGTH;
    BUF.with_borrow_mut(|buf| {
        loop {
            let n = unsafe { libc::getattrlistbulk(fd, &mut al as *mut _ as *mut libc::c_void, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
            if n <= 0 {
                break;
            }
            let mut p = 0usize;
            for _ in 0..n {
                let len = rd32(buf, p) as usize;
                parse_entry(&buf[p..p + len], l);
                p += len;
            }
        }
    });
}

#[cfg(target_os = "macos")]
fn parse_entry(b: &[u8], l: &mut Listing) {
    let common = rd32(b, 4);
    let dirattr = rd32(b, 12);
    let fileattr = rd32(b, 16);
    let mut f = 24;
    if common & ATTR_CMN_ERROR != 0 {
        f += 4;
    }
    if common & libc::ATTR_CMN_NAME == 0 {
        return;
    }
    let off = rd32(b, f) as i32 as isize;
    let nlen = rd32(b, f + 4) as usize;
    let start = (f as isize + off) as usize;
    let name = &b[start..start + nlen.saturating_sub(1)];
    f += 8;
    let mut kind = KIND_OTHER;
    if common & libc::ATTR_CMN_OBJTYPE != 0 {
        kind = match rd32(b, f) {
            1 => KIND_FILE,
            2 => KIND_DIR,
            5 => KIND_LINK,
            _ => KIND_OTHER,
        };
        f += 4;
    }
    let mut mtime = 0u32;
    if common & libc::ATTR_CMN_MODTIME != 0 {
        mtime = (rd64(b, f) as i64).clamp(0, u32::MAX as i64) as u32;
        f += 16;
    }
    if common & libc::ATTR_CMN_FLAGS != 0 {
        if rd32(b, f) & libc::UF_HIDDEN != 0 {
            kind |= FLAG_HIDDEN;
        }
        f += 4;
    }
    if dirattr & libc::ATTR_DIR_MOUNTSTATUS != 0 {
        if rd32(b, f) & (libc::DIR_MNTSTATUS_MNTPOINT | DIR_MNTSTATUS_TRIGGER) != 0 {
            kind |= FLAG_MOUNT;
        }
        f += 4;
    }
    let mut size = 0u64;
    if fileattr & libc::ATTR_FILE_DATALENGTH != 0 {
        size = rd64(b, f);
    }
    if name.is_empty() || name.len() > u16::MAX as usize {
        return;
    }
    l.ents.push(RawEnt { name_off: l.names.len() as u32, name_len: name.len() as u16, kind, size, mtime, child: NONE });
    l.names.extend_from_slice(name);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn scan_reports_kinds_sizes_and_no_symlink_targets() {
        let root = std::env::temp_dir().join(format!("tov-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("d/e")).unwrap();
        std::fs::write(root.join("d/f.txt"), b"hello").unwrap();
        std::os::unix::fs::symlink(&root, root.join("loop")).unwrap();
        let ls = scan(root.as_os_str().as_bytes(), 2);
        let _ = std::fs::remove_dir_all(&root);
        let mut seen: Vec<(String, u8, u64)> = ls
            .iter()
            .flat_map(|l| l.ents.iter().map(move |e| (l, e)))
            .map(|(l, e)| (String::from_utf8_lossy(&l.names[e.name_off as usize..][..e.name_len as usize]).into_owned(), e.kind & 3, e.size))
            .collect();
        seen.sort();
        let link = root.as_os_str().len() as u64;
        let want = [("d", KIND_DIR, 0), ("e", KIND_DIR, 0), ("f.txt", KIND_FILE, 5), ("loop", KIND_LINK, link)];
        assert_eq!(seen, want.map(|(n, k, s)| (n.to_string(), k, s)));
        assert!(ls.iter().flat_map(|l| &l.ents).all(|e| e.mtime > 0));
    }
}
