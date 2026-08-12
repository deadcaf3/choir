//! Single integration-test harness for choir-node.
//!
//! Each module below was previously its own `tests/*.rs` binary; cargo
//! builds one binary per test file, and relinking ~30 binaries dominated
//! the edit-test loop. Merged here they link once. Run one module's tests
//! with a name filter, e.g. `cargo test -p choir-node --test it api::`.
//!
//! Deliberately NOT merged, still their own binaries: `alloc_budget`
//! (needs its own `#[global_allocator]`), `throughput` and
//! `retention_cost` (timing gates, run isolated), `budget` (relative
//! timing harness, documented `--test budget --release` invocation).
//! Tests here share one process and run on parallel threads, so a module
//! added to this harness must not assert on wall-clock time, mutate
//! process globals (env, cwd, allocator), or bind fixed ports.

mod support;

mod api;
mod assignment;
mod auth;
mod batch_barriers;
mod concentration;
mod git_ordering_trace;
mod git_partial_push;
mod git_sequenced;
mod git_signed;
mod graft_attack;
mod identity_pinning;
mod key_binding;
mod key_names;
mod landing;
mod newcomer_config;
mod newcomer_harm;
mod observability;
mod policy_hardening;
mod reconcile_refs;
mod rejections;
mod resync;
mod review;
mod review_outcomes;
mod review_pruning;
mod reviewer_conflict_config;
mod signers_reload;
mod smart_http;
mod snapshot_admission;
mod snapshot_emission;
mod sync_contract;
mod t1_attack_edge;
mod tls;
mod ui;
mod view_growth;
mod window;
mod workspace;
