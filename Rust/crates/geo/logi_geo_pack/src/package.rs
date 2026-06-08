//! The `.logipack` single-file container: header + concatenated page payloads + directory.

use hashbrown::HashMap;

use crate::page::{content_id, Compression, PackError, PageDescriptor, PageId, PageKind, DESCRIPTOR_LEN};

const MAGIC: &[u8; 8] = b"LOGIPACK";
const HEADER_LEN: usize = 36; // magic8 + schema4 + package4 + count4 + dir_offset8 + dir_len8

/// Builds a `.logipack` byte archive. Content-addressed pages dedup; output is deterministic for a
/// given sequence of `add_page` calls (→ reproducible builds).
pub struct PackageBuilder {
    schema_version: u32,
    package_version: u32,
    data: Vec<u8>,
    dir: Vec<PageDescriptor>,
    seen: HashMap<PageId, usize>,
}

impl PackageBuilder {
    pub fn new(package_version: u32) -> Self {
        Self {
            schema_version: crate::PACKAGE_SCHEMA_VERSION,
            package_version,
            data: Vec::new(),
            dir: Vec::new(),
            seen: HashMap::new(),
        }
    }

    /// Add an already-encoded page payload. Returns its content id; identical payloads dedup.
    pub fn add_page(&mut self, kind: PageKind, license_id: u32, payload: &[u8]) -> PageId {
        let id = content_id(payload);
        if self.seen.contains_key(&id) {
            return id;
        }
        let offset = self.data.len() as u64; // relative to the data region; absolutized in build()
        self.data.extend_from_slice(payload);
        self.seen.insert(id, self.dir.len());
        self.dir.push(PageDescriptor {
            id,
            kind,
            compression: Compression::None,
            license_id,
            stored_len: payload.len() as u32,
            offset,
        });
        id
    }

    pub fn page_count(&self) -> usize {
        self.dir.len()
    }

    /// Serialize to the `.logipack` byte archive.
    pub fn build(&self) -> Vec<u8> {
        let dir_offset = (HEADER_LEN + self.data.len()) as u64;
        let dir_len = (self.dir.len() * DESCRIPTOR_LEN) as u64;

        let mut out = Vec::with_capacity(HEADER_LEN + self.data.len() + dir_len as usize);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.schema_version.to_le_bytes());
        out.extend_from_slice(&self.package_version.to_le_bytes());
        out.extend_from_slice(&(self.dir.len() as u32).to_le_bytes());
        out.extend_from_slice(&dir_offset.to_le_bytes());
        out.extend_from_slice(&dir_len.to_le_bytes());
        debug_assert_eq!(out.len(), HEADER_LEN);

        out.extend_from_slice(&self.data);

        for d in &self.dir {
            out.extend_from_slice(&d.id);
            out.extend_from_slice(&(d.kind as u16).to_le_bytes());
            out.push(d.compression as u8);
            out.push(0u8); // pad
            out.extend_from_slice(&d.license_id.to_le_bytes());
            out.extend_from_slice(&d.stored_len.to_le_bytes());
            out.extend_from_slice(&(d.offset + HEADER_LEN as u64).to_le_bytes());
            // absolute
        }
        out
    }
}

/// A parsed `.logipack`, borrowing its backing bytes (works over a file mmap or an HTTP range buffer).
pub struct Package<'a> {
    bytes: &'a [u8],
    pub schema_version: u32,
    pub package_version: u32,
    dir: Vec<PageDescriptor>,
}

fn rd_u16(b: &[u8], at: usize) -> Result<u16, PackError> {
    b.get(at..at + 2)
        .and_then(|s| s.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or(PackError::Truncated)
}
fn rd_u32(b: &[u8], at: usize) -> Result<u32, PackError> {
    b.get(at..at + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or(PackError::Truncated)
}
fn rd_u64(b: &[u8], at: usize) -> Result<u64, PackError> {
    b.get(at..at + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or(PackError::Truncated)
}

impl<'a> Package<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, PackError> {
        if bytes.len() < HEADER_LEN || &bytes[0..8] != MAGIC {
            return Err(PackError::BadMagic);
        }
        let schema_version = rd_u32(bytes, 8)?;
        let package_version = rd_u32(bytes, 12)?;
        let count = rd_u32(bytes, 16)? as usize;
        let dir_offset = rd_u64(bytes, 20)? as usize;
        let dir_len = rd_u64(bytes, 28)? as usize;

        // `checked_mul`: `count` is an untrusted u32 from the header; `count * DESCRIPTOR_LEN` (×36) could
        // wrap on a 32-bit `usize` target and let a crafted count pass the `dir_len` check, after which
        // `Vec::with_capacity(count)` would try to reserve ~gigabytes. On 64-bit it cannot wrap; the check is
        // identical there. Treat an overflowing directory size as malformed.
        let dir_bytes = count.checked_mul(DESCRIPTOR_LEN).ok_or(PackError::BadDirectory)?;
        if dir_len != dir_bytes || dir_offset.checked_add(dir_len).is_none_or(|end| end > bytes.len()) {
            return Err(PackError::BadDirectory);
        }

        let mut dir = Vec::with_capacity(count);
        for i in 0..count {
            let at = dir_offset + i * DESCRIPTOR_LEN;
            let mut id = [0u8; 16];
            id.copy_from_slice(&bytes[at..at + 16]);
            let kind = PageKind::from_u16(rd_u16(bytes, at + 16)?)?;
            let compression = Compression::from_u8(bytes[at + 18])?;
            let license_id = rd_u32(bytes, at + 20)?;
            let stored_len = rd_u32(bytes, at + 24)?;
            let offset = rd_u64(bytes, at + 28)?;
            if offset
                .checked_add(stored_len as u64)
                .is_none_or(|end| end > bytes.len() as u64)
            {
                return Err(PackError::BadDirectory);
            }
            dir.push(PageDescriptor {
                id,
                kind,
                compression,
                license_id,
                stored_len,
                offset,
            });
        }

        Ok(Self {
            bytes,
            schema_version,
            package_version,
            dir,
        })
    }

    pub fn page_count(&self) -> usize {
        self.dir.len()
    }

    pub fn descriptors(&self) -> &[PageDescriptor] {
        &self.dir
    }

    pub fn find(&self, id: &PageId) -> Option<&PageDescriptor> {
        self.dir.iter().find(|d| &d.id == id)
    }

    /// Stored payload bytes for a descriptor, **bounds-checked** (H6): returns `None` for any descriptor
    /// whose `[offset, offset+stored_len)` window falls outside the backing buffer, instead of panicking
    /// or wrapping. `offset`/`stored_len` are widened to `u64` before the add so a `u64→usize` truncation
    /// (32-bit) or an overflowing add can't mint an in-range slice. (`Compression::None` = identity;
    /// compression is asserted out in debug — see `page_bytes` decode note.)
    pub fn page_bytes(&self, desc: &PageDescriptor) -> Option<&'a [u8]> {
        // L (page.rs): the format has exactly one compression today; a non-identity descriptor reaching
        // here would otherwise return still-compressed bytes silently. Catch it loudly in debug builds.
        debug_assert!(
            matches!(desc.compression, Compression::None),
            "page_bytes returns stored (compressed) bytes; decompress before use for {:?}",
            desc.compression
        );
        let end = desc.offset.checked_add(desc.stored_len as u64)?;
        // Reject anything past the buffer (also catches a u64 that doesn't fit usize via the try_into).
        if end > self.bytes.len() as u64 {
            return None;
        }
        let start = usize::try_from(desc.offset).ok()?;
        let end = usize::try_from(end).ok()?;
        self.bytes.get(start..end)
    }

    /// Look up a page by content id, returning its descriptor and payload. `None` if the id is absent or
    /// the descriptor's byte window is out of bounds.
    pub fn get(&self, id: &PageId) -> Option<(PageDescriptor, &'a [u8])> {
        let desc = *self.find(id)?;
        let bytes = self.page_bytes(&desc)?;
        Some((desc, bytes))
    }
}
