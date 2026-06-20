//! Library API for the content-addressed blob store: put / has / read.

#![allow(unused_imports)]

use std::fs;
use std::path::Path;
use std::sync::Mutex;

#[allow(unused_imports)]
use fs4::FileExt;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::blob::BlobStore;
use crate::db::Db;
use crate::error::{CoreError, Result};
use crate::hash::Sha256Hex;
use crate::manifest::{Manifest, ManifestFile};
use crate::paths::LibraryPaths;

use super::*;

impl Library {

    pub fn put_blob(&self, bytes: &[u8]) -> Result<crate::blob::PutResult> {
        self.blobs.put_bytes(bytes)
    }


    /// Stream-and-hash a blob from a reader without buffering its
    /// contents end-to-end in memory. Used by `import_file` to keep
    /// peak memory at one 64 KB buffer when the user drops in a
    /// 500 MB PDF, instead of allocating the whole file's bytes.
    pub fn put_blob_from_reader<R: std::io::Read>(
        &self,
        reader: &mut R,
    ) -> Result<crate::blob::PutResult> {
        self.blobs.put_reader(reader)
    }


    pub fn has_blob(&self, hash: &Sha256Hex) -> bool {
        self.blobs.has(hash)
    }


    pub fn read_blob(&self, hash: &Sha256Hex) -> Result<Vec<u8>> {
        self.blobs.read_to_vec(hash)
    }
}
