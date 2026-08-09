//! Chunk-store seam conformance (plan.md §E, D7) plus blob roundtrip and
//! dedup behavior of the FastCDC layer.

use choir_store::{
    get_blob, put_blob, ChunkStore, ChunkerParams, FsStore, Manifest, MemStore, StoreError,
};

fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    // xorshift; deterministic test data without a rand dependency.
    let mut x = seed | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn conformance(store: &mut dyn ChunkStore) {
    let data = b"chunk contents";
    let h = store.put(data).unwrap();
    assert!(store.has(&h));
    assert_eq!(store.get(&h).unwrap(), data);

    // Idempotent put returns the same address.
    assert_eq!(store.put(data).unwrap(), h);

    // Missing hash is NotFound, not garbage.
    let missing = choir_hash::ContentHash::blake3(b"never stored");
    assert!(!store.has(&missing));
    assert!(matches!(store.get(&missing), Err(StoreError::NotFound(_))));
}

#[test]
fn memstore_conforms() {
    conformance(&mut MemStore::new());
}

#[test]
fn fsstore_conforms() {
    let dir = std::env::temp_dir().join(format!("choir-store-test-{}", std::process::id()));
    conformance(&mut FsStore::open(&dir).unwrap());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn blob_roundtrip_all_sizes() {
    let mut store = MemStore::new();
    for (len, seed) in [(0usize, 1u64), (1, 2), (100, 3), (64 * 1024, 4), (3_000_000, 5)] {
        let data = pseudo_random(len, seed);
        let mh = put_blob(&mut store, &data, ChunkerParams::default()).unwrap();
        assert_eq!(get_blob(&store, &mh).unwrap(), data, "roundtrip len={len}");
    }
}

#[test]
fn small_edit_shares_most_chunks() {
    let mut store = MemStore::new();
    let data = pseudo_random(2_000_000, 42);
    put_blob(&mut store, &data, ChunkerParams::default()).unwrap();
    let baseline = store.chunk_count();

    // Flip 16 bytes in the middle; CDC should localize the damage.
    let mut edited = data.clone();
    for b in &mut edited[1_000_000..1_000_016] {
        *b ^= 0xFF;
    }
    put_blob(&mut store, &edited, ChunkerParams::default()).unwrap();
    let new_chunks = store.chunk_count() - baseline;

    // ~125 chunks at 16 KiB average; a 16-byte edit must not re-chunk the
    // world. Allow generous slack for boundary shifts around the edit.
    assert!(
        new_chunks <= 6,
        "16-byte edit created {new_chunks} new chunks (baseline {baseline})"
    );
}

#[test]
fn corrupted_chunk_fails_loudly() {
    let dir = std::env::temp_dir().join(format!("choir-store-corrupt-{}", std::process::id()));
    let mut store = FsStore::open(&dir).unwrap();
    let h = store.put(b"important bytes").unwrap();

    // Corrupt the stored file behind the store's back.
    let mut victim = None;
    for entry in walk(&dir) {
        victim = Some(entry);
    }
    std::fs::write(victim.unwrap(), b"tampered").unwrap();

    assert!(
        matches!(store.get(&h), Err(StoreError::HashMismatch(_))),
        "tampered chunk must fail verification, never return silently"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn unsupported_manifest_version_fails_loudly() {
    let mut store = MemStore::new();
    let manifest = Manifest {
        format_version: choir_store::FORMAT_VERSION + 1,
        params: ChunkerParams::default(),
        len: 0,
        chunks: Vec::new(),
    };
    let bytes = serde_json::to_vec(&manifest).unwrap();
    let hash = store.put(&bytes).unwrap();

    match get_blob(&store, &hash) {
        Err(StoreError::BadManifest(reason)) => {
            assert!(reason.contains("unsupported manifest format version"), "{reason}");
        }
        other => panic!("unsupported manifest must fail, got {other:?}"),
    }
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_dir() {
            files.extend(walk(&e.path()));
        } else {
            files.push(e.path());
        }
    }
    files
}
