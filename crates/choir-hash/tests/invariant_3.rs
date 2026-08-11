//! Invariant 3 enforced structurally, for shapes that do not exist yet.
//!
//! The golden vectors and the canonicalization property tests both guard
//! *named* types: someone had to look at `Commit`, decide its `tree` must
//! be ordered, and write the test. That works until the next hashed struct
//! is added. A new persisted shape with a `HashMap` in it compiles, passes
//! every existing test, and silently breaks its own addressing — because
//! nothing checks types nobody has written a vector for.
//!
//! So this file checks the source rather than any value. Two rules:
//!
//! 1. No `serde`-serializable item in the workspace holds a `HashMap` or
//!    `HashSet`. Today that is true of all 14 of them with no exceptions,
//!    which is why the rule is stated without an allowlist — the moment an
//!    exception is genuinely needed, adding it is a deliberate edit here
//!    with a reason attached, not an oversight.
//!
//! 2. The set of *persisted roots* — items carrying a `format_version`,
//!    which is invariant 1's marker for "this shape is written down" — is
//!    frozen. Adding one is how a new hashed shape enters the system, and
//!    it must come with a golden vector and a canonicalization property,
//!    so it fails here until it is registered.
//!
//! Rule 2 is the one that closes the hole. Rule 1 only helps if someone
//! reaches for the wrong map type; rule 2 fires on *any* new persisted
//! shape, whatever is inside it.
//!
//! **A source scanner is the classic vacuous test** — one bad assumption
//! about formatting and it silently matches nothing while passing. So the
//! detector is a pure function over a string, exercised below on synthetic
//! sources in both directions, and `the_scanner_sees_the_shapes_we_know`
//! pins it against types that really exist. Read those before trusting
//! anything else here.
//!
//! Known limits, stated rather than discovered later: only `//` comments
//! are stripped (the workspace uses no block comments in item bodies), a
//! type reached exclusively through a `type` alias defined outside the
//! workspace is invisible, and an item with a hand-written `Serialize`
//! impl instead of a derive is not scanned. Aliases defined *inside* the
//! workspace are resolved, because `choir-node` has one.

/// A `serde`-serializable item found in the source: its name and its body
/// text with comments already removed.
#[derive(Debug)]
struct Item {
    name: String,
    body: String,
}

/// Everything after `//` on a line, gone. Deliberately crude: the risk is
/// a `//` inside a string literal truncating a line early, which the
/// workspace does not currently contain and which would only ever hide a
/// hit on that same line.
fn strip_comments(line: &str) -> &str {
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

/// Whether `text` names `ident` as a whole word rather than as a
/// substring, so `Commit` does not match `CommitId`.
fn mentions(text: &str, ident: &str) -> bool {
    let mut rest = text;
    while let Some(i) = rest.find(ident) {
        let before = rest[..i].chars().next_back();
        let after = rest[i + ident.len()..].chars().next();
        let boundary = |c: Option<char>| !c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        if boundary(before) && boundary(after) {
            return true;
        }
        rest = &rest[i + ident.len()..];
    }
    false
}

/// Extracts every item deriving `Serialize` from one file's source, with
/// its body. Pure, so the tests below can drive it with synthetic input.
fn serializable_items(source: &str) -> Vec<Item> {
    let lines: Vec<&str> = source.lines().collect();
    let mut items = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let mut derive = strip_comments(lines[i]).to_string();
        if !derive.contains("#[derive(") {
            i += 1;
            continue;
        }
        // A derive list may wrap across lines.
        let mut j = i;
        while derive.matches('(').count() > derive.matches(')').count() && j + 1 < lines.len() {
            j += 1;
            derive.push_str(strip_comments(lines[j]));
        }
        if !mentions(&derive, "Serialize") {
            i = j + 1;
            continue;
        }
        // Skip any further attributes between the derive and the item.
        let mut k = j + 1;
        while k < lines.len() {
            let t = strip_comments(lines[k]).trim().to_string();
            if t.is_empty() || t.starts_with("#[") {
                k += 1;
            } else {
                break;
            }
        }
        let named = lines.get(k).and_then(|l| item_name(strip_comments(l)));
        if let Some(name) = named {
            let start = k;
            // Body by brace counting, so nested types stay inside.
            let mut body = String::new();
            let mut depth: i32 = 0;
            let mut opened = false;
            let mut p = start;
            while p < lines.len() {
                let s = strip_comments(lines[p]);
                depth += s.matches('{').count() as i32 - s.matches('}').count() as i32;
                if s.contains('{') {
                    opened = true;
                }
                body.push_str(s);
                body.push('\n');
                if opened && depth <= 0 {
                    break;
                }
                if !opened && s.contains(';') {
                    break;
                }
                p += 1;
            }
            items.push(Item { name, body });
            i = p + 1;
            continue;
        }
        i = k.max(j + 1);
    }
    items
}

/// The declared name on a `struct`/`enum` line, if this is one.
fn item_name(line: &str) -> Option<String> {
    let rest = line
        .split_once("struct ")
        .or_else(|| line.split_once("enum "))?
        .1;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Workspace-local `type` aliases that expand to an unordered map, so a
/// field typed `AuthTable` is not invisible.
fn unordered_aliases(source: &str) -> Vec<String> {
    source
        .lines()
        .map(strip_comments)
        .filter(|l| l.contains("type ") && (l.contains("HashMap") || l.contains("HashSet")))
        .filter_map(|l| {
            let rest = l.split_once("type ")?.1;
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            (!name.is_empty()).then_some(name)
        })
        .collect()
}

/// Every `crates/*/src/**/*.rs` in the workspace, as (relative path, text).
fn crate_sources() -> Vec<(String, String)> {
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .to_path_buf();
    let mut out = Vec::new();
    collect(&crates, &crates, &mut out);
    out.sort();
    assert!(
        out.len() > 10,
        "found only {} source files under {}; the scanner is looking in the wrong place",
        out.len(),
        crates.display()
    );
    out
}

fn collect(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<(String, String)>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Only `src`; tests and examples are not persisted shapes.
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            collect(&path, base, out);
        } else if path.extension().is_some_and(|e| e == "rs")
            && path.components().any(|c| c.as_os_str() == "src")
        {
            if let Ok(text) = std::fs::read_to_string(&path) {
                let rel = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                out.push((rel, text));
            }
        }
    }
}

/// Rule 1. Serialization order is the hash input, so an unordered map
/// inside anything serializable is a hash that depends on iteration order.
///
/// Stated over every serializable item rather than the persisted ones
/// only: the cost of the wider rule is zero today (nothing violates it)
/// and it does not require the scanner to decide correctly which shapes
/// reach a hash — a judgement that would itself need maintaining.
#[test]
fn no_serializable_item_holds_an_unordered_map() {
    let sources = crate_sources();
    let aliases: Vec<String> = sources
        .iter()
        .flat_map(|(_, text)| unordered_aliases(text))
        .collect();

    let mut offenders = Vec::new();
    for (file, text) in &sources {
        for item in serializable_items(text) {
            let mut hits: Vec<String> = ["HashMap", "HashSet"]
                .iter()
                .filter(|m| mentions(&item.body, m))
                .map(|m| (*m).to_string())
                .collect();
            hits.extend(
                aliases
                    .iter()
                    .filter(|a| mentions(&item.body, a))
                    .map(|a| format!("{a} (alias for an unordered map)")),
            );
            if !hits.is_empty() {
                offenders.push(format!("{file}: {} holds {}", item.name, hits.join(", ")));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "invariant 3: serialization order is the hash input, so these must use \
         BTreeMap/BTreeSet:\n  {}\n\nIf one is genuinely never hashed or persisted, \
         say so at the declaration and add it to this test with the reason.",
        offenders.join("\n  ")
    );
}

/// Rule 2, the one that covers shapes nobody has written a test for.
///
/// `format_version` is invariant 1's marker for a shape that gets written
/// down, so the set of items carrying one is the set of persisted roots.
/// It is frozen here. A new root is exactly the event that needs a golden
/// vector and a canonicalization property, and it is the event no
/// value-based test can notice.
#[test]
fn the_set_of_persisted_shapes_is_frozen() {
    // Every entry here has a frozen golden vector in
    // choir-view/tests/it/golden.rs and canonicalization properties in
    // the `canonical.rs` suites.
    const REGISTERED: [&str; 4] = ["Commit", "Manifest", "OpEntry", "ViewOp"];

    let mut found: Vec<String> = crate_sources()
        .iter()
        .flat_map(|(_, text)| serializable_items(text))
        .filter(|item| mentions(&item.body, "format_version"))
        .map(|item| item.name)
        .collect();
    found.sort();
    found.dedup();

    assert_eq!(
        found,
        REGISTERED,
        "the set of persisted shapes changed.\n\
         A new one needs: a golden vector in choir-view/tests/it/golden.rs, \
         a canonicalization property, and an entry above.\n\
         A removed one is a data migration, not a refactor."
    );
}

// ---------------------------------------------------------------------------
// Controls. A source scanner that quietly stops matching passes forever, so
// these assert the detector still works — in both directions.
// ---------------------------------------------------------------------------

/// Positive control: the detector must flag the thing it exists to flag.
/// Every shape here is one a real edit could produce.
#[test]
fn the_scanner_flags_synthetic_violations() {
    let cases = [
        (
            "plain struct",
            r#"
#[derive(Serialize, Deserialize)]
pub struct Bad {
    pub tree: std::collections::HashMap<String, u8>,
}
"#,
        ),
        (
            "wrapped derive list",
            r#"
#[derive(
    Debug,
    Serialize,
)]
pub struct AlsoBad {
    pub set: HashSet<String>,
}
"#,
        ),
        (
            "attribute between derive and item",
            r#"
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StillBad {
    pub m: HashMap<String, String>,
}
"#,
        ),
        (
            "enum variant body",
            r#"
#[derive(Serialize)]
pub enum BadKind {
    Variant { m: HashMap<String, String> },
}
"#,
        ),
    ];
    for (what, source) in cases {
        let items = serializable_items(source);
        assert_eq!(items.len(), 1, "{what}: expected exactly one item, got {items:?}");
        assert!(
            mentions(&items[0].body, "HashMap") || mentions(&items[0].body, "HashSet"),
            "{what}: the detector missed an unordered map it must catch"
        );
    }
}

/// Negative control: the detector must not fire on the legitimate cases,
/// or the real rule above would be untrustworthy noise.
#[test]
fn the_scanner_ignores_what_it_should() {
    // A HashMap in a type with no Serialize derive is fine — MemStore and
    // the node's dedup window are both real instances of this.
    let not_serializable = r#"
#[derive(Default)]
pub struct Runtime {
    chunks: std::collections::HashMap<String, Vec<u8>>,
}
"#;
    assert!(
        serializable_items(not_serializable).is_empty(),
        "a non-serializable type must not be scanned"
    );

    // A doc comment discussing HashMap must not read as a field. This is
    // not hypothetical: choir-node/src/platform.rs explains at length why
    // its HashMap is legitimate, and an unstripped comment would flag it.
    let comment_only = r#"
#[derive(Serialize)]
pub struct Fine {
    /// A HashMap would break invariant 3 here, so this is a BTreeMap.
    pub tree: std::collections::BTreeMap<String, u8>,
}
"#;
    let items = serializable_items(comment_only);
    assert_eq!(items.len(), 1);
    assert!(
        !mentions(&items[0].body, "HashMap"),
        "a comment mentioning HashMap must not count as holding one"
    );

    // Whole-word matching, so a longer name is not a false hit.
    assert!(!mentions("pub m: HashMapLike<String>,", "HashMap"));
    assert!(mentions("pub m: HashMap<String, u8>,", "HashMap"));
}

/// The scanner is pinned against types that actually exist, so a change in
/// how the workspace writes its declarations cannot leave it matching
/// nothing while both rules above keep passing.
#[test]
fn the_scanner_sees_the_shapes_we_know() {
    let names: Vec<String> = crate_sources()
        .iter()
        .flat_map(|(_, text)| serializable_items(text))
        .map(|i| i.name)
        .collect();
    for expected in [
        "ContentHash", // this crate, a plain struct
        "OpEntry",     // has a renamed field and a skipped option
        "Witness",
        "ViewOp",
        "OpKind",   // an enum with struct variants
        "TreeEntry",
        "Commit",
        "Manifest",
        "ChunkerParams",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "the scanner no longer finds {expected}; it has stopped matching \
             real declarations and both rules in this file are now vacuous. \
             Found: {names:?}"
        );
    }
    // The workspace is small; a count far off this means the scan broke.
    assert!(
        names.len() >= 12,
        "found only {} serializable items, expected ~14: {names:?}",
        names.len()
    );
}
