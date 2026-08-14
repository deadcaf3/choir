//! L3 node daemon: a minimal git smart-HTTP server (plan.md L3, D12).
//!
//! v1 wraps `git http-backend` (git's own CGI) over bare repositories, the
//! same shape as Tangled's knot: the daemon is a thin, self-hostable shell
//! over git plumbing, and platform behavior (sequencer, queue, identity)
//! layers on top. ForgeMark benchmarks this surface directly.
//!
//! Authentication is per-actor basic auth ([`AuthTable`], `--auth-file`),
//! plus the credentials self-service has issued ([`accounts`],
//! `--accounts-file`); the platform API ([`platform`]) additionally
//! verifies ed25519 op signatures. The bind stays loopback-only: beyond
//! localhost you still need TLS or an SSH tunnel so tokens aren't sent in
//! the clear.

use std::io::Read;
use std::path::{Path, PathBuf};

mod account_page;
pub mod accounts;
pub mod acl;
pub mod hooks;
pub mod limits;
pub mod platform;
mod prepare;
pub mod provision;
pub mod quota;
pub mod reject;
mod browse;
pub mod ssh;
mod ui;

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

/// Per-actor credentials: username → token, checked as HTTP basic auth
/// (the standard git-over-HTTP shape; every forge client speaks it).
///
/// L8 note: usernames are actor ids and tokens are per-actor secrets
/// minted by the operator; key-signature-based challenge auth can
/// replace the token *check* later without changing the wire shape.
pub type AuthTable = std::collections::HashMap<String, String>;

/// A running node daemon serving repos under a root directory.
pub struct Node {
    root: PathBuf,
    server: std::sync::Arc<tiny_http::Server>,
    port: u16,
    auth: std::sync::Arc<Option<AuthTable>>,
    platform: Option<std::sync::Arc<Platform>>,
    scheme: &'static str,
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
    /// The file table and the store's grants, merged, cached against the
    /// generations of both. Rebuilt when either moves, so authorization
    /// does not rebuild a table per request.
    acl_merged: std::sync::RwLock<Option<(u64, u64, std::sync::Arc<acl::Acl>)>>,
    /// Per-user ceilings on push size and workspace count (D37). Both
    /// unset = nothing is ever refused for quota, which is the pre-D37
    /// behaviour.
    quotas: quota::Quotas,
}

/// A watched trusted-keys file and the mtime last folded into
/// `allowed_signers`.
struct KeysWatch {
    path: PathBuf,
    mtime: std::sync::Mutex<Option<std::time::SystemTime>>,
}

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
            scheme,
            internal_token: choir_identity::ActorKey::generate().actor_id().to_hex(),
            keys_watch: None,
            acl_watch: None,
            ui_cache: std::sync::Arc::new(ui::UiCache::new()),
            request_log: None,
            rate: None,
            accounts: None,
            acl_merged: std::sync::RwLock::new(None),
            quotas: quota::Quotas::default(),
        })
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
        // The other half of the join in `enable_accounts`: whichever flag
        // is applied second attaches the store, so passkey verification
        // does not depend on the order the daemon happens to configure in
        // (D39).
        if let Some(store) = self.accounts.as_ref() {
            platform.attach_accounts(store.clone());
        }
        self.platform = Some(std::sync::Arc::new(platform));
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
            mtime: std::sync::Mutex::new(
                std::fs::metadata(&path).and_then(|m| m.modified()).ok(),
            ),
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
        let mtime = std::fs::metadata(&watch.path).and_then(|m| m.modified()).ok();
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
        eprintln!("acl enabled ({} grants)", table.len());
        self.acl_watch = Some(std::sync::Arc::new(AclWatch {
            mtime: std::sync::Mutex::new(
                std::fs::metadata(&path).and_then(|m| m.modified()).ok(),
            ),
            table: std::sync::RwLock::new(std::sync::Arc::new(table)),
            epoch: std::sync::atomic::AtomicU64::new(0),
            path,
        }));
        Ok(())
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
    pub fn enable_accounts(
        &mut self,
        path: PathBuf,
        keys_out: Option<accounts::SshKeysOut>,
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
        let store = accounts::Accounts::open(path, keys_out, reserved)?;
        eprintln!("accounts enabled ({} issued)", store.len());
        let store = std::sync::Arc::new(store);
        // Either order: whichever of the two flags is applied second
        // performs the join, so a passkey submission is verifiable
        // regardless of how the daemon was configured (D39).
        if let Some(platform) = self.platform.as_ref() {
            platform.attach_accounts(store.clone());
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
        let mtime = std::fs::metadata(&watch.path).and_then(|m| m.modified()).ok();
        let mut last = watch.mtime.lock().expect("acl mtime lock");
        if mtime.is_none() || mtime == *last {
            return;
        }
        *last = mtime;
        match acl::Acl::load(&watch.path) {
            Ok(table) => {
                eprintln!("acl: reloaded ({} grants)", table.len());
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
    fn acl_now(&self) -> Option<std::sync::Arc<acl::Acl>> {
        let watch = self.acl_watch.as_ref()?;
        let file = std::sync::Arc::clone(&watch.table.read().expect("acl read lock"));
        let Some(store) = self.accounts.as_ref() else {
            return Some(file);
        };
        let epoch = watch.epoch.load(std::sync::atomic::Ordering::Acquire);
        let generation = store.generation();
        if let Some((cached_epoch, cached_generation, table)) =
            self.acl_merged.read().expect("merged acl read lock").as_ref()
        {
            if *cached_epoch == epoch && *cached_generation == generation {
                return Some(std::sync::Arc::clone(table));
            }
        }
        let merged = std::sync::Arc::new(file.merged(&store.acl()));
        *self.acl_merged.write().expect("merged acl write lock") =
            Some((epoch, generation, std::sync::Arc::clone(&merged)));
        Some(merged)
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
        let path = self.repo_path(name)?;
        if path.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                name.to_string(),
            ));
        }
        let ok = std::process::Command::new("git")
            .args(["init", "--bare", "-q"])
            .arg(&path)
            .status()?
            .success()
            && std::process::Command::new("git")
                .args(["config", "http.receivepack", "true"])
                .current_dir(&path)
                .status()?
                .success()
            // Pin hooks to this repo: a host-global core.hooksPath (set
            // by e.g. husky) would otherwise silently bypass the
            // sequencer hook below.
            && std::process::Command::new("git")
                .args(["config", "core.hooksPath", "hooks"])
                .current_dir(&path)
                .status()?
                .success()
            // Advertise push certificates (`git push --signed`) and
            // verify their ssh signatures against the allowed-signers
            // file (per-actor keys, L8).
            && std::process::Command::new("git")
                .args([
                    "config",
                    "receive.certNonceSeed",
                    &choir_identity::ActorKey::generate().actor_id().to_hex(),
                ])
                .current_dir(&path)
                .status()?
                .success()
            && std::process::Command::new("git")
                .args(["config", "gpg.format", "ssh"])
                .current_dir(&path)
                .status()?
                .success()
            && std::process::Command::new("git")
                .args(["config", "gpg.ssh.allowedSignersFile"])
                .arg(
                    self.root
                        .canonicalize()
                        .unwrap_or_else(|_| self.root.clone())
                        .join(".choir")
                        .join("allowed_signers"),
                )
                .current_dir(&path)
                .status()?
                .success();
        if !ok {
            return Err(std::io::Error::other("git init/config failed"));
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
                "post() {\n",
                "  payload=$(printf '{\"repo\":\"%s\",\"refname\":\"%s\",\"old\":\"%s\",\"new\":\"%s\",\"user\":\"%s\",\"cert_status\":\"%s\",\"signer\":\"%s\"}' \\\n",
                "    \"$CHOIR_REPO\" \"$3\" \"$1\" \"$2\" \"$CHOIR_USER\" \"$GIT_PUSH_CERT_STATUS\" \"$GIT_PUSH_CERT_SIGNER\")\n",
                "  curl -skf -X POST -H \"X-Choir-Internal: $CHOIR_INTERNAL\" \\\n",
                "    -d \"$payload\" \"$4\" >/dev/null\n",
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
                "    echo \"choir: ref update rejected by sequencer: $ref\" >&2\n",
                "    abort\n",
                "    rm -f \"$done_refs\"\n",
                "    exit 1\n",
                "  fi\n",
                "  printf '%s %s %s\\n' \"$old\" \"$new\" \"$ref\" >> \"$done_refs\"\n",
                "done\n",
                "rm -f \"$done_refs\"\n",
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

    /// Rejects path traversal and normalizes the repo path under root.
    fn repo_path(&self, name: &str) -> std::io::Result<PathBuf> {
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
        Ok(self.root.join(name))
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
        for request in self.server.incoming_requests() {
            // Cheap stat between accepting a request and handling it, so
            // an edited keys file takes effect on *this* request: an
            // appended signing key becomes usable, and a newly bound
            // channel name becomes enforced, with no restart and no wait
            // for some later event.
            self.refresh_allowed_signers();
            // Same reasoning, same cost: an appended grant takes effect
            // on this request rather than on a restart.
            self.refresh_acl();
            // A writer that has failed a durability barrier refuses every
            // submission from then on. Staying up in that state is worse
            // than being down: process supervision only restarts a process
            // that *exits*, so the node would sit there looking healthy
            // to launchd while rejecting everything, and a transient fsync
            // error would become permanent downtime that reads as uptime.
            // Exiting hands it back to supervision, which restarts into
            // the same replay path the D20 flip proved with `kill -9`.
            if self.platform.as_ref().is_some_and(|p| p.durability_failed()) {
                eprintln!(
                    "choir: the op log is no longer durable, so this node has stopped \
                     accepting writes; exiting so supervision restarts it. Check the \
                     filesystem backing the log."
                );
                // EX_TEMPFAIL: the condition may well clear on restart.
                std::process::exit(75);
            }
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
            let ui_cache = std::sync::Arc::clone(&self.ui_cache);
            let acl = self.acl_now();
            let internal_token = self.internal_token.clone();
            let request_log = self.request_log.clone();
            let rate = self.rate.clone();
            let accounts = self.accounts.clone();
            let quotas = self.quotas;
            let authenticated = self.auth.is_some();
            let port = self.port;
            let scheme = self.scheme;
            std::thread::spawn(move || {
                // D33. Started before anything else the thread does, so
                // the recorded duration is the node's whole cost. The
                // query string is dropped here and never carried further.
                let access = limits::Access::start(&request);
                let log = request_log.as_deref();
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
                if let Some(table) = auth.as_ref() {
                    match authenticate(table, accounts.as_deref(), &request) {
                        Some(accounts::Principal::Account(u)) => user = u,
                        Some(accounts::Principal::Invite(id)) => {
                            user.clone_from(&id);
                            invite = Some(id);
                        }
                        None if internal_ok => {}
                        None => {
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
                            let outcome = served(request, response, 401, body.len() as u64);
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
                    let node_wide = acl
                        .as_deref()
                        .is_some_and(|table| table.allows(&user, &acl::Scope::Node, acl::Level::Read));
                    !(internal_ok || !authenticated || node_wide)
                };
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
                        let outcome = respond_rate_limited(request, access.path(), retry_after);
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
                    let outcome = served(request, response, 403, body.len() as u64);
                    access.finish(log, &user, &outcome);
                    return;
                }
                // Build one op's bytes for a browser to sign (D39).
                // Ahead of the platform API for the same reason the
                // account routes are: it needs no sequencer.
                if request.url().split('?').next().unwrap_or("") == "/api/prepare" {
                    let outcome = handle_prepare(&user, acl.as_deref(), request);
                    access.finish(log, &user, &outcome);
                    return;
                }
                // The page a person enrols a passkey on (D39). Ahead
                // of the API block because it is a page, not an endpoint,
                // and it is gated by nothing but being authenticated: the
                // only account it can ever show is the caller's own.
                if request.url().split('?').next().unwrap_or("") == "/account" {
                    let page = account_page::render(accounts.as_deref(), &user);
                    let bytes = page.html.len() as u64;
                    let response = tiny_http::Response::from_string(page.html)
                        .with_status_code(page.status)
                        .with_header(
                            tiny_http::Header::from_bytes(
                                &b"Content-Type"[..],
                                &b"text/html; charset=utf-8"[..],
                            )
                            .expect("static header"),
                        );
                    let outcome = served(request, response, page.status, bytes);
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
                        accounts.as_deref(),
                        &user,
                        invite.as_deref(),
                        acl.as_deref(),
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
                // The browser surface. Matched exactly so it can never
                // shadow a repository path: git routes are
                // `/owner/repo.git/...`, and `/` is the one URL that
                // cannot name a repository.
                if request.url() == "/" || request.url() == "/index.html" {
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
                // Repository browsing (D30). Ahead of the git branch
                // below, but `browse::route` refuses any path carrying a
                // `.git` segment, so an owner named `r` keeps their
                // clone URL: this cannot shadow a repository, and the
                // check is theirs rather than this router's ordering.
                if let Some(page) = browse::route(request.url()) {
                    let outcome = handle_browse(
                        &root,
                        &page,
                        &user,
                        acl.as_deref(),
                        platform.as_deref(),
                        request,
                    );
                    access.finish(log, &user, &outcome);
                    return;
                }
                if request.url().starts_with("/api/") {
                    let base_url = format!("{scheme}://127.0.0.1:{port}");
                    // A hook callback carries the loopback secret rather
                    // than a user's grants, so it is not an ACL subject.
                    let acl_for_api = if internal_ok { None } else { acl.as_deref() };
                    let outcome = handle_api(
                        platform.as_deref(),
                        &root,
                        &base_url,
                        &user,
                        acl_for_api,
                        metered.then_some(quotas.workspaces).flatten(),
                        request,
                    );
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
                        Some((repo, level)) => {
                            table.check(&user, &acl::Scope::Repo(repo), level)
                        }
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
                            format!("{scheme}://127.0.0.1:{port}/api/git-update"),
                        ));
                        extra_env.push((
                            "CHOIR_ABORT".to_string(),
                            format!("{scheme}://127.0.0.1:{port}/api/git-abort"),
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
fn served<R: std::io::Read>(
    request: tiny_http::Request,
    response: tiny_http::Response<R>,
    status: u16,
    bytes: u64,
) -> std::io::Result<(u16, u64)> {
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
            tiny_http::Header::from_bytes(
                &b"Retry-After"[..],
                retry_after.to_string().as_bytes(),
            )
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
    let end = path.find(".git/").map(|i| i + 4).or_else(|| {
        path.ends_with(".git").then_some(path.len())
    })?;
    Some(path[1..end].to_string())
}

/// Checks a request's basic-auth credentials against the table; returns
/// the authenticated username.
fn authorized(table: &AuthTable, request: &tiny_http::Request) -> Option<String> {
    let (user, token) = basic_auth(request)?;
    // Compare without early exit on length/content so timing doesn't
    // leak how much of the token matched.
    let expected = table.get(&user)?;
    let a = expected.as_bytes();
    let b = token.as_bytes();
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().min(b.len()) {
        diff |= (a[i] ^ b[i]) as usize;
    }
    (diff == 0).then_some(user)
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
    acl: Option<&acl::Acl>,
    mut request: tiny_http::Request,
) -> std::io::Result<(u16, u64)> {
    let mut req_body = Vec::new();
    request.as_reader().read_to_end(&mut req_body)?;
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

/// Serves one `/api/accounts...` request (D36).
///
/// The ACL decides who may issue, revoke and read the roster, through the
/// same [`acl::api_denial`] table every other endpoint goes through. Two
/// things are checked here instead, because neither is a grant: that the
/// node has a store at all, and that redemption is being performed by the
/// invite it names rather than by somebody who merely holds a credential.
fn handle_accounts(
    store: Option<&accounts::Accounts>,
    user: &str,
    invite: Option<&str>,
    acl: Option<&acl::Acl>,
    mut request: tiny_http::Request,
) -> std::io::Result<(u16, u64)> {
    let mut req_body = Vec::new();
    request.as_reader().read_to_end(&mut req_body)?;
    let method = request.method().as_str().to_string();
    let path = request.url().split('?').next().unwrap_or("").to_string();
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
                    (None, _, _) => (
                        400,
                        r#"{"error":"body must be JSON"}"#.to_string(),
                    ),
                    (Some(json), "POST", "/api/accounts/invite") => store.invite(user, &json),
                    (Some(json), "POST", "/api/accounts/redeem") => match invite {
                        Some(id) => store.redeem(id, &json),
                        None => (
                            403,
                            r#"{"error":"redeem an invite by presenting it as the credential"}"#
                                .to_string(),
                        ),
                    },
                    (Some(json), "POST", "/api/accounts/revoke") => store.revoke(&json),
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
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
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
                return Err(invalid(format!("keys file binds {name:?} to more than one key")));
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
    acl: Option<&acl::Acl>,
    request: tiny_http::Request,
) -> std::io::Result<(u16, u64)> {
    let platform = match platform {
        Some(p) => p,
        None => {
            let body = "the platform API is not enabled on this node, so there is nothing to show\n";
            let response = tiny_http::Response::from_string(body).with_status_code(503);
            return served(request, response, 503, body.len() as u64);
        }
    };

    // Without an ACL every reader sees one page, so the reader key is
    // empty and both the cache and the `ETag` behave exactly as they did
    // before D29 phase B. With one, the key carries the grants, so an
    // edit to the ACL file invalidates a browser's copy of the page as
    // surely as a new op does.
    let reader = acl.map(|table| table.cache_key(user)).unwrap_or_default();
    let seq = platform.view_seq();
    let tag = ui::etag(seq, &reader);
    if header(&request, "If-None-Match").as_deref() == Some(tag.as_str()) {
        let response = tiny_http::Response::empty(304).with_header(
            tiny_http::Header::from_bytes(&b"ETag"[..], tag.as_bytes()).expect("etag header"),
        );
        return served(request, response, 304, 0);
    }

    let page = cache.page(seq, &reader, || {
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
            tiny_http::Header::from_bytes(
                &b"Content-Security-Policy"[..],
                &b"default-src 'none'; style-src 'unsafe-inline'; form-action 'none'; frame-ancestors 'none'; base-uri 'none'"[..],
            )
            .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"X-Content-Type-Options"[..], &b"nosniff"[..])
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
fn handle_browse(
    root: &Path,
    page: &browse::Page,
    user: &str,
    acl: Option<&acl::Acl>,
    platform: Option<&Platform>,
    request: tiny_http::Request,
) -> std::io::Result<(u16, u64)> {
    let readable = |repo: &str| match acl {
        Some(table) => table.allows_repo(user, repo, acl::Level::Read),
        None => true,
    };
    if let Some(repo) = page.repo() {
        if !readable(repo) {
            let body = "no such repository\n";
            let response = tiny_http::Response::from_string(body)
                .with_status_code(404)
                .with_header(
                    tiny_http::Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"text/plain; charset=utf-8"[..],
                    )
                    .expect("static header"),
                );
            return served(request, response, 404, body.len() as u64);
        }
    }

    let rendered = browse::render(root, page, &readable, platform, user);
    // Revalidation happens after the ACL check and before the body is
    // written, so a `304` costs the reader nothing and still cannot be
    // obtained for a repository they may not read.
    if let Some(tag) = rendered.etag.as_deref() {
        if header(&request, "If-None-Match").as_deref() == Some(tag) {
            let response = tiny_http::Response::empty(304).with_header(
                tiny_http::Header::from_bytes(&b"ETag"[..], tag.as_bytes()).expect("etag header"),
            );
            return served(request, response, 304, 0);
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
        .with_header(
            tiny_http::Header::from_bytes(
                &b"Content-Security-Policy"[..],
                &b"default-src 'none'; style-src 'unsafe-inline'; form-action 'none'; frame-ancestors 'none'; base-uri 'none'"[..],
            )
            .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"X-Content-Type-Options"[..], &b"nosniff"[..])
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
    let field = |key: &str| request.get(key).and_then(|v| v.as_str()).unwrap_or_default();
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
fn handle_api(
    platform: Option<&Platform>,
    root: &Path,
    base_url: &str,
    user: &str,
    acl: Option<&acl::Acl>,
    workspaces: Option<std::num::NonZeroU32>,
    mut request: tiny_http::Request,
) -> std::io::Result<(u16, u64)> {
    let (status, body) = match platform {
        Some(p) => {
            let mut req_body = Vec::new();
            request.as_reader().read_to_end(&mut req_body)?;
            let method = request.method().as_str().to_string();
            let path = request.url().to_string();
            // The body is already in hand, which is the only place the
            // repository a submission touches can be recovered from.
            let denial = acl.and_then(|table| {
                acl::api_denial(table, user, &method, &path, &req_body, |id| p.review_repo(id))
            });
            if let Some(denial) = denial {
                (denial.status, serde_json::json!({ "error": denial.reason }).to_string())
            } else if (method.as_str(), path.as_str()) == ("POST", "/api/workspace") {
                match workspace_quota_refusal(p, user, workspaces, &req_body) {
                    Some(refusal) => refusal,
                    None => provision::create_workspace(root, p, base_url, user, &req_body),
                }
            } else if (method.as_str(), path.as_str()) == ("POST", "/api/workspace/archive") {
                provision::archive_workspace(root, p, user, &req_body)
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
                match acl {
                    Some(table) if status == 200 => {
                        (status, acl::filter_response(table, user, &path, &body))
                    }
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
    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(&body)?;
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
        .or_else(|| out.windows(2).position(|w| w == b"\n\n").map(|i| (i, i + 2)));
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
