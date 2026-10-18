//! A whole disk as a flat file of entries, so every run indexes exactly the
//! same thing. Little-endian:
//!
//!   "FSDISK01" | n: u64 | now: u32 | n records
//!   record: parent u32 | kind u8 | size u64 | mtime u32 | name_len u16 | name
//!
//! Record 0 is the root (empty name). Parents come before their children.
//! Names are non-empty (except the root) and never contain '/', '\t', '\n'
//! or '\r'. kind: 0 file, 1 dir, 2 symlink, 3 other; | 4 = hidden flag.
//! `now` is the clock to rank file ages against.

pub const KIND_FILE: u8 = 0;
pub const KIND_DIR: u8 = 1;
pub const KIND_LINK: u8 = 2;
pub const FLAG_HIDDEN: u8 = 4;

const MAGIC: &[u8; 8] = b"FSDISK01";

#[allow(dead_code)] // size and mtime: part of the format, stats does not use them yet
pub struct Ent<'a> {
    pub parent: u32,
    pub name: &'a [u8],
    pub kind: u8,
    pub size: u64,
    pub mtime: u32,
}

pub struct Writer {
    pub buf: Vec<u8>,
    n: u64,
}

impl Writer {
    pub fn new() -> Writer {
        let mut w = Writer { buf: Vec::with_capacity(1 << 28), n: 0 };
        w.buf.extend_from_slice(MAGIC);
        w.buf.extend_from_slice(&[0; 12]);
        w.push(0, b"", KIND_DIR, 0, 0);
        w
    }

    /// Append an entry; returns its index.
    pub fn push(&mut self, parent: u32, name: &[u8], kind: u8, size: u64, mtime: u32) -> u32 {
        assert!(name.len() <= u16::MAX as usize && !name.iter().any(|b| b"/\t\n\r".contains(b)) && (self.n == 0 || !name.is_empty()));
        assert!(self.n < 1 << 24, "over 16M entries: a generator loop ran away");
        self.buf.extend_from_slice(&parent.to_le_bytes());
        self.buf.push(kind);
        self.buf.extend_from_slice(&size.to_le_bytes());
        self.buf.extend_from_slice(&mtime.to_le_bytes());
        self.buf.extend_from_slice(&(name.len() as u16).to_le_bytes());
        self.buf.extend_from_slice(name);
        self.n += 1;
        (self.n - 1) as u32
    }

    pub fn len(&self) -> u32 {
        self.n as u32
    }

    pub fn finish(mut self, now: u32) -> Vec<u8> {
        self.buf[8..16].copy_from_slice(&self.n.to_le_bytes());
        self.buf[16..20].copy_from_slice(&now.to_le_bytes());
        self.buf
    }
}

/// Parse a disk file: its entries (borrowing `b`) and its pinned clock.
pub fn parse(b: &[u8]) -> Result<(Vec<Ent<'_>>, u32), String> {
    if b.len() < 20 || &b[..8] != MAGIC {
        return Err("not a disk file".into());
    }
    let n = u64::from_le_bytes(b[8..16].try_into().unwrap()) as usize;
    let now = u32::from_le_bytes(b[16..20].try_into().unwrap());
    let mut ents = Vec::with_capacity(n);
    let mut p = 20;
    for i in 0..n {
        let r = b.get(p..p + 19).ok_or("truncated")?;
        let parent = u32::from_le_bytes(r[0..4].try_into().unwrap());
        let len = u16::from_le_bytes(r[17..19].try_into().unwrap()) as usize;
        let name = b.get(p + 19..p + 19 + len).ok_or("truncated")?;
        if i > 0 && parent as usize >= i {
            return Err(format!("entry {i}: parent {parent} does not come first"));
        }
        ents.push(Ent {
            parent,
            name,
            kind: r[4],
            size: u64::from_le_bytes(r[5..13].try_into().unwrap()),
            mtime: u32::from_le_bytes(r[13..17].try_into().unwrap()),
        });
        p += 19 + len;
    }
    Ok((ents, now))
}

/// A fast 64-bit digest, identical on every platform.
pub fn digest(b: &[u8]) -> u64 {
    let mut h = 0x9e37_79b9_7f4a_7c15u64 ^ b.len() as u64;
    let mut chunks = b.chunks_exact(8);
    for c in &mut chunks {
        h = (h ^ u64::from_le_bytes(c.try_into().unwrap())).wrapping_mul(0x0100_0000_01b3_0001).rotate_left(29);
    }
    for &x in chunks.remainder() {
        h = (h ^ x as u64).wrapping_mul(0x0100_0000_01b3_0001).rotate_left(29);
    }
    h ^ (h >> 31)
}
