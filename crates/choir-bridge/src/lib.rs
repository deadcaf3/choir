//! Bridge library surface: GitHub App auth, status write-back, and the
//! speculative-train mechanics. The sync loop lives in the binary; this
//! exists so auth and train building are testable offline.

pub mod github;
pub mod queue;
