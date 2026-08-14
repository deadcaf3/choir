//! Single integration-test harness for choir-view; one binary instead of
//! one per file, so an edit relinks once. Filter with e.g.
//! `cargo test -p choir-view --test it golden::`. Modules share a process
//! and run on parallel threads: no wall-clock assertions, process
//! globals, or fixed ports in here.

mod canonical;
mod comments;
mod golden;
mod operator_identity;
mod quarantine;
mod receipts;
mod snapshot;
mod validate;
mod view;
