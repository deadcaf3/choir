//! Single integration-test harness for choir-bridge; one binary instead
//! of one per file, so an edit relinks once. Filter with e.g.
//! `cargo test -p choir-bridge --test it jwt::`. Modules share a process
//! and run on parallel threads: no wall-clock assertions, process
//! globals, or fixed ports in here.

mod bridge_trifecta;
mod differential_worktrees;
mod jwt;
mod queue;
mod replica;
