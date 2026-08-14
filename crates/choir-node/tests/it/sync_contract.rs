//! `SYNC.md`, executed.
//!
//! The file tells another implementation how to catch up on
//! `GET /api/log?from=N` and how to check that what it was handed is
//! really the chain. A procedure nobody runs is a claim, not a contract,
//! so this test is the recipe transcribed from that document and applied
//! to a live node: canonical bytes rebuilt from the served JSON alone,
//! hashed, and compared against what the node asserted.
//!
//! It reconstructs the serialization by hand rather than calling
//! `OpEntry::content_hash`. Calling it would only prove the node agrees
//! with itself; the thing under test is whether the written-down rules
//! are enough for someone who has no Rust structs to serialize.

use choir_identity::{ActorKey, Registry};
use choir_node::platform::{hex_decode, hex_encode};
use choir_node::Platform;
use choir_oplog::{FileLog, OpEntry, OpLog, Witness, FORMAT_VERSION};
use choir_view::{OpKind, ViewOp};

/// Page size promised by `SYNC.md`. Not importable — `LOG_PAGE` is
/// private to the platform — so the number is pinned here and the test
/// below proves the node keeps it.
const PAGE: usize = 500;

/// Submits `n` always-applicable ops in batches, so seeding a log longer
/// than one page does not pay a durability barrier per op.
fn fill(platform: &Platform, key: &ActorKey, n: usize) {
    for chunk in 0..n.div_ceil(100) {
        let ops: Vec<serde_json::Value> = (chunk * 100..(chunk * 100 + 100).min(n))
            .map(|i| {
                let op = ViewOp::new(OpKind::RecordProvenance {
                    subject: "s".into(),
                    kind: format!("k{i}"),
                    body: format!("body {i}"),
                });
                let payload = op.to_payload();
                let sig = key.sign_submission("author", &payload);
                serde_json::json!({
                    "workspace": "author",
                    "payload_hex": hex_encode(&payload),
                    "key_id": sig.key_id,
                    "signature_hex": hex_encode(&sig.signature),
                })
            })
            .collect();
        let body = serde_json::json!({ "ops": ops }).to_string();
        let (status, out) = platform.handle_api("POST", "/api/submit-batch", body.as_bytes());
        assert_eq!(status, 200, "seed batch rejected: {out}");
    }
}

/// A `Vec<u8>` as `serde_json` writes it: an array of byte values, no
/// spaces. `SYNC.md` calls this out because the response carries the same
/// bytes as hex, and hashing the hex string instead of the bytes is the
/// obvious wrong turn.
fn byte_array(bytes: &[u8]) -> String {
    let mut s = String::from("[");
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&b.to_string());
    }
    s.push(']');
    s
}

/// A `ContentHash` in its hashed form: the codec byte is a field, never
/// dropped in favour of a bare digest (invariant 2).
fn content_hash_json(hex: &str) -> String {
    let (codec, digest) = hex.split_once('-').expect("hash is codec-digest");
    format!(
        "{{\"codec\":{},\"digest\":{}}}",
        u8::from_str_radix(codec, 16).expect("codec byte"),
        byte_array(&hex_decode(digest).expect("digest hex"))
    )
}

/// The canonical bytes of one entry, rebuilt from the served JSON by
/// following `SYNC.md` and nothing else.
fn canonical(e: &serde_json::Value) -> Vec<u8> {
    let mut s = format!("{{\"format_version\":{},\"parent\":", e["format_version"]);
    match e["parent"].as_str() {
        None => s.push_str("null"),
        Some(hex) => s.push_str(&content_hash_json(hex)),
    }
    s.push_str(&format!(
        ",\"seq\":{},\"workspace\":{},\"payload\":{},\"witnesses\":{}",
        e["seq"],
        e["workspace"],
        byte_array(&hex_decode(e["payload_hex"].as_str().expect("payload hex")).expect("hex")),
        e["witnesses"]
    ));
    // Omitted entirely when absent, not null: the field is
    // `skip_serializing_if`, which is what keeps pre-L8 entries hashing
    // the same after author signatures were added.
    if let (Some(key_id), Some(sig)) = (
        e["author_key"].as_str(),
        e["author_sig_hex"].as_str(),
    ) {
        s.push_str(&format!(
            ",\"author_sig\":{{\"key_id\":\"{}\",\"signature\":{}",
            key_id,
            byte_array(&hex_decode(sig).expect("signature hex"))
        ));
        // D39's fields, in declaration order, each omitted when absent
        // for the same reason `author_sig` itself is. A client that
        // stops here recomputes the wrong hash for a passkey-signed
        // entry — they are inside the canonical bytes.
        if let Some(scheme) = e["author_scheme"].as_u64() {
            s.push_str(&format!(",\"scheme\":{scheme}"));
        }
        for (served, field) in [
            ("authenticator_data_hex", "authenticator_data"),
            ("client_data_json_hex", "client_data_json"),
        ] {
            if let Some(hex) = e[served].as_str() {
                s.push_str(&format!(
                    ",\"{field}\":{}",
                    byte_array(&hex_decode(hex).expect("hex"))
                ));
            }
        }
        s.push('}');
    }
    s.push('}');
    s.into_bytes()
}

/// Checks one page internally: sequence numbers contiguous, every entry's
/// `parent` the previous entry's `hash`, and every `hash` the node
/// asserted actually the hash of the entry it was attached to. Returns
/// the last entry, so the caller can join it to the next page.
fn verify_page(entries: &[serde_json::Value], first_seq: u64) -> serde_json::Value {
    assert!(!entries.is_empty(), "page must not be empty here");
    for (i, e) in entries.iter().enumerate() {
        assert_eq!(
            e["seq"].as_u64(),
            Some(first_seq + i as u64),
            "sequence numbers must be contiguous from the cursor: {e}"
        );
        assert_eq!(
            choir_hash::ContentHash::blake3(&canonical(e)).to_hex(),
            e["hash"].as_str().expect("hash"),
            "recomputed hash disagrees with the node's at seq {}: SYNC.md's canonical form is \
             wrong or entry_json changed shape",
            e["seq"]
        );
        if i > 0 {
            assert_eq!(
                e["parent"].as_str(),
                entries[i - 1]["hash"].as_str(),
                "entry {} does not chain to its predecessor",
                e["seq"]
            );
        }
    }
    entries.last().expect("non-empty").clone()
}

/// The load-bearing case: two pages, served from *different sources*,
/// join into one chain a client can verify without trusting the node.
///
/// Crossing the boundary is where paging could break silently — page one
/// is replayed off disk, page two comes out of the in-memory window, and
/// a reader has no way to notice a seam except by checking that the first
/// entry of page two names the last entry of page one as its parent.
#[test]
fn pages_join_into_one_verifiable_chain_across_the_source_boundary() {
    let dir = std::env::temp_dir().join(format!("choir-sync-chain-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let log_path = dir.join("ops.jsonl");

    let key = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&key.public_key_bytes()).expect("valid key");
    let log = FileLog::open(&log_path).expect("open log");
    let platform = Platform::start(registry, Box::new(log), ActorKey::generate())
        .expect("platform starts")
        // Small enough that the first page falls off the back of the
        // window and has to be replayed from disk.
        .with_log_window_cap(100)
        .with_log_path(log_path);

    let total = PAGE + 20;
    fill(&platform, &key, total);

    // Page one: behind the window, so it comes off the persisted log.
    let (status, out) = platform.handle_api("GET", "/api/log?from=0", b"");
    assert_eq!(status, 200, "{out}");
    let page1: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(page1["source"].as_str(), Some("log"), "{out}");
    let e1 = page1["entries"].as_array().expect("entries");
    assert_eq!(e1.len(), PAGE, "a page is capped at {PAGE} entries");
    assert!(
        e1[0]["parent"].is_null(),
        "seq 0 is the genesis and has no parent: {}",
        e1[0]
    );
    let last_of_page1 = verify_page(e1, 0);

    // Page two: the cursor is now inside the window, so the same reader
    // is answered from memory instead. Content must not care.
    let next = last_of_page1["seq"].as_u64().expect("seq") + 1;
    let (status, out) = platform.handle_api("GET", &format!("/api/log?from={next}"), b"");
    assert_eq!(status, 200, "{out}");
    let page2: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(
        page2["source"].as_str(),
        Some("window"),
        "the second page should now be inside the window: {out}"
    );
    let e2 = page2["entries"].as_array().expect("entries");
    assert_eq!(e2.len(), total - PAGE);

    // The join. This is the whole point of the file.
    assert_eq!(
        e2[0]["parent"].as_str(),
        last_of_page1["hash"].as_str(),
        "page two does not chain to page one across the window/log seam"
    );
    verify_page(e2, next);

    // And the end of the chain is the end of the log.
    let (status, out) = platform.handle_api("GET", &format!("/api/log?from={total}"), b"");
    assert_eq!(status, 200, "{out}");
    let tail: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert!(
        tail["entries"].as_array().expect("entries").is_empty(),
        "caught up means an empty page, not an error: {out}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Authorship, checked the way `SYNC.md` says a foreign client checks it:
/// build the signing bytes by hand, hash them, and verify the served
/// signature against a key held independently of the node.
#[test]
fn a_served_entry_verifies_against_a_key_the_client_already_holds() {
    let dir = std::env::temp_dir().join(format!("choir-sync-sig-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("scratch dir");

    let key = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&key.public_key_bytes()).expect("valid key");
    let log = FileLog::open(&dir.join("ops.jsonl")).expect("open log");
    let platform = Platform::start(registry, Box::new(log), ActorKey::generate())
        .expect("platform starts");
    fill(&platform, &key, 3);

    let (status, out) = platform.handle_api("GET", "/api/log?from=1", b"");
    assert_eq!(status, 200, "{out}");
    let page: serde_json::Value = serde_json::from_str(&out).expect("json");
    let e = &page["entries"][0];

    let workspace = e["workspace"].as_str().expect("workspace");
    let payload = hex_decode(e["payload_hex"].as_str().expect("payload")).expect("hex");

    // Step 1-2 of the procedure: the signing bytes are the JSON of the
    // two-element tuple `[workspace, payload]`, and the signing hash is
    // BLAKE3 over exactly those bytes. Built here from the document,
    // compared against the library that produced it.
    let signing_bytes = format!("[{},{}]", e["workspace"], byte_array(&payload));
    assert_eq!(
        choir_hash::ContentHash::blake3(signing_bytes.as_bytes()).to_hex(),
        choir_oplog::signing_hash(workspace, &payload).to_hex(),
        "SYNC.md's signing-bytes recipe does not reproduce signing_hash"
    );

    // Step 3: ed25519 over the ASCII of that hex string. The signature
    // check itself is the library's -- a test cannot reimplement ed25519
    // -- but the inputs it is handed all came out of the response.
    let mut client_side = Registry::new();
    client_side.register(&key.public_key_bytes()).expect("valid key");
    let sig = Witness::ed25519(
        e["author_key"].as_str().expect("author_key").to_string(),
        hex_decode(e["author_sig_hex"].as_str().expect("sig hex")).expect("hex"),
    );
    let author = client_side
        .verify_submission(workspace, &payload, &sig)
        .expect("served entry verifies against the author's key");
    assert_eq!(
        author.to_hex(),
        e["author_key"].as_str().expect("author_key"),
        "the id a verification returns is the one the response advertised"
    );

    // A tampered payload must not verify, or the check above proves
    // nothing about what the node served.
    let mut tampered = payload;
    tampered.push(b'!');
    assert!(
        client_side.verify_submission(workspace, &tampered, &sig).is_err(),
        "a modified payload must fail verification"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// The one branch of the canonical form no signed entry can reach.
///
/// `author_sig` is `skip_serializing_if`, so an unsigned entry's bytes
/// end `"witnesses":[]}` with no null anywhere — that omission is what
/// let author signatures be added at L8 without rewriting the hash of a
/// single existing entry. A client that wrote `"author_sig":null`
/// instead would get a different hash for every pre-L8 entry and no clue
/// why, so the rule is in `SYNC.md` and the branch is exercised here
/// rather than left to the first person to hit it.
#[test]
fn an_unsigned_entry_hashes_without_the_field_at_all() {
    let dir = std::env::temp_dir().join(format!("choir-sync-unsigned-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let log_path = dir.join("ops.jsonl");

    // Written straight into the log, because the API will not admit an
    // unsigned op -- which is the point: this shape exists only as
    // history, and history still has to be servable and checkable.
    {
        let mut log = FileLog::open(&log_path).expect("open log");
        let op = ViewOp::new(OpKind::RecordProvenance {
            subject: "s".into(),
            kind: "k".into(),
            body: "written before L8".into(),
        });
        log.append(OpEntry {
            format_version: FORMAT_VERSION,
            parent: None,
            seq: 0,
            channel: "pre-l8".into(),
            payload: op.to_payload(),
            witnesses: Vec::new(),
            author_sig: None,
        })
        .expect("genesis append");
    }

    let log = FileLog::open(&log_path).expect("reopen");
    let platform = Platform::start(Registry::new(), Box::new(log), ActorKey::generate())
        .expect("platform starts");
    let (status, out) = platform.handle_api("GET", "/api/log?from=0", b"");
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(status, 200, "{out}");
    let page: serde_json::Value = serde_json::from_str(&out).expect("json");
    let e = &page["entries"][0];
    assert!(
        e["author_key"].is_null() && e["author_sig_hex"].is_null(),
        "an unsigned entry must say so rather than advertise an author: {e}"
    );
    // The recipe, on the branch that omits the field entirely.
    verify_page(std::slice::from_ref(e), 0);
}

/// `SYNC.md` is a contract other implementations are written against, so
/// the fields it describes and the fields the node sends must be the same
/// set. Adding one to `entry_json` without documenting it silently
/// invalidates every third-party client's canonical form -- for them,
/// not for us, which is exactly the kind of break nobody here would
/// notice.
#[test]
fn every_served_field_is_documented() {
    let dir = std::env::temp_dir().join(format!("choir-sync-fields-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("scratch dir");

    let key = ActorKey::generate();
    let mut registry = Registry::new();
    registry.register(&key.public_key_bytes()).expect("valid key");
    let log = FileLog::open(&dir.join("ops.jsonl")).expect("open log");
    let platform = Platform::start(registry, Box::new(log), ActorKey::generate())
        .expect("platform starts");
    fill(&platform, &key, 1);
    let (_, out) = platform.handle_api("GET", "/api/log?from=0", b"");
    std::fs::remove_dir_all(&dir).ok();

    let page: serde_json::Value = serde_json::from_str(&out).expect("json");
    let entry = page["entries"][0].as_object().expect("entry object");
    let doc = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../SYNC.md"),
    )
    .expect("SYNC.md is at the repository root");

    for field in entry.keys() {
        assert!(
            doc.contains(field),
            "GET /api/log serves `{field}`, which SYNC.md never mentions: document it, \
             including where it sits in the canonical form"
        );
    }
    for envelope in ["window_base", "source", "entries"] {
        assert!(doc.contains(envelope), "SYNC.md omits the `{envelope}` field");
    }
    assert!(
        doc.contains(&PAGE.to_string()),
        "SYNC.md must state the page size the node actually enforces"
    );
}
