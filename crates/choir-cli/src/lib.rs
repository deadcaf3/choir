//! Shared pieces of the `choir` command line.
//!
//! The binary lives in `main.rs`; this library exists so the agent-facing
//! surface can be described once as data and rendered into every place
//! that documents it, with a test able to check that none of them have
//! drifted.
//!
//! # Examples
//!
//! ```
//! let usage = choir_cli::surface::usage();
//! assert!(usage.starts_with("usage:\n"));
//! // Grouped: a section heading, and the command names under it.
//! assert!(usage.contains("\nreview\n"));
//! assert!(usage.contains("verdict"));
//! ```
//!
//! # Where this sits
//!
//! `docs/architecture.md` is the map of the whole workspace.
//! This crate is the `choir` binary, and the surface table every generated document is rendered from.
//!
//! It builds on [`choir_fs`], [`choir_hash`], [`choir_identity`], [`choir_node`], [`choir_oplog`] and [`choir_view`].
//!
//! The complete surface, rendered from the table in
//! [`surface`] and included here so the two cannot disagree:
//!
#![doc = include_str!("../../../docs/using/cli.md")]

/// Regenerating the readable half of an ACL file (D46).
pub mod acl;

// Same reason as `docs`: the module's own header carries intra-doc links.
pub mod backup;
// Deliberately no `///` here, unlike some of its neighbours. A doc
// comment written on the `pub mod` line is merged with the module's own
// `//!` header, and the merged text resolves its intra-doc links in
// *this* module's scope rather than in `docs`'s -- so `docs`'s header
// linking to `docs::target_dir_from_metadata` became an unresolved link
// and, under the workspace's `deny`, failed `cargo doc` outright. The
// module documents itself.
pub mod docs;
// Same reason as `docs` above: this module's own header carries an
// intra-doc link, and a `///` here would resolve it in the wrong scope.
pub mod doctor;
// Same reason as `docs`: intra-doc links in the module's own header.
pub mod init;
// Same reason as `docs`: the module's own header carries intra-doc links.
pub mod join;
pub mod mcp;
// Same reason as `docs`: the module's own header carries intra-doc
// links that must resolve in its scope, not this one.
pub mod node;

/// One-command proposal from a git checkout.
pub mod propose;

// Same reason as `docs`: the module's own header carries intra-doc links.
pub mod prompt;
pub mod runner;

// Same reason as `docs`: the module's own header carries intra-doc links.
pub mod restore;

// Same reason as `docs`: the module's own header carries intra-doc links.
pub mod serve;

// Same reason as `docs`: the module's own header carries intra-doc links.
pub mod supervise;

/// Colour, and the "did you mean" a refusal needs to be useful.
pub mod style;
pub mod surface;
pub mod triage;

/// `SYNC.md`'s checks over a served log page.
pub mod verify;
