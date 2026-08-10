//! Single integration-test harness for choir-cli; one binary instead of
//! one per file, so an edit relinks once. Filter with e.g.
//! `cargo test -p choir-cli --test it mcp::`. Modules share a process and
//! run on parallel threads: no wall-clock assertions, process globals, or
//! fixed ports in here.

mod cli;
mod install_policy;
mod mcp;
mod surface;
