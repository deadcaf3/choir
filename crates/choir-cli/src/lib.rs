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
pub mod mcp;

/// One-command proposal from a git checkout.
pub mod propose;
pub mod runner;

/// Colour, and the "did you mean" a refusal needs to be useful.
pub mod style;
pub mod surface;
pub mod triage;

/// `SYNC.md`'s checks over a served log page.
pub mod verify;
