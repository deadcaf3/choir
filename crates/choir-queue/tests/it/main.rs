//! Single integration-test harness for choir-queue; one binary instead
//! of one per file, so an edit relinks once. Filter with e.g.
//! `cargo test -p choir-queue --test it corpus::`. Modules share a
//! process and run on parallel threads: no wall-clock assertions, process
//! globals, or fixed ports in here.

mod blast;
mod conform;
mod corpus;
mod depends;
mod differential;
mod differential_runner;
mod envelope;
mod executor;
mod identity;
mod memory;
mod queue;
mod safety;
mod speculate;
