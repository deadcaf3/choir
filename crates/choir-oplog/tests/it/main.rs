//! Single integration-test harness for choir-oplog; one binary instead
//! of one per file, so an edit relinks once. Filter with e.g.
//! `cargo test -p choir-oplog --test it durability::`. Modules share a
//! process and run on parallel threads: no wall-clock assertions, process
//! globals, or fixed ports in here.

mod canonical;
mod conformance;
mod durability;
