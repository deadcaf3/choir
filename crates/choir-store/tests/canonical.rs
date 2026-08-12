//! Canonicalization properties for L0: a blob's address must be a pure
//! function of its bytes and the parameters that cut them.
//!
//! `conformance.rs` covers the seam (put/get/has), the round-trip, dedup
//! and the two loud-failure paths. What it does not state is the property
//! the whole content-address layer rests on: two nodes that store the same
//! file must compute the same address for it, or dedup, sharing and every
//! `prev` comparison built on those addresses quietly stop matching.
//!
//! Nothing in `put_blob` guarantees that by construction — the address is
//! whatever the chunker and `serde_json` happened to produce — so it is
//! asserted here over generated blobs, including across the two store
//! implementations and across independently built stores.
//!
//! This is the L0 half of the properties in `choir-oplog/tests/it/
//! canonical.rs`; both exist because determinism in this codebase is a
//! convention rather than something the types enforce.

use choir_hash::ContentHash;
use choir_store::{
    get_blob, put_blob, ChunkStore, ChunkerParams, FsStore, Manifest, MemStore, FORMAT_VERSION,
};

/// Sizes spanning every regime the chunker has: below `min` (one chunk,
/// no boundary search), across `avg` and `max`, and large enough to cut
/// many chunks so ordering and boundary placement actually matter.
const SIZES: [usize; 9] = [0, 1, 4095, 4096, 16_384, 65_535, 65_537, 200_000, 1_000_000];

/// Parameter sets used wherever a test must not be true only for the
/// defaults. Ordered by average chunk size, which is what
/// [`the_chunker_parameters_are_part_of_the_address`] leans on.
const CUTS: [ChunkerParams; 3] = [
    ChunkerParams {
        min: 2 * 1024,
        avg: 8 * 1024,
        max: 32 * 1024,
    },
    ChunkerParams {
        min: 4 * 1024,
        avg: 16 * 1024,
        max: 64 * 1024,
    },
    ChunkerParams {
        min: 8 * 1024,
        avg: 32 * 1024,
        max: 128 * 1024,
    },
];

/// Deterministic test bytes, same xorshift the sibling suite uses.
fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
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

/// Scratch dir that removes itself, so a failing test leaves no store
/// behind for the next one to find.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "choir-store-canonical-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn manifest_at(store: &dyn ChunkStore, hash: &ContentHash) -> Manifest {
    serde_json::from_slice(&store.get(hash).expect("manifest is stored")).expect("manifest decodes")
}

/// The core property: the same bytes get the same address, no matter which
/// store computed it, what that store already held, or how many times it
/// has been asked.
///
/// The cross-implementation half is the one that cannot be inferred from
/// the code — `MemStore` and `FsStore` share `put_blob`, but each is free
/// to hash differently and the seam does not forbid it. If they ever
/// diverge, a node that switches backends silently re-addresses its whole
/// history.
#[test]
fn a_blobs_address_is_a_pure_function_of_its_bytes() {
    let scratch = Scratch::new("pure");
    for (i, len) in SIZES.iter().enumerate() {
        let data = pseudo_random(*len, i as u64 + 1);

        let mut fresh = MemStore::new();
        let first = put_blob(&mut fresh, &data, ChunkerParams::default()).expect("put");
        let second = put_blob(&mut fresh, &data, ChunkerParams::default()).expect("re-put");
        assert_eq!(first, second, "len={len}: re-storing changed the address");

        // A store with unrelated history must not address this differently.
        let mut used = MemStore::new();
        put_blob(&mut used, b"unrelated prior blob", ChunkerParams::default()).expect("prior");
        let with_history = put_blob(&mut used, &data, ChunkerParams::default()).expect("put");
        assert_eq!(
            first, with_history,
            "len={len}: the address depended on what the store already held"
        );

        // And neither may the backend.
        let mut on_disk = FsStore::open(&scratch.0.join(format!("s{i}"))).expect("open");
        let fs_hash = put_blob(&mut on_disk, &data, ChunkerParams::default()).expect("put");
        assert_eq!(
            first, fs_hash,
            "len={len}: MemStore and FsStore disagree on a blob's address"
        );
        assert_eq!(
            get_blob(&on_disk, &fs_hash).expect("get"),
            data,
            "len={len}: FsStore round-trip"
        );
    }
}

/// The manifest's documented purpose, asserted: "a blob is always
/// rechunkable for verification because its manifest says exactly how it
/// was cut" (`ChunkerParams`). That claim is only true if re-cutting the
/// reassembled bytes under the manifest's *own recorded* parameters
/// reproduces its chunk list exactly — same addresses, same order, same
/// count. Nothing else in the suite checks it, and without it the recorded
/// params are decoration.
/// Deliberately run under non-default parameters as well as the defaults.
/// A `put_blob` that recorded the caller's params but always cut with the
/// defaults would satisfy every default-only assertion in this file, and
/// would corrupt exactly the case the recorded params exist to serve.
#[test]
fn a_manifest_rechunks_to_itself() {
    let mut store = MemStore::new();
    for (i, (len, params)) in SIZES
        .iter()
        .flat_map(|len| CUTS.iter().map(move |p| (len, *p)))
        .enumerate()
    {
        let data = pseudo_random(*len, i as u64 + 100);
        let hash = put_blob(&mut store, &data, params).expect("put");
        let manifest = manifest_at(&store, &hash);
        assert_eq!(manifest.format_version, FORMAT_VERSION);
        assert_eq!(manifest.len, *len as u64, "len={len}: recorded length");
        assert_eq!(
            manifest.params, params,
            "len={len}: the manifest recorded parameters the caller did not ask for"
        );

        // Re-cut the blob using only what the manifest says.
        let reassembled = get_blob(&store, &hash).expect("get");
        let mut recut = MemStore::new();
        let recut_hash = put_blob(&mut recut, &reassembled, manifest.params).expect("re-cut");
        assert_eq!(
            recut_hash, hash,
            "len={len}: re-cutting under the recorded params gave a different manifest"
        );
        let recut_manifest = manifest_at(&recut, &recut_hash);
        assert_eq!(
            recut_manifest.chunks, manifest.chunks,
            "len={len}: chunk list is not reproducible from the recorded params"
        );

        // The chunk list is ordered, and its order is load-bearing:
        // `get_blob` concatenates in list order and only checks the total
        // length afterwards, so a reordered list of equal-length chunks
        // would reassemble into wrong bytes. What makes that unreachable
        // is that the manifest is itself content-addressed — reordering
        // the list changes the address being asked for. Stated here
        // because the guard lives one layer up from the check.
        let mut concatenated = Vec::new();
        for chunk in &manifest.chunks {
            concatenated.extend_from_slice(&store.get(chunk).expect("chunk is stored"));
        }
        assert_eq!(
            concatenated, data,
            "len={len}: chunks in list order do not reassemble the blob"
        );
    }
}

/// Parameters are inside the address, which is what lets the chunker
/// change without orphaning existing data (D6). Identical bytes cut
/// differently must therefore land on different manifest addresses — while
/// still reassembling to the same blob.
///
/// Distinct addresses alone would be a weak assertion: a `put_blob` that
/// recorded `params` but ignored them while cutting still writes three
/// different manifests, because the parameters are a hashed field. So the
/// *cut itself* is checked too — a smaller average chunk size must produce
/// strictly more chunks. That is the assertion that distinguishes
/// "parameters are honoured" from "parameters are stored".
#[test]
fn the_chunker_parameters_are_part_of_the_address() {
    let mut store = MemStore::new();
    let data = pseudo_random(300_000, 7);
    let mut seen = std::collections::HashMap::new();
    let mut counts = Vec::new();
    for params in CUTS {
        let hash = put_blob(&mut store, &data, params).expect("put");
        assert_eq!(
            get_blob(&store, &hash).expect("get"),
            data,
            "{params:?}: every cut must reassemble to the same blob"
        );
        if let Some(previous) = seen.insert(hash.to_hex(), params) {
            panic!("{params:?} and {previous:?} produced the same manifest address");
        }
        counts.push((params, manifest_at(&store, &hash).chunks.len()));
    }
    assert_eq!(seen.len(), CUTS.len(), "the parameter loop did not run");
    for pair in counts.windows(2) {
        let ((small, many), (large, few)) = (pair[0], pair[1]);
        assert!(
            many > few,
            "avg {} cut into {many} chunks and avg {} into {few}: \
             the parameters were recorded but not applied ({small:?} vs {large:?})",
            small.avg,
            large.avg
        );
    }
}

/// Distinct blobs get distinct addresses, including the adjacent cases a
/// length-only check would miss: a one-byte edit, a one-byte truncation,
/// and a byte appended.
#[test]
fn distinct_blobs_get_distinct_addresses() {
    let mut store = MemStore::new();
    let mut seen: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
    for (i, len) in SIZES.iter().enumerate() {
        let base = pseudo_random(*len, i as u64 + 200);
        let mut variants = vec![base.clone()];
        let mut appended = base.clone();
        appended.push(0xff);
        variants.push(appended);
        if !base.is_empty() {
            let mut flipped = base.clone();
            flipped[base.len() / 2] ^= 0xff;
            variants.push(flipped);
            variants.push(base[..base.len() - 1].to_vec());
        }
        for data in variants {
            let hex = put_blob(&mut store, &data, ChunkerParams::default())
                .expect("put")
                .to_hex();
            if let Some(previous) = seen.get(&hex) {
                assert_eq!(
                    previous, &data,
                    "len={len}: two different blobs share address {hex}"
                );
            } else {
                seen.insert(hex, data);
            }
        }
    }
    assert!(seen.len() > SIZES.len(), "the variant loop did not run");
}

/// A manifest is a hashed struct, so decode-then-encode must reproduce its
/// bytes — the same property the op log needs, for the same reason: the
/// bytes are the address.
#[test]
fn manifests_round_trip_byte_for_byte() {
    let mut store = MemStore::new();
    for (i, len) in SIZES.iter().enumerate() {
        let data = pseudo_random(*len, i as u64 + 300);
        let hash = put_blob(&mut store, &data, ChunkerParams::default()).expect("put");
        let bytes = store.get(&hash).expect("manifest is stored");
        let decoded: Manifest = serde_json::from_slice(&bytes).expect("decodes");
        let re_encoded = serde_json::to_vec(&decoded).expect("re-serializes");
        assert_eq!(
            re_encoded, bytes,
            "len={len}: manifest re-serialization drifted from the stored bytes"
        );
        assert_eq!(
            ContentHash::blake3(&re_encoded),
            hash,
            "len={len}: the manifest no longer hashes to its own address"
        );
    }
}
