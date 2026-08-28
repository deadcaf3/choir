//! `choir init` — from nothing to a node you can push to.
//!
//! The four things a new instance needs are a repository root, a
//! credential, an actor key the node will trust, and a `.choir/config`
//! so the other commands stop asking which node you mean. Each was
//! already possible by hand — `openssl rand`, `choir key`, a `printf`
//! into a file at the right mode — and every one of them is a step
//! where a first-time reader gets a permission bit wrong and finds out
//! much later.
//!
//! # What it will not do
//!
//! It refuses when anything it would write already exists, and names
//! what. Overwriting an auth file locks you out of your own node with
//! no undo: the token in it is the only copy, the node was started
//! reading it, and nothing anywhere else can reissue it. `--force` is
//! the deliberate reset, and says what it destroyed.
//!
//! # Examples
//!
//! ```
//! use choir_cli::init::Plan;
//!
//! // The default plan is loopback, because a node that binds anything
//! // else refuses to start without TLS (invariant 9) — and a first run
//! // that fails on a certificate teaches nothing about choir.
//! let plan = Plan::new(std::path::Path::new("/tmp/choir-example"), 8417);
//! assert!(plan.node_url().starts_with("http://127.0.0.1:"));
//! ```

use std::path::{Path, PathBuf};

/// Where everything `choir init` writes is going.
///
/// Built before anything is created so the refusal can name every
/// conflict at once. A tool that fails on the first collision, is fixed,
/// and then fails on the second is a tool that gets run four times.
pub struct Plan {
    /// `~/.choir`, or the directory given.
    pub state: PathBuf,
    /// Where bare repositories live; the node's positional root.
    pub repos: PathBuf,
    /// `user:token`, mode 0600.
    pub auth: PathBuf,
    /// This actor's 32 secret bytes, mode 0600.
    pub key: PathBuf,
    /// The public keys the node will accept ops from.
    pub trusted: PathBuf,
    /// `.choir/config` in the working directory, not in `state`: it is
    /// found the way git finds a repository, by walking up from wherever
    /// you are, so a checkout can name a different node than the one
    /// your home directory happens to point at.
    pub config: PathBuf,
    /// The port the node will bind.
    pub port: u16,
}

impl Plan {
    /// The default layout under `state`.
    #[must_use]
    pub fn new(state: &Path, port: u16) -> Plan {
        Plan {
            state: state.to_path_buf(),
            repos: state.join("repos"),
            auth: state.join("auth"),
            key: state.join("agent.key"),
            trusted: state.join("keys"),
            config: PathBuf::from(".choir/config"),
            port,
        }
    }

    /// The URL the other commands will use.
    ///
    /// Loopback, and not configurable here on purpose: [`Node`] refuses
    /// a non-loopback bind without TLS, which is the privacy rule
    /// expressed as code rather than a doc note. Offering `--bind` on
    /// the command that runs *first* would put a certificate between a
    /// reader and their first push.
    ///
    /// [`Node`]: choir_node::Node
    #[must_use]
    pub fn node_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Every file this plan would write, in the order it writes them.
    #[must_use]
    pub fn files(&self) -> Vec<&Path> {
        vec![
            self.auth.as_path(),
            self.key.as_path(),
            self.trusted.as_path(),
            self.config.as_path(),
        ]
    }

    /// Which of them are already there.
    #[must_use]
    pub fn conflicts(&self) -> Vec<&Path> {
        self.files().into_iter().filter(|p| p.exists()).collect()
    }
}

/// Mints a bearer token.
///
/// `openssl rand`, not a crate: this workspace adds dependencies
/// reluctantly, already requires `openssl` for every other secret it
/// handles, and a randomness crate pulled in for one call site is a
/// supply-chain edge bought for nothing. A failure here is fatal rather
/// than fallen back on — there is no second source of randomness worth
/// having, and a predictable token is worse than no node.
///
/// # Errors
///
/// Returns a description when `openssl` is missing or fails.
pub fn mint_token() -> Result<String, String> {
    let out = std::process::Command::new("openssl")
        .args(["rand", "-hex", "32"])
        .output()
        .map_err(|_| {
            "could not run `openssl`, which mints the token: install it and run this again"
                .to_string()
        })?;
    if !out.status.success() {
        return Err("`openssl rand` failed; the token would not be random".to_string());
    }
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // 32 bytes as hex is 64 characters. Anything shorter means openssl
    // answered something this did not expect, and a short token must
    // never be written as though it were a full one.
    if token.len() != 64 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("`openssl rand` returned something that is not 32 hex bytes".to_string());
    }
    Ok(token)
}

/// What `choir init` did, so the caller can print it and the tests can
/// assert on it.
pub struct Created {
    /// The credential line's user half.
    pub user: String,
    /// Files written, in order.
    pub wrote: Vec<PathBuf>,
    /// Files replaced because `--force` was given.
    pub replaced: Vec<PathBuf>,
}

/// Runs the plan.
///
/// Order matters: the directories first, then the secrets, then the
/// config last. `.choir/config` is what makes every other command point
/// here, so writing it before the credential exists would leave a
/// working directory that confidently names a node nobody can reach.
///
/// # Errors
///
/// Refuses when anything the plan would write already exists and
/// `force` is false, naming all of them. Otherwise fails on the first
/// filesystem or `openssl` error.
pub fn run(plan: &Plan, force: bool) -> Result<Created, String> {
    let existing: Vec<PathBuf> = plan
        .conflicts()
        .into_iter()
        .map(Path::to_path_buf)
        .collect();
    if !existing.is_empty() && !force {
        let names: Vec<String> = existing.iter().map(|p| p.display().to_string()).collect();
        return Err(format!(
            "these already exist:\n  {}\n\nNothing was changed. Overwriting the auth file \
             locks you out of the node it belongs to — the token in it is the only copy, and \
             nothing can reissue it. Use --force to replace them deliberately.",
            names.join("\n  ")
        ));
    }

    for dir in [&plan.state, &plan.repos] {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    if let Some(parent) = plan.config.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }

    let user = "choir".to_string();
    let token = mint_token()?;
    // Private from creation, never write-then-chmod: the window between
    // the two is a window where the token is world-readable.
    choir_fs::write_atomic_private(&plan.auth, format!("{user}:{token}\n"))
        .map_err(|e| format!("write {}: {e}", plan.auth.display()))?;

    let key = choir_identity::ActorKey::generate();
    choir_fs::write_atomic_private(&plan.key, key.secret_bytes())
        .map_err(|e| format!("write {}: {e}", plan.key.display()))?;

    // The public half, in the form the node's --keys-file reads. Not
    // private: it is a public key, and a 0600 file the node runs as a
    // different user cannot read is a node that trusts nobody.
    let hex: String = key
        .public_key_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    choir_fs::write_atomic(&plan.trusted, format!("op {hex}\n"))
        .map_err(|e| format!("write {}: {e}", plan.trusted.display()))?;

    choir_fs::write_atomic(
        &plan.config,
        format!(
            "# Which node the `choir` commands talk to when they are not\n\
             # given one. Found by walking up from the working directory,\n\
             # the way git finds a repository.\n\
             node = {}\n",
            plan.node_url()
        ),
    )
    .map_err(|e| format!("write {}: {e}", plan.config.display()))?;

    Ok(Created {
        user,
        wrote: plan.files().into_iter().map(Path::to_path_buf).collect(),
        replaced: existing,
    })
}
