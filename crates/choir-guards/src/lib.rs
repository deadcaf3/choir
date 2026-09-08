//! Source-scanning tripwires over the whole workspace (D77).
//!
//! Two invariants here hold by the *absence* of something — an edge in
//! the dependency graph, a type named in the wrong crate — and an
//! absence is what no ordinary test can assert. Both are checked by
//! reading other crates as text: `quarantine` for D40, `invariant_3`
//! for the canonical-serialization rule.
//!
//! **Why they live in a crate of their own.** A test's inputs decide
//! two things in `gate`: whether the `touched` lane selects it, and
//! whether a cached green verdict is still valid. Both are computed
//! from the crate the test lives in — its own files, plus the
//! dependency closure cargo reports. A scanner's inputs are neither.
//! `quarantine` sat in `choir-view` and read `choir-node`, which
//! `choir-view` does not depend on and must not; so editing
//! `choir-node` neither selected the test nor invalidated its verdict,
//! and a green could stand over a tree that broke it. Moving the
//! scanners to the crate they scan is not open either: `choir-hash` is
//! depended on by everything, so widening *its* inputs to the whole of
//! `crates/` would invalidate every verdict in the workspace on every
//! edit.
//!
//! This crate is the shape that works, and D77 is its row. It is a leaf — nothing depends
//! on it, and it depends on nothing — so the `gate-inputs` file beside
//! its manifest can name the whole of `crates/` and the cost lands on
//! this crate alone. That file is one line and cannot rot, because the
//! scanners read the same directory it names.
//!
//! The helpers below are shared by both suites and public so that both
//! can reach them. They are text predicates, not analysis: read the
//! stated limits in each before trusting a green.
//!
//! # Examples
//!
//! ```
//! use choir_guards::{mentions, strip_comments};
//!
//! // A type named only in a doc link is not a use of that type.
//! assert_eq!(strip_comments("/// see [`TreeEntry`]").trim(), "");
//! // Whole-word, so a longer name is not a hit.
//! assert!(mentions("let e: TreeEntry = x;", "TreeEntry"));
//! assert!(!mentions("struct TreeEntryId;", "TreeEntry"));
//! ```

/// Everything after `//` on a line, gone.
///
/// This is what separates prose from code for every scanner here, and
/// `//` covers `///` and `//!` as well, so a doc comment that names a
/// quarantined type — including a working intra-doc link to it — is
/// not a use of it.
///
/// Deliberately crude, with two stated limits: a `//` inside a string
/// literal truncates the line early, which can only ever *hide* a hit
/// on that same line; and block comments are not stripped, because the
/// workspace uses none in item bodies.
pub fn strip_comments(line: &str) -> &str {
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

/// Whether `text` names `ident` as a whole word rather than as a
/// substring, so `Commit` does not match `CommitId`.
pub fn mentions(text: &str, ident: &str) -> bool {
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
