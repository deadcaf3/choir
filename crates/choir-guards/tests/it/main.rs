//! Single integration-test harness for choir-guards; one binary instead
//! of one per file, so an edit relinks once. Filter with e.g.
//! `cargo test -p choir-guards --test it quarantine::`. Modules share a
//! process and run on parallel threads: no wall-clock assertions,
//! process globals, or fixed ports in here.

mod invariant_3;
mod quarantine;
