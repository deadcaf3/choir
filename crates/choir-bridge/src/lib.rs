//! Bridge library surface: GitHub App auth, status write-back, and the
//! speculative-train mechanics. The sync loop lives in the binary; this
//! exists so auth and train building are testable offline.
//!
//! # Where this sits
//!
//! `docs/architecture.md` is the map of the whole workspace.
//! This crate is the mirror-first forge bridge (D21).
//!
//! It builds on [`choir_fs`], [`choir_hash`], [`choir_identity`] and [`choir_view`].

pub mod github;
pub mod queue;
