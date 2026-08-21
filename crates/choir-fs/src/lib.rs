//! Durable-file utilities shared by the binaries (internal/oak.md item 6):
//! atomic replace-by-rename writes, a private variant for secrets, and a
//! PID-bearing directory lock that enforces one writer process.
//!
//! Ported from Oak (oak.space) `cli/src/atomic_file.rs` and
//! `cli/src/workdir_lock.rs`, v0.102.1 (commit `8de9515`), Apache-2.0;
//! adapted to std errors and this workspace's needs.
//!
//! # Where this sits
//!
//! `docs/architecture.md` is the map of the whole workspace.
//! This crate is the durable-file primitives — atomic writes and a working-directory lock — that the binaries share.
//!
//! It depends on no other crate in this workspace.

pub mod atomic_file;
pub mod workdir_lock;

pub use atomic_file::{write_atomic, write_atomic_private};
pub use workdir_lock::{LockError, WorkdirLock};
