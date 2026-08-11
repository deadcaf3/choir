//! Invariant 3 as a property: canonical serialization of the hashed shapes
//! that contain maps.
//!
//! `golden.rs` freezes one seven-path commit and notes that a `HashMap`
//! substituted for the `tree`'s `BTreeMap` would fail it "essentially every
//! run". Essentially is the weak word. `HashMap` iteration order is a
//! function of the keys and of a per-instance seed, so a golden vector with
//! one fixed key set is one draw: it can only ever fail on the keys the
//! author happened to pick, and a small or unlucky tree passes.
//!
//! These tests remove both hedges. They compare *two independently built
//! maps holding the same entries in different insertion orders* — under
//! `HashMap` those are two instances with different seeds, so their orders
//! disagree — and they do it over randomly generated key sets rather than
//! one. A map type whose order is not the key order fails here on every
//! seed, not on a coin flip.
//!
//! The randomness is a hand-rolled xorshift (house convention) over a fixed
//! seed list, so the suite is deterministic and reproducible.

use choir_hash::ContentHash;
use choir_view::{Commit, OpKind, TreeEntry, Verdict, ViewOp, FORMAT_VERSION};

/// Fixed seeds: deterministic, but more than one draw.
const SEEDS: [u64; 3] = [0x0123_4567_89ab_cdef, 0xdead_beef, 7];

/// Paths per generated tree. Large enough that an order-losing map type
/// cannot coincide with sorted order by luck (`1/16!` per draw, per seed).
const TREE_SIZE: usize = 16;

/// xorshift64*; see the sibling suite in `choir-oplog` for why this is
/// hand-rolled rather than a dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 { 0x9e37_79b9_7f4a_7c15 } else { seed })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    fn hash(&mut self) -> ContentHash {
        ContentHash::blake3(&self.next_u64().to_le_bytes())
    }

    /// A repo-shaped path. Shared directory prefixes matter: they make
    /// sorted order and any other order differ in more places than a set of
    /// unrelated names would.
    ///
    /// Some names carry characters JSON has to escape. A tree key is a file
    /// path, and a path may legally contain a quote or a backslash, so the
    /// escaped form is part of the canonical form and belongs in the
    /// corpus. This is only safe because
    /// [`tree_keys_are_emitted_in_sorted_order`] looks keys up by their
    /// *encoded* spelling; searching for the raw key would report a hostile
    /// key as missing from output that in fact contains it.
    fn path(&mut self) -> String {
        const DIRS: [&str; 6] = ["src", "src/bin", "tests", "docs", "crates/a", ""];
        const NAMES: [&str; 6] = ["lib.rs", "ma\"in.rs", "back\\slash.rs", "a.rs", "Zed.toml", ".hidden"];
        let dir = DIRS[self.below(DIRS.len())];
        let name = NAMES[self.below(NAMES.len())];
        let tag = self.next_u64() % 1_000;
        if dir.is_empty() {
            format!("{tag:03}-{name}")
        } else {
            format!("{dir}/{tag:03}-{name}")
        }
    }

    fn tree_entry(&mut self) -> TreeEntry {
        if self.next_u64().is_multiple_of(4) {
            TreeEntry::Conflict {
                base: if self.next_u64() & 1 == 0 {
                    Some(self.hash())
                } else {
                    None
                },
                left: self.hash(),
                right: self.hash(),
            }
        } else {
            TreeEntry::File { blob: self.hash() }
        }
    }

    /// A tree as a flat list of pairs, so callers can insert it into a map
    /// in whatever order they like.
    fn tree_pairs(&mut self) -> Vec<(String, TreeEntry)> {
        let mut pairs: Vec<(String, TreeEntry)> = Vec::new();
        while pairs.len() < TREE_SIZE {
            let path = self.path();
            if pairs.iter().any(|(p, _)| p == &path) {
                continue;
            }
            let entry = self.tree_entry();
            pairs.push((path, entry));
        }
        pairs
    }
}

fn commit_from(pairs: &[(String, TreeEntry)]) -> Commit {
    Commit {
        format_version: FORMAT_VERSION,
        parents: vec![ContentHash::blake3(b"parent")],
        // The map type is inferred from the field, never named here: a
        // test that guards the choice of map must not restate it, or
        // swapping the field breaks compilation instead of failing.
        tree: pairs.iter().cloned().collect(),
        author: "agent-1".into(),
        message: "property".into(),
    }
}

fn canonical<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("hashed shapes always serialize")
}

/// The property that invariant 3 actually asserts: a commit's bytes depend
/// on the *contents* of its tree, never on how the tree was built.
///
/// Two maps built from the same pairs in opposite insertion orders are two
/// distinct instances. For a `BTreeMap` they serialize identically because
/// order is a function of the keys; for any map whose order also depends on
/// a per-instance seed or on insertion history, they do not.
#[test]
fn tree_serialization_is_independent_of_insertion_order() {
    for seed in SEEDS {
        let mut rng = Rng::new(seed);
        for case in 0..8 {
            let at = format!("seed {seed:#x} case {case}");
            let pairs = rng.tree_pairs();
            let mut reversed = pairs.clone();
            reversed.reverse();
            let mut rotated = pairs.clone();
            rotated.rotate_left(1 + rng.below(TREE_SIZE - 1));

            let forward = commit_from(&pairs);
            let backward = commit_from(&reversed);
            let rotated = commit_from(&rotated);

            assert_eq!(
                canonical(&forward),
                canonical(&backward),
                "{at}: reversing insertion order changed the canonical bytes"
            );
            assert_eq!(
                canonical(&forward),
                canonical(&rotated),
                "{at}: rotating insertion order changed the canonical bytes"
            );
            assert_eq!(
                ContentHash::blake3(canonical(&forward).as_bytes()),
                ContentHash::blake3(canonical(&backward).as_bytes()),
                "{at}: the commit address depends on how its tree was built"
            );
        }
    }
}

/// The stronger, positive form: keys appear in sorted order, not merely in
/// *some* stable order. A stable-but-unsorted encoding would pass the test
/// above and still break interoperability with any other implementation of
/// this format, which is what "canonical" has to mean across replicas.
#[test]
fn tree_keys_are_emitted_in_sorted_order() {
    // Set when some key's encoded spelling differs from its raw one, i.e.
    // JSON had to escape it. Asserted at the end: if the path alphabet is
    // ever tamed, this test silently stops covering escaped keys, and the
    // escaping-aware lookup below stops being exercised at all.
    let mut saw_escaped_key = false;
    for seed in SEEDS {
        let mut rng = Rng::new(seed);
        for case in 0..8 {
            let at = format!("seed {seed:#x} case {case}");
            let pairs = rng.tree_pairs();
            let bytes = canonical(&commit_from(&pairs));

            let mut sorted: Vec<&String> = pairs.iter().map(|(p, _)| p).collect();
            sorted.sort();
            let mut previous = 0usize;
            for key in sorted {
                // Look the key up by its *encoded* spelling. A key holding a
                // quote or backslash appears escaped in the output, so a
                // needle built from the raw key would not match and this test
                // would report a present key as missing — a false failure
                // whose obvious "fix" is to make the generator tamer, which
                // would quietly delete the coverage. serde_json emits the
                // quotes, so only the colon is appended.
                let needle = format!("{}:", serde_json::to_string(key).expect("a key encodes"));
                saw_escaped_key |= needle != format!("\"{key}\":");
                let found = bytes.find(&needle).unwrap_or_else(|| {
                    panic!("{at}: key {key} (encoded {needle}) missing from the canonical form")
                });
                assert!(
                    found > previous,
                    "{at}: key {key} is emitted out of sorted order"
                );
                previous = found;
            }
        }
    }
    assert!(
        saw_escaped_key,
        "no generated tree key needed escaping, so the escaping-aware lookup \
         is untested; restore a quote or backslash to the path alphabet"
    );
}

/// A commit must survive the store round-trip byte-for-byte, since its
/// address is those bytes. `Commit::get` re-decodes what `Commit::put`
/// wrote, and every later re-encode has to reproduce it.
#[test]
fn commits_round_trip_byte_for_byte() {
    for seed in SEEDS {
        let mut rng = Rng::new(seed);
        for case in 0..8 {
            let at = format!("seed {seed:#x} case {case}");
            let commit = commit_from(&rng.tree_pairs());
            let bytes = canonical(&commit);
            let decoded: Commit = serde_json::from_str(&bytes).expect("canonical bytes decode");
            assert_eq!(decoded, commit, "{at}: commit did not round-trip");
            assert_eq!(
                canonical(&decoded),
                bytes,
                "{at}: re-serialization drifted from the stored bytes"
            );
        }
    }
}

/// `ViewOp` is what a client signs, so a client that rebuilds "the same"
/// op must produce the same bytes. Covered here over every variant with a
/// generated body, complementing the fixed vectors in `golden.rs`.
#[test]
fn view_ops_round_trip_byte_for_byte() {
    for seed in SEEDS {
        let mut rng = Rng::new(seed);
        for case in 0..8 {
            let at = format!("seed {seed:#x} case {case}");
            let id = format!("review-{}", rng.next_u64() % 1_000);
            let ops = [
                ViewOp::new(OpKind::SetWorkspaceHead {
                    workspace: rng.path(),
                    commit: rng.hash(),
                    prev: Some(rng.hash()),
                }),
                ViewOp::new(OpKind::SetRef {
                    name: format!("repo.git:refs/heads/{}", rng.path()),
                    commit: rng.hash(),
                    prev: None,
                }),
                ViewOp::new(OpKind::DeleteRef {
                    name: format!("repo.git:refs/heads/{}", rng.path()),
                    prev: Some(rng.hash()),
                }),
                ViewOp::new(OpKind::RequestReview {
                    id: id.clone(),
                    target: rng.hash(),
                    reviewers: vec!["alice".into(), "bob".into()],
                    target_ref: None,
                }),
                ViewOp::new(OpKind::PostVerdict {
                    id: id.clone(),
                    reviewer: "alice".into(),
                    verdict: Verdict::Approve,
                    note: rng.path(),
                }),
                ViewOp::new(OpKind::BindKey {
                    operator: "alice".into(),
                    key: rng.hash(),
                    channel: None,
                }),
            ];
            for op in &ops {
                let bytes = canonical(op);
                let decoded: ViewOp = serde_json::from_str(&bytes).expect("op decodes");
                assert_eq!(
                    canonical(&decoded),
                    bytes,
                    "{at}: op re-serialization drifted"
                );
            }
            // The additive fields stay absent when unset, on every draw —
            // that is what keeps a pre-schema-edit client's bytes valid.
            assert!(
                !canonical(&ops[3]).contains("target_ref"),
                "{at}: unbound RequestReview emitted the additive field"
            );
            assert!(
                !canonical(&ops[5]).contains("channel"),
                "{at}: channelless BindKey emitted the additive field"
            );
        }
    }
}
