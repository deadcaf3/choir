//! Blast radius (D23 signal #1): reverse-dependency reach must rank a
//! foundational crate above a leaf, follow edges transitively, and be
//! honest about what it could not attribute.
//!
//! Metadata is inline rather than produced by running cargo: the analysis
//! is a pure function over JSON, and invoking cargo inside `cargo test`
//! would trade a hermetic test for a slower, lock-contending one.

use choir_queue::blast::{blast_radius, BlastRadius};

/// The real workspace shape, trimmed to the layering that matters. Mirrors
/// the dependency arrows in CLAUDE.md, so if the layering is ever inverted
/// this test is a place that notices.
const WORKSPACE: &str = r#"{
  "workspace_root": "/w",
  "packages": [
    {"name":"choir-hash","manifest_path":"/w/crates/choir-hash/Cargo.toml",
     "dependencies":[]},
    {"name":"choir-oplog","manifest_path":"/w/crates/choir-oplog/Cargo.toml",
     "dependencies":[{"name":"choir-hash"}]},
    {"name":"choir-view","manifest_path":"/w/crates/choir-view/Cargo.toml",
     "dependencies":[{"name":"choir-oplog"},{"name":"choir-store"}]},
    {"name":"choir-store","manifest_path":"/w/crates/choir-store/Cargo.toml",
     "dependencies":[{"name":"choir-hash"}]},
    {"name":"choir-sequencer","manifest_path":"/w/crates/choir-sequencer/Cargo.toml",
     "dependencies":[{"name":"choir-oplog"}]},
    {"name":"choir-node","manifest_path":"/w/crates/choir-node/Cargo.toml",
     "dependencies":[{"name":"choir-view"},{"name":"choir-sequencer"}]},
    {"name":"choir-demo","manifest_path":"/w/crates/choir-demo/Cargo.toml",
     "dependencies":[{"name":"choir-node"}]}
  ]
}"#;

fn radius(changed: &[&str]) -> BlastRadius {
    blast_radius(WORKSPACE, changed).expect("metadata parses")
}

#[test]
fn a_foundation_outranks_a_leaf() {
    // The whole point of the signal: touching the bottom of the stack is
    // not the same risk as touching the top, and the score has to say so.
    let foundation = radius(&["crates/choir-hash/src/lib.rs"]);
    let leaf = radius(&["crates/choir-demo/src/main.rs"]);

    assert_eq!(leaf.reached, ["choir-demo"], "a leaf reaches only itself");
    assert!(
        foundation.reached.len() > leaf.reached.len(),
        "foundation {:?} did not outrank leaf {:?}",
        foundation.reached,
        leaf.reached
    );
    assert!(foundation.fraction() > leaf.fraction());
    // choir-hash is under everything in this shape.
    assert_eq!(foundation.reached.len(), 7);
    assert!((foundation.fraction() - 1.0).abs() < f64::EPSILON);
}

#[test]
fn reach_is_transitive_not_just_direct() {
    // choir-oplog has two direct dependents (view, sequencer) but reaches
    // node and demo through them. A direct-only count would say 3.
    let r = radius(&["crates/choir-oplog/src/lib.rs"]);
    assert_eq!(
        r.reached,
        [
            "choir-demo",
            "choir-node",
            "choir-oplog",
            "choir-sequencer",
            "choir-view"
        ]
    );
    assert_eq!(r.touched, ["choir-oplog"]);
}

#[test]
fn several_changed_files_union_their_reach() {
    let r = radius(&[
        "crates/choir-store/src/lib.rs",
        "crates/choir-sequencer/src/lib.rs",
    ]);
    assert_eq!(r.touched, ["choir-sequencer", "choir-store"]);
    // store -> view -> node -> demo, and sequencer -> node -> demo.
    assert!(r.reached.contains(&"choir-view".to_string()));
    assert!(r.reached.contains(&"choir-demo".to_string()));
    assert!(!r.reached.contains(&"choir-hash".to_string()), "reach runs up, not down");
}

#[test]
fn absolute_and_relative_paths_agree() {
    let rel = radius(&["crates/choir-view/src/lib.rs"]);
    let abs = radius(&["/w/crates/choir-view/src/lib.rs"]);
    assert_eq!(rel, abs, "path form must not change the score");
}

#[test]
fn unattributed_paths_are_reported_not_dropped() {
    // A change that is entirely docs scores zero — and the zero has to be
    // distinguishable from "touched a package that nothing depends on",
    // or a doc-only commit and a leaf commit look identical.
    let r = radius(&["plan.md", "PHASE0.md"]);
    assert!(r.touched.is_empty());
    assert!(r.reached.is_empty());
    assert_eq!(r.unattributed, ["PHASE0.md", "plan.md"]);
    assert!((r.fraction() - 0.0).abs() < f64::EPSILON);

    // Mixed: the code part still scores, and the doc part is still named.
    let r = radius(&["plan.md", "crates/choir-demo/src/main.rs"]);
    assert_eq!(r.touched, ["choir-demo"]);
    assert_eq!(r.unattributed, ["plan.md"]);
}

#[test]
fn a_nested_package_wins_over_its_parent() {
    // Longest-directory attribution. Without it, an inner package's files
    // would be credited to whichever outer package matched first, and the
    // reach would be computed from the wrong node.
    let nested = r#"{
      "workspace_root": "/w",
      "packages": [
        {"name":"outer","manifest_path":"/w/a/Cargo.toml","dependencies":[]},
        {"name":"inner","manifest_path":"/w/a/inner/Cargo.toml",
         "dependencies":[{"name":"outer"}]}
      ]
    }"#;
    let r = blast_radius(nested, &["a/inner/src/lib.rs"]).unwrap();
    assert_eq!(r.touched, ["inner"], "inner package must win");
    assert_eq!(r.reached, ["inner"]);

    let r = blast_radius(nested, &["a/src/lib.rs"]).unwrap();
    assert_eq!(r.touched, ["outer"]);
    assert_eq!(r.reached, ["inner", "outer"]);
}

#[test]
fn a_dependency_cycle_terminates() {
    // Cargo forbids cycles, so this cannot arise from real metadata — but
    // the traversal should not depend on cargo continuing to forbid them,
    // because the failure mode is a hang inside the merge queue.
    let cyclic = r#"{
      "workspace_root": "/w",
      "packages": [
        {"name":"a","manifest_path":"/w/a/Cargo.toml","dependencies":[{"name":"b"}]},
        {"name":"b","manifest_path":"/w/b/Cargo.toml","dependencies":[{"name":"a"}]}
      ]
    }"#;
    let r = blast_radius(cyclic, &["a/src/lib.rs"]).unwrap();
    assert_eq!(r.reached, ["a", "b"]);
}

#[test]
fn external_dependencies_are_not_workspace_reach() {
    // serde is a dependency but not a workspace member; it must not appear
    // as a reachable package or inflate the denominator.
    let with_external = r#"{
      "workspace_root": "/w",
      "packages": [
        {"name":"lib","manifest_path":"/w/lib/Cargo.toml",
         "dependencies":[{"name":"serde"},{"name":"serde_json"}]}
      ]
    }"#;
    let r = blast_radius(with_external, &["lib/src/lib.rs"]).unwrap();
    assert_eq!(r.reached, ["lib"]);
    assert_eq!(r.total, 1);
}

#[test]
fn malformed_metadata_is_an_error_not_a_zero_score() {
    // Failing to a zero score would read as "this change is safe", which
    // is the most dangerous possible way for this to break.
    assert!(blast_radius("not json", &["a.rs"]).is_err());
    assert!(blast_radius(r#"{"nope":[]}"#, &["a.rs"]).is_err());
    let err = blast_radius(r#"{"packages":[{"name":"x"}]}"#, &["a.rs"]).unwrap_err();
    assert!(err.contains("manifest_path"), "{err}");
}
