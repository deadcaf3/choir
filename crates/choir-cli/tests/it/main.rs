//! Single integration-test harness for choir-cli; one binary instead of
//! one per file, so an edit relinks once. Filter with e.g.
//! `cargo test -p choir-cli --test it mcp::`. Modules share a process and
//! run on parallel threads: no wall-clock assertions, process globals, or
//! fixed ports in here.

mod acl_render;
mod backup;
mod batch;
mod check_exit_codes;
mod claude_hooks;
mod cli;
mod config;
mod credential;
mod deploy;
mod doctor;
mod gate;
mod host;
mod init;
mod insight;
mod install_policy;
mod join;
mod mcp;
mod no_tty_block;
mod node;
mod propose;
mod python;
mod repair;
mod restore;
mod runner;
mod serve;
mod skill;
mod supervise;
mod surface;
mod symphony_backend;
mod ux;
