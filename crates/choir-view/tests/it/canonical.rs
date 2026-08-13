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
use choir_view::{
    ArchiveAuthorization, Commit, CreateAuthorization, OpKind, RefSnapshot, TreeEntry, Verdict,
    ViewOp, FORMAT_VERSION,
};

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

    /// A content address under any of the three codecs the envelope
    /// carries, with the digest width that codec really has: 20 bytes for
    /// git SHA-1, 32 for git SHA-256 and BLAKE3.
    ///
    /// Varying the codec matters because git oids genuinely appear inside
    /// persisted shapes — `SetRef` carries whatever git reported, via
    /// [`ContentHash::from_git_oid`] — so a hashed struct holding a
    /// non-BLAKE3 address is the normal case, not an exotic one. A
    /// generator that only ever embeds BLAKE3 would never serialize a
    /// 20-byte digest, which is the case that rules out narrowing the
    /// field to `[u8; 32]`.
    fn hash(&mut self) -> ContentHash {
        let codec = [0x1e_u8, 0x11, 0x12][self.below(3)];
        let width = if codec == 0x11 { 20 } else { 32 };
        let mut digest = Vec::with_capacity(width);
        while digest.len() < width {
            digest.extend_from_slice(&self.next_u64().to_le_bytes());
        }
        digest.truncate(width);
        ContentHash { codec, digest }
    }

    /// Fisher-Yates. Insertion-order tests need real permutations: a
    /// reversal and a rotation are two of the 16! orders, and both preserve
    /// adjacency, so they are the orders least likely to expose a map that
    /// keys off neighbouring inserts.
    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }

    /// A repo-shaped path. Shared directory prefixes matter: they make
    /// sorted order and any other order differ in more places than a set of
    /// unrelated names would.
    ///
    /// Some names carry characters JSON has to escape, because real ones
    /// do: git permits any byte in a path except NUL and `/`, so a quote,
    /// a backslash, a newline and a tab are all legal in a filename and
    /// therefore in a tree key. Verified rather than assumed — `git add`
    /// stores all four without complaint. The escaped form is thus part of
    /// this format's canonical form, not adversarial exotica, and it had
    /// no coverage while every generated key was plain ASCII.
    ///
    /// NUL is the one byte git refuses, so it is deliberately absent.
    ///
    /// This is only affordable because
    /// [`tree_keys_are_emitted_in_sorted_order`] looks keys up by their
    /// *encoded* spelling; searching for the raw key would report a hostile
    /// key as missing from output that in fact contains it.
    fn path(&mut self) -> String {
        const DIRS: [&str; 6] = ["src", "src/bin", "tests", "docs", "crates/a", ""];
        const NAMES: [&str; 6] = [
            "lib.rs",
            "ma\"in.rs",
            "back\\slash.rs",
            "nl\nhere.rs",
            "tab\there.rs",
            ".hidden",
        ];
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

/// Builds a commit around `pairs`, with `parents` parent ids.
///
/// The arity is a parameter because `Commit::parents` documents "0 = root,
/// 2+ = merge", and a generator that always supplies exactly one covers
/// neither. An empty `Vec` and a multi-entry one serialize differently
/// (`[]` versus a populated array), and a root commit is the first thing
/// any history contains.
fn commit_from(pairs: &[(String, TreeEntry)], parents: Vec<ContentHash>) -> Commit {
    Commit {
        format_version: FORMAT_VERSION,
        parents,
        // The map type is inferred from the field, never named here: a
        // test that guards the choice of map must not restate it, or
        // swapping the field breaks compilation instead of failing.
        tree: pairs.iter().cloned().collect(),
        author: "agent-1".into(),
        message: "property".into(),
    }
}

/// Parent lists spanning every arity the field's own documentation names:
/// root, linear, and a merge.
fn parent_arities(rng: &mut Rng) -> Vec<Vec<ContentHash>> {
    vec![
        Vec::new(),
        vec![rng.hash()],
        vec![rng.hash(), rng.hash()],
        vec![rng.hash(), rng.hash(), rng.hash()],
    ]
}

fn canonical<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("hashed shapes always serialize")
}

/// The generator earns its keep, same discipline as the sibling suites:
/// a property is only as strong as the corpus it runs over, and both of
/// these are shapes the rest of the workspace never builds.
///
/// Merge commits especially. Every other call site in the suite passes
/// zero or one parent, so a two-element `parents` array had never been
/// serialized anywhere before this file — despite the field documenting
/// `2+ = merge`. Root commits, by contrast, were already covered in
/// `view.rs`; this keeps them here so the arity loop is not silently
/// reduced to the linear case later.
#[test]
fn the_generator_produces_merge_commits_and_every_codec() {
    let mut codecs = std::collections::BTreeSet::new();
    let mut widths = std::collections::BTreeSet::new();
    let mut arities = std::collections::BTreeSet::new();
    for seed in SEEDS {
        let mut rng = Rng::new(seed);
        for _ in 0..8 {
            for (path, entry) in rng.tree_pairs() {
                assert!(!path.is_empty(), "a tree key must not be empty");
                for h in hashes_in(&entry) {
                    codecs.insert(h.codec);
                    widths.insert(h.digest.len());
                }
            }
            for parents in parent_arities(&mut rng) {
                arities.insert(parents.len());
            }
        }
    }
    assert_eq!(
        codecs,
        [0x11, 0x12, 0x1e].into_iter().collect(),
        "the corpus must embed git SHA-1, git SHA-256 and BLAKE3 addresses"
    );
    assert_eq!(
        widths,
        [20, 32].into_iter().collect(),
        "a 20-byte digest is the case that rules out narrowing the field to [u8; 32]"
    );
    assert!(
        arities.contains(&0) && arities.contains(&2),
        "expected root and merge commits, got arities {arities:?}"
    );
}

/// Every content address inside one tree entry.
fn hashes_in(entry: &TreeEntry) -> Vec<&ContentHash> {
    match entry {
        TreeEntry::File { blob } => vec![blob],
        TreeEntry::Conflict { base, left, right } => {
            let mut v = vec![left, right];
            v.extend(base.as_ref());
            v
        }
    }
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
            let parents = vec![rng.hash()];
            let forward = commit_from(&pairs, parents.clone());

            // Reversal and rotation are structured orders; the shuffles are
            // arbitrary ones. Both are kept: the structured pair states the
            // property in a form a reader can check by eye, and the shuffles
            // search orders neither of them reaches.
            let mut reversed = pairs.clone();
            reversed.reverse();
            let mut rotated = pairs.clone();
            rotated.rotate_left(1 + rng.below(TREE_SIZE - 1));

            let mut others = vec![
                ("reversing", commit_from(&reversed, parents.clone())),
                ("rotating", commit_from(&rotated, parents.clone())),
            ];
            for _ in 0..4 {
                let mut shuffled = pairs.clone();
                rng.shuffle(&mut shuffled);
                others.push(("shuffling", commit_from(&shuffled, parents.clone())));
            }

            for (how, other) in &others {
                assert_eq!(
                    canonical(&forward),
                    canonical(other),
                    "{at}: {how} the insertion order changed the canonical bytes"
                );
                assert_eq!(
                    ContentHash::blake3(canonical(&forward).as_bytes()),
                    ContentHash::blake3(canonical(other).as_bytes()),
                    "{at}: the commit address depends on how its tree was built"
                );
            }
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
    // Tracked separately from the quote/backslash class. A control
    // character in a key is the case that would break line framing if an
    // encoder ever emitted it raw, and git allows it in a path, so losing
    // it from the corpus is the more expensive silent regression.
    let mut saw_control_char_key = false;
    for seed in SEEDS {
        let mut rng = Rng::new(seed);
        for case in 0..8 {
            let at = format!("seed {seed:#x} case {case}");
            let pairs = rng.tree_pairs();
            let bytes = canonical(&commit_from(&pairs, vec![rng.hash()]));

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
                saw_control_char_key |= key.chars().any(|c| c.is_control());
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
    assert!(
        saw_control_char_key,
        "no generated tree key held a control character; git allows them in \
         paths, and they are the class that would break line framing if an \
         encoder emitted them raw"
    );
}

/// A commit must survive the store round-trip byte-for-byte, since its
/// address is those bytes. `Commit::get` re-decodes what `Commit::put`
/// wrote, and every later re-encode has to reproduce it.
///
/// Run across every parent arity, so root and merge commits are covered
/// rather than only the linear case.
#[test]
fn commits_round_trip_byte_for_byte() {
    let mut arities_seen = 0;
    for seed in SEEDS {
        let mut rng = Rng::new(seed);
        for case in 0..8 {
            let pairs = rng.tree_pairs();
            for parents in parent_arities(&mut rng) {
                let at = format!("seed {seed:#x} case {case} parents {}", parents.len());
                arities_seen |= 1 << parents.len();
                let commit = commit_from(&pairs, parents);
                let bytes = canonical(&commit);
                let decoded: Commit =
                    serde_json::from_str(&bytes).expect("canonical bytes decode");
                assert_eq!(decoded, commit, "{at}: commit did not round-trip");
                assert_eq!(
                    canonical(&decoded),
                    bytes,
                    "{at}: re-serialization drifted from the stored bytes"
                );
            }
        }
    }
    assert_eq!(
        arities_seen, 0b1111,
        "expected commits with 0, 1, 2 and 3 parents; a root or a merge was never built"
    );
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
                ViewOp::new(OpKind::RecordRefSnapshot {
                    snapshot: RefSnapshot {
                        format_version: FORMAT_VERSION,
                        refs: (0..4)
                            .map(|_| {
                                (format!("repo.git:refs/heads/{}", rng.path()), rng.hash())
                            })
                            .collect(),
                        at_seq: rng.next_u64() % 1_000,
                        prev_snapshot: None,
                    },
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
            assert!(
                !canonical(&ops[6]).contains("prev_snapshot"),
                "{at}: a first snapshot emitted the additive chain field"
            );
        }
    }
}

/// The D25 snapshot is a signature over canonical bytes whose map keys are
/// ref names — legal carriers of quotes and non-ASCII bytes in git's own
/// grammar, and arbitrary strings through the signed API. An attestation
/// that serialized differently for a differently-built map would be an
/// equivocation detector that cries wolf, so order independence is pinned
/// here over hostile names, complementing `golden.rs`'s fixed vector.
#[test]
fn ref_snapshots_serialize_independent_of_insertion_order() {
    for seed in SEEDS {
        let mut rng = Rng::new(seed);
        for case in 0..8 {
            let at = format!("seed {seed:#x} case {case}");
            let mut pairs: Vec<(String, ContentHash)> = (0..TREE_SIZE)
                .map(|_| (format!("repo.git:refs/heads/{}", rng.path()), rng.hash()))
                .collect();
            let snapshot_from = |pairs: &[(String, ContentHash)]| RefSnapshot {
                format_version: FORMAT_VERSION,
                refs: pairs.iter().cloned().collect(),
                at_seq: 7,
                prev_snapshot: Some(ContentHash::blake3(b"previous")),
            };
            let reference = snapshot_from(&pairs);
            let bytes = canonical(&reference);
            let decoded: RefSnapshot = serde_json::from_str(&bytes).expect("snapshot decodes");
            assert_eq!(
                canonical(&decoded),
                bytes,
                "{at}: snapshot re-serialization drifted"
            );
            assert_eq!(decoded.id(), reference.id(), "{at}: identity drifted");
            for _ in 0..3 {
                rng.shuffle(&mut pairs);
                assert_eq!(
                    canonical(&snapshot_from(&pairs)),
                    bytes,
                    "{at}: insertion order leaked into the attestation bytes"
                );
            }
        }
    }
}

/// The two lifecycle authorizations are what an owner signs, and the node
/// verifies that signature against bytes it rebuilds from the decoded
/// fields rather than against the bytes it was handed. So a rebuild must
/// reproduce the signed encoding exactly — otherwise a genuine
/// authorization verifies as a forgery — and two authorizations that
/// differ must never encode alike, or one owner's proof would authorize
/// another's request. Fixed vectors live in `golden.rs`; this covers
/// generated bodies, including field values carrying the `/` that the
/// surrounding identifiers are themselves built from.
#[test]
fn lifecycle_authorizations_round_trip_and_stay_distinct() {
    for seed in SEEDS {
        let mut rng = Rng::new(seed);
        for case in 0..8 {
            let at = format!("seed {seed:#x} case {case}");

            let create = CreateAuthorization::new(
                format!("change/{}", rng.path()),
                format!("operator/{}", rng.path()),
                format!("owner/repo/{}", rng.path()),
                rng.hash(),
                format!("request/{}", rng.path()),
            );
            let bytes = create.to_payload();
            let decoded = CreateAuthorization::from_payload(&bytes).expect("create decodes");
            assert_eq!(decoded, create, "{at}: create did not round-trip");
            assert_eq!(
                decoded.to_payload(),
                bytes,
                "{at}: rebuilt create bytes drifted from the signed ones"
            );

            // Moving the boundary between two adjacent string fields must
            // change the bytes. A concatenating encoder would collide here.
            let shifted = CreateAuthorization::new(
                format!("{}/{}", create.id, create.owner),
                String::new(),
                create.workspace.clone(),
                create.base_revision.clone(),
                create.idempotency_key.clone(),
            );
            assert_ne!(
                shifted.to_payload(),
                bytes,
                "{at}: a field boundary did not survive encoding"
            );

            let archive = ArchiveAuthorization::new(
                format!("change/{}", rng.path()),
                format!("owner/repo/{}", rng.path()),
                rng.hash(),
            );
            let bytes = archive.to_payload();
            let decoded = ArchiveAuthorization::from_payload(&bytes).expect("archive decodes");
            assert_eq!(decoded, archive, "{at}: archive did not round-trip");
            assert_eq!(
                decoded.to_payload(),
                bytes,
                "{at}: rebuilt archive bytes drifted from the signed ones"
            );

            let shifted = ArchiveAuthorization::new(
                format!("{}/{}", archive.id, archive.workspace),
                String::new(),
                archive.prev_revision.clone(),
            );
            assert_ne!(
                shifted.to_payload(),
                bytes,
                "{at}: a field boundary did not survive encoding"
            );
        }
    }
}
