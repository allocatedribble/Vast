//! The `.logipack` single-file container: header + concatenated page payloads + directory.

use std::collections::HashMap;

use crate::page::{
    content_id, Compression, PackError, PageDescriptor, PageId, PageKind, DESCRIPTOR_LEN,
};

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
            uncompressed_len: payload.len() as u32,
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
            out.extend_from_slice(&d.uncompressed_len.to_le_bytes());
            out.extend_from_slice(&d.stored_len.to_le_bytes());
            out.extend_from_slice(&(d.offset + HEADER_LEN as u64).to_le_bytes()); // absolute
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
    b.get(at..at + 2).and_then(|s| s.try_into().ok()).map(u16::from_le_bytes).ok_or(PackError::Truncated)
}
fn rd_u32(b: &[u8], at: usize) -> Result<u32, PackError> {
    b.get(at..at + 4).and_then(|s| s.try_into().ok()).map(u32::from_le_bytes).ok_or(PackError::Truncated)
}
fn rd_u64(b: &[u8], at: usize) -> Result<u64, PackError> {
    b.get(at..at + 8).and_then(|s| s.try_into().ok()).map(u64::from_le_bytes).ok_or(PackError::Truncated)
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

        if dir_len != count * DESCRIPTOR_LEN
            || dir_offset.checked_add(dir_len).is_none_or(|end| end > bytes.len())
        {
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
            let uncompressed_len = rd_u32(bytes, at + 24)?;
            let stored_len = rd_u32(bytes, at + 28)?;
            let offset = rd_u64(bytes, at + 32)?;
            if offset.checked_add(stored_len as u64).is_none_or(|end| end > bytes.len() as u64) {
                return Err(PackError::BadDirectory);
            }
            dir.push(PageDescriptor { id, kind, compression, license_id, uncompressed_len, stored_len, offset });
        }

        Ok(Self { bytes, schema_version, package_version, dir })
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

    /// Decompressed payload bytes for a descriptor (`None` compression = identity).
    pub fn page_bytes(&self, desc: &PageDescriptor) -> &'a [u8] {
        let bytes: &'a [u8] = self.bytes;
        let start = desc.offset as usize;
        &bytes[start..start + desc.stored_len as usize]
    }

    /// Look up a page by content id, returning its descriptor and payload.
    pub fn get(&self, id: &PageId) -> Option<(PageDescriptor, &'a [u8])> {
        let desc = *self.find(id)?;
        Some((desc, self.page_bytes(&desc)))
    }
}
