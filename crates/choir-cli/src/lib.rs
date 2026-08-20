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

/// Regenerating the readable half of an ACL file (D46).
pub mod acl;
pub mod mcp;

/// One-command proposal from a git checkout.
pub mod propose;
pub mod runner;
pub mod surface;
pub mod triage;

/// `SYNC.md`'s checks over a served log page.
pub mod verify;
