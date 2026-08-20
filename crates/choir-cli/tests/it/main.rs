//! Single integration-test harness for choir-cli; one binary instead of
//! one per file, so an edit relinks once. Filter with e.g.
//! `cargo test -p choir-cli --test it mcp::`. Modules share a process and
//! run on parallel threads: no wall-clock assertions, process globals, or
//! fixed ports in here.

mod acl_render;
mod batch;
mod check_exit_codes;
mod claude_hooks;
mod cli;
mod gate;
mod insight;
mod install_policy;
mod join;
mod mcp;
mod no_tty_block;
mod propose;
mod python;
mod repair;
mod runner;
mod skill;
mod surface;
mod symphony_backend;
