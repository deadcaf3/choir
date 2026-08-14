//! L0 content-addressed store: BLAKE3 + FastCDC chunking (DECISIONS.md D6).
//!
//! One-way-door rules (DECISIONS.md) enforced here:
//! - manifests carry `format_version` and record the exact chunking
//!   parameters used, per object, so parameter changes never orphan data
//! - all identifiers are self-describing [`ContentHash`] envelopes
//!
//! The chunk-store seam (D7) is [`ChunkStore`]; the production backend is an
//! S3-compatible object store, so implementations must stay within plain
//! put/get/has semantics.
//!
//! # Examples
//!
//! ```
//! use choir_store::{ChunkerParams, MemStore, put_blob, get_blob};
//!
//! let mut store = MemStore::new();
//! let data = vec![7u8; 100_000];
//! let manifest_hash = put_blob(&mut store, &data, ChunkerParams::default()).unwrap();
//! assert_eq!(get_blob(&store, &manifest_hash).unwrap(), data);
//! ```

use choir_hash::ContentHash;
use serde::{Deserialize, Serialize};

/// Current manifest-format version. Bump on any incompatible change;
/// additive changes keep the version (DECISIONS.md).
pub const FORMAT_VERSION: u16 = 1;

/// FastCDC parameters, recorded per object (one-way-door rule, D6): a blob
/// is always rechunkable for verification because its manifest says exactly
/// how it was cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkerParams {
    /// Minimum chunk size in bytes.
    pub min: u32,
    /// Target average chunk size in bytes.
    pub avg: u32,
    /// Maximum chunk size in bytes.
    pub max: u32,
}

impl Default for ChunkerParams {
    /// 4 KiB / 16 KiB / 64 KiB, the plan's small-file-friendly starting point.
    fn default() -> Self {
        Self {
            min: 4 * 1024,
            avg: 16 * 1024,
            max: 64 * 1024,
        }
    }
}

/// A blob's recipe: ordered chunk addresses plus the parameters that cut it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Manifest wire-format version; see [`FORMAT_VERSION`].
    pub format_version: u16,
    /// Chunking parameters this blob was cut with.
    pub params: ChunkerParams,
    /// Total blob length in bytes.
    pub len: u64,
    /// Content addresses of the chunks, in order.
    pub chunks: Vec<ContentHash>,
}

/// Failure modes of a [`ChunkStore`] or blob operation.
#[derive(Debug)]
pub enum StoreError {
    /// Requested hash is not in the store.
    NotFound(ContentHash),
    /// Stored bytes do not hash to their address (corruption or tampering).
    HashMismatch(ContentHash),
    /// Stored manifest could not be decoded.
    BadManifest(String),
    /// Underlying storage I/O failure.
    Io(std::io::Error),
}

/// The chunk-store seam (D7). Conformance suite: `tests/conformance.rs`.
///
/// `put` is content-addressed and idempotent; `get` must return exactly the
/// bytes that hash to `hash` or an error, never silently wrong data.
pub trait ChunkStore: Send {
    /// Stores `data` and returns its content address. Idempotent.
    fn put(&mut self, data: &[u8]) -> Result<ContentHash, StoreError>;

    /// Retrieves the bytes for `hash`, verifying them against the address.
    fn get(&self, hash: &ContentHash) -> Result<Vec<u8>, StoreError>;

    /// Whether `hash` is present.
    fn has(&self, hash: &ContentHash) -> bool;
}

/// Primary in-memory implementation (also the dev/test runtime).
#[derive(Default)]
pub struct MemStore {
    chunks: std::collections::HashMap<ContentHash, Vec<u8>>,
}

impl MemStore {
    /// Creates an empty in-memory store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of distinct chunks held (used by dedup tests).
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }
}

impl ChunkStore for MemStore {
    fn put(&mut self, data: &[u8]) -> Result<ContentHash, StoreError> {
        let hash = ContentHash::blake3(data);
        self.chunks.entry(hash.clone()).or_insert_with(|| data.to_vec());
        Ok(hash)
    }

    fn get(&self, hash: &ContentHash) -> Result<Vec<u8>, StoreError> {
        let data = self
            .chunks
            .get(hash)
            .ok_or_else(|| StoreError::NotFound(hash.clone()))?;
        if &ContentHash::blake3(data) != hash {
            return Err(StoreError::HashMismatch(hash.clone()));
        }
        Ok(data.clone())
    }

    fn has(&self, hash: &ContentHash) -> bool {
        self.chunks.contains_key(hash)
    }
}

/// Second implementation (seam rule): filesystem store, one file per chunk,
/// sharded by the first digest byte.
pub struct FsStore {
    root: std::path::PathBuf,
}

impl FsStore {
    /// Opens (creating if absent) a store rooted at `root`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] when the root cannot be created.
    pub fn open(root: &std::path::Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(root).map_err(StoreError::Io)?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    fn path_for(&self, hash: &ContentHash) -> std::path::PathBuf {
        let hex = hash.to_hex();
        // hex is "cc-dddd…"; shard on the first two digest nibbles.
        self.root.join(&hex[3..5]).join(&hex)
    }
}

impl ChunkStore for FsStore {
    fn put(&mut self, data: &[u8]) -> Result<ContentHash, StoreError> {
        let hash = ContentHash::blake3(data);
        let path = self.path_for(&hash);
        if !path.exists() {
            std::fs::create_dir_all(path.parent().unwrap()).map_err(StoreError::Io)?;
            // Content-addressed entries are immutable: write to a temp name,
            // then atomically rename, so readers never see partial chunks.
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, data).map_err(StoreError::Io)?;
            std::fs::rename(&tmp, &path).map_err(StoreError::Io)?;
        }
        Ok(hash)
    }

    fn get(&self, hash: &ContentHash) -> Result<Vec<u8>, StoreError> {
        let path = self.path_for(hash);
        let data = std::fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StoreError::NotFound(hash.clone())
            } else {
                StoreError::Io(e)
            }
        })?;
        if &ContentHash::blake3(&data) != hash {
            return Err(StoreError::HashMismatch(hash.clone()));
        }
        Ok(data)
    }

    fn has(&self, hash: &ContentHash) -> bool {
        self.path_for(hash).exists()
    }
}

/// Chunks `data` with FastCDC under `params`, stores every chunk and the
/// manifest, and returns the manifest's content address.
///
/// # Errors
///
/// Propagates any [`StoreError`] from the underlying store.
pub fn put_blob(
    store: &mut dyn ChunkStore,
    data: &[u8],
    params: ChunkerParams,
) -> Result<ContentHash, StoreError> {
    let mut chunks = Vec::new();
    for chunk in fastcdc::v2020::FastCDC::new(data, params.min, params.avg, params.max) {
        chunks.push(store.put(&data[chunk.offset..chunk.offset + chunk.length])?);
    }
    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        params,
        len: data.len() as u64,
        chunks,
    };
    let bytes = serde_json::to_vec(&manifest).expect("Manifest is always serializable");
    store.put(&bytes)
}

/// Reassembles a blob from its manifest address, verifying every chunk.
///
/// # Errors
///
/// Returns [`StoreError::BadManifest`] when the manifest fails to decode,
/// names an unsupported format version, or disagrees with the reassembled
/// length, and propagates chunk lookup and verification failures.
pub fn get_blob(store: &dyn ChunkStore, manifest_hash: &ContentHash) -> Result<Vec<u8>, StoreError> {
    let manifest: Manifest = serde_json::from_slice(&store.get(manifest_hash)?)
        .map_err(|e| StoreError::BadManifest(e.to_string()))?;
    if manifest.format_version != FORMAT_VERSION {
        return Err(StoreError::BadManifest(format!(
            "unsupported manifest format version {} (expected {FORMAT_VERSION})",
            manifest.format_version
        )));
    }
    let mut data = Vec::with_capacity(manifest.len as usize);
    for chunk_hash in &manifest.chunks {
        data.extend_from_slice(&store.get(chunk_hash)?);
    }
    if data.len() as u64 != manifest.len {
        return Err(StoreError::BadManifest(format!(
            "reassembled {} bytes, manifest says {}",
            data.len(),
            manifest.len
        )));
    }
    Ok(data)
}
