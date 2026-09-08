//! D40 tripwires: two properties that hold today by construction —
//! nothing enforces them but the absence of an edge in the dependency
//! graph. These tests turn each absence into a contract.
//!
//! **Replay purity.** `View::materialize` is a pure fold only because
//! merge resolution happens at write time (in `choir-queue` and other
//! callers) and *results* enter the log, never merge requests. The
//! guarantee is exactly "choir-view has no path to a merge strategy":
//! the day this crate depends on `choir-merge`, replayed state can
//! depend on the installed Mergiraf version and determinism dies.
//!
//! **Conflict quarantine.** A vanilla git client can never observe a
//! `TreeEntry::Conflict` because the two object models never mix: the
//! conflicted commit lives in choir's content-addressed store, and the
//! git-facing crates (`choir-node`, `choir-cli`) move only git oids.
//! That is enforced by nothing except those crates never naming the
//! type in code — so the tripwire is that they never name it in code.
//!
//! **Prose is not code, and the difference is load-bearing** (D77). The
//! scan runs over [`strip_comments`], so a doc comment may name
//! `TreeEntry` and link to it. It has to be able to: `browse.rs`
//! renders a committed conflict, and the comment explaining *why* that
//! is git's markers rather than choir's own type is the comment most
//! worth linking. Reading a rustdoc link as an opened one-way door
//! failed `main` for a comment that was documenting the quarantine.
//! What the tripwire watches is a `use`, a field, a match arm.
//!
//! A failure here is not a bug to route around: it means a one-way
//! door is being opened. Update D40 in `DECISIONS.md` deliberately or
//! remove the new edge.

use choir_guards::{mentions, strip_comments};
use std::fs;
use std::path::{Path, PathBuf};

/// Root of a sibling crate, resolved from this crate's manifest dir so
/// the test is independent of the process working directory.
fn crate_root(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/ parent exists")
        .join(name)
}

/// Every `.rs` file under `dir`, recursively. Hand-rolled walk rather
/// than a `walkdir` dev-dep (house convention).
fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("source dir is readable") {
        let path = entry.expect("dir entry is readable").path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn view_never_depends_on_merge() {
    let manifest = crate_root("choir-view").join("Cargo.toml");
    let manifest = fs::read_to_string(manifest).expect("choir-view manifest is readable");
    assert!(
        !manifest.contains("choir-merge"),
        "choir-view must not depend on choir-merge: the fold would gain \
         a path to a subprocess-versioned merge strategy and replay \
         determinism would die (D40). Resolve merges at write time and \
         log the result instead."
    );
}

#[test]
fn git_facing_crates_never_name_tree_entries() {
    for krate in ["choir-node", "choir-cli"] {
        let src = crate_root(krate).join("src");
        let mut files = Vec::new();
        rust_sources(&src, &mut files);
        assert!(!files.is_empty(), "{krate}/src has sources");
        for file in files {
            let text = fs::read_to_string(&file).expect("source file is readable");
            let named = text
                .lines()
                .enumerate()
                .find(|(_, line)| mentions(strip_comments(line), "TreeEntry"));
            assert!(
                named.is_none(),
                "{}:{} names TreeEntry in code: conflicted commits must \
                 stay in choir's own object model, quarantined from git \
                 transport (D40). If this is deliberate, update the \
                 register row first. (A doc comment naming the type is \
                 not this finding; the scan strips comments.)",
                file.display(),
                named.expect("a hit, since the assert fired").0 + 1
            );
        }
    }
}

/// The scanner, driven in both directions on synthetic sources.
///
/// A source scanner is the classic vacuous test: one bad assumption
/// about formatting and it matches nothing while passing. This pins
/// exactly the distinction the tripwire now rests on — that prose
/// naming the type is not a use of it — so that loosening the detector
/// cannot quietly become loosening the invariant.
#[test]
fn the_scanner_reads_code_and_not_prose() {
    let hit = |source: &str| {
        source
            .lines()
            .any(|line| mentions(strip_comments(line), "TreeEntry"))
    };

    // Prose, including a working intra-doc link. Not a finding.
    assert!(!hit(
        "/// That is the state [`choir_view::TreeEntry::Conflict`] names."
    ));
    assert!(!hit("//! A vanilla client never observes a TreeEntry."));
    assert!(!hit("let x = 1; // TreeEntry lives in the store"));
    // A longer name that merely contains it. Not a finding.
    assert!(!hit("struct TreeEntryId(u64);"));

    // Code. Every one of these is a finding.
    assert!(hit("use choir_view::TreeEntry;"));
    assert!(hit("    entries: Vec<TreeEntry>,"));
    assert!(hit("        TreeEntry::Conflict { .. } => todo!(),"));
    assert!(hit("let e: TreeEntry = read(); // safe, honest"));
}
