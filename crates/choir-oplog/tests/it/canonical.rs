//! Canonicalization properties: what has to hold for *every* entry, not
//! just the frozen examples.
//!
//! `choir-view/tests/it/golden.rs` pins a handful of hand-built values to
//! exact bytes. That catches a deliberate format edit, which is what it is
//! for. It cannot catch the other failure mode: a value the goldens never
//! contain — an empty channel, a payload with a newline in it, a channel
//! carrying a quote or a NUL — that serializes in a way the format did not
//! intend. Determinism here is a convention (`serde_json` emits struct
//! fields in declaration order and `BTreeMap` keys in sorted order), and a
//! convention is exactly the thing worth testing over random input.
//!
//! These are properties, checked over adversarially generated entries. The
//! generator is a hand-rolled xorshift (house convention: tests that need
//! randomness do not pull `rand`) seeded from a fixed list, so a failure
//! reproduces exactly and the suite never flakes. Every assertion names the
//! seed and case index that produced it.
//!
//! The five properties, and what breaks in production if one stops holding:
//!
//! 1. `content_hash` == BLAKE3 over the canonical bytes. Anything that
//!    hashes an entry by another route (a streaming serializer) must agree.
//! 2. Decode-then-encode reproduces the bytes. `FileLog::open` recomputes
//!    the head by re-hashing decoded entries; if this fails, reopening a
//!    log yields a different head and the next append is a `HeadMismatch`.
//! 3. Distinct entries get distinct hashes, and every field is covered.
//! 4. Canonical bytes never contain a raw newline. `FileLog` is JSON-lines;
//!    an entry that embeds one splits into two undecodable records.
//! 5. `signing_hash` covers `(channel, payload)` and nothing else, with no
//!    ambiguity between where the channel ends and the payload begins.

use choir_hash::ContentHash;
use choir_oplog::{signing_hash, FileLog, OpEntry, OpLog, Witness, FORMAT_VERSION};

/// Seeds the properties run under. Fixed, so this suite is deterministic;
/// several, so one unlucky stream cannot hide a break. Add a seed, never
/// randomize one — a property test that flakes teaches nobody anything.
const SEEDS: [u64; 4] = [0x0123_4567_89ab_cdef, 1, 0xdead_beef, 0xffff_ffff_ffff_ffff];

/// Cases per seed. Cheap enough (a few hundred small serializations) that
/// the whole file runs in well under a second.
const CASES: usize = 128;

/// xorshift64*. Not cryptographic and does not need to be: the job is a
/// wide spread of shapes, reproducibly.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // xorshift is stuck at zero, and any other state is fine.
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

    /// Bytes with no structure at all, including `\n`, `\0` and 0xff.
    fn payload(&mut self) -> Vec<u8> {
        let len = self.below(40);
        (0..len).map(|_| (self.next_u64() >> 13) as u8).collect()
    }

    /// A channel name drawn from characters that are hostile to JSON
    /// escaping, line framing, and UTF-8 assumptions — the classes that a
    /// hand-written golden vector would never think to include.
    fn text(&mut self) -> String {
        const ALPHABET: [char; 14] = [
            'a', 'Z', '0', '/', '-', ' ', '"', '\\', '\n', '\t', '\u{0}', '\u{7f}', 'é', '𝄞',
        ];
        let len = self.below(12);
        (0..len).map(|_| ALPHABET[self.below(ALPHABET.len())]).collect()
    }

    fn hash(&mut self) -> ContentHash {
        ContentHash::blake3(&self.next_u64().to_le_bytes())
    }

    fn witness(&mut self) -> Witness {
        Witness::ed25519(self.text(), self.payload())
    }

    fn option<T>(&mut self, value: T) -> Option<T> {
        if self.next_u64() & 1 == 0 {
            Some(value)
        } else {
            None
        }
    }

    /// An entry with every optional shape reachable: absent and present
    /// parent, zero to two witnesses, signed and unsigned.
    fn entry(&mut self) -> OpEntry {
        let parent = self.hash();
        let witness_count = self.below(3);
        let sig = self.witness();
        OpEntry {
            format_version: FORMAT_VERSION,
            parent: self.option(parent),
            seq: self.next_u64(),
            channel: self.text(),
            payload: self.payload(),
            witnesses: (0..witness_count).map(|_| self.witness()).collect(),
            author_sig: self.option(sig),
        }
    }
}

/// Canonical bytes of a value, as everything in the workspace computes
/// them.
fn canonical<T: serde::Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).expect("hashed shapes always serialize")
}

/// Runs `check` over `CASES` generated entries for every seed, labelling
/// failures with the seed and index that produced them.
fn for_each_entry(mut check: impl FnMut(&OpEntry, String)) {
    for seed in SEEDS {
        let mut rng = Rng::new(seed);
        for case in 0..CASES {
            let entry = rng.entry();
            check(&entry, format!("seed {seed:#x} case {case}"));
        }
    }
}

/// Property 0: the generator earns its keep.
///
/// Every property below is only as strong as the input it sees, and a
/// property over inputs that never contain the hard case is a test that
/// passes for no reason. Trimming a character out of the alphabet, or
/// shrinking a length bound, could empty the interesting cases without
/// failing anything — so the coverage the other tests depend on is
/// asserted here rather than assumed.
#[test]
fn the_generator_produces_the_hostile_cases() {
    let (mut newline, mut quote, mut nul, mut wide, mut empty_channel) =
        (false, false, false, false, false);
    let (mut newline_byte, mut high_byte, mut empty_payload) = (false, false, false);
    let (mut genesis, mut chained, mut signed, mut unsigned, mut witnessed) =
        (false, false, false, false, false);
    for_each_entry(|entry, _| {
        newline |= entry.channel.contains('\n');
        quote |= entry.channel.contains('"');
        nul |= entry.channel.contains('\u{0}');
        wide |= entry.channel.contains('𝄞');
        empty_channel |= entry.channel.is_empty();
        newline_byte |= entry.payload.contains(&b'\n');
        high_byte |= entry.payload.iter().any(|b| *b > 0x7f);
        empty_payload |= entry.payload.is_empty();
        genesis |= entry.parent.is_none();
        chained |= entry.parent.is_some();
        signed |= entry.author_sig.is_some();
        unsigned |= entry.author_sig.is_none();
        witnessed |= !entry.witnesses.is_empty();
    });
    for (covered, what) in [
        (newline, "a channel containing a newline"),
        (quote, "a channel containing a quote"),
        (nul, "a channel containing a NUL"),
        (wide, "a channel containing a non-BMP character"),
        (empty_channel, "an empty channel"),
        (newline_byte, "a payload containing a newline byte"),
        (high_byte, "a payload containing a non-ASCII byte"),
        (empty_payload, "an empty payload"),
        (genesis, "an entry with no parent"),
        (chained, "an entry with a parent"),
        (signed, "a signed entry"),
        (unsigned, "an unsigned entry"),
        (witnessed, "an entry carrying witnesses"),
    ] {
        assert!(covered, "the corpus never produced {what}");
    }
}

/// Property 1. `content_hash` is defined as BLAKE3 over the canonical
/// serialization, and a second implementation of that definition has to
/// agree with it — including the empty and escape-heavy cases.
///
/// This is the assertion that a streaming serializer (serde straight into
/// the hasher, skipping the intermediate `Vec<u8>`) would have to keep
/// passing to be a legal optimization.
#[test]
fn content_hash_is_blake3_over_the_canonical_bytes() {
    for_each_entry(|entry, at| {
        assert_eq!(
            entry.content_hash(),
            ContentHash::blake3(&canonical(entry)),
            "{at}: content_hash diverged from hashing the canonical bytes"
        );
    });
}

/// Property 2, the load-bearing one for persistence. `FileLog::open`
/// rebuilds its head by decoding each stored line and re-hashing the
/// decoded entry. That is only sound if decode-then-encode is the identity
/// on bytes; otherwise a reopened log reports a head no writer ever
/// produced, and the next append fails its parent check forever.
#[test]
fn decoding_and_re_encoding_reproduces_the_bytes() {
    for_each_entry(|entry, at| {
        let bytes = canonical(entry);
        let decoded: OpEntry = serde_json::from_slice(&bytes).expect("canonical bytes decode");
        assert_eq!(&decoded, entry, "{at}: value did not round-trip");
        assert_eq!(
            canonical(&decoded),
            bytes,
            "{at}: re-serialization drifted from the stored bytes"
        );
        assert_eq!(
            decoded.content_hash(),
            entry.content_hash(),
            "{at}: hash moved across a round-trip"
        );
    });
}

/// Property 3a. Two entries that differ at all must hash differently —
/// stated over the generated corpus, where value collisions are the thing
/// being ruled out rather than assumed.
///
/// A failure here means either the hash is not injective over the shapes we
/// actually store, or serialization is dropping a field.
#[test]
fn distinct_entries_get_distinct_hashes() {
    let mut seen: std::collections::HashMap<String, OpEntry> = std::collections::HashMap::new();
    for_each_entry(|entry, at| {
        let hex = entry.content_hash().to_hex();
        if let Some(previous) = seen.get(&hex) {
            assert_eq!(
                previous, entry,
                "{at}: two different entries share hash {hex}"
            );
        } else {
            seen.insert(hex, entry.clone());
        }
    });
    assert!(seen.len() > 1, "the generator produced no variety");
}

/// Property 3b. Every field of `OpEntry` is inside the hash, so no part of
/// a stored entry can be edited without invalidating its address. Stated
/// per field rather than in aggregate, so a break names the field it lost.
#[test]
fn every_field_is_covered_by_the_entry_hash() {
    for_each_entry(|entry, at| {
        let base = entry.content_hash();
        let mutations: [(&str, OpEntry); 6] = [
            ("format_version", {
                let mut e = entry.clone();
                e.format_version = e.format_version.wrapping_add(1);
                e
            }),
            ("parent", {
                let mut e = entry.clone();
                e.parent = match e.parent {
                    None => Some(ContentHash::blake3(b"a parent appears")),
                    Some(_) => None,
                };
                e
            }),
            ("seq", {
                let mut e = entry.clone();
                e.seq = e.seq.wrapping_add(1);
                e
            }),
            ("channel", {
                let mut e = entry.clone();
                e.channel.push('x');
                e
            }),
            ("payload", {
                let mut e = entry.clone();
                e.payload.push(0xff);
                e
            }),
            ("witnesses", {
                let mut e = entry.clone();
                e.witnesses.push(Witness::ed25519("extra", vec![7]));
                e
            }),
        ];
        for (field, mutated) in mutations {
            assert_ne!(
                mutated.content_hash(),
                base,
                "{at}: changing `{field}` left the hash unchanged"
            );
        }
        // `author_sig` is additive (`skip_serializing_if`), so it gets the
        // stronger pair of assertions: present changes the hash, and absent
        // is omitted entirely rather than encoded as `null` — an entry
        // written before L8 existed must re-serialize byte-identically.
        let mut toggled = entry.clone();
        toggled.author_sig = match &entry.author_sig {
            None => Some(Witness::ed25519("author", vec![1, 2, 3])),
            Some(_) => None,
        };
        assert_ne!(
            toggled.content_hash(),
            base,
            "{at}: toggling `author_sig` left the hash unchanged"
        );
        let unsigned = if entry.author_sig.is_none() {
            entry.clone()
        } else {
            toggled
        };
        assert!(
            !String::from_utf8_lossy(&canonical(&unsigned)).contains("author_sig"),
            "{at}: an unsigned entry emitted the additive field"
        );
    });
}

/// Property 4. `FileLog` stores one entry per line, so a canonical form
/// containing a raw `\n` would split a single entry into two records that
/// neither decode nor re-hash. JSON escapes control characters inside
/// strings and encodes `payload` as a number array, which is *why* this
/// holds — but nothing in the type system says so, and a future switch to a
/// bytes-as-string encoding would break it silently.
///
/// **Unproven by mutation, but the input is reachable.** No non-invasive
/// edit makes this fail, so unlike its neighbours it has not been shown to
/// catch anything. It is kept because the hostile value is not exotic:
/// [`OpEntry::channel`] is a `String` written straight from the client's
/// submission (`choir-node` platform, submit path) with no rejection of
/// control characters, and it lands in the line-framed log verbatim. A
/// submitter can therefore put a newline in a field that shares a line
/// with every other field of the entry. That is the input this test
/// drives, through a real `FileLog` and a real reopen, below.
///
/// A file path can carry one too — git permits any byte in a path except
/// NUL and `/`, so `nl\nhere.txt` is a legal filename — but that lands in
/// a `Commit` tree key, which reaches the chunk store rather than this
/// log, so it is a canonicalization concern and not a framing one. The
/// framing exposure is the channel string.
///
/// **This half cannot fail, and that is stated rather than hidden.** It
/// asserts a property of `serde_json` — that it escapes control characters
/// inside strings — which no edit to this workspace can change. It is kept
/// as the executable form of *why* line framing is safe, so a future
/// encoder swap has something to break. The half with teeth is
/// [`a_log_of_newline_bearing_channels_reopens_intact`], which was proven
/// by making `FileLog` pretty-print.
#[test]
fn canonical_bytes_never_contain_a_raw_newline() {
    for_each_entry(|entry, at| {
        assert!(
            !canonical(entry).contains(&b'\n'),
            "{at}: canonical bytes contain a raw newline, which breaks line framing"
        );
    });
}

/// The proven half: entries whose channel really holds a newline survive a
/// write and a reopen of a real `FileLog`.
///
/// Mutation-verified, unlike its neighbour above. Changing `FileLog` to
/// `serde_json::to_vec_pretty` — a plausible "make the log readable" edit
/// — fails this with `Corrupt("EOF while parsing an object")` on reopen,
/// because one entry has become many lines. Five pre-existing durability
/// tests catch that too, so this is not the only guard; it is the one that
/// states the reason.
#[test]
fn a_log_of_newline_bearing_channels_reopens_intact() {
    let dir = std::env::temp_dir().join(format!(
        "choir-canonical-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let path = dir.join("ops.jsonl");
    let mut head = None;
    {
        let mut log = FileLog::open(&path).expect("open");
        let mut rng = Rng::new(SEEDS[0]);
        for _ in 0..16 {
            let mut entry = rng.entry();
            entry.parent = head.clone();
            entry.channel = format!("line\nbreak\t\"{}\"", entry.channel);
            head = Some(log.append(entry).expect("append"));
        }
        log.sync().expect("sync");
    }
    let reopened = FileLog::open(&path).expect("reopen");
    assert_eq!(reopened.len(), 16, "line framing lost or invented records");
    assert_eq!(
        reopened.head(),
        head,
        "reopened head differs from the head the writer computed"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Property 5a. The author signs *what* they submitted, never *where* it
/// landed. So `signing_hash` must be blind to `seq`, `parent`, witnesses
/// and the author signature itself — otherwise a signature could only be
/// produced after sequencing, which is not the protocol.
#[test]
fn signing_hash_ignores_everything_but_channel_and_payload() {
    for_each_entry(|entry, at| {
        let mut repositioned = entry.clone();
        repositioned.seq = entry.seq.wrapping_add(1_000);
        repositioned.parent = Some(ContentHash::blake3(b"a different position"));
        repositioned.witnesses = vec![Witness::ed25519("witness", vec![4, 5, 6])];
        repositioned.author_sig = None;
        assert_eq!(
            repositioned.signing_hash(),
            entry.signing_hash(),
            "{at}: signing_hash moved when only the entry's position changed"
        );
        assert_eq!(
            entry.signing_hash(),
            signing_hash(&entry.channel, &entry.payload),
            "{at}: the method and the standalone function disagree"
        );
    });
}

/// Property 5b. The other half: the two signed fields must be
/// distinguishable from each other. If the encoding were a concatenation,
/// `("ab", "c")` and `("a", "bc")` would sign the same bytes, and an
/// attacker could move content across the boundary under a valid
/// signature. Checked over every split of a fixed string, so the boundary
/// is exercised at both ends as well as in the middle.
#[test]
fn moving_bytes_across_the_channel_payload_boundary_changes_the_signature() {
    let subject = "agent-1/workspace";
    let mut seen = std::collections::HashSet::new();
    for split in 0..=subject.len() {
        if !subject.is_char_boundary(split) {
            continue;
        }
        let (channel, payload) = subject.split_at(split);
        let hex = signing_hash(channel, payload.as_bytes()).to_hex();
        assert!(
            seen.insert(hex),
            "split at {split} signs the same bytes as an earlier split"
        );
    }
    assert!(seen.len() > 2, "the split loop did not run");

    // The same property over generated pairs: changing either signed field
    // must move the hash.
    for_each_entry(|entry, at| {
        assert_ne!(
            signing_hash(&format!("{}x", entry.channel), &entry.payload),
            entry.signing_hash(),
            "{at}: extending the channel left the signing hash unchanged"
        );
        let mut payload = entry.payload.clone();
        payload.push(0xff);
        assert_ne!(
            signing_hash(&entry.channel, &payload),
            entry.signing_hash(),
            "{at}: extending the payload left the signing hash unchanged"
        );
    });
}

/// Invariant 2, as a property rather than an example: the codec byte is
/// part of the identity, so the same digest bytes under two hash functions
/// are two different addresses. Without this, a git SHA-256 oid and a
/// BLAKE3 digest that happened to coincide would be indistinguishable —
/// which is the whole reason the envelope carries a codec.
#[test]
fn the_codec_byte_separates_equal_digests() {
    for seed in SEEDS {
        let mut rng = Rng::new(seed);
        for case in 0..CASES {
            let at = format!("seed {seed:#x} case {case}");
            let blake = rng.hash();
            // Re-enter the same 32 digest bytes as a git SHA-256 oid.
            let hex: String = blake.to_hex()["1e-".len()..].to_string();
            let git = ContentHash::from_git_oid(&hex).expect("64 hex chars is a sha-256 oid");
            assert_eq!(git.digest, blake.digest, "{at}: hex round-trip lost bytes");
            assert_ne!(git, blake, "{at}: codecs collapsed into one identity");
            assert_ne!(
                git.to_hex(),
                blake.to_hex(),
                "{at}: two codecs rendered identically"
            );
            assert_eq!(git.to_hex(), format!("12-{hex}"), "{at}: hex drifted");
        }
    }
}

/// `from_git_oid` accepts exactly the two oid widths git can produce, and
/// nothing else. A silent `None` here would drop a ref update; a silent
/// `Some` would put a malformed address in the log.
#[test]
fn only_git_oid_widths_enter_the_envelope() {
    let mut rng = Rng::new(SEEDS[0]);
    for case in 0..CASES {
        let at = format!("case {case}");
        let hex: String = (0..64)
            .map(|_| char::from_digit(rng.below(16) as u32, 16).expect("hex digit"))
            .collect();
        assert_eq!(
            ContentHash::from_git_oid(&hex[..40]).map(|h| h.codec),
            Some(0x11),
            "{at}: 40 hex chars must read as sha-1"
        );
        assert_eq!(
            ContentHash::from_git_oid(&hex).map(|h| h.codec),
            Some(0x12),
            "{at}: 64 hex chars must read as sha-256"
        );
        for len in [0, 1, 39, 41, 63, 65] {
            let candidate: String = hex.chars().cycle().take(len).collect();
            assert!(
                ContentHash::from_git_oid(&candidate).is_none(),
                "{at}: {len} hex chars is not an oid width"
            );
        }
        // Right width, wrong alphabet.
        let mut bad = hex[..39].to_string();
        bad.push('z');
        assert!(
            ContentHash::from_git_oid(&bad).is_none(),
            "{at}: non-hex must not decode"
        );
    }
}

/// D39's format change is additive, and "additive" here means something
/// stricter than "old logs still decode": every entry written before the
/// change must re-serialize to the *same bytes* and therefore keep the
/// *same hash*. A signature is inside `OpEntry`, so a `Witness` that
/// gained a serialized field would silently rewrite the hash of every
/// signed entry ever stored — invariant 1 and invariant 3 at once.
///
/// Checked over the generated entries rather than one example, because
/// the failure would be uniform and a single hand-built case proves the
/// least interesting instance of it.
#[test]
fn the_d39_witness_fields_change_no_pre_d39_byte() {
    for_each_entry(|entry, at| {
        let bytes = canonical(entry);
        let text = String::from_utf8_lossy(&bytes);
        for field in ["scheme", "authenticator_data", "client_data_json"] {
            assert!(
                !text.contains(field),
                "{at}: an ed25519 entry emitted the additive field `{field}`"
            );
        }
        // And the round trip still holds through the wider struct: a
        // decoder that filled `scheme` with a default on the way in
        // would pass the check above and fail here.
        let back: OpEntry = serde_json::from_slice(&bytes).expect("decodes");
        assert_eq!(canonical(&back), bytes, "{at}: re-encode changed the bytes");
        assert_eq!(back.content_hash(), entry.content_hash(), "{at}: hash moved");
    });
}

/// A `Witness` stored before D39 has no `scheme` member at all, and the
/// absence has to mean ed25519 rather than "unknown". Written as literal
/// JSON on purpose: constructing one through the current struct cannot
/// reproduce a document written by an older binary, which is the only
/// input this property is about.
#[test]
fn a_witness_without_a_scheme_reads_as_ed25519_and_survives_a_round_trip() {
    let stored = br#"{"key_id":"1e-abc","signature":[1,2,3]}"#;
    let w: Witness = serde_json::from_slice(stored).expect("a pre-D39 witness decodes");
    assert_eq!(w.scheme, None, "no tag was written, so none is read");
    assert_eq!(
        w.scheme_id(),
        choir_oplog::scheme::ED25519,
        "an absent tag must resolve to the only scheme that existed"
    );
    assert_eq!(
        canonical(&w),
        stored.to_vec(),
        "a pre-D39 witness must re-serialize to the byte string it was stored as"
    );
}

/// Measured rather than assumed, because D39 rests on it: a `Witness`
/// carrying a member this binary has never heard of still decodes. No
/// struct in this workspace sets `deny_unknown_fields`, so a *new field*
/// is backward-compatible where a new enum variant would not be — the
/// distinction D35 records. If this ever fails, an old reader can no
/// longer replay a log a newer writer produced, which is a migration
/// rather than a release.
#[test]
fn a_witness_from_a_newer_writer_still_decodes() {
    let future = br#"{"key_id":"k","signature":[9],"scheme":2,"a_field_from_2027":{"x":1}}"#;
    let w: Witness = serde_json::from_slice(future).expect("an unknown member must not stop a read");
    assert_eq!(w.scheme_id(), choir_oplog::scheme::WEBAUTHN_ES256);
    assert_eq!(w.signature, vec![9]);
}

/// An unrecognised scheme decodes too, and this is the deliberate half of
/// the same decision: replay must not stop at a signature this binary
/// cannot check. The refusal belongs at verification, where the caller
/// knows whether an unverifiable signature matters, and never at decode,
/// where refusing means an old node cannot read the log at all.
#[test]
fn an_unknown_scheme_decodes_and_is_reported_verbatim() {
    let w: Witness =
        serde_json::from_slice(br#"{"key_id":"k","signature":[1],"scheme":40000}"#).expect("decodes");
    assert_eq!(w.scheme_id(), 40_000, "the tag is reported, not normalised");
}
