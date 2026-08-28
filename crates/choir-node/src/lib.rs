//! L3 node daemon: a minimal git smart-HTTP server (DECISIONS.md D12).
//!
//! v1 wraps `git http-backend` (git's own CGI) over bare repositories, so
//! the daemon is a thin, self-hostable shell over git plumbing, and
//! platform behavior (sequencer, queue, identity) layers on top.
//! ForgeMark benchmarks this surface directly.
//!
//! Authentication is per-actor basic auth ([`AuthTable`], `--auth-file`),
//! plus the credentials self-service has issued ([`accounts`],
//! `--accounts-file`); the platform API ([`platform`]) additionally
//! verifies ed25519 op signatures. The bind stays loopback-only: beyond
//! localhost you still need TLS or an SSH tunnel so tokens aren't sent in
//! the clear.
//!
//! # Where this sits
//!
//! `docs/architecture.md` is the map of the whole workspace.
//! This crate is L3, the daemon that serves both git smart-HTTP and the platform API.
//!
//! It builds on [`choir_fs`], [`choir_hash`], [`choir_identity`], [`choir_oplog`], [`choir_sequencer`] and [`choir_view`].
//!
//! The operator's guide to this daemon:
//!
#![doc = include_str!("../../../docs/operating/running-a-node.md")]

use std::io::Read;
use std::path::{Path, PathBuf};

mod account_page;
pub mod accounts;
pub mod acl;
mod bound;
mod browse;
pub mod hooks;
mod join_page;
pub mod limits;
pub mod platform;
/// How many times this process has shelled out to git while serving.
///
/// Exposed so a test can budget it. A page's read latency is mostly its
/// process spawns, and the count is the half of that which does not move
/// with machine load -- see `tests/phase1_spawns.rs`.
#[must_use]
pub fn git_invocations() -> u64 {
    browse::GIT_INVOCATIONS.load(std::sync::atomic::Ordering::Relaxed)
}

mod people_page;
pub mod portable;
mod prepare;
pub mod profile;
pub mod provision;
pub mod queue;
pub mod queue_api;
pub mod quota;
mod readme;
pub mod reject;
mod session;
mod signin_page;
pub mod ssh;
mod ui;
mod work;

pub use platform::Platform;

/// The commit this binary was built from, or the literal `unknown` when
/// the build had no way to find out. See `build.rs`.
///
/// `choirctl status` could already name the file that is serving; it
/// could not say what that file was built from, and "the rebuild never
/// reached the running process" is indistinguishable from "it did" until
/// something the process itself reports says otherwise.
pub const BUILD_COMMIT: &str = env!("CHOIR_BUILD_COMMIT");

/// Where [`BUILD_COMMIT`] came from: `env` (the installer passed
/// `CHOIR_GIT_HEAD`, the sound path), `git` (best-effort at build time),
/// or `unavailable` (no commit could be determined).
pub const BUILD_SOURCE: &str = env!("CHOIR_BUILD_SOURCE");

/// Whether the build tree had uncommitted changes. Only meaningful under
/// `BUILD_SOURCE == "git"`, and even then best-effort: cargo cannot rerun
/// the build script on every source edit, so this can be stale where
/// [`BUILD_COMMIT`] cannot.
pub const BUILD_DIRTY: &str = env!("CHOIR_BUILD_DIRTY");

/// The build stamp as served under `/api/view.build`.
#[must_use]
pub fn build_json() -> serde_json::Value {
    serde_json::json!({
        "format_version": 1,
        "commit": BUILD_COMMIT,
        "source": BUILD_SOURCE,
        "dirty": BUILD_DIRTY == "true",
        "dirty_trusted": BUILD_SOURCE == "git",
    })
}

/// One line naming the running binary's provenance, for the startup log.
#[must_use]
pub fn build_line() -> String {
    let commit = match BUILD_COMMIT.len() {
        40 => &BUILD_COMMIT[..12],
        _ => BUILD_COMMIT,
    };
    let dirty = if BUILD_DIRTY == "true" { " +dirty" } else { "" };
    format!("build {commit}{dirty} (stamp source: {BUILD_SOURCE})")
}

/// The design tokens from `ui.css`, alone, for the documentation book.
///
/// The book and this daemon's own pages should look like one product,
/// and the honest way to get that is one definition of the palette
/// rather than two that agree today. What is shared is deliberately
/// *only* the tokens: `ui.css` below the reset styles `h2` as a small
/// uppercase eyebrow and gives `body` a centred max-width, which is
/// right for a node page built out of `section` elements and wrong for
/// a book with a sidebar. Components stay per-surface; colour, type
/// scale, spacing and easing are shared.
///
/// The cut is the reset marker rather than a line number, and a missing
/// marker is a panic rather than a silent half-file: this is rendered
/// into a generated artifact that a staleness test compares, so a slice
/// that quietly returned everything would put the whole sheet in the
/// book and still look like it worked.
///
/// # Panics
///
/// If `ui.css` no longer carries the reset marker that separates its
/// tokens from its component styles, or either theme selector this
/// widens for mdBook.
#[must_use]
pub fn ui_tokens_css() -> String {
    const SHEET: &str = include_str!("ui.css");
    const RESET: &str = "/* --- global reset (token-only) ---";

    // mdBook picks a palette by putting a class on `<html>`; this
    // workspace picks one with `data-theme` (D56). Rather than keep a
    // second copy of the palette in mdBook's spelling, each selector is
    // widened to answer to both. Two literal rewrites, and a missing
    // one is a panic: a silent no-op here renders the book's light
    // theme in dark colours, which reads as a CSS bug rather than as a
    // generator that stopped generating.
    // The `:not()` is load-bearing and is not about matching: it is
    // there for specificity. `ui.css` resolves "follow the system" with
    // `@media (prefers-color-scheme:light){ :root:not([data-theme="dark"]) }`,
    // which scores (0,2,0). A plain `html.coal` scores (0,1,1) and
    // loses to it, so mdBook's dark themes rendered in light colours on
    // a machine set to light -- a book that looked like the theme
    // picker was broken rather than like a specificity bug. Each
    // selector below scores (0,3,0) and beats it, while still matching
    // exactly the same elements.
    const THEMES: [(&str, &str); 2] = [
        (
            ":root[data-theme=\"dark\"]{",
            ":root[data-theme=\"dark\"],\
             :root.coal:not([data-theme=\"light\"]),\
             :root.navy:not([data-theme=\"light\"]),\
             :root.ayu:not([data-theme=\"light\"]){",
        ),
        (
            ":root[data-theme=\"light\"]{",
            ":root[data-theme=\"light\"],\
             :root.light:not([data-theme=\"dark\"]),\
             :root.rust:not([data-theme=\"dark\"]){",
        ),
    ];

    let mut tokens = SHEET
        .split_once(RESET)
        .expect("ui.css must keep the reset marker that ends its token block")
        .0
        .trim_end()
        .to_string();

    for (from, to) in THEMES {
        assert!(
            tokens.contains(from),
            "ui.css must keep the `{from}` selector the book's theme mapping widens"
        );
        tokens = tokens.replace(from, to);
    }

    format!(
        "/* generated from crates/choir-node/src/ui.css -- do not edit.\n\
         \x20  Regenerate: cargo run -p choir-cli --example gen-surface\n\
         \x20\n\
         \x20  The token block of that sheet, cut at its reset marker, with\n\
         \x20  each theme selector widened to answer to mdBook's `<html>`\n\
         \x20  class as well as this workspace's `data-theme`. Edit the\n\
         \x20  tokens there and the node's pages and the book move together.\n\
         \x20  Component styles are deliberately NOT shared: that sheet\n\
         \x20  styles `h2` as an uppercase eyebrow and centres `body`,\n\
         \x20  which is right for a node page and wrong for a book. */\n\n{tokens}\n"
    )
}

/// Per-actor credentials: username → token, checked as HTTP basic auth
/// (the standard git-over-HTTP shape; every forge client speaks it).
///
/// L8 note: usernames are actor ids and tokens are per-actor secrets
/// minted by the operator; key-signature-based challenge auth can
/// replace the token *check* later without changing the wire shape.
pub type AuthTable = std::collections::HashMap<String, String>;

/// Default maximum body size for every `/api/...` request: 1 MiB.
pub const DEFAULT_API_BODY_BYTES: u64 = 1024 * 1024;

/// Readiness refuses when the filesystem reports less than 1 GiB free.
pub const DEFAULT_READY_MIN_FREE_BYTES: u64 = 1024 * 1024 * 1024;

/// A running node daemon serving repos under a root directory.
pub struct Node {
    root: PathBuf,
    server: std::sync::Arc<tiny_http::Server>,
    port: u16,
    auth: std::sync::Arc<Option<AuthTable>>,
    platform: Option<std::sync::Arc<Platform>>,
    /// What the merge queue needs before `/api/queue/run` can do
    /// anything (D68). `None` answers 501: a node with no CI command
    /// cannot decide whether a candidate is good, and a queue that
    /// landed everything unchecked would be a worse `git push`.
    queue: Option<queue_api::QueueConfig>,
    /// The rounds running right now, so a second request for a target
    /// already in flight is refused rather than raced.
    queue_in_flight: std::sync::Arc<queue_api::InFlight>,
    /// The scheme absolute URLs handed to *people* are written with:
    /// invite links, page origins, and the `Secure` attribute on cookies.
    ///
    /// [`Node::behind_tls_proxy`] sets it, because a node terminating
    /// plaintext on loopback behind a proxy is reached over https by
    /// everyone except the proxy.
    scheme: &'static str,
    /// The scheme this node's own socket actually speaks.
    ///
    /// Separate from [`Node::scheme`] and never overridden, because the
    /// two answer different questions and one field answering both is a
    /// bug this repository has already shipped: git's `pre-receive` hook
    /// calls back to `127.0.0.1` on this very socket, and when
    /// `behind_tls_proxy` moved the single field the hook began speaking
    /// TLS to a plaintext port and every push hung in the handshake.
    /// Anything addressed to loopback uses this one.
    listener_scheme: &'static str,
    /// Loopback secret handed to repo hooks via env so their callback to
    /// `/api/git-update` passes the auth gate without user credentials.
    internal_token: String,
    /// Trusted-keys file to watch, so `allowed_signers` tracks it
    /// without a restart. `None` = generated once at startup.
    keys_watch: Option<std::sync::Arc<KeysWatch>>,
    /// Per-repository authorization table (D29), watched like the keys
    /// file. `None` = no `--acl-file`, so every authenticated actor
    /// reaches every repository, which is the pre-D29 behaviour.
    acl_watch: Option<std::sync::Arc<AclWatch>>,
    /// The browser page, prebuilt and keyed by view sequence. Shared
    /// across request threads so one render serves every reader until
    /// the state it describes changes.
    ui_cache: std::sync::Arc<ui::UiCache>,
    /// The attributed request log (D33). `None` = no `--request-log`, so
    /// nothing is recorded, which is the pre-D33 behaviour.
    request_log: Option<std::sync::Arc<limits::RequestLog>>,
    /// Per-user token buckets (D33). `None` = no ceiling was configured,
    /// so no request is ever refused for rate.
    rate: Option<std::sync::Arc<limits::RateLimiter>>,
    /// Self-service credentials (D36). `None` = no `--accounts-file`, so
    /// the only credentials are the ones the operator wrote by hand.
    accounts: Option<std::sync::Arc<accounts::Accounts>>,
    /// Whether passkeys are usable on this node (D39, D71). Off unless
    /// [`Node::enable_passkeys`] is called, and separate from
    /// [`Node::accounts`] on purpose: enrolment and the WebAuthn write
    /// path both live behind the accounts store, so without this switch
    /// turning on self-service credentials would turn on browser
    /// signing with them in the same move, and a deployment that says
    /// it does not offer passkeys could not be telling the truth.
    passkeys: bool,
    /// Browser sessions opened by a passkey (D71). In memory, so they end
    /// with the process.
    sessions: std::sync::Arc<session::Sessions>,
    /// Admission control for the pre-auth routes (D57).
    ///
    /// Always present, unlike [`Node::rate`]: those routes answer before
    /// any credential is checked, so "no ceiling was configured" is not
    /// an option there the way it is for an authenticated caller.
    public_rate: std::sync::Arc<limits::PublicLimiter>,
    /// Whether this node hands out ssh access, which decides whether the
    /// join page offers a key field. Offering one on a node with no ssh
    /// surface collects a key nothing will ever use.
    ssh_enabled: bool,
    /// The file table and the store's grants, merged and dated, cached
    /// against the generations of both and the second it was dated at.
    /// Rebuilt when any of the three moves, so authorization does not
    /// rebuild a table per request and cannot serve a grant past its
    /// deadline (D66).
    acl_merged: std::sync::RwLock<Option<MergedAcl>>,
    /// Per-user ceilings on push size and workspace count (D37). Both
    /// unset = nothing is ever refused for quota, which is the pre-D37
    /// behaviour.
    quotas: quota::Quotas,
    /// Absolute API request-body ceiling. Unlike per-user quotas this is
    /// never exempt: parsing an operator's unbounded JSON consumes the
    /// same memory as parsing anyone else's.
    api_body_limit: std::num::NonZeroU64,
    /// Whether authenticated review pages expose passkey-backed write
    /// controls. The private beta disables this and keeps signed CLI
    /// submissions as the only mutation path.
    browser_writes: bool,
    site_repo: Option<String>,
    /// Free-space floor used by the authenticated readiness endpoint.
    ready_min_free_bytes: u64,
    /// Running request totals, exported by `/metrics`. Shared with every
    /// request thread rather than owned by one, since the counting
    /// happens on whichever thread served the request.
    counters: std::sync::Arc<limits::Counters>,
    /// When this process began serving, as seconds since the epoch.
    /// `choir_process_start_time_seconds`, which is how a restart-loop
    /// alert sees a restart at all.
    started_unix: u64,
    /// Distinguishes an intentional `unblock()` from a receive timeout.
    /// tiny_http reports both as `Ok(None)` from `recv_timeout`.
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// A watched trusted-keys file and the mtime last folded into
/// `allowed_signers`.
struct KeysWatch {
    path: PathBuf,
    mtime: std::sync::Mutex<Option<std::time::SystemTime>>,
}

/// The `, N expired` half of the ACL startup and reload lines (D66),
/// or nothing at all when no grant has a deadline in the past.
///
/// Said out loud because a deadline that has already passed reads
/// exactly like a grant that was never written: the holder is refused,
/// and the file still shows the line. A count is the cheapest thing that
/// tells those two apart.
fn expired_note(table: &acl::Acl) -> String {
    match table.expired(accounts::now_secs()) {
        0 => String::new(),
        n => format!(", {n} expired"),
    }
}

/// The cached merged table and everything it is only valid for: the ACL
/// file's epoch, the account store's generation, and the second it was
/// dated at (D66). All three have to match or it is rebuilt.
type MergedAcl = (u64, u64, u64, std::sync::Arc<acl::Effective>);

/// A watched ACL file, the mtime last parsed, and the table in force.
struct AclWatch {
    path: PathBuf,
    mtime: std::sync::Mutex<Option<std::time::SystemTime>>,
    table: std::sync::RwLock<std::sync::Arc<acl::Acl>>,
    /// Bumped on every successful reload. A counter rather than the
    /// table's address, because an address can be reused by the next
    /// allocation and a stale merge on an authorization path is exactly
    /// the failure worth spending a `u64` to make impossible.
    epoch: std::sync::atomic::AtomicU64,
}

impl Node {
    /// Binds to `127.0.0.1:port` (0 = ephemeral) over `root`, with no
    /// authentication — localhost/dev only.
    ///
    /// # Errors
    ///
    /// Returns an error when the socket cannot be bound or `root` cannot be
    /// created.
    pub fn bind(root: &Path, port: u16) -> std::io::Result<Self> {
        Self::bind_with_auth(root, port, None)
    }

    /// Binds like [`Node::bind`]; when `auth` is `Some`, every request
    /// must carry valid basic-auth credentials from the table or it is
    /// answered with 401 before touching git.
    ///
    /// # Errors
    ///
    /// Same failure modes as [`Node::bind`].
    pub fn bind_with_auth(
        root: &Path,
        port: u16,
        auth: Option<AuthTable>,
    ) -> std::io::Result<Self> {
        Self::bind_full(root, "127.0.0.1", port, auth, None)
    }

    /// Full-control bind: address, port, auth, and optional TLS
    /// (PEM certificate chain + PEM private key).
    ///
    /// A non-loopback `addr` is refused without TLS — plaintext basic
    /// auth must never cross a real network (standing privacy rule:
    /// nothing leaves loopback without an explicit, protected choice).
    ///
    /// # Errors
    ///
    /// Returns an error when the socket cannot be bound, `root` cannot
    /// be created, the TLS material is invalid, or a non-loopback bind
    /// is requested without TLS.
    pub fn bind_full(
        root: &Path,
        addr: &str,
        port: u16,
        auth: Option<AuthTable>,
        tls: Option<(Vec<u8>, Vec<u8>)>,
    ) -> std::io::Result<Self> {
        let loopback = addr
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
        if !loopback && tls.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "refusing non-loopback bind without TLS",
            ));
        }
        std::fs::create_dir_all(root)?;
        let scheme = if tls.is_some() { "https" } else { "http" };
        let listener_scheme = scheme;
        let server = match tls {
            Some((certificate, private_key)) => tiny_http::Server::https(
                (addr, port),
                tiny_http::SslConfig {
                    certificate,
                    private_key,
                },
            ),
            None => tiny_http::Server::http((addr, port)),
        }
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        let port = match server.server_addr().to_ip() {
            Some(addr) => addr.port(),
            None => 0,
        };
        Ok(Self {
            root: root.to_path_buf(),
            server: std::sync::Arc::new(server),
            port,
            auth: std::sync::Arc::new(auth),
            platform: None,
            queue: None,
            queue_in_flight: std::sync::Arc::new(queue_api::InFlight::default()),
            site_repo: None,
            scheme,
            listener_scheme,
            internal_token: choir_identity::ActorKey::generate().actor_id().to_hex(),
            keys_watch: None,
            acl_watch: None,
            ui_cache: std::sync::Arc::new(ui::UiCache::new()),
            request_log: None,
            rate: None,
            accounts: None,
            passkeys: false,
            sessions: std::sync::Arc::new(session::Sessions::default()),
            // One ceiling for the whole pre-auth surface, because there
            // is no per-client key to hold a second one against (D59).
            // Sized for a node's worth of real joining rather than for one
            // reader: a reader fetches the join page, submits it, and
            // lands on the welcome page, which is three requests.
            public_rate: std::sync::Arc::new(limits::PublicLimiter::new(600)),
            ssh_enabled: false,
            acl_merged: std::sync::RwLock::new(None),
            quotas: quota::Quotas::default(),
            api_body_limit: std::num::NonZeroU64::new(DEFAULT_API_BODY_BYTES)
                .expect("the default API body limit is nonzero"),
            browser_writes: true,
            ready_min_free_bytes: DEFAULT_READY_MIN_FREE_BYTES,
            counters: std::sync::Arc::new(limits::Counters::default()),
            started_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_secs())
                .unwrap_or_default(),
            shutdown: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    /// Sets the absolute body ceiling for every `/api/...` route.
    pub fn enable_api_body_limit(&mut self, bytes: std::num::NonZeroU64) {
        self.api_body_limit = bytes;
    }

    /// Removes browser mutation controls and their preparation endpoint.
    ///
    /// **It withholds authorship, not credentials** (D73). A browser under
    /// this flag renders no control that would put an operation in the op
    /// log -- no verdict, no comment, no `/api/prepare` -- because that is
    /// the launch gate D39 shipped behind and the thing an operator
    /// switches on when they are ready for it.
    ///
    /// It does not withhold signing in, enrolling the passkey that signs
    /// in, asking for access (D72), or the operator's console. None of
    /// those reaches the log: the accounts store is a node-owned file and
    /// revocation is deletion (D36). Withholding them was the same flag
    /// doing two jobs, and the second job made the node's own manifest
    /// untrue -- `passkeys=enabled` beside a posture that would not serve
    /// the file the ceremony is written in.
    pub fn disable_browser_writes(&mut self) {
        self.browser_writes = false;
    }

    /// Presents one repository as this node's entire browser surface.
    ///
    /// `/` becomes that repository instead of the index, and the browser
    /// answers for no other repository. This is what a node serving a
    /// project's own domain wants: a reader arriving at
    /// `git.example.com` came for that project, and an index naming
    /// every other repository the host holds is both noise and a
    /// disclosure.
    ///
    /// Presentation only. It changes no grant: git access stays the
    /// ACL's answer, and a repository hidden here is still clonable by
    /// whoever could clone it before.
    /// # Errors
    ///
    /// Refuses a name this node could not hold, checked here rather than
    /// at the call site: the router trusts this value against the disk,
    /// so the grammar has to be enforced where it is stored.
    pub fn serve_single_repository(&mut self, repo: &str) -> std::io::Result<()> {
        let invalid = || {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("not a repository this node can present: {repo}"),
            )
        };
        let (owner, name) = repo.split_once('/').ok_or_else(invalid)?;
        if !provision::safe_segment(owner) || !provision::safe_segment(name) {
            return Err(invalid());
        }
        self.site_repo = Some(repo.to_string());
        Ok(())
    }

    /// Sets the free-space floor below which `/readyz` refuses traffic.
    pub fn enable_ready_min_free_bytes(&mut self, bytes: u64) {
        self.ready_min_free_bytes = bytes;
    }

    /// Records every served request to `path` (D33), rotating it at
    /// `max_bytes` — see [`limits::RequestLog`] for the line format, the
    /// rotation rule, and what is deliberately never written.
    ///
    /// # Errors
    ///
    /// Returns the failure to open the file. Fatal by design: an operator
    /// who asked for a record and did not get one should learn that at
    /// startup rather than from its absence during an incident.
    pub fn enable_request_log(&mut self, path: PathBuf, max_bytes: u64) -> std::io::Result<()> {
        self.request_log = Some(std::sync::Arc::new(limits::RequestLog::open(
            path, max_bytes,
        )?));
        Ok(())
    }

    /// Limits each authenticated user to the given requests per minute
    /// per class (D33). `None` leaves that class unlimited.
    ///
    /// Never applied to the loopback hook callback, to a holder of a D29
    /// `@node` grant, or on a node without authentication — see
    /// [`Node::serve_forever`] for why each of those would be worse than
    /// the flood it prevents.
    pub fn enable_rate_limit(
        &mut self,
        api_per_minute: Option<std::num::NonZeroU32>,
        git_per_minute: Option<std::num::NonZeroU32>,
    ) {
        let limiter = limits::RateLimiter::new(api_per_minute, git_per_minute);
        self.rate = limiter.is_active().then(|| std::sync::Arc::new(limiter));
    }

    /// Sets the per-user quotas (D37): the largest git request body one
    /// user may send, and the most workspaces one user may hold at once.
    /// `None` leaves that ceiling off.
    ///
    /// Exempt exactly where the D33 rate limiter is exempt, and for the
    /// same reason — see [`Node::serve_forever`]. A quota that can lock
    /// an operator out of their own node is the failure this must not
    /// cause.
    pub fn enable_quotas(
        &mut self,
        push_bytes: Option<std::num::NonZeroU64>,
        workspaces: Option<std::num::NonZeroU32>,
    ) {
        self.quotas = quota::Quotas {
            push_bytes,
            workspaces,
        };
    }

    /// Enables the platform API (`/api/submit`, `/api/view`) backed by
    /// `platform`. Call before [`Node::serve_forever`].
    pub fn enable_platform(&mut self, platform: Platform) {
        // One of the three halves of the join in `enable_accounts` and
        // `enable_passkeys`: whichever flag is applied last attaches the
        // store, so passkey verification does not depend on the order the
        // daemon happens to configure in (D39). The store is withheld
        // while passkeys are off, which is what keeps the write path shut
        // rather than merely unadvertised.
        if self.passkeys {
            if let Some(store) = self.accounts.as_ref() {
                platform.attach_accounts(store.clone());
            }
        }
        self.platform = Some(std::sync::Arc::new(platform));
    }

    /// Enables `POST /api/queue/run` (D5, D68).
    ///
    /// Without it the endpoint answers 501. There is deliberately no
    /// timer: a node that spends CI on its own schedule surprises
    /// whoever pays for it, and a round only a clock can start is one no
    /// test can reach without waiting on wall-clock time. An operator's
    /// cron, a hook, or a person decides the cadence.
    pub fn enable_queue(&mut self, config: queue_api::QueueConfig) {
        self.queue = Some(config);
    }

    /// The platform this node serves, or `None` if the platform API was
    /// never enabled.
    #[must_use]
    pub fn platform(&self) -> Option<&Platform> {
        self.platform.as_deref()
    }

    /// Brings the bare repos back into agreement with the view before the
    /// node serves anything. See [`Platform::reconcile_git_refs`] for what
    /// it repairs and what it refuses to.
    ///
    /// Separate from [`Node::enable_platform`] and from
    /// [`Node::serve_forever`] so it is called deliberately: it writes git
    /// refs and can append compensating ops, which is not something a
    /// constructor should do behind a caller's back.
    pub fn reconcile_refs(&self) -> crate::platform::RefReconciliation {
        self.platform
            .as_ref()
            .map(|p| p.reconcile_git_refs(&self.root))
            .unwrap_or_default()
    }

    /// Watches the trusted-keys file and regenerates
    /// `<root>/.choir/allowed_signers` whenever its mtime moves, so
    /// registering a *signing* key is "append a line" — the same
    /// mechanism the platform registry already uses for submission
    /// keys, which until now diverged from push-certificate
    /// verification and left the two lists out of step.
    pub fn watch_keys_file(&mut self, path: PathBuf) {
        self.keys_watch = Some(std::sync::Arc::new(KeysWatch {
            mtime: std::sync::Mutex::new(std::fs::metadata(&path).and_then(|m| m.modified()).ok()),
            path,
        }));
    }

    /// Rewrites `allowed_signers` if the watched keys file changed.
    /// Runs on the accept loop, so rewrites never race each other.
    ///
    /// A malformed keys file leaves the existing signer list in place
    /// (a partial list would silently stop verifying somebody's
    /// pushes); the mtime is still recorded so the complaint is printed
    /// once per edit rather than once per request.
    fn refresh_allowed_signers(&self) {
        let Some(watch) = &self.keys_watch else {
            return;
        };
        let mtime = std::fs::metadata(&watch.path)
            .and_then(|m| m.modified())
            .ok();
        let mut last = watch.mtime.lock().expect("keys mtime lock");
        if mtime.is_none() || mtime == *last {
            return;
        }
        *last = mtime;
        match parse_keys_file(&watch.path) {
            Ok(signers) => {
                if let Err(e) = write_allowed_signers(&self.root, &signers) {
                    eprintln!("allowed_signers: write failed: {e}");
                } else {
                    eprintln!("allowed_signers: reloaded ({} keys)", signers.len());
                }
                // Same file, same edit: pick up name bindings here too, so
                // binding a name to an already-trusted key does not wait
                // for the policy's failed-signature reload trigger.
                if let Some(platform) = &self.platform {
                    platform.set_key_names(&signers);
                }
            }
            Err(e) => eprintln!("allowed_signers: keys file unusable, keeping previous: {e}"),
        }
    }

    /// Enforces per-repository authorization (D29) from `path`, reloaded
    /// whenever its mtime moves — so granting access is "append a line",
    /// the same discipline as the trusted-keys file.
    ///
    /// # Errors
    ///
    /// Returns a message when the file cannot be read or does not parse.
    /// This is fatal by design: there is no previous table to fall back
    /// to at startup, and an empty table under a fail-closed ACL locks
    /// out everyone including the operator.
    pub fn watch_acl_file(&mut self, path: PathBuf) -> Result<(), String> {
        let table = acl::Acl::load(&path)?;
        eprintln!(
            "acl enabled ({} grants{})",
            table.len(),
            expired_note(&table)
        );
        self.acl_watch = Some(std::sync::Arc::new(AclWatch {
            mtime: std::sync::Mutex::new(std::fs::metadata(&path).and_then(|m| m.modified()).ok()),
            table: std::sync::RwLock::new(std::sync::Arc::new(table)),
            epoch: std::sync::atomic::AtomicU64::new(0),
            path,
        }));
        Ok(())
    }

    /// Declares that a TLS-terminating proxy sits in front of this node,
    /// so absolute URLs it builds are written `https` and the cookies it
    /// sets carry `Secure`.
    ///
    /// Declared by the operator rather than read from
    /// `X-Forwarded-Proto`, because the node cannot tell a header its
    /// proxy set from one a client sent: trusting it would mean any
    /// caller that can reach the node decides how its invite links are
    /// spelled. The proxy this repository ships already *sets* that
    /// header rather than appending to it, for the same reason
    /// `X-Forwarded-For` is cleared there (D59), and a declaration needs
    /// no such care.
    ///
    /// The defect this exists for: an invite is a bearer credential
    /// carried in a URL, and behind the proxy the node was writing that
    /// URL with `http`. The recipient's first request would carry the
    /// credential in cleartext and only then be redirected.
    pub fn behind_tls_proxy(&mut self) {
        // Only the public half. `listener_scheme` stays what this socket
        // speaks, because the hook callbacks address loopback directly and
        // never pass the proxy at all.
        self.scheme = "https";
    }

    /// Turns on passkeys: WebAuthn enrolment and the browser write path
    /// that verifies assertions against enrolled keys (D39, D71).
    ///
    /// Separate from [`Node::enable_accounts`] even though both need the
    /// accounts store, because a node can reasonably offer self-service
    /// credentials without offering browser signing, and the private beta
    /// says in its manifest that it does exactly that. Without this
    /// switch that sentence could not be true: the enrolment routes and
    /// `platform`'s assertion check are both reachable the moment the
    /// store exists.
    pub fn enable_passkeys(&mut self) {
        self.passkeys = true;
        // The third way into the same join. Enabling passkeys after both
        // of the others is the ordinary case, and without this the store
        // would never reach the policy.
        if let (Some(platform), Some(store)) = (self.platform.as_ref(), self.accounts.as_ref()) {
            platform.attach_accounts(store.clone());
        }
    }

    /// Turns on account and token self-service (D36) from the store at
    /// `path`, optionally generating the `authorized_keys` D31's forced
    /// commands live in.
    ///
    /// # Errors
    ///
    /// Refuses without `--auth-file` and without `--acl-file`, for the
    /// reason D29 and D33 refuse the same combinations: a credential
    /// issued on a node that authenticates nobody is not a credential,
    /// and one issued on a node with no ACL is a credential to every
    /// repository, which is the thing being issued *against*. Also
    /// returns the store's own load failures.
    /// `actor_keys` names the trusted-keys file a redemption may bind
    /// one actor key into (`--invite-binds-keys`). `None` keeps the
    /// pre-existing behaviour, in which an actor key reaches the node
    /// only by an operator editing that file.
    pub fn enable_accounts(
        &mut self,
        path: PathBuf,
        keys_out: Option<accounts::SshKeysOut>,
        actor_keys: Option<PathBuf>,
    ) -> Result<(), String> {
        let Some(table) = self.auth.as_ref().as_ref() else {
            return Err(
                "--accounts-file needs --auth-file: an issued token is checked where every \
                 other credential is"
                    .to_string(),
            );
        };
        if self.acl_watch.is_none() {
            return Err(
                "--accounts-file needs --acl-file: without a table to grade them against, \
                 an issued grant would be a grant to every repository"
                    .to_string(),
            );
        }
        // Every operator credential's name, so self-service can never
        // issue an account that shadows one.
        let reserved = table.keys().cloned().collect();
        self.ssh_enabled = keys_out.is_some();
        let mut store = accounts::Accounts::open(path, keys_out, reserved)?;
        if let Some(actor_keys) = actor_keys {
            store = store.binding_actor_keys_into(actor_keys);
        }
        eprintln!("accounts enabled ({} issued)", store.len());
        let store = std::sync::Arc::new(store);
        // Any order: whichever flag is applied last performs the join, so
        // a passkey submission is verifiable regardless of how the daemon
        // was configured (D39). Withheld while passkeys are off.
        if self.passkeys {
            if let Some(platform) = self.platform.as_ref() {
                platform.attach_accounts(store.clone());
            }
        }
        self.accounts = Some(store);
        Ok(())
    }

    /// Reparses the ACL file if it changed. Runs on the accept loop, so
    /// an edit takes effect on the *next* request with no restart.
    ///
    /// A malformed file leaves the previous table in force and complains
    /// once per edit — the same rule as the keys file, for the same
    /// reason: a partially parsed ACL would silently revoke access.
    fn refresh_acl(&self) {
        let Some(watch) = &self.acl_watch else {
            return;
        };
        let mtime = std::fs::metadata(&watch.path)
            .and_then(|m| m.modified())
            .ok();
        let mut last = watch.mtime.lock().expect("acl mtime lock");
        if mtime.is_none() || mtime == *last {
            return;
        }
        *last = mtime;
        match acl::Acl::load(&watch.path) {
            Ok(table) => {
                eprintln!(
                    "acl: reloaded ({} grants{})",
                    table.len(),
                    expired_note(&table)
                );
                *watch.table.write().expect("acl write lock") = std::sync::Arc::new(table);
                watch
                    .epoch
                    .fetch_add(1, std::sync::atomic::Ordering::Release);
            }
            Err(e) => eprintln!("acl: file unusable, keeping previous: {e}"),
        }
    }

    /// The ACL table currently in force, if one is configured: the
    /// operator's file, merged with the grants self-service has issued
    /// (D36).
    ///
    /// Merged rather than checked separately so that every enforcement
    /// point — the git chokepoint, the API's per-endpoint table, the
    /// response filter, the D33 rate-limit exemption — keeps asking one
    /// table one question. The merge is cached against both sources'
    /// generations, so the ordinary request pays two atomic loads.
    ///
    /// The cache is keyed on the current second as well (D66), which is
    /// the whole reason a deadline can be trusted here: a table merged
    /// once and held would keep answering for a grant that has lapsed,
    /// and no reload would happen to invalidate it because nothing about
    /// the file changed. One rebuild per second per node is what that
    /// costs, over a table of a few dozen rows.
    fn acl_now(&self) -> Option<std::sync::Arc<acl::Effective>> {
        let watch = self.acl_watch.as_ref()?;
        let now = crate::accounts::now_secs();
        let epoch = watch.epoch.load(std::sync::atomic::Ordering::Acquire);
        let generation = self.accounts.as_ref().map_or(0, |store| store.generation());
        if let Some((cached_epoch, cached_generation, cached_now, table)) = self
            .acl_merged
            .read()
            .expect("merged acl read lock")
            .as_ref()
        {
            if *cached_epoch == epoch && *cached_generation == generation && *cached_now == now {
                return Some(std::sync::Arc::clone(table));
            }
        }
        let file = std::sync::Arc::clone(&watch.table.read().expect("acl read lock"));
        let merged = match self.accounts.as_ref() {
            Some(store) => file.merged(&store.acl()),
            None => (*file).clone(),
        };
        let effective = std::sync::Arc::new(merged.at(now));
        *self.acl_merged.write().expect("merged acl write lock") =
            Some((epoch, generation, now, std::sync::Arc::clone(&effective)));
        Some(effective)
    }

    /// Port the daemon is listening on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Writes the handoff file the SSH shim reads (D31): where this
    /// daemon is listening, and the loopback secret its git hooks
    /// authenticate with.
    ///
    /// Both are per-process values — an ephemeral port, a secret minted
    /// at startup — so a forced command written once cannot carry them.
    /// The secret is not returned to the caller, only written, at 0600.
    ///
    /// The ACL file goes in too, when one is configured, so a forced
    /// command that forgot `--acl-file` still enforces what this daemon
    /// enforces rather than reaching every repository.
    ///
    /// # Errors
    ///
    /// Any I/O error creating or writing the file.
    pub fn write_ssh_handoff(&self, path: &Path) -> std::io::Result<()> {
        let base = format!("{}://127.0.0.1:{}", self.scheme, self.port);
        let acl = self.acl_watch.as_ref().map(|watch| watch.path.as_path());
        // D36: the store goes in too, so the shim grades a self-served
        // account by the same grants the HTTP route does. Call
        // `enable_accounts` before this, or the line is absent and every
        // issued grant is invisible over SSH.
        let accounts = self.accounts.as_ref().map(|store| store.path());
        ssh::write_handoff(path, &base, &self.internal_token, acl, accounts)
    }

    /// Creates a bare repo `name` (e.g. `"owner/repo.git"`) with pushes
    /// enabled.
    ///
    /// # Errors
    ///
    /// Fails when the path exists or `git init` fails.
    pub fn create_repo(&self, name: &str) -> std::io::Result<()> {
        create_repo_in(&self.root, name).map(|_| ())
    }

    /// Brings an *existing* bare repo under this root back under the
    /// sequencer: the same hook and the same git config
    /// [`Node::create_repo`] installs.
    ///
    /// This is what a restore needs, and without it a restore is silently
    /// unsound. Objects arrive as a bundle, which `git clone --bare` and
    /// `git fetch` both unpack into a repo with **no `pre-receive` hook** —
    /// and a repo with no hook is served normally while every push into it
    /// bypasses the sequencer entirely, landing refs that no op in the log
    /// ever records. The alternative order is worse: letting the node
    /// create the repos empty and unpacking afterwards means startup
    /// reconciliation sees a log naming commits git does not have, decides
    /// the log is unbackable, and *appends retractions for every restored
    /// ref* (see [`Platform::reconcile_git_refs`]).
    ///
    /// It also re-points `gpg.ssh.allowedSignersFile`, which
    /// [`Node::create_repo`] wrote as an absolute path into the root that
    /// existed then. A restore onto a different path leaves that config
    /// naming a directory that may not exist, or worse, one that does and
    /// holds someone else's keys.
    ///
    /// Idempotent, and safe to run on every start. The one value it does
    /// not overwrite is `receive.certNonceSeed`: rotating it would refuse
    /// the signed pushes already in flight against the old seed.
    ///
    /// # Errors
    ///
    /// Fails when the path does not exist, is not a git repository, or a
    /// `git config` call fails.
    pub fn adopt_repo(&self, name: &str) -> std::io::Result<()> {
        let path = self.repo_path(name)?;
        if !path.join("objects").is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{name}: not a bare git repository"),
            ));
        }
        self.configure_repo(&path)
    }

    /// Adopts every bare repository already under this node's root,
    /// whatever put it there, and returns how many.
    ///
    /// Adoption is what installs the `pre-receive` hook, so until a
    /// repository has been adopted it is served with **no hook** and
    /// every push into it bypasses the sequencer, landing refs that no
    /// op in the log ever records — invariants 5 and 6 both, broken
    /// silently.
    ///
    /// That used to be reachable in ordinary operation, because
    /// adoption only ever happened for repositories named in
    /// `--create`. A repository restored from a bundle, moved in, or
    /// created against a running node was listed nowhere and hooked
    /// never. Walking the root closes the gap at its source: the
    /// question "what is this node about to serve" is answered by the
    /// filesystem, which is the same thing [`portable::export`] already
    /// asks.
    ///
    /// Idempotent, and meant to run on every start: the one value
    /// [`Node::adopt_repo`] does not overwrite is
    /// `receive.certNonceSeed`, so an ordinary restart costs a few `git
    /// config` calls and changes nothing.
    ///
    /// # Errors
    ///
    /// Fails when the root cannot be walked, or when a repository under
    /// it cannot be adopted. Both are fatal rather than skipped:
    /// serving an unadopted repository is the exact failure this
    /// prevents, so a node that cannot guarantee the hook must not
    /// start.
    pub fn adopt_existing_repos(&self) -> std::io::Result<usize> {
        let found = crate::portable::repos(&self.root).map_err(|why| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("cannot list repositories under the root: {why}"),
            )
        })?;
        for name in &found {
            self.adopt_repo(name)?;
        }
        Ok(found.len())
    }

    /// The configuration and hook shared by [`Node::create_repo`] and
    /// [`Node::adopt_repo`], applied to an initialized bare repo.
    fn configure_repo(&self, path: &Path) -> std::io::Result<()> {
        configure_repo_in(&self.root, path)
    }
}

/// [`Node::configure_repo`], against a root rather than a bound node.
///
/// Split out because creating a repository needs nothing from a `Node`
/// except where its repositories live, and the API handler that now
/// creates them holds the root but not the node — the same reason
/// `/api/queue/run` is routed outside the platform.
fn configure_repo_in(root: &Path, path: &Path) -> std::io::Result<()> {
    // Seeded once and then left alone: `create_repo` reaches here with
    // it unset, `adopt_repo` with it already set from whenever the repo
    // was made, and re-seeding would invalidate every signed push
    // holding a nonce from the old seed.
    if !std::process::Command::new("git")
        .args(["config", "--get", "receive.certNonceSeed"])
        .current_dir(path)
        .stdout(std::process::Stdio::null())
        .status()?
        .success()
        && !std::process::Command::new("git")
            .args([
                "config",
                "receive.certNonceSeed",
                &choir_identity::ActorKey::generate().actor_id().to_hex(),
            ])
            .current_dir(path)
            .status()?
            .success()
    {
        return Err(std::io::Error::other("git config failed"));
    }
    let ok = std::process::Command::new("git")
            .args(["config", "http.receivepack", "true"])
            .current_dir(path)
            .status()?
            .success()
            // Pin hooks to this repo: a host-global core.hooksPath (set
            // by e.g. husky) would otherwise silently bypass the
            // sequencer hook below.
            && std::process::Command::new("git")
                .args(["config", "core.hooksPath", "hooks"])
                .current_dir(path)
                .status()?
                .success()
            // Advertise push certificates (`git push --signed`) and
            // verify their ssh signatures against the allowed-signers
            // file (per-actor keys, L8).
            && std::process::Command::new("git")
                .args(["config", "gpg.format", "ssh"])
                .current_dir(path)
                .status()?
                .success()
            && std::process::Command::new("git")
                .args(["config", "gpg.ssh.allowedSignersFile"])
                .arg(
                    root.canonicalize()
                        .unwrap_or_else(|_| root.to_path_buf())
                        .join(".choir")
                        .join("allowed_signers"),
                )
                .current_dir(path)
                .status()?
                .success()
            // Serve filtered fetches, which is what makes `git clone
            // --filter=blob:none --sparse` and `git sparse-checkout` work
            // against this node. Git refuses a filter without it (D48).
            //
            // This is the whole of choir's answer to lazy checkouts of a
            // large repository: the client already ships the feature, and
            // the alternative -- a userspace filesystem hydrating blobs on
            // demand -- would be our code on the read path of every file
            // access, needing a kernel extension per platform, to
            // reimplement something the transport already does. A node
            // that speaks git inherits partial clone; it should not
            // reimplement it.
            && std::process::Command::new("git")
                .args(["config", "uploadpack.allowFilter", "true"])
                .current_dir(path)
                .status()?
                .success()
            // A promisor client fetches missing blobs by exact oid, and
            // those oids are reachable-but-not-advertised as far as
            // upload-pack is concerned. Without this every lazy hydration
            // after the initial clone fails, which presents as a working
            // clone whose first `git checkout` cannot read a file.
            && std::process::Command::new("git")
                .args(["config", "uploadpack.allowAnySHA1InWant", "true"])
                .current_dir(path)
                .status()?
                .success();
    if !ok {
        return Err(std::io::Error::other("git config failed"));
    }
    // The pre-receive hook routes every ref update of a push through
    // the platform sequencer (pre-receive, not update: only
    // pre-/post-receive see GIT_PUSH_CERT_* for signed pushes, and
    // one invocation covers the whole push). Outside the daemon (no
    // CHOIR_API) it is a no-op.
    //
    // Git applies no ref until this hook exits zero, so a refusal on
    // the third ref of a push has already left two ops in the durable
    // log for refs git will never create. Those refs are then stuck:
    // the pusher's `old` is git's absent value while the view holds
    // the stranded one, so every retry loses the CAS. Hence the
    // retraction pass over what this push already had accepted, which
    // is a compensating op rather than an erasure -- the log is
    // append-only, and an aborted push belongs in the history.
    let hook = path.join("hooks").join("pre-receive");
    std::fs::write(
            &hook,
            concat!(
                "#!/bin/sh\n",
                "# choir: route this push's ref updates through the platform sequencer.\n",
                "if [ -z \"$CHOIR_API\" ]; then cat >/dev/null; exit 0; fi\n",
                // The response body is captured rather than discarded
                // (which is what `curl -f` did): the node's refusals name
                // a reason and a repair, and a pusher told only "rejected
                // by sequencer" has to ask a human what they did wrong.
                "reply=$(mktemp) || exit 1\n",
                "post() {\n",
                "  payload=$(printf '{\"repo\":\"%s\",\"refname\":\"%s\",\"old\":\"%s\",\"new\":\"%s\",\"user\":\"%s\",\"cert_status\":\"%s\",\"signer\":\"%s\"}' \\\n",
                "    \"$CHOIR_REPO\" \"$3\" \"$1\" \"$2\" \"$CHOIR_USER\" \"$GIT_PUSH_CERT_STATUS\" \"$GIT_PUSH_CERT_SIGNER\")\n",
                "  code=$(curl -sk -o \"$reply\" -w '%{http_code}' \\\n",
                "    --connect-timeout 5 --max-time 30 \\\n",
                "    -X POST -H \"X-Choir-Internal: $CHOIR_INTERNAL\" \\\n",
                "    -d \"$payload\" \"$4\")\n",
                // A connection that never completed reports 000, which
                // falls through to the failure branch with the others.
                //
                // The timeouts are what make that sentence true. Without
                // them curl waits forever, and this hook holds the push
                // open while it does: the client has already sent every
                // object and sits there with no output and no failure.
                // That is exactly how a wrong scheme in this URL was
                // experienced -- a TLS handshake against a plaintext
                // port, hanging rather than refusing. A push that cannot
                // reach the sequencer must be rejected, loudly and soon;
                // it must never become a push that never ends. 30s is far
                // above a healthy decision, which the gate holds under
                // 100ms at p99, and far below a person's patience.
                "  case \"$code\" in 2??) return 0 ;; *) return 1 ;; esac\n",
                "}\n",
                "reason() {\n",
                "  sed -n 's/.*\"error\":\"\\([^\"]*\\)\".*/\\1/p' \"$reply\" | head -1\n",
                "}\n",
                // A file, not a shell variable: this has to survive being
                // read back line by line, and the accepted list is the
                // only record of what needs undoing.
                "done_refs=$(mktemp) || exit 1\n",
                "abort() {\n",
                "  while read -r a_old a_new a_ref; do\n",
                "    post \"$a_old\" \"$a_new\" \"$a_ref\" \"$CHOIR_ABORT\" ||\n",
                "      echo \"choir: could not retract $a_ref; the node's log now holds a ref \\\n",
                "this push did not create\" >&2\n",
                "  done < \"$done_refs\"\n",
                "}\n",
                "while read old new ref; do\n",
                "  if ! post \"$old\" \"$new\" \"$ref\" \"$CHOIR_API\"; then\n",
                "    why=$(reason)\n",
                "    if [ -n \"$why\" ]; then\n",
                "      echo \"choir: $ref rejected: $why\" >&2\n",
                "    else\n",
                "      echo \"choir: ref update rejected by sequencer: $ref\" >&2\n",
                "    fi\n",
                "    abort\n",
                "    rm -f \"$done_refs\" \"$reply\"\n",
                "    exit 1\n",
                "  fi\n",
                "  printf '%s %s %s\\n' \"$old\" \"$new\" \"$ref\" >> \"$done_refs\"\n",
                "done\n",
                "rm -f \"$done_refs\" \"$reply\"\n",
                "exit 0\n",
            ),
        )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// `POST /api/repo` — create a repository on a running node.
///
/// Until this existed, a repository could only be made by naming it in
/// `--create` at startup, so adding one to a live node meant stopping
/// it. That is the whole reason this endpoint is here: a person who has
/// just installed choir should be able to put a repository on their own
/// instance without restarting the thing they just started.
///
/// Nothing is appended to the log. A repository is not modelled in the
/// [`View`](choir_view::View) — `refs` is keyed by name and there is no
/// repository entity — and `portable::export` already answers "which
/// repositories exist" by walking the filesystem. Creating one is
/// therefore a node-local act with no op to write, and adding an
/// `OpKind` for it would be a format commitment bought for nothing. If
/// repositories should later become first-class in the total order that
/// stays open: the field would be additive, like every other.
///
/// The response carries the clone URL because the next thing the caller
/// does is clone, and making them assemble it from the name and the
/// base is how a trailing `.git` goes missing.
fn create_repo_request(root: &Path, body: &[u8]) -> (u16, String) {
    let refuse = |code: u16, error: &str, next: &str| {
        (
            code,
            serde_json::json!({ "error": error, "next": next }).to_string(),
        )
    };
    let Ok(request): Result<serde_json::Value, _> = serde_json::from_slice(body) else {
        return refuse(
            400,
            "the request body is not JSON",
            "POST {\"name\": \"owner/repo.git\"}",
        );
    };
    let Some(name) = request["name"].as_str() else {
        return refuse(
            400,
            "no `name` in the request",
            "POST {\"name\": \"owner/repo.git\"}",
        );
    };
    // A repository served without `.git` is one that `git clone` finds
    // by a name the node does not use, so the suffix is required rather
    // than guessed at. Appending it silently would make two spellings of
    // one repository, which is how an ACL entry comes to govern nothing.
    if !name.ends_with(".git") {
        return refuse(
            400,
            "a repository name must end in `.git`",
            "name it `owner/repo.git`",
        );
    }
    match create_repo_in(root, name) {
        Ok(_) => (
            201,
            serde_json::json!({ "format_version": 1, "name": name, "created": true }).to_string(),
        ),
        // Already there is not an error worth a 500: the caller asked
        // for a repository to exist and it does. It is still not a 201,
        // because a caller that would have pushed into an empty one
        // needs to know it is not empty.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => refuse(
            409,
            "that repository already exists",
            "clone it, or choose another name",
        ),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => refuse(
            400,
            "that is not a legal repository name",
            "use `owner/repo.git`, without `..`, `@` or `*`",
        ),
        Err(e) => (
            500,
            serde_json::json!({ "error": format!("could not create the repository: {e}") })
                .to_string(),
        ),
    }
}

/// Creates a bare repository under `root` and brings it under the
/// sequencer, against a root rather than a bound node.
///
/// The whole create path needs nothing from a [`Node`] but where its
/// repositories live, which is what lets a request create one: the API
/// handler holds the root and not the node.
///
/// # Errors
///
/// Fails when the name is not a legal repository name, when the path
/// already exists, or when `git init` or a `git config` call fails.
fn create_repo_in(root: &Path, name: &str) -> std::io::Result<PathBuf> {
    let path = repo_path_in(root, name)?;
    if path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            name.to_string(),
        ));
    }
    if !std::process::Command::new("git")
        .args(["init", "--bare", "-q"])
        .arg(&path)
        .status()?
        .success()
    {
        return Err(std::io::Error::other("git init failed"));
    }
    configure_repo_in(root, &path)?;
    Ok(path)
}

/// [`Node::repo_path`], against a root rather than a bound node.
fn repo_path_in(root: &Path, name: &str) -> std::io::Result<PathBuf> {
    // `@` is reserved so a repository can never alias the ACL's
    // `@node` pseudo-repository (D29); `*` likewise for its wildcard.
    if name.split('/').any(|c| c == ".." || c.is_empty())
        || name.starts_with('/')
        || name.contains('@')
        || name.contains('*')
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "bad repo name",
        ));
    }
    Ok(root.join(name))
}

impl Node {
    /// Rejects path traversal and normalizes the repo path under root.
    fn repo_path(&self, name: &str) -> std::io::Result<PathBuf> {
        repo_path_in(&self.root, name)
    }

    /// Serves requests until the process exits. Run on a dedicated thread.
    ///
    /// # Rate limiting and who is exempt (D33)
    ///
    /// The check runs on the request's own thread, after authentication —
    /// it has to be after, because the bucket is per authenticated user,
    /// and it must not be on the accept loop, which exists to accept.
    ///
    /// Three exemptions, each because applying the limit would be worse
    /// than the flood it prevents:
    ///
    /// 1. **The loopback hook callback.** A push of N refs makes N
    ///    `/api/git-update` calls; refusing the fourth one fails the push
    ///    halfway and drives the retraction path for refs git will never
    ///    create. It carries a node-minted secret over loopback, so it is
    ///    not an untrusted caller in the first place.
    /// 2. **A holder of a D29 `@node` grant.** That grant is already total
    ///    authority over the node. Throttling the one actor who can repair
    ///    it, during the incident the limiter is reporting, is the
    ///    lockout this feature must not cause.
    /// 3. **Any node without `--auth-file`.** There is no per-user
    ///    identity to bucket by, and a single shared `anon` bucket is a
    ///    self-inflicted denial of service rather than a limit.
    ///
    /// The D37 quotas ([`Node::enable_quotas`]) take the same three,
    /// from the same computed value rather than a second copy of the
    /// rule: whether a request is metered is one question, and answering
    /// it twice is how the two answers start to differ.
    pub fn serve_forever(&self) {
        loop {
            // A writer that has failed a durability barrier refuses every
            // submission from then on. Staying up in that state is worse
            // than being down: process supervision only restarts a process
            // that *exits*, so the node would sit there looking healthy
            // to launchd while rejecting everything, and a transient fsync
            // error would become permanent downtime that reads as uptime.
            // Exiting hands it back to supervision, which restarts into
            // the same replay path the D20 flip proved with `kill -9`.
            if self
                .platform
                .as_ref()
                .is_some_and(|p| p.durability_failed())
            {
                eprintln!(
                    "choir: the op log is no longer durable, so this node has stopped \
                     accepting writes; exiting so supervision restarts it. Check the \
                     filesystem backing the log."
                );
                // EX_TEMPFAIL: the condition may well clear on restart.
                std::process::exit(75);
            }
            // Do not block forever in `accept`: durability loss is
            // published by the writer thread and must take an otherwise
            // idle daemon down without waiting for another client to
            // arrive. One tenth of a second is far below supervisor and
            // human timescales while still avoiding a busy poll.
            let request = match self
                .server
                .recv_timeout(std::time::Duration::from_millis(100))
            {
                Ok(Some(request)) => request,
                Ok(None) if self.shutdown.load(std::sync::atomic::Ordering::Acquire) => return,
                Ok(None) => continue,
                Err(error) => {
                    eprintln!("choir: HTTP accept loop failed: {error}");
                    return;
                }
            };
            // Cheap stat between accepting a request and handling it, so
            // an edited keys file takes effect on *this* request: an
            // appended signing key becomes usable, and a newly bound
            // channel name becomes enforced, with no restart and no wait
            // for some later event.
            self.refresh_allowed_signers();
            // Same reasoning, same cost: an appended grant takes effect
            // on this request rather than on a restart.
            self.refresh_acl();
            // Same reasoning, quieter failure: the writer thread can only
            // record that an op missed the latency gate, never decide what
            // to do about it. Draining here puts the breach in the
            // operator's lag log while the node keeps serving.
            if let Some(platform) = self.platform.as_ref() {
                platform.drain_lag_log();
            }
            let root = self.root.clone();
            let auth = self.auth.clone();
            let platform = self.platform.clone();
            let queue = self.queue.clone();
            let queue_in_flight = std::sync::Arc::clone(&self.queue_in_flight);
            let ui_cache = std::sync::Arc::clone(&self.ui_cache);
            let acl = self.acl_now();
            let internal_token = self.internal_token.clone();
            let request_log = self.request_log.clone();
            let rate = self.rate.clone();
            let accounts = self.accounts.clone();
            let passkeys = self.passkeys;
            let sessions = self.sessions.clone();
            let quotas = self.quotas;
            let api_body_limit = self.api_body_limit;
            let browser_writes = self.browser_writes;
            let site_repo = self.site_repo.clone();
            let ready_min_free_bytes = self.ready_min_free_bytes;
            let counters = std::sync::Arc::clone(&self.counters);
            let started_unix = self.started_unix;
            let authenticated = self.auth.is_some();
            let port = self.port;
            let scheme = self.scheme;
            let listener_scheme = self.listener_scheme;
            let public_rate = std::sync::Arc::clone(&self.public_rate);
            let ssh_enabled = self.ssh_enabled;
            std::thread::spawn(move || {
                // D33. Started before anything else the thread does, so
                // the recorded duration is the node's whole cost. The
                // query string is dropped here and never carried further.
                let access = limits::Access::start(&request, std::sync::Arc::clone(&counters));
                let log = request_log.as_deref();
                // D39's client half, ahead of authentication and
                // deliberately so.
                //
                // It is a compile-time constant with no node state in
                // it, identical on every choir node, and the 401 it
                // would otherwise get names the realm anyway — so
                // there is nothing here a challenge would protect. What
                // a challenge *would* cost is the feature: a subresource
                // 401 that a browser declines to retry leaves the
                // ceremony `hidden`, which is not a visible failure but
                // an absent control, with the `<noscript>` sentence
                // suppressed because scripting is in fact enabled. A
                // write path that silently disappears is worse than a
                // public constant.
                //
                // Served whatever `--read-only-browser` says (D73). Each
                // ceremony in the file asks whether its own element is on
                // the page, and the pages decide that: a read-only browse
                // surface renders no `#verdict` and no `#comment`, so the
                // file that would drive them runs nothing. Withholding the
                // file instead took sign-in and D72's ask down with them,
                // which is a page whose button does nothing rather than a
                // page that offers no button.
                if request.url().split('?').next() == Some(ui::WEBAUTHN_JS_PATH) {
                    let outcome = respond_static_script(request);
                    // "anon" and not a name: no credential was
                    // evaluated on this path, and the request log must
                    // say what happened rather than what was sent.
                    access.finish(log, "anon", &outcome);
                    return;
                }
                // D57's front door, also ahead of authentication, and for
                // a plainer reason than D39's: the people these pages are
                // for do not have a credential yet. That is the whole
                // point of them.
                //
                // Everything the auth gate would have done downstream has
                // to be done here instead, because a request that returns
                // from this block never reaches it. In order: admission
                // control, which is *not* the authenticated limiter and
                // takes no argument describing the caller (see
                // `PublicLimiter`: a map keyed on something an anonymous
                // caller supplies is the attack, and the one key that is
                // not supplied by them is their address, which this
                // deployment does not handle at all); a bounded read of
                // any body; and `access.finish` on every exit, since each
                // branch logs itself.
                let public_path = request
                    .url()
                    .split(['?', '#'])
                    .next()
                    .unwrap_or("")
                    .to_string();
                let method = request.method().as_str();
                // D71's ceremony, public for the same reason D57's front
                // door is: a person signing in has no credential yet, and
                // the whole point of the page is to give them one. Both
                // API halves are pre-auth too, which is what makes the
                // challenge the one allocation an unauthenticated caller
                // can repeat, and why they sit behind the same
                // `PublicLimiter` as everything else in this block.
                //
                // The page and its form are public whatever `--passkeys`
                // says (D74): the credential an operator issued is what
                // every person holds before they hold anything else, and
                // the page that takes it is the page that replaced the
                // browser's own dialog. Only the ceremony's two halves
                // are gated on the switch that offers it.
                let signin_route = matches!(
                    (method, public_path.as_str()),
                    ("GET" | "POST", "/signin") | ("POST", "/api/signout")
                ) || (passkeys
                    && matches!(
                        (method, public_path.as_str()),
                        ("POST", "/api/signin") | ("POST", "/api/signin/challenge")
                    ));
                // D72's queue. Public for the same reason the front
                // door is: the caller holds no credential and the whole
                // point is that they can ask for one. What keeps it from
                // being an open registration is that it writes a row
                // that authorizes nothing, behind a proof of work, into
                // a capped table -- and that only an operator can turn
                // one of those rows into an invite.
                let asking_route = accounts.is_some()
                    && matches!(
                        (method, public_path.as_str()),
                        ("POST", "/api/access") | ("POST", "/api/access/challenge")
                    );
                let public = signin_route
                    || asking_route
                    || matches!((method, public_path.as_str()), ("GET" | "POST", "/join"))
                    || (method == "GET" && public_path == ui::CARD_PATH)
                    // The landing page replaces the challenge only for a
                    // request that presented nothing. A credential that
                    // was presented and is wrong still gets the `401`,
                    // because a reader who mistyped their password needs
                    // the browser to ask again rather than a page telling
                    // them what choir is.
                    || (method == "GET"
                        && matches!(public_path.as_str(), "/" | "/index.html")
                        && authenticated
                        && header(&request, "authorization").is_none());
                if public {
                    if let Some(retry) = public_rate.check() {
                        let outcome = respond_public_busy(request, retry);
                        access.finish(log, "anon", &outcome);
                        return;
                    }
                    let outcome = if signin_route {
                        respond_signin(
                            request,
                            &public_path,
                            Credentials {
                                auth: auth.as_ref().as_ref(),
                                accounts: accounts.as_deref(),
                            },
                            &sessions,
                            passkeys,
                            scheme,
                            api_body_limit,
                        )
                    } else if asking_route {
                        respond_access(
                            request,
                            &public_path,
                            accounts.as_deref(),
                            &sessions,
                            scheme,
                            api_body_limit,
                        )
                    } else if public_path == ui::CARD_PATH {
                        respond_card(request)
                    } else {
                        respond_join(
                            request,
                            accounts.as_deref(),
                            &public_path,
                            join_page::Offers {
                                ssh: ssh_enabled,
                                passkeys,
                            },
                            &sessions,
                            scheme,
                            &root,
                        )
                    };
                    // "anon", like the script constant above: no
                    // credential was evaluated, and the record must say
                    // what happened rather than what was presented. The
                    // invite id is deliberately not logged either — it is
                    // half of a live credential.
                    access.finish(log, "anon", &outcome);
                    return;
                }
                // Hook callbacks authenticate with the loopback secret
                // instead of user credentials.
                let internal_ok = (request.url().starts_with("/api/git-update")
                    || request.url().starts_with("/api/git-abort"))
                    && header(&request, "X-Choir-Internal").as_deref() == Some(&internal_token);
                let mut user = "anon".to_string();
                // D36. Set when the presented credential is an unredeemed
                // invite rather than an account, which may reach exactly
                // one route.
                let mut invite: Option<String> = None;
                // A signed-in browser presents no credential at all: the
                // cookie is the whole claim, and it is checked before the
                // Authorization header so a stale header cannot shadow a
                // live session.
                let session_token =
                    session::cookie(header(&request, "Cookie").as_deref(), session::COOKIE);
                let session_user = session_token
                    .as_deref()
                    .and_then(|token| sessions.user(token));
                let session_user_present = session_user.is_some();
                if let Some(table) = auth.as_ref() {
                    match session_user
                        .map(accounts::Principal::Account)
                        .or_else(|| authenticate(table, accounts.as_deref(), &request))
                    {
                        Some(accounts::Principal::Account(u)) => user = u,
                        Some(accounts::Principal::Invite(id)) => {
                            user.clone_from(&id);
                            invite = Some(id);
                        }
                        None if internal_ok => {}
                        None => {
                            // A browser that can run the ceremony is shown
                            // the page instead of the challenge, because
                            // `WWW-Authenticate` is answered by chrome no
                            // page can style, explain, or offer a passkey
                            // through. Everything else keeps the header:
                            // git speaks it, every API client speaks it,
                            // and a node with passkeys off has no other
                            // way in.
                            //
                            // The git routes are excluded by name rather
                            // than trusted to not send `text/html`, since
                            // what a client sends is not a promise about
                            // what it can do with the answer.
                            // Not gated on `--passkeys` any more (D74).
                            // The page's other half is a username and
                            // password form, which is the way in on every
                            // node whatever it offers, so withholding the
                            // page from a node without passkeys withheld
                            // the form too and left the grey box as the
                            // whole answer.
                            let wants_page = !request.url().contains(".git")
                                && header(&request, "accept")
                                    .is_some_and(|a| a.contains("text/html"));
                            let outcome = if wants_page {
                                let next = request.url().split('?').next().unwrap_or("/");
                                let next = if next.starts_with('/') { next } else { "/" };
                                let page = signin_page::render(
                                    passkeys,
                                    next,
                                    signin_page::Said::Nothing,
                                    reader_chrome(&request),
                                );
                                respond_scripted_page(request, page.status, page.html)
                            } else {
                                let body = "unauthorized\n";
                                let response = tiny_http::Response::from_string(body)
                                    .with_status_code(401)
                                    .with_header(
                                        tiny_http::Header::from_bytes(
                                            &b"WWW-Authenticate"[..],
                                            &b"Basic realm=\"choir\""[..],
                                        )
                                        .expect("static header"),
                                    );
                                served(request, response, 401, body.len() as u64)
                            };
                            access.finish(log, &user, &outcome);
                            return;
                        }
                    }
                }
                // The hook callbacks are privileged: they submit a ref op
                // under any user's name, spending authorization that the
                // git route which triggered them already checked.
                // Requiring the loopback secret keeps a user credential
                // from reaching them directly — which would otherwise
                // forge a ref update on any repository and walk straight
                // around the git-route check below.
                if (request.url().starts_with("/api/git-update")
                    || request.url().starts_with("/api/git-abort"))
                    && !internal_ok
                {
                    let body = "{\"error\":\"internal endpoint\"}\n";
                    let response = tiny_http::Response::from_string(body)
                        .with_status_code(403)
                        .with_header(
                            tiny_http::Header::from_bytes(
                                &b"Content-Type"[..],
                                &b"application/json"[..],
                            )
                            .expect("static header"),
                        );
                    let outcome = served(request, response, 403, body.len() as u64);
                    access.finish(log, &user, &outcome);
                    return;
                }
                // D33's three exemptions, hoisted because D37's quotas
                // take exactly the same three: the callback that a push
                // fans out into, the actor who can repair the node, and a
                // node with no identity to meter. A metering rule that
                // exempted one of them and not the other would be two
                // rules for one question.
                //
                // Short-circuited on "is anything metered at all" so a
                // node running none of this pays no ACL lookup per
                // request for it.
                let metered = (rate.is_some() || quotas.is_active()) && {
                    let node_wide = acl.as_deref().is_some_and(|table| {
                        table.allows(&user, &acl::Scope::Node, acl::Level::Read)
                    });
                    !(internal_ok || !authenticated || node_wide)
                };
                // Operational endpoints are authenticated by reaching this
                // point. Readiness performs independent checks rather than
                // echoing liveness: verified log structure, a live durable
                // sequencer, writable storage, free space, and ref agreement.
                if request.method().as_str() == "GET"
                    && matches!(request.url(), "/healthz" | "/readyz" | "/metrics")
                {
                    let outcome = handle_observability(
                        &root,
                        platform.as_deref(),
                        ready_min_free_bytes,
                        &counters,
                        started_unix,
                        request,
                    );
                    access.finish(log, &user, &outcome);
                    return;
                }
                // D33. After authentication, because the bucket is per
                // user; before any work, because a refused request should
                // cost the node as little as possible. The exemptions are
                // documented on `serve_forever`.
                if let Some(rate) = rate.as_deref() {
                    let refusal = if metered {
                        rate.check(&user, limits::class_of(access.path()))
                    } else {
                        None
                    };
                    if let Some(retry_after) = refusal {
                        // A reader who refreshed too fast gets a page; an
                        // agent gets the JSON it parses. Same refusal,
                        // same `Retry-After`, told in the surface the
                        // caller is already in.
                        let outcome = if is_browser_route(access.path()) {
                            let seconds = format!("{retry_after} seconds");
                            let html = ui::refusal(
                                "Too many requests, briefly",
                                429,
                                &ui::Refusal {
                                    code: "rate_limited",
                                    error: "This credential has spent its request allowance \
                                            for the moment. Nothing is wrong with the node \
                                            or with what you asked for.",
                                    expected: Some("requests within this node's per-user rate"),
                                    actual: Some(&seconds),
                                    next: "Wait, then reload. If this keeps happening while \
                                           you are reading rather than scripting, the \
                                           operator set the limit low enough to catch a \
                                           person and would want to know.",
                                },
                                &[],
                                reader_chrome(&request),
                            );
                            respond_page(request, 429, html, Some(retry_after))
                        } else {
                            respond_rate_limited(request, access.path(), retry_after)
                        };
                        access.finish(log, &user, &outcome);
                        return;
                    }
                }
                // D36. An unredeemed invite is a credential for exactly
                // one thing. Refused here, ahead of every route, rather
                // than by each route remembering to ask: the ACL grades
                // accounts, and an invite is not one yet — it holds no
                // grant, so several endpoints that require none would
                // otherwise let it through.
                if invite.is_some()
                    && !(request.method().as_str() == "POST"
                        && request.url() == "/api/accounts/redeem")
                {
                    // A person who was handed an invite and pasted the
                    // node's URL into a browser lands here, and it is
                    // very likely their first minute on this node. The
                    // JSON below says the true thing and tells them
                    // nothing they can act on without reading the source.
                    let outcome = if is_browser_route(request.url()) {
                        let html = ui::refusal(
                            "That invite is not an account yet",
                            403,
                            &ui::Refusal {
                                code: "invite_only",
                                error: "The credential you signed in with is an unredeemed \
                                        invite. An invite may do exactly one thing — become \
                                        an account — and it holds no grants until it does.",
                                expected: Some("an account token"),
                                actual: Some("an unredeemed invite"),
                                next: "Redeem it once, with the invite as the credential: \
                                       POST /api/accounts/redeem. It answers with a token; \
                                       sign in with that and this page will open. Invites \
                                       expire and are single-use, so do it now rather than \
                                       later.",
                            },
                            &[],
                            reader_chrome(&request),
                        );
                        respond_page(request, 403, html, None)
                    } else {
                        let body = "{\"error\":\"an invite may only be redeemed\"}\n";
                        let response = tiny_http::Response::from_string(body)
                            .with_status_code(403)
                            .with_header(
                                tiny_http::Header::from_bytes(
                                    &b"Content-Type"[..],
                                    &b"application/json"[..],
                                )
                                .expect("static header"),
                            );
                        served(request, response, 403, body.len() as u64)
                    };
                    access.finish(log, &user, &outcome);
                    return;
                }
                // Build one op's bytes for a browser to sign (D39).
                // Ahead of the platform API for the same reason the
                // account routes are: it needs no sequencer.
                if request.url().split('?').next().unwrap_or("") == "/api/prepare" {
                    if !browser_writes {
                        let body = r#"{"error":"browser writes are disabled; use the signed CLI"}"#;
                        let response = tiny_http::Response::from_string(body)
                            .with_status_code(403)
                            .with_header(
                                tiny_http::Header::from_bytes(
                                    &b"Content-Type"[..],
                                    &b"application/json"[..],
                                )
                                .expect("static header"),
                            );
                        let outcome = served(request, response, 403, body.len() as u64);
                        access.finish(log, &user, &outcome);
                        return;
                    }
                    let outcome = handle_prepare(&user, acl.as_deref(), api_body_limit, request);
                    access.finish(log, &user, &outcome);
                    return;
                }
                // The page a person enrols a passkey on (D39). Ahead
                // of the API block because it is a page, not an endpoint,
                // and it is gated by nothing but being authenticated: the
                // only account it can ever show is the caller's own.
                // D75. A passwordless account's one need for a
                // secret: git and the CLI speak basic auth and cannot
                // present a passkey. Minted here rather than at
                // redemption, so the caller is somebody this node has
                // already authenticated and is asking because something
                // wanted one.
                if request.url().split('?').next() == Some("/account/token") {
                    let outcome = respond_account_token(
                        request,
                        accounts.as_deref(),
                        &user,
                        acl.as_deref(),
                        scheme,
                    );
                    access.finish(log, &user, &outcome);
                    return;
                }
                if request.url().split('?').next().unwrap_or("") == "/account" {
                    // No `--read-only-browser` refusal here (D73).
                    // Enrolling a passkey writes to the accounts store and
                    // never to the op log, and it is the prerequisite for
                    // signing in -- so refusing it under a posture that
                    // also advertises `passkeys=enabled` left the one
                    // credential a browser can hold unobtainable from a
                    // browser.
                    if !passkeys {
                        let html = ui::refusal(
                            "Passkeys are not enabled",
                            503,
                            &ui::Refusal {
                                code: "passkeys_disabled",
                                error: "This node does not offer passkeys.",
                                expected: Some("a node with passkeys enabled"),
                                actual: Some("a passkey enrollment page"),
                                next: "Ask the operator to start the node with --passkeys; \
                                       until then, sign in with the credential you were given.",
                            },
                            &[],
                            reader_chrome(&request),
                        );
                        let outcome = respond_page(request, 503, html, None);
                        access.finish(log, &user, &outcome);
                        return;
                    }
                    let page = account_page::render(
                        accounts.as_deref(),
                        &user,
                        session_user_present,
                        acl.as_deref().is_some_and(|table| {
                            table
                                .check(&user, &acl::Scope::Node, acl::Level::Write)
                                .is_none()
                        }),
                        header(&request, "host")
                            .map(|host| format!("{scheme}://{host}"))
                            .as_deref(),
                        browse::Chrome {
                            site: None,
                            theme: chosen_theme(&request),
                            here: "/account",
                        },
                    );
                    let bytes = page.html.len() as u64;
                    // The same headers every other browser surface
                    // carries, which this page was missing entirely: it
                    // was the one page in the node running script with
                    // nothing constraining it.
                    let response = tiny_http::Response::from_string(page.html)
                        .with_status_code(page.status)
                        .with_header(
                            tiny_http::Header::from_bytes(
                                &b"Content-Type"[..],
                                &b"text/html; charset=utf-8"[..],
                            )
                            .expect("static header"),
                        )
                        .with_header(
                            tiny_http::Header::from_bytes(
                                &b"Cache-Control"[..],
                                &b"private, no-cache"[..],
                            )
                            .expect("static header"),
                        )
                        .with_header(
                            tiny_http::Header::from_bytes(
                                &b"Content-Security-Policy"[..],
                                SCRIPTED_PAGE_CSP,
                            )
                            .expect("static header"),
                        );
                    let outcome = served(request, response, page.status, bytes);
                    access.finish(log, &user, &outcome);
                    return;
                }
                // The operator's console (D72). A page, like `/account`
                // above, and ahead of the API block for the same reason.
                if request.url().split('?').next().unwrap_or("") == "/people" {
                    let outcome = respond_people(
                        request,
                        accounts.as_deref(),
                        acl.as_deref(),
                        &user,
                        &root,
                        scheme,
                        api_body_limit,
                    );
                    access.finish(log, &user, &outcome);
                    return;
                }
                // Credential self-service (D36). Ahead of the platform
                // API because it needs no sequencer: a node that serves
                // git and nothing else still issues credentials.
                if request.url().split('?').next().unwrap_or("") == "/api/accounts"
                    || request.url().starts_with("/api/accounts/")
                {
                    let outcome = handle_accounts(
                        SelfService {
                            store: accounts.as_deref(),
                            passkeys,
                        },
                        &user,
                        invite.as_deref(),
                        acl.as_deref(),
                        api_body_limit,
                        scheme,
                        request,
                    );
                    access.finish(log, &user, &outcome);
                    return;
                }
                // The surface as plain text, for an agent that has never
                // seen choir, and the sync contract it points at. Behind
                // auth like everything else; it describes the node
                // rather than exposing its contents. `llms.txt` naming a
                // file only a cloner can read would be worse than not
                // naming it, so the document a remote agent is told to
                // follow is served from the same place it is told about.
                // The same surface, machine-readable, with what *this*
                // node will actually accept merged in (D17). The static
                // half is generated and committed; the capabilities are
                // read off the live node, because a deployment's gates
                // are not a fact a committed file can carry honestly.
                // Search, for a caller that is not a browser (D62). Ahead
                // of the platform API for the same reason credentials
                // are: it reads git and needs no sequencer, so a node
                // serving nothing but repositories can still answer
                // "where is this". The grant closure is the browser's,
                // built here rather than passed down, because the two
                // surfaces answering differently about what a reader may
                // see is the one bug this endpoint could introduce.
                if request.url().split('?').next().unwrap_or("") == "/api/search" {
                    let url = request.url().to_string();
                    let readable = |repo: &str| match acl.as_deref() {
                        Some(table) => table.allows_repo(&user, repo, acl::Level::Read),
                        None => true,
                    };
                    let (status, body) = browse::api_search(
                        &root,
                        &readable,
                        browse::raw_param(&url, "repo"),
                        browse::param(&url, "rev").as_deref(),
                        browse::param(&url, "q").unwrap_or_default().as_str(),
                        browse::param(&url, "in").as_deref(),
                        browse::param(&url, "limit").as_deref(),
                    );
                    let bytes = body.len() as u64;
                    let response = tiny_http::Response::from_string(body)
                        .with_status_code(status)
                        .with_header(
                            tiny_http::Header::from_bytes(
                                &b"Content-Type"[..],
                                &b"application/json"[..],
                            )
                            .expect("static header"),
                        );
                    let outcome = served(request, response, status, bytes);
                    access.finish(log, &user, &outcome);
                    return;
                }
                // One actor's standing (D63). It reads the view rather
                // than git, so unlike search it needs the sequencer --
                // but it takes the *filtered* view, the same body this
                // caller would get from `/api/view`, which is what keeps
                // it from disclosing a change or review the ACL withheld.
                if request.url().split('?').next().unwrap_or("") == "/api/profile" {
                    let url = request.url().to_string();
                    // `raw_param` and not `param`: a channel legitimately
                    // carries a `/`, and `param`'s decoder refuses one
                    // because a decoded slash invents a path segment --
                    // the same split `raw_param` exists for on a
                    // repository name. Decoded by the function the `/p/`
                    // page uses, so the two surfaces cannot come to
                    // disagree about what a channel name is. Until this,
                    // every agent channel -- `operator/agent`, which is
                    // the ordinary shape here -- answered 400 on this
                    // endpoint while its page rendered.
                    let channel = browse::raw_param(&url, "channel")
                        .and_then(browse::decode_channel)
                        .unwrap_or_default();
                    let (status, body) = match (platform.as_deref(), channel.is_empty()) {
                        (_, true) => (
                            400,
                            serde_json::json!({ "error": "channel is required" }).to_string()
                                + "\n",
                        ),
                        (None, _) => (
                            503,
                            serde_json::json!({
                                "error": "this node runs no sequencer, so it holds no view to \
                                          derive a profile from"
                            })
                            .to_string()
                                + "\n",
                        ),
                        (Some(platform), false) => {
                            let seen = visible_view(platform, acl.as_deref(), &user);
                            match serde_json::from_str::<serde_json::Value>(&seen) {
                                Ok(view) => (200, profile::of(&view, &channel).to_string() + "\n"),
                                Err(error) => (
                                    500,
                                    serde_json::json!({
                                        "error": format!("the view did not parse: {error}")
                                    })
                                    .to_string()
                                        + "\n",
                                ),
                            }
                        }
                    };
                    let bytes = body.len() as u64;
                    let response = tiny_http::Response::from_string(body)
                        .with_status_code(status)
                        .with_header(
                            tiny_http::Header::from_bytes(
                                &b"Content-Type"[..],
                                &b"application/json"[..],
                            )
                            .expect("static header"),
                        );
                    let outcome = served(request, response, status, bytes);
                    access.finish(log, &user, &outcome);
                    return;
                }
                if request.url().split('?').next().unwrap_or("") == "/api/schema" {
                    let body = schema_with_capabilities(
                        accounts.is_some(),
                        acl.is_some(),
                        platform.is_some(),
                    );
                    let bytes = body.len() as u64;
                    let response = tiny_http::Response::from_string(body).with_header(
                        tiny_http::Header::from_bytes(
                            &b"Content-Type"[..],
                            &b"application/json"[..],
                        )
                        .expect("static header"),
                    );
                    let outcome = served(request, response, 200, bytes);
                    access.finish(log, &user, &outcome);
                    return;
                }
                if let Some(text) = match request.url() {
                    "/llms.txt" => Some(LLMS_TXT),
                    "/sync.md" => Some(SYNC_MD),
                    _ => None,
                } {
                    let response = tiny_http::Response::from_string(text).with_header(
                        tiny_http::Header::from_bytes(
                            &b"Content-Type"[..],
                            &b"text/plain; charset=utf-8"[..],
                        )
                        .expect("static header"),
                    );
                    let outcome = served(request, response, 200, text.len() as u64);
                    access.finish(log, &user, &outcome);
                    return;
                }
                // The node's own telemetry. Matched exactly so it can
                // never shadow a repository path: git routes are
                // `/owner/repo.git/...`, and a single bare segment
                // cannot name a repository, which always carries an
                // owner. It used to be the front door; `/` now belongs
                // to the repository index, because a reader arriving at
                // a code host is looking for code.
                //
                // The *path* is what is matched exactly, not the URL.
                // Comparing the whole URL made this the one page on the
                // node that a query string turned into a 404, so a
                // shared link carrying any `?…` — a cache-buster, a
                // tracking parameter a mail client appended — answered
                // "nothing is served at that address" about an address
                // that is served.
                // Setting the palette. A `GET` that mutates nothing but
                // one display cookie, so it is a link rather than a
                // form: the read surface runs no script, and a form
                // would put a `POST` and a button in a bar that is
                // otherwise navigation.
                if request.url().split(['?', '#']).next().unwrap_or("") == "/theme" {
                    let url = request.url().to_string();
                    let set = browse::param(&url, "set").unwrap_or_default();
                    let outcome = respond_theme(request, &set, &return_to(&url), scheme);
                    access.finish(log, &user, &outcome);
                    return;
                }
                if request.url().split(['?', '#']).next().unwrap_or("") == "/status" {
                    let outcome = handle_ui(
                        platform.as_deref(),
                        &ui_cache,
                        &user,
                        acl.as_deref(),
                        request,
                    );
                    access.finish(log, &user, &outcome);
                    return;
                }
                // One actor's standing, as a page (D63). The reader who
                // wants this is looking at a verdict and asking who gave
                // it, so it is a link from a review rather than a
                // document they were going to fetch as JSON.
                if let Some(rest) = request
                    .url()
                    .split(['?', '#'])
                    .next()
                    .unwrap_or("")
                    .strip_prefix("/p/")
                {
                    let channel = browse::decode_channel(rest);
                    let chrome = reader_chrome(&request);
                    let rendered = match (platform.as_deref(), channel) {
                        (Some(platform), Some(channel)) => {
                            let seen = visible_view(platform, acl.as_deref(), &user);
                            match serde_json::from_str::<serde_json::Value>(&seen) {
                                Ok(view) => browse::profile_page(
                                    &channel,
                                    &profile::of(&view, &channel),
                                    chrome,
                                ),
                                Err(_) => browse::no_such_actor(chrome),
                            }
                        }
                        // A name that is not a channel and a node with no
                        // view answer the same way, for the same reason
                        // an unreadable repository does: a reader is
                        // never told which of the two it was.
                        _ => browse::no_such_actor(chrome),
                    };
                    let outcome = respond_page(request, rendered.status, rendered.html, None);
                    access.finish(log, &user, &outcome);
                    return;
                }
                // Repository browsing (D30). Ahead of the git branch
                // below, but `browse::route` refuses any path carrying a
                // `.git` segment, so an owner named `r` keeps their
                // clone URL: this cannot shadow a repository, and the
                // check is theirs rather than this router's ordering.
                if let Some(page) = browse::route(request.url()) {
                    // A single-repository node narrows the page here,
                    // before anything reads the disk: `/` becomes that
                    // repository, and a page about another one is the
                    // same refusal a reader without a grant receives.
                    let page = match site_repo.as_deref() {
                        Some(site) => browse::scope(page, site),
                        None => Some(page),
                    };
                    let outcome = match page {
                        Some(page) => handle_browse(
                            &BrowseContext {
                                root: &root,
                                user: &user,
                                acl: acl.as_deref(),
                                platform: platform.as_deref(),
                                browser_writes,
                                site: site_repo.as_deref(),
                                scheme,
                                self_service: accounts.is_some(),
                            },
                            &page,
                            request,
                        ),
                        None => {
                            let denied = browse::no_such_repository(reader_chrome(&request));
                            respond_page(request, denied.status, denied.html, None)
                        }
                    };
                    access.finish(log, &user, &outcome);
                    return;
                }
                if request.url().starts_with("/api/") {
                    let base_url = format!("{listener_scheme}://127.0.0.1:{port}");
                    // A hook callback carries the loopback secret rather
                    // than a user's grants, so it is not an ACL subject.
                    let acl_for_api = if internal_ok { None } else { acl.as_deref() };
                    let outcome = handle_api(
                        ApiRequestContext {
                            platform: platform.as_deref(),
                            root: &root,
                            base_url: &base_url,
                            user: &user,
                            acl: acl_for_api,
                            push_acl: acl.as_deref(),
                            workspaces: metered.then_some(quotas.workspaces).flatten(),
                            body_limit: api_body_limit,
                            queue: queue.as_ref(),
                            queue_in_flight: &queue_in_flight,
                        },
                        request,
                    );
                    access.finish(log, &user, &outcome);
                    return;
                }
                // A mistyped address. Everything below is git
                // smart-HTTP, and every smart-HTTP path carries a `.git`
                // segment — the claim `browse.rs` makes and a test pins —
                // so a `GET` without one matched no route above and can
                // never be a git client. It is a person who typed
                // something slightly wrong, and `git http-backend`'s CGI
                // 404 tells them nothing about where they are. Answering
                // here also means the two named destinations are the
                // same whether or not an ACL is configured; before this,
                // an ACL node said "no such repository" and a node
                // without one said whatever git said.
                if repo_from_path(request.url()).is_none()
                    && matches!(request.method().as_str(), "GET" | "HEAD")
                {
                    let html = ui::refusal(
                        "Nothing is served at that address",
                        404,
                        &ui::Refusal {
                            code: "no_such_page",
                            error: "This node answers on a small, fixed set of addresses, and \
                                    that is not one of them.",
                            expected: Some(
                                "/ for node state, /r/ for repositories, /api/… \
                                            for the JSON surface",
                            ),
                            actual: Some(access.path()),
                            next: "Start from the node page and follow links; every address \
                                   this surface has is reachable from one of the two below. \
                                   A clone URL is different — it ends in .git.",
                        },
                        &[("/", "node state"), ("/r/", "repositories")],
                        reader_chrome(&request),
                    );
                    let outcome = respond_page(request, 404, html, None);
                    access.finish(log, &user, &outcome);
                    return;
                }
                // Git smart-HTTP. The repository is in the URL, so this
                // decision needs nothing but the path — which is why it
                // sits here, once, rather than inside the CGI bridge. A
                // path naming no repository, or an operation outside the
                // smart-HTTP surface, is refused rather than handed to
                // `git http-backend`.
                if let Some(table) = acl.as_ref() {
                    let method = request.method().as_str().to_string();
                    let denial = match acl::git_requirement(&method, request.url()) {
                        Some((repo, level)) => table.check(&user, &acl::Scope::Repo(repo), level),
                        None => Some(acl::Denial {
                            status: 404,
                            reason: "no such repository".to_string(),
                        }),
                    };
                    if let Some(denial) = denial {
                        let outcome = respond_git_denial(request, &denial);
                        access.finish(log, &user, &outcome);
                        return;
                    }
                }
                // Platform-enabled daemons pass the sequencer callback
                // into git's hook environment.
                let mut extra_env = Vec::new();
                if platform.is_some() {
                    if let Some(repo) = repo_from_path(request.url()) {
                        extra_env.push((
                            "CHOIR_API".to_string(),
                            format!("{listener_scheme}://127.0.0.1:{port}/api/git-update"),
                        ));
                        extra_env.push((
                            "CHOIR_ABORT".to_string(),
                            format!("{listener_scheme}://127.0.0.1:{port}/api/git-abort"),
                        ));
                        extra_env.push(("CHOIR_REPO".to_string(), repo));
                        extra_env.push(("CHOIR_USER".to_string(), user.clone()));
                        extra_env.push(("CHOIR_INTERNAL".to_string(), internal_token));
                    }
                }
                let outcome = handle(
                    root,
                    request,
                    &extra_env,
                    metered.then_some(quotas.push_bytes).flatten(),
                );
                access.finish(log, &user, &outcome);
            });
        }
    }

    /// Handle for stopping the accept loop (used by tests).
    pub fn unblock(&self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Release);
        self.server.unblock();
    }
}

/// Value of the first header named `name`, if present.
fn header(request: &tiny_http::Request, name: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
}

/// Writes `response` and reports what was served, so the caller can hand
/// the outcome to [`limits::Access::finish`] (D33). The byte count is the
/// body size the caller already knows; tiny_http does not expose it back.
///
/// **`X-Content-Type-Options: nosniff` is added here, to everything.**
/// It used to sit on the four responders somebody remembered to put it
/// on, which left git's own CGI output — a pusher's bytes, typed by git —
/// without it. That is the response `script-src 'self'` most needs it on:
/// the review page licenses any same-origin URL as a script source, and
/// a browser will happily execute a `text/plain` body as JavaScript
/// unless this header says not to. One funnel, so the claim in
/// [`SCRIPTED_PAGE_CSP`] is true by construction rather than by
/// inspection.
fn served<R: std::io::Read>(
    request: tiny_http::Request,
    mut response: tiny_http::Response<R>,
    status: u16,
    bytes: u64,
) -> std::io::Result<(u16, u64)> {
    response.add_header(
        tiny_http::Header::from_bytes(&b"X-Content-Type-Options"[..], &b"nosniff"[..])
            .expect("static header"),
    );
    request.respond(response).map(|()| (status, bytes))
}

/// Answers a request that exhausted its per-user allowance (D33).
///
/// `Retry-After` in whole seconds is the machine-readable half; the body
/// repeats it because a git client shows the operator the body and
/// nothing else. Plain text on a git path for that reason, JSON on an API
/// path so a client that parses every response still can.
fn respond_rate_limited(
    request: tiny_http::Request,
    path: &str,
    retry_after: u64,
) -> std::io::Result<(u16, u64)> {
    let (body, content_type) = match limits::class_of(path) {
        limits::Class::Git => (
            format!("rate limit exceeded; retry in {retry_after}s\n"),
            &b"text/plain; charset=utf-8"[..],
        ),
        limits::Class::Api => (
            serde_json::json!({
                "error": format!("rate limit exceeded; retry in {retry_after}s"),
                "retry_after_secs": retry_after,
            })
            .to_string(),
            &b"application/json"[..],
        ),
    };
    let bytes = body.len() as u64;
    let response = tiny_http::Response::from_string(body)
        .with_status_code(429)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], content_type)
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Retry-After"[..], retry_after.to_string().as_bytes())
                .expect("retry-after header"),
        );
    served(request, response, 429, bytes)
}

/// Answers a git request whose body was over the per-user push ceiling
/// (D37).
///
/// `413` is the exact HTTP meaning, and the body is plain text because a
/// git client shows the operator the body and nothing else. It names both
/// numbers: a refusal that says only "too large" leaves the pusher
/// guessing how much to split by.
fn respond_push_too_large(
    request: tiny_http::Request,
    limit: u64,
    size: u64,
) -> std::io::Result<(u16, u64)> {
    let body = format!(
        "push refused: {size} bytes, and this node's per-user limit is {limit} bytes per \
         request\npush fewer objects at a time, or ask the operator to raise the limit\n"
    );
    let bytes = body.len() as u64;
    let response = tiny_http::Response::from_string(body)
        .with_status_code(413)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=utf-8"[..])
                .expect("static header"),
        );
    served(request, response, 413, bytes)
}

/// Answers an API request whose body exceeded the node-wide ceiling.
fn respond_api_too_large(
    request: tiny_http::Request,
    limit: u64,
    size: u64,
) -> std::io::Result<(u16, u64)> {
    let body = serde_json::json!({
        "error": "request body too large",
        "limit_bytes": limit,
        "actual_bytes": size,
    })
    .to_string();
    let bytes = body.len() as u64;
    let response = tiny_http::Response::from_string(body)
        .with_status_code(413)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                .expect("static header"),
        );
    served(request, response, 413, bytes)
}

/// A `304`, carrying the headers a client must not keep a stale copy of.
///
/// **A `304` is a header update, not just "nothing changed".** RFC 9111
/// has a cache replace its stored response's headers with the ones the
/// `304` carries; any header the `304` omits keeps whatever value it had
/// when the body was first stored. So a policy header left out of a
/// `304` is not merely absent for one exchange — it is *frozen* at the
/// version the client first saw, for as long as the entry lives.
///
/// This is not hypothetical. The browse pages' `ETag` is derived from a
/// commit, so it survives a daemon upgrade; the `304` carried only the
/// tag; and a reader whose cache held a page from before
/// [`BROWSER_CSP`] gained `form-action 'self'` kept the old
/// `form-action 'none'` through every reload. The search box rendered,
/// focused, took a term, and was refused by a policy the server had
/// already stopped sending. Only a cache-bypassing reload fixed it,
/// which is not a thing a reader knows to do.
///
/// One function rather than four call sites, for the reason
/// [`BROWSER_CSP`] is one constant: a fourth hand-assembled `304` is how
/// one of them ends up missing a header again.
fn not_modified(tag: &str, csp: &[u8]) -> tiny_http::Response<std::io::Empty> {
    tiny_http::Response::empty(304)
        .with_header(
            tiny_http::Header::from_bytes(&b"ETag"[..], tag.as_bytes()).expect("etag header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Security-Policy"[..], csp)
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"private, no-cache"[..])
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Referrer-Policy"[..], &b"no-referrer"[..])
                .expect("static header"),
        )
}

/// The policy every page on the browser surface carries.
///
/// One constant rather than a copy per responder: it is the layer that
/// holds if the escaper ever misses something, and a fifth hand-typed
/// copy is how one of them ends up subtly weaker than the rest.
///
/// **`form-action 'self'`, not `'none'`.** It was `'none'` for as long as
/// this surface had no form, and that was the right value then. It is a
/// one-word difference with a silent failure mode: a browser blocks the
/// submission and reports it to the console *only*, so the search box
/// rendered correctly, focused correctly, accepted a term, and did
/// nothing at all on `Enter`. Nothing in the served HTML was wrong, which
/// is why no assertion over the HTML could have caught it.
///
/// `'self'` is still the whole guarantee that matters here: a form on
/// this surface can submit to this origin and to no other, so no page
/// this node renders can be turned into a way of posting a reader's
/// input somewhere else.
const BROWSER_CSP: &[u8] = b"default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

/// [`BROWSER_CSP`] plus permission to run [`ui::WEBAUTHN_JS`] and to
/// `fetch` this node (D39), carried by the two pages with a ceremony on
/// them and by nothing else.
///
/// **Per page, never node-wide.** A single header would license script
/// on a dozen pages that must never run any, and the read surface's whole
/// guarantee is that it runs none. Pages with no ceremony keep
/// [`BROWSER_CSP`] untouched, which is `default-src 'none'` with no
/// `script-src` at all.
///
/// **`'self'` rather than a digest per script, and what that costs.** The
/// digests were narrower: they licensed three exact byte strings, where
/// this licenses any same-origin URL a `<script src>` can name. What
/// makes that trade sound is `X-Content-Type-Options: nosniff` on every
/// response this node sends — with it, a browser refuses to execute
/// anything whose type is not JavaScript, and the only JavaScript type
/// served here is [`ui::WEBAUTHN_JS_PATH`], a compile-time constant.
/// Repository content is served as escaped HTML, and git's own CGI
/// output gets the header added on the way out for exactly this reason.
///
/// What it buys: the digests were computed by shelling out to `openssl`
/// at bind, and a host without `openssl` fell back to `'unsafe-inline'`
/// — a weaker policy than intended, reached silently, visible only in a
/// served header. That path is gone rather than documented.
const SCRIPTED_PAGE_CSP: &[u8] = b"default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'; script-src 'self'; connect-src 'self'";

/// Serves [`ui::WEBAUTHN_JS`], the only script this node has.
///
/// A weak `ETag` over the bytes rather than a version string: the file
/// changes when the binary does and never otherwise, so a digest of what
/// is being sent is both the cheapest correct tag and the one that cannot
/// go stale against a rebuild. `no-cache` with a tag means a browser
/// revalidates and is answered `304` with an empty body, which is one
/// round trip per page load and no bytes.
///
/// The response carries `nosniff` for the same reason every other one
/// does, and here it is load-bearing in the other direction: this is the
/// single URL on the node that *is* JavaScript, and a browser under
/// `script-src 'self'` must be able to tell it apart from everything
/// else.
fn respond_static_script(request: tiny_http::Request) -> std::io::Result<(u16, u64)> {
    static ETAG: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let tag = ETAG.get_or_init(|| {
        format!(
            "W/\"{}\"",
            &choir_oplog::ContentHash::blake3(ui::WEBAUTHN_JS.as_bytes()).to_hex()[..18]
        )
    });
    if header(&request, "If-None-Match").as_deref() == Some(tag.as_str()) {
        return served(request, not_modified(tag, BROWSER_CSP), 304, 0);
    }
    let bytes = ui::WEBAUTHN_JS.len() as u64;
    let response = tiny_http::Response::from_string(ui::WEBAUTHN_JS)
        .with_header(
            tiny_http::Header::from_bytes(
                &b"Content-Type"[..],
                &b"text/javascript; charset=utf-8"[..],
            )
            .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"ETag"[..], tag.as_bytes()).expect("etag header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"private, no-cache"[..])
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Security-Policy"[..], BROWSER_CSP)
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Referrer-Policy"[..], &b"no-referrer"[..])
                .expect("static header"),
        );
    served(request, response, 200, bytes)
}

/// The chrome facts for one request: the palette this reader chose and
/// the address they are on.
///
/// One helper rather than two lines at every refusal, because a refusal
/// rendered without it is a page that flips to the system palette at the
/// worst moment — the reader has just hit a wall, and the page changing
/// colour reads as a second thing going wrong.
fn reader_chrome(request: &tiny_http::Request) -> browse::Chrome<'static> {
    browse::Chrome {
        site: None,
        theme: chosen_theme(request),
        // Deliberately not the failing address: these pages are
        // refusals, and a palette link that returns the reader to the
        // page that just refused them is a link back into a wall. The
        // empty string is the front door.
        here: "",
    }
}

/// The theme this reader has chosen, or `None` for "follow the system".
///
/// The stylesheet has carried `:root[data-theme="light"]` and its dark
/// twin since it was written, and nothing ever set the attribute, so the
/// manual override was decoration: a reader whose system said light read
/// a light page and had no way to say otherwise.
///
/// A cookie rather than a query parameter, because a preference that
/// only holds for the link you clicked is not a preference. It carries
/// no identity, is not a credential, and is never trusted for anything
/// but which palette to paint — which is why the parser accepts exactly
/// two spellings and treats everything else, including a value some
/// other software set, as absent.
fn chosen_theme(request: &tiny_http::Request) -> Option<&'static str> {
    let jar = header(request, "cookie")?;
    jar.split(';').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        if name.trim() != "theme" {
            return None;
        }
        match value.trim() {
            "dark" => Some("dark"),
            "light" => Some("light"),
            _ => None,
        }
    })
}

/// Where `/theme` may send a reader back to.
///
/// One leading slash and nothing that leaves this origin. `//evil.test`
/// is the case worth naming: a browser reads a protocol-relative URL as
/// another host, so a redirector that checks only "starts with `/`" is
/// an open redirect. A backslash is refused for the same reason — some
/// clients normalize it to a slash before resolving.
///
/// Anything that fails lands on the front door rather than being
/// reported: this is a preference control, and a reader who arrives at
/// the repository list with their theme changed has lost nothing.
/// The `to=` parameter of a `/theme` request, decoded and vetted.
///
/// Read here rather than through [`browse::param`], which refuses any
/// value that decodes to contain a `/` — right for a path *segment*,
/// which is all it was ever asked for, and wrong for a whole path.
/// Reusing it silently sent every reader to the front door instead of
/// back to the page they were on: the exact shape of the D52 change-id
/// bug, where a segment grammar was applied to something that is not a
/// segment.
///
/// Everything that fails lands on the front door via
/// [`safe_return_to`], including a value that is not UTF-8 at all.
fn return_to(url: &str) -> String {
    let Some((_, query)) = url.split_once('?') else {
        return "/".to_string();
    };
    let raw = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("to="))
        .unwrap_or("");
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let Some(hex) = raw
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            else {
                return "/".to_string();
            };
            out.push(hex);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    match String::from_utf8(out) {
        Ok(path) => safe_return_to(&path),
        Err(_) => "/".to_string(),
    }
}

fn safe_return_to(to: &str) -> String {
    let ok = to.starts_with('/')
        && !to.starts_with("//")
        && !to.contains('\\')
        && !to.chars().any(char::is_control);
    if ok {
        to.to_string()
    } else {
        "/".to_string()
    }
}

/// Whether this URL is one a person is reading in a browser.
///
/// The browser surface is the node page, its alias, and everything under
/// the D30 browse prefix — minus anything carrying a `.git` segment,
/// which belongs to git however it is spelled (an owner really named `r`
/// has a clone URL under `/r/`, and this must not claim it).
///
/// Decided on the route rather than on `Accept`: that header says what a
/// client will *take*, not who it is, and every agent in this workspace
/// sends `*/*`. Keying on the route is what lets a refusal be a page for
/// the reader who met a wall while staying the JSON an agent can parse
/// everywhere else.
fn is_browser_route(url: &str) -> bool {
    if repo_from_path(url).is_some() {
        return false;
    }
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path == "/"
        || path == "/index.html"
        || path == "/status"
        || path == "/theme"
        || path == "/r"
        || path.starts_with("/r/")
}

/// Answers a browser-surface request with a page.
///
/// Carries the same headers the D28 and D30 pages carry, because a
/// refusal renders content this node did not author just as they do —
/// a ref name in an error message is still a ref name somebody chose.
fn respond_page(
    request: tiny_http::Request,
    status: u16,
    html: String,
    retry_after: Option<u64>,
) -> std::io::Result<(u16, u64)> {
    let bytes = html.len() as u64;
    let mut response = tiny_http::Response::from_string(html)
        .with_status_code(status)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"private, no-cache"[..])
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Security-Policy"[..], BROWSER_CSP)
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Referrer-Policy"[..], &b"no-referrer"[..])
                .expect("static header"),
        )
        // The palette is chosen by a cookie, so two readers of the same
        // address can be owed two different documents. `private` already
        // keeps a shared cache out; this says *why* the body varies, for
        // anything that stores it anyway.
        .with_header(
            tiny_http::Header::from_bytes(&b"Vary"[..], &b"Cookie"[..]).expect("static header"),
        );
    if let Some(secs) = retry_after {
        response.add_header(
            tiny_http::Header::from_bytes(&b"Retry-After"[..], secs.to_string().as_bytes())
                .expect("retry-after header"),
        );
    }
    served(request, response, status, bytes)
}

/// A page that carries the ceremony script, under the policy that lets
/// it run.
///
/// [`respond_page`] sends [`BROWSER_CSP`], which has no `script-src`
/// at all -- correct for every refusal and every read-only page, and
/// wrong for the one page whose entire purpose is a control the script
/// reveals. Kept as a separate function rather than a flag on the
/// other, so widening the policy is something a caller asks for by
/// name.
fn respond_scripted_page(
    request: tiny_http::Request,
    status: u16,
    html: String,
) -> std::io::Result<(u16, u64)> {
    let bytes = html.len() as u64;
    let response = tiny_http::Response::from_string(html)
        .with_status_code(status)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..])
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"X-Content-Type-Options"[..], &b"nosniff"[..])
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Referrer-Policy"[..], &b"no-referrer"[..])
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Security-Policy"[..], SCRIPTED_PAGE_CSP)
                .expect("static header"),
        );
    served(request, response, status, bytes)
}

/// A JSON body with a status, for the pre-auth ceremony that has no
/// other responder to borrow.
fn respond_json(
    request: tiny_http::Request,
    status: u16,
    body: &str,
) -> std::io::Result<(u16, u64)> {
    let bytes = body.len() as u64;
    let response = tiny_http::Response::from_string(body.to_string())
        .with_status_code(status)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..])
                .expect("static header"),
        );
    served(request, response, status, bytes)
}

/// base64url as WebAuthn writes it, tolerating the padded spelling.
///
/// `clientDataJSON` is required to be unpadded base64url, but the node
/// reads it rather than writing it, and a decoder that refuses padding
/// would make this node the one that rejects an otherwise valid
/// authenticator over a spelling nobody would think to check.
fn base64url_any(input: &str) -> Option<Vec<u8>> {
    let standard: String = input
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .filter(|c| *c != '=')
        .collect();
    base64_decode(&standard)
}

/// Serves the three halves of the passkey sign-in ceremony (D71): the
/// page, the challenge it spends, and the assertion it posts back.
///
/// All three answer before any credential has been evaluated, so this
/// function does what the auth gate would have done: it never trusts a
/// name from the body, it bounds the read, and it returns an outcome the
/// caller logs as `anon`.
///
/// The account is looked up from the credential id in the assertion
/// rather than from anything the caller says they are. That is the
/// property that makes a sign-in unforgeable without also making it
/// enumerable: a wrong credential id and a wrong signature produce the
/// same refusal, so the endpoint never says whether an account exists.
#[allow(clippy::too_many_arguments)]
/// The two tables a presented username and password are graded against.
///
/// One argument rather than two because they answer one question -- is
/// this a credential this node issued or an operator wrote down -- and
/// the sign-in form has to ask both. An operator's own credential lives
/// in the auth file and never in the store, so a form that consulted
/// only the store would refuse the one person who has to be able to get
/// in before anybody else does.
#[derive(Clone, Copy)]
struct Credentials<'a> {
    auth: Option<&'a AuthTable>,
    accounts: Option<&'a accounts::Accounts>,
}

impl Credentials<'_> {
    /// The account name a `user`/`secret` pair proves, or `None`.
    ///
    /// An unredeemed invite is deliberately not a sign-in: it reaches one
    /// route, its own redemption, and a session opened for it would be a
    /// session for an account that does not exist yet.
    fn account_for(&self, user: &str, secret: &str) -> Option<String> {
        if let Some(expected) = self.auth.and_then(|table| table.get(user)) {
            if constant_time_eq(expected.as_bytes(), secret.as_bytes()) {
                return Some(user.to_string());
            }
        }
        match self.accounts?.authenticate(user, secret) {
            Some(accounts::Principal::Account(name)) => Some(name),
            _ => None,
        }
    }
}

fn respond_signin(
    mut request: tiny_http::Request,
    path: &str,
    credentials: Credentials<'_>,
    sessions: &session::Sessions,
    passkeys: bool,
    scheme: &'static str,
    body_limit: std::num::NonZeroU64,
) -> std::io::Result<(u16, u64)> {
    let accounts = credentials.accounts;
    if path == "/api/signout" {
        // Forgetting the token is the whole mechanism: nothing else
        // anywhere would still honour it. Idempotent on purpose, and it
        // never says whether the token was live, because a caller signing
        // out has no use for that answer and a caller guessing tokens
        // would.
        if let Some(token) = session::cookie(header(&request, "Cookie").as_deref(), session::COOKIE)
        {
            sessions.close(&token);
        }
        // A redirect and a cleared cookie rather than JSON, so the control
        // that calls this can be an ordinary form and work with scripting
        // off, like every other control on these pages.
        let secure = if scheme == "https" { "; Secure" } else { "" };
        let cleared = format!(
            "{}=; Path=/; Max-Age=0; SameSite=Lax; HttpOnly{secure}",
            session::COOKIE
        );
        let response = tiny_http::Response::empty(303)
            .with_header(
                tiny_http::Header::from_bytes(&b"Location"[..], &b"/"[..])
                    .expect("location header"),
            )
            .with_header(
                tiny_http::Header::from_bytes(&b"Set-Cookie"[..], cleared.as_bytes())
                    .expect("set-cookie header"),
            )
            .with_header(
                tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..])
                    .expect("static header"),
            );
        return served(request, response, 303, 0);
    }
    if path == "/signin" && request.method().as_str() != "POST" {
        let next = safe_next(
            request
                .url()
                .split_once("?next=")
                .map(|(_, raw)| raw.split('&').next().unwrap_or("").to_string()),
            "/",
        );
        let chrome = reader_chrome(&request);
        let page = signin_page::render(passkeys, &next, signin_page::Said::Nothing, chrome);
        return respond_scripted_page(request, page.status, page.html);
    }
    // The form (D74). Plain `POST`, so it works with scripting off and a
    // browser's password manager can offer to keep what was typed.
    if path == "/signin" {
        if !same_origin(&request, scheme) {
            let html = ui::refusal(
                "Cross-origin sign-in refused",
                403,
                &ui::Refusal {
                    code: "cross_origin",
                    error: "That form was submitted from another site.",
                    expected: Some("this node's own sign-in page"),
                    actual: Some("a form somewhere else"),
                    next: "Open this node's address and sign in there.",
                },
                &[],
                reader_chrome(&request),
            );
            return respond_page(request, 403, html, None);
        }
        let chrome = reader_chrome(&request);
        let body = match quota::read_bounded(request.as_reader(), Some(body_limit))? {
            quota::Body::Complete(body) => String::from_utf8_lossy(&body).into_owned(),
            quota::Body::OverLimit { limit, size } => {
                return respond_api_too_large(request, limit, size)
            }
        };
        let next = safe_next(join_page::form_value(&body, "next"), "/");
        let signed_in = join_page::form_value(&body, "user")
            .zip(join_page::form_value(&body, "secret"))
            .and_then(|(user, secret)| credentials.account_for(user.trim(), &secret));
        let Some(user) = signed_in else {
            // The same page again, at the same status, saying the one
            // thing it may say. No redirect: a `303` here would put the
            // failure in history and lose what was typed in the other
            // field.
            let page = signin_page::render(passkeys, &next, signin_page::Said::NoMatch, chrome);
            return respond_scripted_page(request, page.status, page.html);
        };
        // Straight to the page that enrols a passkey, when this node
        // offers them and this account has none (D74). That is the step
        // everybody has to take exactly once and the one nobody knows to
        // look for, and a redirect is a cheaper way to say it than a
        // sentence somebody has to read.
        let enrol_first = passkeys
            && accounts.is_some_and(|store| {
                store.has_account(&user) && store.passkeys_json(&user).is_empty()
            });
        let land = if enrol_first {
            "/account"
        } else {
            next.as_str()
        };
        let token = sessions.open(&user);
        return respond_session(request, &token, land, scheme);
    }

    let body = match quota::read_bounded(request.as_reader(), Some(body_limit))? {
        quota::Body::Complete(body) => body,
        quota::Body::OverLimit { limit, size } => {
            return respond_api_too_large(request, limit, size)
        }
    };

    if path == "/api/signin/challenge" {
        let issued = sessions.issue_challenge();
        // WebAuthn wants the challenge base64url, and what it stands for
        // is the hex of a hash: the same bytes `webauthn_challenge` hands
        // a browser approving an operation, so one verifier checks both.
        let answer = serde_json::json!({
            "format_version": 1,
            "challenge": prepare::base64url_nopad(issued.to_hex().as_bytes()),
        })
        .to_string();
        return respond_json(request, 200, &answer);
    }

    // `/api/signin`.
    let refuse = |request| {
        respond_json(
            request,
            401,
            r#"{"error":"that passkey did not sign this node's challenge"}"#,
        )
    };
    let Some(store) = accounts else {
        return respond_json(
            request,
            503,
            r#"{"error":"account self-service is not enabled on this node"}"#,
        );
    };
    let Ok(body) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return respond_json(request, 400, r#"{"error":"body must be JSON"}"#);
    };
    let field = |name: &str| {
        body.get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let (Some(key_id), Some(signature), Some(authenticator), Some(client_data)) = (
        field("key_id"),
        field("signature_hex"),
        field("authenticator_data_hex"),
        field("client_data_json_hex"),
    ) else {
        return respond_json(request, 400, r#"{"error":"the assertion is incomplete"}"#);
    };
    let Some((user, spki)) = store.account_for_credential(&key_id) else {
        return refuse(request);
    };
    let witness = choir_oplog::Witness {
        key_id,
        scheme: Some(choir_oplog::scheme::WEBAUTHN_ES256),
        signature: match platform::hex_decode(&signature) {
            Some(bytes) => bytes,
            None => return refuse(request),
        },
        authenticator_data: platform::hex_decode(&authenticator),
        client_data_json: platform::hex_decode(&client_data),
        credential_key: None,
    };
    // The challenge inside the assertion decides which challenge is being
    // spent, and spending it is what stops the same assertion opening a
    // second session. Read before verification only because verification
    // needs to know which bytes were promised; an assertion naming a
    // challenge this node never issued is refused here.
    let Some(claimed) = claimed_challenge(&witness) else {
        return refuse(request);
    };
    if !sessions.spend_challenge(&claimed) {
        return refuse(request);
    }
    if choir_identity::verify_webauthn_assertion(&spki, &claimed, &witness).is_err() {
        return refuse(request);
    }

    let token = sessions.open(&user);
    let secure = if scheme == "https" { "; Secure" } else { "" };
    let cookie = format!(
        "{}={token}; Path=/; Max-Age=43200; SameSite=Lax; HttpOnly{secure}",
        session::COOKIE
    );
    let answer = serde_json::json!({ "format_version": 1, "user": user }).to_string();
    let bytes = answer.len() as u64;
    let response = tiny_http::Response::from_string(answer)
        .with_status_code(200)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Set-Cookie"[..], cookie.as_bytes())
                .expect("set-cookie header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..])
                .expect("static header"),
        );
    served(request, response, 200, bytes)
}

/// A `next` from a request, reduced to somewhere on this node.
///
/// A path, starting with one slash. `//evil.example` is a *protocol
/// relative URL* and would send somebody who signed in here to another
/// origin, which is the whole open-redirect family in one line.
fn safe_next(raw: Option<String>, fallback: &str) -> String {
    raw.filter(|next| next.starts_with('/') && !next.starts_with("//"))
        .unwrap_or_else(|| fallback.to_string())
}

/// Opens the browser session cookie and sends the reader on to `land`.
///
/// `SameSite=Lax` is what keeps every state-changing form on this node
/// out of reach of another site: a cross-site `POST` does not carry this
/// cookie. `HttpOnly` because no script here reads it, and `Secure`
/// whenever the node is speaking https, so a session cannot be sent in
/// clear by a downgrade.
fn respond_session(
    request: tiny_http::Request,
    token: &str,
    land: &str,
    scheme: &'static str,
) -> std::io::Result<(u16, u64)> {
    let secure = if scheme == "https" { "; Secure" } else { "" };
    let cookie = format!(
        "{}={token}; Path=/; Max-Age=43200; SameSite=Lax; HttpOnly{secure}",
        session::COOKIE
    );
    let response = tiny_http::Response::empty(303)
        .with_header(
            tiny_http::Header::from_bytes(&b"Location"[..], land.as_bytes())
                .expect("location header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Set-Cookie"[..], cookie.as_bytes())
                .expect("set-cookie header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..])
                .expect("static header"),
        );
    served(request, response, 303, 0)
}

/// The challenge an assertion says it signed, as the node spells one.
///
/// `clientDataJSON` carries it base64url, and what it decodes to is the
/// hex of a [`ContentHash`]; this turns that back into the hash so the
/// store can be asked whether it issued it.
fn claimed_challenge(witness: &choir_oplog::Witness) -> Option<choir_hash::ContentHash> {
    let json = witness.client_data_json.as_ref()?;
    let client: serde_json::Value = serde_json::from_slice(json).ok()?;
    let encoded = client.get("challenge")?.as_str()?;
    let hex = String::from_utf8(base64url_any(encoded)?).ok()?;
    choir_hash::ContentHash::from_hex(&hex)
}

/// Serves the social-preview card (D57).
///
/// Immutable and cached for a year: the bytes are compiled into the
/// binary, so the only way they change is a new binary, and a chat client
/// that has to refetch a card it already holds is spending a stranger's
/// request budget on a picture.
fn respond_card(request: tiny_http::Request) -> std::io::Result<(u16, u64)> {
    let bytes = ui::CARD.len() as u64;
    let response = tiny_http::Response::from_data(ui::CARD)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"image/png"[..])
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(
                &b"Cache-Control"[..],
                &b"public, max-age=31536000, immutable"[..],
            )
            .expect("static header"),
        );
    served(request, response, 200, bytes)
}

/// The most a pre-auth request body may be (D57).
///
/// Two hex credentials and an ssh public key, with room to spare. It is
/// not [`Node::api_body_limit`] because that ceiling is for authenticated
/// callers doing real work, and this one is reached by anybody at all: a
/// megabyte a stranger can spend without a credential is a megabyte they
/// can spend in a loop.
const JOIN_BODY_BYTES: u64 = 8 * 1024;

/// Answers a pre-auth request that the public limiter refused.
///
/// Plain text rather than a page. A caller hitting this is a loop, not a
/// reader, and rendering the whole stylesheet to tell them so would spend
/// exactly the resource the limiter is protecting.
fn respond_public_busy(request: tiny_http::Request, retry: u64) -> std::io::Result<(u16, u64)> {
    let body = "too many requests\n";
    let response = tiny_http::Response::from_string(body)
        .with_status_code(429)
        .with_header(
            tiny_http::Header::from_bytes(&b"Retry-After"[..], retry.to_string().as_bytes())
                .expect("a number is a valid header value"),
        );
    served(request, response, 429, body.len() as u64)
}

/// Serves the public front door: the landing page and the invite link.
///
/// Headers differ from [`respond_page`] in one deliberate way:
/// `Cache-Control: no-store` rather than `private, no-cache`. A request
/// here carries a live credential in its URL and an answer may carry a
/// freshly minted one in its body, and neither belongs in a cache that
/// something else can read — including the browser's own disk cache on a
/// shared machine.
fn respond_join(
    mut request: tiny_http::Request,
    store: Option<&accounts::Accounts>,
    path: &str,
    offers: join_page::Offers,
    sessions: &session::Sessions,
    scheme: &'static str,
    root: &std::path::Path,
) -> std::io::Result<(u16, u64)> {
    let join_page::Offers { ssh, passkeys } = offers;
    let _ = ssh;
    let contact = operator_contact(root);
    let contact = contact.as_deref();
    let theme = chosen_theme(&request);
    let chrome = browse::Chrome {
        site: None,
        theme,
        // Empty: these pages carry no navigation bar, so there is no
        // palette link that would need somewhere to return to — and an
        // address that did carry one would be carrying the invite secret
        // into an `href`.
        here: "",
    };
    let origin = header(&request, "host").map(|host| format!("{scheme}://{host}"));
    let url = request.url().to_string();
    let page = if path == "/join" {
        if request.method().as_str() == "POST" {
            match quota::read_bounded(
                request.as_reader(),
                std::num::NonZeroU64::new(JOIN_BODY_BYTES),
            ) {
                Ok(quota::Body::Complete(bytes)) => {
                    let body = String::from_utf8_lossy(&bytes).into_owned();
                    join_page::post(store, &body, origin.as_deref(), chrome)
                }
                // An over-long body is not told apart from a bad one: the
                // page a stranger sees is the same either way, and the
                // distinction is only useful to somebody probing.
                _ => join_page::not_valid(403, theme),
            }
        } else {
            join_page::get(
                store,
                join_page::param(&url, "i").as_deref(),
                join_page::param(&url, "k").as_deref(),
                join_page::Offers { ssh, passkeys },
                chrome,
                origin.as_deref(),
                join_page::now_unix_secs(),
            )
        }
    } else {
        join_page::landing(
            theme,
            &join_page::Door {
                contact,
                // A node with no store cannot hold a queue, so the front
                // door does not offer a form whose one button would
                // answer 503 (D72).
                asking: store.is_some(),
            },
        )
    };
    let bytes = page.html.len() as u64;
    let scripted = page.scripted;
    // A passwordless redemption ends signed in (D75): there is no
    // credential for the reader to present afterwards, so the cookie is
    // the only thing that makes the route finish rather than end at a
    // page saying "now log in with the nothing you were given".
    let opened = page.session.as_deref().map(|user| sessions.open(user));
    let mut response = tiny_http::Response::from_string(page.html)
        .with_status_code(page.status)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..])
                .expect("static header"),
        )
        .with_header(
            // The page says which it needs, because it is the only thing
            // that knows whether it emitted a `<script>` tag (D72). A
            // responder deciding from the route is how D71's sign-in page
            // shipped with a button the header would not let run.
            tiny_http::Header::from_bytes(
                &b"Content-Security-Policy"[..],
                if scripted {
                    SCRIPTED_PAGE_CSP
                } else {
                    BROWSER_CSP
                },
            )
            .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Referrer-Policy"[..], &b"no-referrer"[..])
                .expect("static header"),
        );
    if let Some(token) = opened.as_deref() {
        let secure = if scheme == "https" { "; Secure" } else { "" };
        let cookie = format!(
            "{}={token}; Path=/; Max-Age=43200; SameSite=Lax; HttpOnly{secure}",
            session::COOKIE
        );
        response = response.with_header(
            tiny_http::Header::from_bytes(&b"Set-Cookie"[..], cookie.as_bytes())
                .expect("set-cookie header"),
        );
    }
    // An invite link that reaches a search index is an invite spent by a
    // crawler. The header carries where a `<meta>` tag cannot: on the
    // redirect-free fetch a crawler actually makes.
    if path == "/join" {
        response = response.with_header(
            tiny_http::Header::from_bytes(&b"X-Robots-Tag"[..], &b"noindex, nofollow"[..])
                .expect("static header"),
        );
    }
    served(request, response, page.status, bytes)
}

/// Serves D72's two pre-auth endpoints: issue a challenge, and take a
/// request that solved it.
///
/// The order inside the `POST /api/access` arm is the security property.
/// The challenge is spent **first**, from an in-memory table, before a
/// single hash is computed: that makes the work verifiable exactly once
/// per challenge issued, and it keeps a caller from turning one solved
/// stamp into sixty-four rows. Verifying the stamp before spending the
/// challenge would leave the same solution good until it expired.
///
/// Refusals here are deliberately not specific. A caller that sends a
/// stale challenge, an unsolved nonce or a challenge this node never
/// issued gets the same sentence, because the differences are only useful
/// to somebody probing how the cost is checked.
fn respond_access(
    mut request: tiny_http::Request,
    path: &str,
    store: Option<&accounts::Accounts>,
    sessions: &session::Sessions,
    scheme: &'static str,
    body_limit: std::num::NonZeroU64,
) -> std::io::Result<(u16, u64)> {
    let origin = header(&request, "host").map(|host| format!("{scheme}://{host}"));
    if path == "/api/access/challenge" {
        let challenge = sessions.issue_challenge();
        return respond_json(
            request,
            200,
            &serde_json::json!({
                "challenge": challenge.to_hex(),
                "bits": work::WORK_BITS,
            })
            .to_string(),
        );
    }
    let body = match quota::read_bounded(request.as_reader(), Some(body_limit))? {
        quota::Body::Complete(body) => body,
        quota::Body::OverLimit { limit, size } => {
            return respond_api_too_large(request, limit, size)
        }
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return respond_json(request, 400, r#"{"error":"body must be JSON"}"#);
    };
    let refused = r#"{"error":"that stamp is not one this node issued and has not been spent; reload the page and try again"}"#;
    let (Some(challenge), Some(nonce)) = (
        json.get("challenge").and_then(serde_json::Value::as_str),
        json.get("nonce").and_then(serde_json::Value::as_str),
    ) else {
        return respond_json(request, 400, refused);
    };
    let Some(parsed) = choir_hash::ContentHash::from_hex(challenge) else {
        return respond_json(request, 400, refused);
    };
    if !sessions.spend_challenge(&parsed) {
        return respond_json(request, 400, refused);
    }
    if !work::solved(challenge, nonce) {
        return respond_json(request, 400, refused);
    }
    let (status, answer) = match store {
        Some(store) => with_join_url(store.request_access(&json), origin.as_deref(), "request"),
        None => (
            503,
            r#"{"error":"account self-service is not enabled on this node"}"#.to_string(),
        ),
    };
    respond_json(request, status, &answer)
}

/// Sets or clears the theme cookie and sends the reader back where they
/// were.
///
/// `303`, not `302`: it is the status that says "the result of this is
/// at another address, fetch it with `GET`", which is exactly true here
/// and leaves no room for a client to repeat the request as something
/// else.
///
/// The cookie is `SameSite=Lax` and `HttpOnly`. Lax because a theme
/// chosen from a link on this node is the only way it is ever set, and
/// `HttpOnly` because nothing on this surface runs script — a cookie
/// script cannot read is one less thing for a future page to leak.
/// `Secure` follows the scheme this node is actually serving rather
/// than being hardcoded either way. Always-on would be a control that
/// silently does nothing on the loopback node this is developed
/// against: the browser drops the cookie, the palette never sticks, and
/// nothing in the response says why. Never-on would leave a real
/// deployment setting a cookie over TLS that a browser then sends in
/// clear if anything ever reaches it over plain HTTP. The node knows
/// which one it is, so it says.
fn respond_theme(
    request: tiny_http::Request,
    set: &str,
    back: &str,
    scheme: &str,
) -> std::io::Result<(u16, u64)> {
    // Clearing is `Max-Age=0`, which is how a cookie is deleted; any
    // spelling other than the two real ones clears rather than errors,
    // so a hand-typed `/theme?set=nonsense` returns the reader to the
    // system default instead of to a refusal page.
    let secure = if scheme == "https" { "; Secure" } else { "" };
    let cookie = match set {
        "dark" | "light" => {
            format!("theme={set}; Path=/; Max-Age=31536000; SameSite=Lax; HttpOnly{secure}")
        }
        _ => format!("theme=; Path=/; Max-Age=0; SameSite=Lax; HttpOnly{secure}"),
    };
    let response = tiny_http::Response::empty(303)
        .with_header(
            tiny_http::Header::from_bytes(&b"Location"[..], back.as_bytes())
                .expect("location header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Set-Cookie"[..], cookie.as_bytes())
                .expect("set-cookie header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..])
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Referrer-Policy"[..], &b"no-referrer"[..])
                .expect("static header"),
        );
    served(request, response, 303, 0)
}

/// Answers a git request the ACL refused. Plain text, because that is
/// what a git client surfaces to whoever ran the command.
fn respond_git_denial(
    request: tiny_http::Request,
    denial: &acl::Denial,
) -> std::io::Result<(u16, u64)> {
    let body = format!("{}\n", denial.reason);
    let bytes = body.len() as u64;
    let response = tiny_http::Response::from_string(body)
        .with_status_code(denial.status)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=utf-8"[..])
                .expect("static header"),
        );
    served(request, response, denial.status, bytes)
}

/// Extracts `owner/repo.git` from a smart-HTTP path like
/// `/owner/repo.git/git-receive-pack`.
pub(crate) fn repo_from_path(url: &str) -> Option<String> {
    let path = url.split('?').next().unwrap_or(url);
    let end = path
        .find(".git/")
        .map(|i| i + 4)
        .or_else(|| path.ends_with(".git").then_some(path.len()))?;
    Some(path[1..end].to_string())
}

/// Checks a request's basic-auth credentials against the table; returns
/// the authenticated username.
fn authorized(table: &AuthTable, request: &tiny_http::Request) -> Option<String> {
    let (user, token) = basic_auth(request)?;
    let expected = table.get(&user)?;
    constant_time_eq(expected.as_bytes(), token.as_bytes()).then_some(user)
}

/// Compares two secrets without exiting early on length or content, so
/// timing does not leak how much of one matched the other.
///
/// Extracted when session tokens became a second thing worth comparing
/// this way. Written as one function rather than two loops because the
/// property is easy to state and easy to lose: an early `return false`
/// added later for readability would be invisible in review and would
/// undo it.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().min(b.len()) {
        diff |= (a[i] ^ b[i]) as usize;
    }
    diff == 0
}

/// Identifies a request: an operator credential from the auth file, or a
/// self-service account or invite from the store (D36).
///
/// The file is consulted first, so a name the operator wrote by hand can
/// never be shadowed by an issued one. The store refuses to issue those
/// names in the first place; checking in this order means the property
/// does not depend on that refusal alone.
fn authenticate(
    table: &AuthTable,
    accounts: Option<&accounts::Accounts>,
    request: &tiny_http::Request,
) -> Option<accounts::Principal> {
    if let Some(user) = authorized(table, request) {
        return Some(accounts::Principal::Account(user));
    }
    let (user, secret) = basic_auth(request)?;
    accounts?.authenticate(&user, &secret)
}

/// The `user` and `secret` halves of a basic-auth header, undecided.
fn basic_auth(request: &tiny_http::Request) -> Option<(String, String)> {
    let auth_header = header(request, "Authorization")?;
    let b64 = auth_header.strip_prefix("Basic ")?.trim();
    let creds = String::from_utf8(base64_decode(b64)?).ok()?;
    let (user, secret) = creds.split_once(':')?;
    Some((user.to_string(), secret.to_string()))
}

/// Serves `POST /api/prepare`: the bytes of one op, for a browser that
/// is about to sign them with a passkey (D39).
///
/// The author is always the authenticated caller and never a name in the
/// body, the same rule passkey enrolment follows. A body that could
/// choose its author would let this endpoint mint a payload claiming
/// somebody else — refused at admission by the view's own author check,
/// but only after the node had helpfully built it.
///
/// Only comments come through here. A verdict is fully determined by the
/// review and the button, so it is rendered into the page; a comment
/// carries text nobody has typed yet, which is the whole reason this
/// round trip exists.
fn handle_prepare(
    user: &str,
    acl: Option<&acl::Effective>,
    body_limit: std::num::NonZeroU64,
    mut request: tiny_http::Request,
) -> std::io::Result<(u16, u64)> {
    let req_body = match quota::read_bounded(request.as_reader(), Some(body_limit))? {
        quota::Body::Complete(body) => body,
        quota::Body::OverLimit { limit, size } => {
            return respond_api_too_large(request, limit, size)
        }
    };
    let method = request.method().as_str().to_string();
    let path = request.url().split('?').next().unwrap_or("").to_string();
    let (status, body) = match acl {
        Some(table) => match acl::api_denial(table, user, &method, &path, &req_body, |_| None) {
            Some(denial) => (
                denial.status,
                serde_json::json!({ "error": denial.reason }).to_string(),
            ),
            None => prepare_body(user, &req_body),
        },
        None => prepare_body(user, &req_body),
    };
    let bytes = body.len() as u64;
    let response = tiny_http::Response::from_string(body)
        .with_status_code(status)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                .expect("static header"),
        );
    served(request, response, status, bytes)
}

/// The body half of [`handle_prepare`], split out so the authorization
/// half above has one shape regardless of whether a table exists.
fn prepare_body(user: &str, req_body: &[u8]) -> (u16, String) {
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(req_body) else {
        return (400, r#"{"error":"body must be JSON"}"#.to_string());
    };
    let field = |name: &str| {
        json.get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    if field("kind") != Some("comment") {
        return (
            400,
            r#"{"error":"`kind` must be \"comment\"; a verdict is prepared in the page"}"#
                .to_string(),
        );
    }
    let (Some(id), Some(text)) = (field("id"), field("body")) else {
        return (
            400,
            r#"{"error":"`id` (the review) and `body` (the comment) are required"}"#.to_string(),
        );
    };
    if text.chars().count() > MAX_COMMENT_CHARS {
        return (
            400,
            serde_json::json!({
                "error": format!("a comment is at most {MAX_COMMENT_CHARS} characters"),
            })
            .to_string(),
        );
    }
    // Minted here, not taken from the body: the comment id is the
    // author's retry identity, so a browser that submits one prepared
    // payload twice must hit the refusal that identity exists to give.
    let comment_id = choir_identity::ActorKey::generate().actor_id().to_hex();
    let prepared = prepare::comment(id, user, &comment_id, text);
    (
        200,
        serde_json::json!({
            "payload_hex": prepared.payload_hex,
            "challenge": prepared.challenge,
            "comment": comment_id,
            "channel": user,
        })
        .to_string(),
    )
}

/// Longest comment the prepare endpoint will build. A review thread is
/// discussion, and the op log keeps every byte of it forever.
const MAX_COMMENT_CHARS: usize = 4096;

/// Serves the operator's console (D72), both the page and the three
/// actions on it.
///
/// One function for `GET` and `POST` because they are one surface: the
/// forms post back to the address that rendered them, which is what keeps
/// the page and its actions from drifting into disagreeing about which
/// grants exist.
///
/// Gated at `@node write`, the same authority
/// [`crate::acl::api_denial`] requires to mint an invite through the API.
/// It has to be: every button here is one of those calls.
fn respond_people(
    mut request: tiny_http::Request,
    store: Option<&accounts::Accounts>,
    acl: Option<&acl::Effective>,
    user: &str,
    root: &std::path::Path,
    scheme: &'static str,
    body_limit: std::num::NonZeroU64,
) -> std::io::Result<(u16, u64)> {
    let chrome = browse::Chrome {
        site: None,
        theme: chosen_theme(&request),
        here: "/people",
    };
    let Some(store) = store else {
        let html = ui::refusal(
            "Account self-service is off",
            503,
            &ui::Refusal {
                code: "accounts_disabled",
                error: "This node was started without an accounts file.",
                expected: Some("a node that issues credentials"),
                actual: Some("the operator's console"),
                next: "Start the daemon with --accounts-file to issue and answer invites here.",
            },
            &[],
            reader_chrome(&request),
        );
        return respond_page(request, 503, html, None);
    };
    let denial = acl.and_then(|table| table.check(user, &acl::Scope::Node, acl::Level::Write));
    if let Some(denial) = denial {
        let html = ui::refusal(
            "Not yours to see",
            denial.status,
            &ui::Refusal {
                code: "forbidden",
                error: "This page shows and changes who may reach this node.",
                expected: Some("@node write"),
                actual: Some("the grants you hold"),
                next: "Ask the operator, who is whoever holds the node's own credential.",
            },
            &[],
            reader_chrome(&request),
        );
        return respond_page(request, denial.status, html, None);
    }
    // What an operator may grant is what they may read, which for a
    // `@node write` holder is everything on the node.
    let repos = browse::repositories(root, &|_| true);

    if request.method().as_str() != "POST" {
        let said = match request.url().split_once("?said=") {
            Some((_, code)) => said_in_words(code.split('&').next().unwrap_or("")),
            None => "",
        };
        let page = people_page::render(
            store,
            &repos,
            operator_contact(root).as_deref(),
            said,
            chrome,
        );
        return respond_console(request, page);
    }
    if !same_origin(&request, scheme) {
        let html = ui::refusal(
            "Cross-origin write refused",
            403,
            &ui::Refusal {
                code: "cross_origin",
                error: "That form was submitted from another site.",
                expected: Some("a form on this node"),
                actual: Some("a form somewhere else"),
                next: "Open this node's own page and try again.",
            },
            &[],
            reader_chrome(&request),
        );
        return respond_page(request, 403, html, None);
    }
    let body = match quota::read_bounded(request.as_reader(), Some(body_limit))? {
        quota::Body::Complete(body) => String::from_utf8_lossy(&body).into_owned(),
        quota::Body::OverLimit { limit, size } => {
            return respond_api_too_large(request, limit, size)
        }
    };
    let origin = header(&request, "host").map(|host| format!("{scheme}://{host}"));
    let field = |key: &str| join_page::form_value(&body, key);
    // `.git` is how the ACL spells a repository, and leaving it off is
    // the mistake that grants nothing at all. Added here rather than
    // refused: the select offers repository names, and there is one right
    // answer.
    let grant = || {
        let repo = field("repo")?;
        let level = field("level")?;
        Some(format!("{repo}.git {level}"))
    };
    match field("action").as_deref() {
        Some("grant") => {
            let (Some(id), Some(grant)) = (field("request_id"), grant()) else {
                return respond_people_result(request, "bad");
            };
            let (status, _) = store.grant_request(
                user,
                &serde_json::json!({ "request_id": id, "grants": [grant] }),
            );
            respond_people_result(request, if status == 200 { "granted" } else { "bad" })
        }
        Some("contact") => {
            // Absent rather than empty is how the form spells "clear
            // it", since `form_value` refuses a blank value.
            let value = field("contact").unwrap_or_default();
            match write_operator_contact(root, &value) {
                Ok(()) => respond_people_result(request, "contact"),
                Err(_) => respond_people_result(request, "bad"),
            }
        }
        Some("decline") => {
            let Some(id) = field("request_id") else {
                return respond_people_result(request, "bad");
            };
            let (status, _) = store.decline_request(&serde_json::json!({ "request_id": id }));
            respond_people_result(request, if status == 200 { "declined" } else { "bad" })
        }
        Some("invite") => {
            let Some(grant) = grant() else {
                return respond_people_result(request, "bad");
            };
            // A readable name is optional (D75): the seat stays open
            // either way, and the person redeeming picks the username.
            let name = field("display_name");
            let mut body = serde_json::json!({ "grants": [grant] });
            if let Some(name) = name.as_deref() {
                body["display_name"] = serde_json::json!(name);
            }
            let (status, answer) =
                with_join_url(store.invite(user, &body), origin.as_deref(), "invite");
            let parsed: serde_json::Value = serde_json::from_str(&answer).unwrap_or_default();
            match (status, parsed["join_url"].as_str()) {
                (200, Some(link)) => {
                    let page = people_page::minted(
                        link,
                        name.as_deref().unwrap_or("whoever opens it"),
                        chrome,
                    );
                    respond_console(request, page)
                }
                _ => respond_people_result(request, "bad"),
            }
        }
        _ => respond_people_result(request, "bad"),
    }
}

/// The outcome of a console action, as a sentence.
///
/// Chosen from a fixed set here rather than echoed out of the query
/// string, so the page cannot be made to say anything by a link somebody
/// sends an operator.
fn said_in_words(code: &str) -> &'static str {
    match code {
        "granted" => "Let in. The link they already hold now works.",
        "declined" => "Declined. Their link says only that it is not valid.",
        "contact" => "Saved. The front page offers it to anybody who arrives with nothing.",
        "bad" => "That did not work. Nothing changed.",
        _ => "",
    }
}

/// Post/redirect/get for a console action, so a refresh does not repeat it.
fn respond_people_result(request: tiny_http::Request, said: &str) -> std::io::Result<(u16, u64)> {
    let location = format!("/people?said={said}");
    let response = tiny_http::Response::empty(303)
        .with_header(
            tiny_http::Header::from_bytes(&b"Location"[..], location.as_bytes())
                .expect("location header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..])
                .expect("static header"),
        );
    served(request, response, 303, 0)
}

/// Sends a console page with the headers every browser surface carries.
///
/// [`BROWSER_CSP`], not the scripted one: nothing on this page runs, and
/// a header that allowed script here would be permission granted to a
/// page that has no use for it.
fn respond_console(
    request: tiny_http::Request,
    page: people_page::Page,
) -> std::io::Result<(u16, u64)> {
    let bytes = page.html.len() as u64;
    let response = tiny_http::Response::from_string(page.html)
        .with_status_code(page.status)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"private, no-store"[..])
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Security-Policy"[..], BROWSER_CSP)
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Referrer-Policy"[..], &b"no-referrer"[..])
                .expect("static header"),
        );
    served(request, response, page.status, bytes)
}

/// The operator's contact, for the one page a stranger can reach.
///
/// One line of `<root>/.choir/contact`, absent by default, and
/// deliberately not a compiled-in constant: a personal identifier baked
/// into a published binary cannot be taken back out of the copies of it,
/// which is the same reason host addresses and owner names are
/// placeholders in every tracked file here.
///
/// Read per request rather than once at startup. That was the other way
/// round until the console could edit it (D72), and a value an operator
/// can change from a page but only see take effect after a restart is a
/// control that appears not to work. The read is one small file on a
/// route that is already rendering several kilobytes, behind the same
/// public limiter as the rest of the pre-auth surface.
fn operator_contact(root: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(root.join(".choir/contact"))
        .ok()
        .and_then(|text| text.lines().next().map(str::trim).map(str::to_string))
        .filter(|line| !line.is_empty())
}

/// Where [`operator_contact`] reads from.
fn contact_path(root: &std::path::Path) -> std::path::PathBuf {
    root.join(".choir/contact")
}

/// Sets or clears the operator's contact from the console (D72).
///
/// Written with the private-file primitive the rest of this node's own
/// state uses, and validated first: the string lands on the page that
/// answers anybody, so a control character in it would be a header or a
/// second line in a file whose grammar is one line.
///
/// An empty submission removes the file rather than writing an empty
/// one, because "no contact" and "a contact that is nothing" render the
/// same and only one of them is a state somebody meant.
fn write_operator_contact(root: &std::path::Path, value: &str) -> Result<(), String> {
    let value = value.trim();
    let path = contact_path(root);
    if value.is_empty() {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        };
    }
    if value.chars().count() > 200 {
        return Err("a contact must be at most 200 characters".to_string());
    }
    if value.chars().any(char::is_control) {
        return Err(
            "a contact must be one line and must not contain control characters".to_string(),
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    choir_fs::write_atomic_private(&path, format!("{value}\n").as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Mints the token an account uses for git and the CLI (D75).
///
/// `POST` only, same-origin only, and rendered directly rather than
/// redirected to: the node keeps only a hash, so a redirect would drop
/// the one copy of the secret that exists.
fn respond_account_token(
    request: tiny_http::Request,
    store: Option<&accounts::Accounts>,
    user: &str,
    acl: Option<&acl::Effective>,
    scheme: &'static str,
) -> std::io::Result<(u16, u64)> {
    let chrome = browse::Chrome {
        site: None,
        theme: chosen_theme(&request),
        here: "/account",
    };
    let refuse = |request, title: &str, status: u16, reason: &'static str, next: &'static str| {
        let html = ui::refusal(
            title,
            status,
            &ui::Refusal {
                code: "token",
                error: reason,
                expected: None,
                actual: None,
                next,
            },
            &[],
            reader_chrome(&request),
        );
        respond_page(request, status, html, None)
    };
    if request.method().as_str() != "POST" {
        return refuse(
            request,
            "Not a page",
            405,
            "A token is made by pressing the button on your account page.",
            "Open /account and use the form there.",
        );
    }
    if !same_origin(&request, scheme) {
        return refuse(
            request,
            "Cross-origin write refused",
            403,
            "That form was submitted from another site.",
            "Open this node's own account page and try again.",
        );
    }
    let Some(store) = store else {
        return refuse(
            request,
            "Account self-service is off",
            503,
            "This node was started without an accounts file.",
            "Ask the operator to start the daemon with --accounts-file.",
        );
    };
    let (status, answer) = store.mint_token(user);
    let parsed: serde_json::Value = serde_json::from_str(&answer).unwrap_or_default();
    let Some(token) = parsed["token"].as_str() else {
        return refuse(
            request,
            "No token for this credential",
            status,
            "Only an issued account can hold a token, and yours is not one.",
            "An operator credential from the auth file already is a password; use it.",
        );
    };
    let _ = acl;
    let node = header(&request, "host").map(|host| format!("{scheme}://{host}"));
    let page = account_page::minted(
        token,
        user,
        node.as_deref(),
        parsed["replaced"].as_bool().unwrap_or(false),
        chrome,
    );
    let bytes = page.html.len() as u64;
    let response = tiny_http::Response::from_string(page.html)
        .with_status_code(page.status)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"private, no-store"[..])
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Security-Policy"[..], BROWSER_CSP)
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Referrer-Policy"[..], &b"no-referrer"[..])
                .expect("static header"),
        );
    served(request, response, page.status, bytes)
}

/// Whether a state-changing request came from this node's own pages.
///
/// **Absent means yes.** A browser sends `Origin` on every `POST`; a
/// program does not, and `curl`, `choirctl` and every test in this
/// workspace are programs. So a missing header is a non-browser client
/// and passes, while a header naming somewhere else is a page on another
/// site steering a browser that still holds a credential for this one.
///
/// It matters because of what a browser sends unasked. The session
/// cookie is `SameSite=Lax` and so never rides a cross-site `POST`, but
/// cached Basic credentials do -- an operator who authenticated with the
/// auth file once has a browser that will re-present it to any page that
/// asks. Without this check, a link could mint an invite in their name.
fn same_origin(request: &tiny_http::Request, scheme: &'static str) -> bool {
    let Some(origin) = header(request, "origin") else {
        return true;
    };
    match header(request, "host") {
        Some(host) => origin == format!("{scheme}://{host}"),
        // No `Host` and an `Origin` that claims one: nothing to compare
        // against, so refuse rather than guess.
        None => false,
    }
}

/// Serves one `/api/accounts...` request (D36).
///
/// The ACL decides who may issue, revoke and read the roster, through the
/// same [`acl::api_denial`] table every other endpoint goes through. Two
/// things are checked here instead, because neither is a grant: that the
/// node has a store at all, and that redemption is being performed by the
/// invite it names rather than by somebody who merely holds a credential.
/// Adds the one-click join link to a freshly minted invite (D57).
///
/// The operator's own reason for this: the store answers with an
/// `id:secret` pair shaped for `curl -u`, which is right for a script and
/// is not something anybody pastes into a chat window. The link is the
/// artefact that actually gets sent to a person, so the endpoint that
/// mints the invite is the place to build it.
///
/// A node that was reached without a `Host` header gets no link rather
/// than a guessed one, on the same reasoning as `browse::node_url`: an
/// operator who pastes a wrong address has a mystery, and one who finds
/// no link goes and looks.
///
/// Anything that is not a successful mint passes straight through: an
/// error body has no invite in it to link to.
///
/// `field` names the member holding the `id:secret` pair, because D72's
/// access request answers with the same two halves under a different name
/// and lands on the same page. One builder rather than two: the link
/// grammar is `/join?i=&k=` in exactly one place, and a second copy is
/// how the two would come to disagree.
fn with_join_url(answer: (u16, String), origin: Option<&str>, field: &str) -> (u16, String) {
    let (status, body) = answer;
    if status != 200 {
        return (status, body);
    }
    let (Some(origin), Ok(mut parsed)) = (origin, serde_json::from_str::<serde_json::Value>(&body))
    else {
        return (status, body);
    };
    let Some((id, secret)) = parsed[field].as_str().and_then(|p| p.split_once(':')) else {
        return (status, body);
    };
    let url = format!("{origin}/join?i={id}&k={secret}");
    parsed["join_url"] = serde_json::Value::String(url);
    (status, parsed.to_string())
}

/// The self-service surface a request is answered against: the store, and
/// whether passkeys are switched on (D71).
///
/// One argument rather than two because they are one decision. Passing
/// them separately is what let the store's presence stand in for the
/// passkey answer in the first place.
struct SelfService<'a> {
    store: Option<&'a accounts::Accounts>,
    passkeys: bool,
}

fn handle_accounts(
    self_service: SelfService<'_>,
    user: &str,
    invite: Option<&str>,
    acl: Option<&acl::Effective>,
    body_limit: std::num::NonZeroU64,
    scheme: &'static str,
    mut request: tiny_http::Request,
) -> std::io::Result<(u16, u64)> {
    let req_body = match quota::read_bounded(request.as_reader(), Some(body_limit))? {
        quota::Body::Complete(body) => body,
        quota::Body::OverLimit { limit, size } => {
            return respond_api_too_large(request, limit, size)
        }
    };
    let method = request.method().as_str().to_string();
    let path = request.url().split('?').next().unwrap_or("").to_string();
    // Issuing and revoking are the two calls on this node a hostile page
    // would most like to make with somebody else's cached credential.
    if method == "POST" && !same_origin(&request, scheme) {
        return respond_json(request, 403, r#"{"error":"cross-origin write refused"}"#);
    }
    // The address this operator actually reached the node on. The store
    // cannot know it — it has never seen a request — so the link is
    // assembled here, where the `Host` header is.
    let origin = header(&request, "host").map(|host| format!("{scheme}://{host}"));
    let SelfService { store, passkeys } = self_service;
    let (status, body) = match (store, acl) {
        (None, _) => (
            503,
            r#"{"error":"account self-service is not enabled on this node"}"#.to_string(),
        ),
        // Unreachable through the daemon, which refuses `--accounts-file`
        // without `--acl-file`. Fail closed anyway rather than assume the
        // only caller stays the only caller.
        (_, None) => (
            403,
            r#"{"error":"account self-service needs an ACL"}"#.to_string(),
        ),
        (Some(store), Some(table)) => {
            let denial = acl::api_denial(table, user, &method, &path, &req_body, |_| None);
            if let Some(denial) = denial {
                (
                    denial.status,
                    serde_json::json!({ "error": denial.reason }).to_string(),
                )
            } else {
                let json = if req_body.iter().all(u8::is_ascii_whitespace) {
                    Some(serde_json::Value::Null)
                } else {
                    serde_json::from_slice::<serde_json::Value>(&req_body).ok()
                };
                match (json, method.as_str(), path.as_str()) {
                    (None, _, _) => (400, r#"{"error":"body must be JSON"}"#.to_string()),
                    (Some(json), "POST", "/api/accounts/invite") => {
                        with_join_url(store.invite(user, &json), origin.as_deref(), "invite")
                    }
                    (Some(json), "POST", "/api/accounts/redeem") => match invite {
                        Some(id) => store.redeem(id, &json),
                        None => (
                            403,
                            r#"{"error":"redeem an invite by presenting it as the credential"}"#
                                .to_string(),
                        ),
                    },
                    (Some(json), "POST", "/api/accounts/revoke") => store.revoke(&json),
                    (Some(json), "POST", "/api/accounts/request/grant") => {
                        store.grant_request(user, &json)
                    }
                    (Some(json), "POST", "/api/accounts/request/decline") => {
                        store.decline_request(&json)
                    }
                    (Some(_), "POST", "/api/accounts/passkey" | "/api/accounts/passkey/remove")
                        if !passkeys =>
                    {
                        (
                            503,
                            r#"{"error":"passkeys are not enabled on this node"}"#.to_string(),
                        )
                    }
                    (Some(json), "POST", "/api/accounts/passkey") => {
                        store.enroll_passkey(user, &json)
                    }
                    (Some(json), "POST", "/api/accounts/passkey/remove") => {
                        store.remove_passkey(user, &json)
                    }
                    (Some(_), "GET", "/api/accounts") => (200, store.list_json().to_string()),
                    _ => (404, r#"{"error":"no such endpoint"}"#.to_string()),
                }
            }
        }
    };
    let bytes = body.len() as u64;
    let response = tiny_http::Response::from_string(body)
        .with_status_code(status)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                .expect("static header"),
        );
    served(request, response, status, bytes)
}

/// Encodes bytes as standard base64 with `=` padding.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Renders a raw ed25519 public key in OpenSSH `ssh-ed25519 <b64>` form
/// (the wire blob is two length-prefixed strings: key type, key bytes).
pub fn ssh_ed25519_pubkey(raw: &[u8; 32]) -> String {
    let mut blob = Vec::new();
    for part in [b"ssh-ed25519".as_slice(), raw.as_slice()] {
        blob.extend_from_slice(&(part.len() as u32).to_be_bytes());
        blob.extend_from_slice(part);
    }
    format!("ssh-ed25519 {}", base64_encode(&blob))
}

/// One line of the trusted-keys file: a public key the node accepts, and
/// optionally the channel name its holder is allowed to speak as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedKey {
    /// Operator-assigned channel name this key may act as, when the line
    /// carries one. `None` = key trusted, name unconstrained (the
    /// pre-existing behaviour, and what a bare hex line still means).
    pub name: Option<String>,
    /// Actor id (hex): the git push principal, and the `key_id` a
    /// signature carries. **Always derived from the key**, never from the
    /// name — so adding a name column cannot shift push attribution.
    pub actor_id: String,
    /// The raw ed25519 public key.
    pub key: [u8; 32],
}

/// Parses a trusted-keys file. One key per line, `#` comments and blank
/// lines skipped, in either form:
///
/// ```text
/// <64-char hex>            # trusted, speaks as any channel
/// <name> <64-char hex>     # trusted, bound to that channel name
/// ```
///
/// The name column is additive: files written before it existed parse
/// unchanged, and a key with no name keeps exactly its old permissions.
/// Binding is opt-in per key, so adding one line cannot lock anybody
/// else out.
///
/// # Errors
///
/// Filesystem failures, and [`std::io::ErrorKind::InvalidData`] for a
/// line that is not a valid ed25519 public key, or that binds one name
/// to two keys. An invalid line fails the whole parse: a partial signer
/// list would silently drop the ability to verify somebody's pushes.
pub fn parse_keys_file(path: &Path) -> std::io::Result<Vec<TrustedKey>> {
    let invalid = |msg: String| std::io::Error::new(std::io::ErrorKind::InvalidData, msg);
    let mut registry = choir_identity::Registry::new();
    let mut out: Vec<TrustedKey> = Vec::new();
    for line in std::fs::read_to_string(path)?.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, hex) = match line.rsplit_once(char::is_whitespace) {
            Some((name, hex)) => (Some(name.trim().to_string()), hex),
            None => (None, line),
        };
        let bytes = platform::hex_decode(hex)
            .filter(|b| b.len() == 32)
            .ok_or_else(|| {
                invalid("keys file lines are `<hex>` or `<name> <hex>`, 64 hex chars".to_string())
            })?;
        // One name, one key. Two keys sharing a name would make the
        // binding meaningless in exactly the direction it exists to
        // prevent: either holder could speak as that channel.
        if let Some(name) = &name {
            if out.iter().any(|k| k.name.as_ref() == Some(name)) {
                return Err(invalid(format!(
                    "keys file binds {name:?} to more than one key"
                )));
            }
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        let actor_id = registry
            .register(&key)
            .map_err(|e| invalid(format!("{e:?}")))?;
        out.push(TrustedKey {
            name,
            actor_id: actor_id.to_hex(),
            key,
        });
    }
    Ok(out)
}

/// Writes `<root>/.choir/allowed_signers` — the file git's ssh signature
/// verification checks push certificates against. One line per actor:
/// principal (the actor id) followed by the OpenSSH public key.
///
/// # Errors
///
/// Propagates filesystem failures.
pub fn write_allowed_signers(root: &Path, keys: &[TrustedKey]) -> std::io::Result<PathBuf> {
    let dir = root.join(".choir");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("allowed_signers");
    let mut contents = String::new();
    for k in keys {
        // Principal stays the actor id even when the line carries a name:
        // push attribution (`key/<principal>`) is a property of the key,
        // and rewriting it would silently reattribute pushes.
        contents.push_str(&format!("{} {}\n", k.actor_id, ssh_ed25519_pubkey(&k.key)));
    }
    std::fs::write(&path, contents)?;
    Ok(path)
}

/// Decodes standard base64 (with `=` padding); `None` on any bad input.
pub(crate) fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut rev = [255u8; 256];
    for (i, &c) in ALPHABET.iter().enumerate() {
        rev[c as usize] = i as u8;
    }
    let input = input.trim_end_matches('=');
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for c in input.bytes() {
        let v = rev[c as usize];
        if v == 255 {
            return None;
        }
        buf = (buf << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// The agent-facing surface as plain text, generated from
/// `crates/choir-cli/src/surface.rs` and checked for staleness by
/// `choir-cli/tests/it/surface.rs`. Included rather than depended on: the
/// node has no business linking the CLI, and a generated file with a
/// staleness test is the cheaper coupling.
const LLMS_TXT: &str = include_str!("llms.txt");

/// The machine-readable API description (D17), generated from
/// `choir-cli`'s surface table and checked for staleness by
/// `choir-cli/tests/it/surface.rs`.
///
/// Included rather than depended on, for the reason `llms.txt` already
/// is: the node has no business linking the CLI, and a generated file
/// with a staleness test is the cheaper coupling.
const SCHEMA_JSON: &str = include_str!("schema.json");

/// [`SCHEMA_JSON`] with a `capabilities` object describing what this
/// particular node will accept (D17).
///
/// The row calls for "versioned API + deprecation policy + capability
/// negotiation", and the three parts land in different places on
/// purpose. The version and the deprecations are properties of the API
/// and are generated. The capabilities are properties of *this
/// deployment* — whether it issues accounts, grades requests against an
/// ACL, or runs a sequencer at all — and change with the flags it was
/// started with. A client that reads only the committed file would build
/// against a node that does not exist.
///
/// Deliberately coarse: what a client can *branch on*, not the operator's
/// configuration. Whether review is required or a quota is set changes
/// which requests succeed, not which requests are well-formed, and the
/// node already answers those in its own words with a code and a repair
/// (`ERRORS.md`). Listing them here would invite a client to
/// pre-emptively refuse what the node would have explained.
fn schema_with_capabilities(accounts: bool, acl: bool, platform: bool) -> String {
    let mut doc: serde_json::Value =
        serde_json::from_str(SCHEMA_JSON).expect("the generated schema is JSON");
    doc["capabilities"] = serde_json::json!({
        // Credentials can be issued through the API rather than by hand.
        "accounts": accounts,
        // Requests are graded per repository, so a 404 may mean "not
        // granted" rather than "not here" — the distinction `llms.txt`
        // spells out and a client has to know before it retries.
        "acl": acl,
        // There is a sequencer behind this node, so signed operations
        // are admitted at all. Without one it serves git and nothing
        // else, and every `/api/submit` is refused for a reason no
        // amount of client-side correctness fixes.
        "platform": platform,
    });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&doc).expect("the schema is always serializable")
    )
}

/// The sync contract, served at `/sync.md`. Hand-authored, unlike
/// `llms.txt`, and included from the repository root so the served copy
/// and the committed one cannot disagree.
const SYNC_MD: &str = include_str!("../../../SYNC.md");

/// Routes one `/api/...` request to the platform (503 when disabled).
/// Serves the browser page from the cache, or `304` when the client
/// already holds the current one.
///
/// The conditional check happens before the cache lookup and before
/// any rendering, so a reader polling an idle node costs one integer
/// comparison and an empty response. That is the whole reason the page
/// can be refreshed aggressively without the node noticing.
fn handle_ui(
    platform: Option<&Platform>,
    cache: &ui::UiCache,
    user: &str,
    acl: Option<&acl::Effective>,
    request: tiny_http::Request,
) -> std::io::Result<(u16, u64)> {
    let platform = match platform {
        Some(p) => p,
        None => {
            // A page, not a line of plain text: this is the node's front
            // door, so it is the first thing a person sees, and "there is
            // nothing to show" reads as a broken node rather than as a
            // node deliberately started without a sequencer.
            let html = ui::refusal(
                "This node has no view to show",
                503,
                &ui::Refusal {
                    code: "platform_disabled",
                    error: "This node serves git repositories, but its platform API is \
                            switched off, so there is no op log, no sequencer and no \
                            materialized view behind this page.",
                    expected: Some("a node started with the platform API enabled"),
                    actual: Some("a git-only node"),
                    next: "Browse the repositories instead — the link above works, and \
                           cloning and pushing work exactly as they always did. This page \
                           fills in once the operator restarts the node with the platform \
                           API on.",
                },
                &[("/r/", "repositories")],
                reader_chrome(&request),
            );
            return respond_page(request, 503, html, None);
        }
    };

    // Without an ACL every reader sees one page, so the reader key is
    // empty and both the cache and the `ETag` behave exactly as they did
    // before D29 phase B. With one, the key carries the grants, so an
    // edit to the ACL file invalidates a browser's copy of the page as
    // surely as a new op does.
    let reader = acl.map(|table| table.cache_key(user)).unwrap_or_default();
    let seq = platform.view_seq();
    // Read once and used for both the tag and the render, so the page a
    // reader is handed resolved names against exactly the store state
    // its `ETag` claims (D46).
    let (generation, roster) = platform.roster();
    let chrome = browse::Chrome {
        site: None,
        theme: chosen_theme(&request),
        here: "/status",
    };
    let tag = ui::etag(seq, generation, &reader, chrome.theme);
    if header(&request, "If-None-Match").as_deref() == Some(tag.as_str()) {
        return served(request, not_modified(&tag, BROWSER_CSP), 304, 0);
    }

    let page = cache.page(seq, generation, &reader, &roster, chrome, || {
        let body = platform.handle_api("GET", "/api/view", &[]).1;
        match acl {
            Some(table) => acl::filter_response(table, user, "/api/view", &body),
            None => body,
        }
    });
    let response = tiny_http::Response::from_string(page.as_str())
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"ETag"[..], tag.as_bytes()).expect("etag header"),
        )
        // The page is private per node and changes with every op; a
        // shared cache must never hold it, and a browser must ask.
        .with_header(
            tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"private, no-cache"[..])
                .expect("static header"),
        )
        // Defence in depth behind the escaper: even if a value slipped
        // through unescaped, the page may not run scripts, load
        // anything remote, or be framed by another origin.
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Security-Policy"[..], BROWSER_CSP)
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Referrer-Policy"[..], &b"no-referrer"[..])
                .expect("static header"),
        );
    served(request, response, 200, page.len() as u64)
}

/// Serves one repository-browsing page (D30).
///
/// Authorization is the same grant a clone needs, checked here rather
/// than in the renderer: a reader without `read` must be told the
/// repository does not exist, and a page cannot say that convincingly
/// after it has already started describing one.
struct BrowseContext<'a> {
    /// Where the bare repositories live.
    root: &'a Path,
    /// The authenticated reader, whose grants decide what renders.
    user: &'a str,
    /// The grant table, or `None` on a node that gates nothing.
    acl: Option<&'a acl::Effective>,
    /// The view, for the pages served from it rather than from disk.
    platform: Option<&'a Platform>,
    /// Whether the browser may offer mutation controls.
    browser_writes: bool,
    /// The one repository this node presents, if it presents one.
    site: Option<&'a str>,
    /// `http` or `https`, from how this node was started rather than
    /// from anything the request said: a client must not be able to talk
    /// the node into advertising `https` for a plaintext port.
    scheme: &'static str,
    /// Whether this node runs invite-based credential self-service
    /// (D36). A page that tells a newcomer to redeem an invite on a node
    /// that issues none is sending them to a command that cannot work.
    self_service: bool,
}

/// The `/api/view` body this caller may see.
///
/// One reading, used by `/api/profile` and by the profile page, because
/// the ACL narrowing lives here and a second copy of it is a second
/// thing to keep right -- the first time the two disagreed, one of the
/// two surfaces would be disclosing more than the other.
fn visible_view(platform: &Platform, acl: Option<&acl::Effective>, user: &str) -> String {
    let raw = platform.handle_api("GET", "/api/view", &[]).1;
    match acl {
        Some(table) => acl::filter_response(table, user, "/api/view", &raw),
        None => raw,
    }
}

fn handle_browse(
    context: &BrowseContext,
    page: &browse::Page,
    request: tiny_http::Request,
) -> std::io::Result<(u16, u64)> {
    let &BrowseContext {
        root,
        user,
        acl,
        platform,
        browser_writes,
        site,
        scheme,
        self_service,
    } = context;
    let readable = |repo: &str| match acl {
        Some(table) => table.allows_repo(user, repo, acl::Level::Read),
        None => true,
    };
    if let Some(repo) = page.repo() {
        if !readable(repo) {
            // The body lives in `browse` because a repository that does
            // not exist reaches the same refusal from inside the
            // renderer. Two constructions that agree today are a
            // coincidence with a test on it; one construction is the
            // property.
            let denied = browse::no_such_repository(reader_chrome(&request));
            return respond_page(request, denied.status, denied.html, None);
        }
    }

    // The origin the reader actually reached this node on, so a page
    // that prints a command can print one they can paste. Scheme comes
    // from the connection rather than the request: a client cannot talk
    // this node into advertising `https` for a plaintext port.
    let origin = header(&request, "host").map(|host| format!("{scheme}://{host}"));
    let rendered = browse::render(
        root,
        page,
        &readable,
        platform,
        browse::Viewer {
            user,
            browser_writes,
            site,
            origin: origin.as_deref(),
            theme: chosen_theme(&request),
            here: request.url().split(['?', '#']).next().unwrap_or("/"),
            self_service,
        },
    );
    // Revalidation happens after the ACL check and before the body is
    // written, so a `304` costs the reader nothing and still cannot be
    // obtained for a repository they may not read.
    if let Some(tag) = rendered.etag.as_deref() {
        if header(&request, "If-None-Match").as_deref() == Some(tag) {
            // The same policy the `200` would have carried: a `304`
            // that named a weaker one would leave the client on it.
            let csp = match page {
                browse::Page::Review { .. } if browser_writes => SCRIPTED_PAGE_CSP,
                _ => BROWSER_CSP,
            };
            return served(request, not_modified(tag, csp), 304, 0);
        }
    }

    let (status, bytes) = (rendered.status, rendered.html.len() as u64);
    let mut response = tiny_http::Response::from_string(rendered.html)
        .with_status_code(rendered.status)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"private, no-cache"[..])
                .expect("static header"),
        )
        // The same defence in depth the D28 page carries: file contents
        // are attacker-supplied by definition here, so even an escaping
        // miss must not be able to run or fetch anything.
        //
        // The review page is the one exception and it is narrow by
        // construction: the strict header plus one same-origin source,
        // so every other page under `/r/` still runs nothing at all
        // (D39).
        .with_header(
            tiny_http::Header::from_bytes(
                &b"Content-Security-Policy"[..],
                match page {
                    browse::Page::Review { .. } if browser_writes => SCRIPTED_PAGE_CSP,
                    _ => BROWSER_CSP,
                },
            )
            .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Referrer-Policy"[..], &b"no-referrer"[..])
                .expect("static header"),
        );
    if let Some(tag) = rendered.etag {
        response.add_header(
            tiny_http::Header::from_bytes(&b"ETag"[..], tag.as_bytes()).expect("etag header"),
        );
    }
    served(request, response, status, bytes)
}

#[derive(Debug)]
struct Readiness {
    log_verified: bool,
    sequencer_live: bool,
    storage_writable: bool,
    free_disk_bytes: Option<u64>,
    disk_space_ok: bool,
    ref_disagreements: usize,
}

impl Readiness {
    fn ready(&self) -> bool {
        self.log_verified
            && self.sequencer_live
            && self.storage_writable
            && self.disk_space_ok
            && self.ref_disagreements == 0
    }
}

fn storage_probe(root: &Path) -> bool {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = root.join(format!(
        ".choir-ready-probe-{}-{sequence}",
        std::process::id()
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        std::io::Write::write_all(&mut file, b"ready\n")?;
        file.sync_all()
    })();
    std::fs::remove_file(path).ok();
    result.is_ok()
}

fn free_disk_bytes(root: &Path) -> Option<u64> {
    let output = std::process::Command::new("df")
        .args(["-Pk"])
        .arg(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let available_kib = text
        .lines()
        .nth(1)?
        .split_whitespace()
        .nth(3)?
        .parse::<u64>()
        .ok()?;
    available_kib.checked_mul(1024)
}

fn readiness(root: &Path, platform: Option<&Platform>, min_free_bytes: u64) -> Readiness {
    let log_verified = choir_oplog::repair::verify(&root.join(".choir/ops.jsonl"))
        .is_ok_and(|report| report.fault.is_none());
    let sequencer_live = platform.is_some_and(|p| !p.durability_failed());
    let free_disk_bytes = free_disk_bytes(root);
    let ref_disagreements = platform.map_or(usize::MAX, |p| p.survey_git_refs(root).len());
    Readiness {
        log_verified,
        sequencer_live,
        storage_writable: storage_probe(root),
        free_disk_bytes,
        disk_space_ok: free_disk_bytes.is_some_and(|bytes| bytes >= min_free_bytes),
        ref_disagreements,
    }
}

fn handle_observability(
    root: &Path,
    platform: Option<&Platform>,
    min_free_bytes: u64,
    counters: &limits::Counters,
    started_unix: u64,
    request: tiny_http::Request,
) -> std::io::Result<(u16, u64)> {
    if request.url() == "/healthz" {
        let healthy = platform.is_none_or(|p| !p.durability_failed());
        let body = serde_json::json!({
            "format_version": 1,
            "healthy": healthy,
        })
        .to_string();
        let status = if healthy { 200 } else { 503 };
        let bytes = body.len() as u64;
        let response = tiny_http::Response::from_string(body)
            .with_status_code(status)
            .with_header(
                tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                    .expect("static header"),
            );
        return served(request, response, status, bytes);
    }

    let state = readiness(root, platform, min_free_bytes);
    if request.url() == "/metrics" {
        // This scrape is itself a request, and it has not finished yet,
        // so it is not in these numbers. That is the ordinary shape and
        // not a defect: every scrape is one request behind, uniformly.
        let traffic = counters.snapshot();
        let body = format!(
            "# TYPE choir_ready gauge\nchoir_ready {}\n\
             # TYPE choir_log_verified gauge\nchoir_log_verified {}\n\
             # TYPE choir_sequencer_live gauge\nchoir_sequencer_live {}\n\
             # TYPE choir_storage_writable gauge\nchoir_storage_writable {}\n\
             # TYPE choir_free_disk_bytes gauge\nchoir_free_disk_bytes {}\n\
             # TYPE choir_ref_disagreements gauge\nchoir_ref_disagreements {}\n\
             # TYPE choir_process_start_time_seconds gauge\n\
             choir_process_start_time_seconds {}\n\
             # TYPE choir_requests_total counter\nchoir_requests_total {}\n\
             # TYPE choir_requests_unauthorized_total counter\n\
             choir_requests_unauthorized_total {}\n\
             # TYPE choir_requests_throttled_total counter\n\
             choir_requests_throttled_total {}\n\
             # TYPE choir_requests_failed_total counter\n\
             choir_requests_failed_total {}\n\
             # TYPE choir_request_duration_microseconds_total counter\n\
             choir_request_duration_microseconds_total {}\n",
            u8::from(state.ready()),
            u8::from(state.log_verified),
            u8::from(state.sequencer_live),
            u8::from(state.storage_writable),
            state.free_disk_bytes.unwrap_or(0),
            state.ref_disagreements,
            started_unix,
            traffic.requests,
            traffic.unauthorized,
            traffic.throttled,
            traffic.failed,
            traffic.duration_us,
        );
        let bytes = body.len() as u64;
        let response = tiny_http::Response::from_string(body).with_header(
            tiny_http::Header::from_bytes(
                &b"Content-Type"[..],
                &b"text/plain; version=0.0.4; charset=utf-8"[..],
            )
            .expect("static header"),
        );
        return served(request, response, 200, bytes);
    }

    let status = if state.ready() { 200 } else { 503 };
    let body = serde_json::json!({
        "format_version": 1,
        "ready": state.ready(),
        "checks": {
            "log_verified": state.log_verified,
            "sequencer_live": state.sequencer_live,
            "storage_writable": state.storage_writable,
            "free_disk_bytes": state.free_disk_bytes,
            "minimum_free_disk_bytes": min_free_bytes,
            "disk_space_ok": state.disk_space_ok,
            "ref_agreement": state.ref_disagreements == 0,
            "ref_disagreements": state.ref_disagreements,
        }
    })
    .to_string();
    let bytes = body.len() as u64;
    let response = tiny_http::Response::from_string(body)
        .with_status_code(status)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                .expect("static header"),
        );
    served(request, response, status, bytes)
}

/// Whether this workspace request is over the caller's D37 ceiling, and
/// the refusal to send if it is.
///
/// A request naming a workspace that already exists cannot raise anyone's
/// count, so it is not checked: an idempotent retry (D35 binds one to a
/// change id and an idempotency key precisely so it can be retried) must
/// not be refused for creating nothing. That is also why this reads the
/// body — it is the only place the workspace being asked for is named —
/// and why it reads nothing else out of it, leaving every other judgement
/// about the request to `provision`.
fn workspace_quota_refusal(
    platform: &Platform,
    user: &str,
    ceiling: Option<std::num::NonZeroU32>,
    body: &[u8],
) -> Option<(u16, String)> {
    let ceiling = ceiling?.get() as usize;
    let request: serde_json::Value = serde_json::from_slice(body).ok()?;
    let field = |key: &str| {
        request
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
    };
    let (repo, name) = (field("repo"), field("name"));
    if repo.is_empty() || name.is_empty() {
        // Malformed: let `provision` say so, in its own words.
        return None;
    }
    if platform.workspace_head(&format!("{repo}/{name}")).is_some() {
        return None;
    }
    let channel = quota::channel_for(user);
    let held = platform.workspaces_held_by(&channel);
    if held < ceiling {
        return None;
    }
    let rejection = reject::Rejection::new(
        reject::Code::QuotaExceeded,
        format!("you already hold {held} workspaces, which is this node's per-user limit"),
        "archive a workspace you are finished with (POST /api/workspace/archive) to free the \
         allowance, or ask the operator to raise the limit",
    )
    .with_states(
        Some(format!("at most {ceiling} workspaces")),
        Some(format!("{held} workspaces")),
    );
    Some((403, rejection.body()))
}

/// Serves one platform-API request.
///
/// `workspaces` is the caller's per-user workspace ceiling (D37),
/// already `None` for anyone the metering exempts. It is checked here
/// rather than inside [`provision::create_workspace`] so the quota
/// observes the creation path instead of editing it.
struct ApiRequestContext<'a> {
    platform: Option<&'a Platform>,
    root: &'a Path,
    base_url: &'a str,
    user: &'a str,
    acl: Option<&'a acl::Effective>,
    /// The same table as `acl`, but supplied for the hook callbacks too,
    /// which deliberately receive `acl: None` (D60).
    ///
    /// Those callbacks are privileged: they spend authorization the git
    /// route already checked, so running the ordinary API denials over
    /// them would re-ask a question that has been answered. One question
    /// has *not* been answered there, because it could not be: a
    /// `propose` grant is admitted at the smart-HTTP boundary before any
    /// refname exists. This field carries the table for that one check
    /// and nothing else.
    push_acl: Option<&'a acl::Effective>,
    workspaces: Option<std::num::NonZeroU32>,
    body_limit: std::num::NonZeroU64,
    queue: Option<&'a queue_api::QueueConfig>,
    queue_in_flight: &'a queue_api::InFlight,
}

fn handle_api(
    context: ApiRequestContext<'_>,
    mut request: tiny_http::Request,
) -> std::io::Result<(u16, u64)> {
    let ApiRequestContext {
        platform,
        root,
        base_url,
        user,
        acl,
        push_acl,
        workspaces,
        body_limit,
        queue,
        queue_in_flight,
    } = context;
    let (status, body) = match platform {
        Some(p) => {
            let req_body = match quota::read_bounded(request.as_reader(), Some(body_limit))? {
                quota::Body::Complete(body) => body,
                quota::Body::OverLimit { limit, size } => {
                    return respond_api_too_large(request, limit, size)
                }
            };
            let method = request.method().as_str().to_string();
            let path = request.url().to_string();
            // The body is already in hand, which is the only place the
            // repository a submission touches can be recovered from.
            let denial = acl
                .and_then(|table| {
                    acl::api_denial(table, user, &method, &path, &req_body, |id| {
                        p.review_repo(id)
                    })
                })
                .or_else(|| {
                    // D60. Deliberately not inside `api_denial`: that one
                    // authorizes the caller of this endpoint, and the
                    // caller here is the hook. This authorizes the person
                    // whose push triggered it, named in the body.
                    ((method.as_str(), path.as_str()) == ("POST", "/api/git-update"))
                        .then_some(push_acl)
                        .flatten()
                        .and_then(|table| platform::proposal_denial(table, &req_body))
                });
            if let Some(denial) = denial {
                (
                    denial.status,
                    serde_json::json!({ "error": denial.reason }).to_string(),
                )
            } else if (method.as_str(), path.as_str()) == ("POST", "/api/queue/run") {
                // Routed here rather than inside the platform for the
                // same reason as `/api/workspace`: it needs the repo
                // root, which the platform does not hold.
                match queue {
                    Some(config) => {
                        queue_api::run(root, p, config, queue_in_flight, &req_body)
                    }
                    None => (
                        501,
                        serde_json::json!({
                            "error": "this node was started without --ci-command, so it runs no queue"
                        })
                        .to_string(),
                    ),
                }
            } else if (method.as_str(), path.as_str()) == ("POST", "/api/workspace") {
                match workspace_quota_refusal(p, user, workspaces, &req_body) {
                    Some(refusal) => refusal,
                    None => provision::create_workspace(root, p, base_url, user, &req_body),
                }
            } else if (method.as_str(), path.as_str()) == ("POST", "/api/workspace/archive") {
                provision::archive_workspace(root, p, user, &req_body)
            } else if (method.as_str(), path.as_str()) == ("POST", "/api/repo") {
                // Routed here rather than inside the platform for the
                // same reason as `/api/workspace`: it needs the repo
                // root, which the platform does not hold.
                //
                // Creating a repository is a write against the node
                // itself rather than against any repository — there is
                // no repository yet to be scoped to — so it asks for
                // `@node` write, the same authority that governs the
                // log and the attestation (D29).
                let denial =
                    acl.and_then(|table| table.check(user, &acl::Scope::Node, acl::Level::Write));
                if let Some(denial) = denial {
                    (
                        denial.status,
                        serde_json::json!({ "error": denial.reason }).to_string(),
                    )
                } else {
                    create_repo_request(root, &req_body)
                }
            } else if (method.as_str(), path.as_str()) == ("GET", "/api/ref-agreement") {
                // Routed here rather than inside the platform for the
                // same reason as `/api/workspace`: it needs the repo
                // root, which the platform does not hold.
                let findings = p.survey_git_refs(root);
                let body = serde_json::json!({
                    "format_version": 1,
                    "agree": findings.is_empty(),
                    "findings": findings.iter().map(platform::RefFinding::to_json)
                        .collect::<Vec<_>>(),
                });
                (200, body.to_string())
            } else {
                let (status, body) = p.handle_api(&method, &path, &req_body);
                // Phase B: the aggregate reads are narrowed rather than
                // refused, because a reader granted one repository still
                // has a legitimate view — of that repository. Applied to
                // the response rather than inside the handler so the
                // platform keeps answering one question, and the ACL
                // stays the only thing that knows about grants.
                let body = match acl {
                    Some(table) if status == 200 => acl::filter_response(table, user, &path, &body),
                    _ => body,
                };
                // Bounded last, after any narrowing, because
                // `<section>_omitted` is a row count and a count of rows
                // this caller may not read discloses that they exist.
                // See `bound`'s module docs; the ordering is the design.
                match status {
                    200 => (status, bound::apply(&path, &body)),
                    _ => (status, body),
                }
            }
        }
        None => (503, r#"{"error":"platform API not enabled"}"#.to_string()),
    };
    let bytes = body.len() as u64;
    let response = tiny_http::Response::from_string(body)
        .with_status_code(status)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                .expect("static header"),
        );
    served(request, response, status, bytes)
}

/// Bridges one HTTP request to `git http-backend` CGI. `extra_env` is
/// added to the CGI child (and thus inherited by git hooks).
///
/// `push_bytes` is the caller's per-user ceiling on the request body
/// (D37). It is checked here, before the CGI child is spawned, which is
/// the point of the whole design: `git http-backend` is what runs the
/// `pre-receive` hook, and the hook is what submits ops. A body refused
/// on this side of the spawn means no hook ran, so no op was submitted
/// and the hook's retraction path — the one that has to undo the refs an
/// aborted push already got into the durable log — is never entered.
fn handle(
    root: PathBuf,
    mut request: tiny_http::Request,
    extra_env: &[(String, String)],
    push_bytes: Option<std::num::NonZeroU64>,
) -> std::io::Result<(u16, u64)> {
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url.clone(), String::new()),
    };

    let body = match quota::read_bounded(request.as_reader(), push_bytes)? {
        quota::Body::Complete(body) => body,
        quota::Body::OverLimit { limit, size } => {
            return respond_push_too_large(request, limit, size)
        }
    };

    let method = request.method().as_str().to_string();
    let content_type = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Content-Type"))
        .map(|h| h.value.as_str().to_string())
        .unwrap_or_default();

    let mut child = std::process::Command::new("git")
        .arg("http-backend")
        .envs(extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .env("GIT_PROJECT_ROOT", &root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", &path)
        .env("QUERY_STRING", &query)
        .env("REQUEST_METHOD", &method)
        .env("CONTENT_TYPE", &content_type)
        .env("CONTENT_LENGTH", body.len().to_string())
        .env("REMOTE_ADDR", "127.0.0.1")
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    use std::io::Write;
    child.stdin.take().expect("stdin piped").write_all(&body)?;
    let mut out = Vec::new();
    child
        .stdout
        .take()
        .expect("stdout piped")
        .read_to_end(&mut out)?;
    child.wait()?;

    // Split the CGI response into headers and body.
    let split = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, i + 4))
        .or_else(|| {
            out.windows(2)
                .position(|w| w == b"\n\n")
                .map(|i| (i, i + 2))
        });
    let (head, rest) = match split {
        Some((h, b)) => (&out[..h], &out[b..]),
        None => (&[][..], &out[..]),
    };

    let mut status = 200;
    let mut headers = Vec::new();
    for line in String::from_utf8_lossy(head).lines() {
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim(), v.trim());
            if k.eq_ignore_ascii_case("Status") {
                status = v
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(200);
            } else if let Ok(h) = tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()) {
                headers.push(h);
            }
        }
    }

    let bytes = rest.len() as u64;
    let mut response = tiny_http::Response::from_data(rest.to_vec()).with_status_code(status);
    for h in headers {
        response.add_header(h);
    }
    served(request, response, status, bytes)
}
