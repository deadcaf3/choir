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

mod accounts;
mod acl;
mod api;
mod assignment;
mod auth;
mod batch_barriers;
mod bounded;
mod browse;
mod checks;
mod concentration;
mod git_ordering_trace;
mod git_partial_push;
mod git_sequenced;
mod git_signed;
mod graft_attack;
mod handles;
mod hooks;
mod identity_pinning;
mod invocation;
mod join;
mod journal;
mod key_binding;
mod key_names;
mod landing;
mod landing_record;
mod magic_refspec;
mod newcomer_config;
mod newcomer_harm;
mod noscript;
mod observability;
mod ownership;
mod partial_clone;
mod passkeys;
mod policy_hardening;
mod portable;
mod provenance;
mod quotas;
mod reconcile_refs;
mod rejections;
mod require_scope_config;
mod restore;
mod resync;
mod review;
mod review_outcomes;
mod review_pruning;
mod reviewer_conflict_config;
mod schema;
mod signers_reload;
mod smart_http;
mod snapshot_admission;
mod snapshot_emission;
mod ssh;
mod state_lock;
mod sync_contract;
mod t1_attack_edge;
mod tls;
mod ui;
mod view_growth;
mod window;
mod workspace;
