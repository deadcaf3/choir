//! Bridge library surface: GitHub App auth + status write-back. The
//! sync loop lives in the binary; this exists so the auth flow is
//! testable offline.

pub mod github;
