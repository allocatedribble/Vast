//! `logi_geo_pack` — the Logi-native streamable package format (`.logipack`) and page codecs.
//!
//! See `docs/architecture/data-model.md`. The container is a single-file archive: a header, concatenated
//! page payloads, and a directory ("index") at the end. Pages are **content-addressed** (128-bit hash →
//! dedup + deterministic rebuilds), each carrying its **kind**, **compression**, and **license id**. The
//! directory's `(offset, len)` per page is what enables HTTP range reads / mmap / partial install.

pub mod height;
pub mod package;
pub mod page;

pub use height::HeightPage;
pub use package::{Package, PackageBuilder};
pub use page::{content_id, Compression, PackError, PageDescriptor, PageId, PageKind};

/// Container schema version. Bump on any change to the on-disk layout.
pub const PACKAGE_SCHEMA_VERSION: u32 = 1;
