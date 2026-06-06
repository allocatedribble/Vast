//! Page primitives: kinds, compression, content-addressed ids, and the directory descriptor.

use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PackError {
    BadMagic,
    Truncated,
    BadDirectory,
    UnknownKind(u16),
    UnknownCompression(u8),
    BadPage,
}

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PackError::BadMagic => write!(f, "not a logipack (bad magic)"),
            PackError::Truncated => write!(f, "package truncated"),
            PackError::BadDirectory => write!(f, "bad page directory"),
            PackError::UnknownKind(k) => write!(f, "unknown page kind {k}"),
            PackError::UnknownCompression(c) => write!(f, "unknown compression {c}"),
            PackError::BadPage => write!(f, "malformed page payload"),
        }
    }
}

impl std::error::Error for PackError {}

/// 128-bit content-addressed page id.
pub type PageId = [u8; 16];

/// Deterministic 128-bit content id (FNV-1a with two seeds). Same bytes → same id (dedup + reproducible).
pub fn content_id(bytes: &[u8]) -> PageId {
    let h1 = fnv1a64(bytes, 0xcbf2_9ce4_8422_2325);
    let h2 = fnv1a64(bytes, 0x9e37_79b9_7f4a_7c15);
    let mut id = [0u8; 16];
    id[0..8].copy_from_slice(&h1.to_le_bytes());
    id[8..16].copy_from_slice(&h2.to_le_bytes());
    id
}

fn fnv1a64(bytes: &[u8], mut h: u64) -> u64 {
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[repr(u16)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PageKind {
    Height = 0,
    Normal = 1,
    Material = 2,
    Vector = 3,
    ProcSeed = 4,
    Collision = 5,
    Attribution = 6,
    RenderPatch = 7,
}

impl PageKind {
    pub fn from_u16(v: u16) -> Result<Self, PackError> {
        Ok(match v {
            0 => Self::Height,
            1 => Self::Normal,
            2 => Self::Material,
            3 => Self::Vector,
            4 => Self::ProcSeed,
            5 => Self::Collision,
            6 => Self::Attribution,
            7 => Self::RenderPatch,
            other => return Err(PackError::UnknownKind(other)),
        })
    }
}

/// Per-page compression. `None` today; zstd/lz4 plug in here without touching the container.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Compression {
    None = 0,
}

impl Compression {
    pub fn from_u8(v: u8) -> Result<Self, PackError> {
        Ok(match v {
            0 => Self::None,
            other => return Err(PackError::UnknownCompression(other)),
        })
    }
}

/// A directory entry: where a page lives and how to read it.
#[derive(Clone, Copy, Debug)]
pub struct PageDescriptor {
    pub id: PageId,
    pub kind: PageKind,
    pub compression: Compression,
    pub license_id: u32,
    pub uncompressed_len: u32,
    pub stored_len: u32,
    /// Absolute byte offset of the stored payload within the package.
    pub offset: u64,
}

/// Serialized size of one directory entry.
pub(crate) const DESCRIPTOR_LEN: usize = 40;
