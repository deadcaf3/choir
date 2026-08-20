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
//! assert!(usage.contains("choir review"));
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
