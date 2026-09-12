//! `choir node serve` — run the daemon without spelling out its flags.
//!
//! Starting a node used to mean `cargo run -p choir-node --` followed by
//! a repository root, a port and four `--*-file` paths, retyped or
//! copied out of a shell history. Every one of those is derivable from
//! the layout [`crate::init`] already writes, so this derives them: the
//! command that starts a node takes the same argument as the command
//! that created one, which is none.
//!
//! # It does not stay in the middle
//!
//! On unix this `exec`s the daemon rather than spawning it, so the
//! process the supervisor watches, the process that receives a signal
//! and the process in `ps` are all `choir-node` itself. A wrapper that
//! lingered would add a pid that means nothing, swallow the exit code
//! the daemon uses to ask for supervision (75), and put a second thing
//! between `launchd` and the thing it is meant to restart.
//!
//! # Examples
//!
//! ```
//! use choir_cli::serve::Layout;
//!
//! let layout = Layout::new(std::path::Path::new("/tmp/choir-serve-example"), 8417);
//! // Derived, not configured: the same paths `choir init` wrote.
//! assert!(layout.repos.ends_with("repos"));
//! assert!(layout.auth.ends_with("auth"));
//! ```

use std::path::{Path, PathBuf};

/// Where a node's state lives, by the convention `choir init` writes.
///
/// Fields rather than a lookup, so a caller cannot ask for a path this
/// does not define. The layout is deliberately not configurable
/// file-by-file: a node whose four paths can each point somewhere else
/// is a node whose backup, restore and status commands each need to be
/// told all four, and every one of them is a place to get it wrong.
pub struct Layout {
    /// The state directory itself, `~/.choir` unless told otherwise.
    pub state: PathBuf,
    /// Bare repositories; the daemon's positional root.
    pub repos: PathBuf,
    /// `user:token`, the credential the API checks.
    pub auth: PathBuf,
    /// Public keys the node accepts ops from. Without it the platform
    /// API stays off and every `/api/*` read answers 503.
    pub keys: PathBuf,
    /// Where the daemon's own log is appended when supervised.
    pub log: PathBuf,
    /// Two lines — certificate path, then key path — when this node
    /// terminates TLS itself. Its *existence* is the switch, the same
    /// marker discipline the review and scope gates already use: an
    /// empty file and an absent one mean opposite things, and a separate
    /// enable-flag beside the file it guards is a pair that can disagree.
    pub tls_marker: PathBuf,
    /// The D36 accounts file. Present means invites can be minted;
    /// absent means `/api/accounts/invite` answers 503.
    pub accounts: PathBuf,
    /// The D29 per-repository grants. Its existence is the switch, and
    /// the daemon refuses an accounts file without one — an issued grant
    /// with no table to grade it against is a grant to everything.
    pub acl: PathBuf,
    /// The one URL people outside this machine use, when there is one.
    pub public_url: PathBuf,
    /// Three `key = value` lines when this node is a seed of another
    /// node's log (D80): `home`, `credential`, `serve`. Its existence is
    /// the switch, the same discipline as `tls.enabled`: a seed and a
    /// home are the same layout started with different flags, and the
    /// marker is what `choir seed` writes so that `node serve`, `node
    /// install` and every command after them need no seed-specific
    /// argument.
    pub seed_marker: PathBuf,
    /// Where `choir seed` keeps the `user:token` line the home issued,
    /// mode 0600, so the unit never points outside the state directory.
    pub seed_credential: PathBuf,
    /// Present when landings are pushed to each repository's remotes
    /// (D21). Written by `choir repo follower add`; its existence is
    /// `--followers`.
    pub followers_marker: PathBuf,
    /// The port to bind.
    pub port: u16,
}

/// What the seed marker says.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Seed {
    /// The home whose log this node copies.
    pub home: String,
    /// The credential file the home issued to this seed's principal.
    pub credential: PathBuf,
    /// Whether it binds a port. An archival seed replicates, stores and
    /// serves nobody.
    pub serve: bool,
}

impl Seed {
    /// The marker's text.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "# This node is a seed of another node's log (D80). Written by\n\
             # `choir seed`; `choir node serve` reads it. Delete it to make\n\
             # this state directory a home again (its log would then be\n\
             # one nobody else writes to).\n\
             home = {}\n\
             credential = {}\n\
             serve = {}\n",
            self.home,
            self.credential.display(),
            if self.serve { "yes" } else { "no" }
        )
    }

    /// Reads a marker's text back.
    ///
    /// Anything short of a home, a credential and a `serve` answer is
    /// `None`: a half-written marker must not become a node that starts
    /// as a home over a log that was a copy.
    #[must_use]
    pub fn parse(text: &str) -> Option<Seed> {
        let mut home = None;
        let mut credential = None;
        let mut serve = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once('=')?;
            match key.trim() {
                "home" => home = Some(value.trim().trim_end_matches('/').to_string()),
                "credential" => credential = Some(PathBuf::from(value.trim())),
                "serve" => {
                    serve = match value.trim() {
                        "yes" => Some(true),
                        "no" => Some(false),
                        _ => return None,
                    }
                }
                _ => return None,
            }
        }
        Some(Seed {
            home: home?,
            credential: credential?,
            serve: serve?,
        })
    }
}

impl Layout {
    /// The standard layout under `state`.
    #[must_use]
    pub fn new(state: &Path, port: u16) -> Layout {
        Layout {
            state: state.to_path_buf(),
            repos: state.join("repos"),
            auth: state.join("auth"),
            keys: state.join("keys"),
            log: state.join("node.log"),
            tls_marker: state.join("tls.enabled"),
            accounts: state.join("accounts.jsonl"),
            acl: state.join("acl"),
            public_url: state.join("public-url"),
            seed_marker: state.join("seed"),
            seed_credential: state.join("seed-credential"),
            followers_marker: state.join("followers"),
            port,
        }
    }

    /// The home this node seeds, when it is a seed.
    #[must_use]
    pub fn seed(&self) -> Option<Seed> {
        Seed::parse(&std::fs::read_to_string(&self.seed_marker).ok()?)
    }

    /// The certificate and key this node terminates TLS with, if it does.
    ///
    /// Read at *start* time rather than baked into the supervision file,
    /// which is what makes renewal a restart rather than a re-render: a
    /// certbot deploy hook replaces the two files and restarts the unit,
    /// and the unit still says `choir node serve`.
    ///
    /// Anything but two non-empty lines is `None`. A half-written marker
    /// must not become a public bind with no certificate — that is
    /// invariant 9 by another route.
    #[must_use]
    pub fn tls(&self) -> Option<(PathBuf, PathBuf)> {
        let text = std::fs::read_to_string(&self.tls_marker).ok()?;
        let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
        let cert = PathBuf::from(lines.next()?);
        let key = PathBuf::from(lines.next()?);
        Some((cert, key))
    }

    /// The public URL, when one was written.
    #[must_use]
    pub fn public(&self) -> Option<String> {
        let text = std::fs::read_to_string(&self.public_url).ok()?;
        let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
        Some(line.to_string())
    }

    /// The address the daemon should bind.
    ///
    /// `0.0.0.0` exactly when there is a certificate to present, never
    /// otherwise. The daemon refuses the unsafe combination on its own
    /// (invariant 9); this side never asks for it, so the refusal is a
    /// backstop rather than the mechanism.
    #[must_use]
    pub fn bind(&self) -> &'static str {
        match self.tls() {
            Some(_) => "0.0.0.0",
            None => "127.0.0.1",
        }
    }

    /// What must already exist for a node to start, and does not.
    ///
    /// Checked before the daemon is launched so the answer names
    /// `choir init` rather than arriving as whatever the daemon says
    /// about a path it could not read. The repository root is not in
    /// this list: the daemon creates it, and an empty node is a valid
    /// one.
    ///
    /// A seed needs neither: it learns every key from its home's
    /// `/api/signers`, and an archival seed has no readers to name. What
    /// it cannot start without is the credential its marker points at.
    #[must_use]
    pub fn missing(&self) -> Vec<&Path> {
        if self.seed_marker.exists() {
            return match self.seed() {
                Some(seed) if seed.credential.exists() => Vec::new(),
                _ => vec![self.seed_credential.as_path()],
            };
        }
        [self.auth.as_path(), self.keys.as_path()]
            .into_iter()
            .filter(|p| !p.exists())
            .collect()
    }
}

/// The daemon invocation, built but not yet run.
///
/// Separated from running it so a test can assert on the exact argv
/// without a daemon, a port or a temp directory that outlives it. The
/// argv *is* the contract here — every flag this omits is a default the
/// daemon picks, and a test that only checked the process started would
/// not notice one going missing.
#[derive(Debug, Eq, PartialEq)]
pub struct Invocation {
    /// The `choir-node` binary that will be executed.
    pub program: PathBuf,
    /// Its arguments, in order, not including the program name.
    pub args: Vec<String>,
}

impl Invocation {
    /// The command as a person would type it, for printing before it runs.
    #[must_use]
    pub fn display(&self) -> String {
        let mut line = self.program.display().to_string();
        for arg in &self.args {
            line.push(' ');
            line.push_str(arg);
        }
        line
    }
}

/// Finds the `choir-node` binary.
///
/// Beside this executable first, `PATH` second. The sibling wins
/// because the two binaries are built and installed as a pair: an older
/// `choir-node` earlier on `PATH` would serve a different build than
/// the `choir` that launched it, and every symptom afterwards would
/// look like the running node being wrong rather than being old.
///
/// # Errors
///
/// Returns a description, naming both places it looked, when neither
/// holds one.
pub fn find_daemon() -> Result<PathBuf, String> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("choir-node");
            if sibling.is_file() {
                return Ok(sibling);
            }
        }
    }
    crate::doctor::on_path("choir-node").ok_or_else(|| {
        "no `choir-node` binary beside `choir` or on PATH\n\n  \
         build it:   cargo build --release -p choir-node\n  \
         install it: cp target/release/choir-node ~/.local/bin/"
            .to_string()
    })
}

/// Builds the daemon invocation for a layout.
///
/// `extra` is passed through verbatim after the derived flags, so any
/// daemon flag this does not know about is still reachable without
/// this function having to grow a copy of the daemon's argument parser
/// — which would be a second parser to keep in step, and the kind that
/// fails by silently dropping a flag rather than by refusing it.
///
/// # Errors
///
/// Refuses when the credential or the trusted-key file is missing,
/// naming the command that writes them. Starting without them would
/// produce a node that either accepts everyone or answers 503 to every
/// read, and both look like a broken install rather than an unfinished
/// one.
pub fn plan(
    program: PathBuf,
    layout: &Layout,
    create: &[String],
    extra: &[String],
) -> Result<Invocation, String> {
    let missing = layout.missing();
    if !missing.is_empty() {
        let names: Vec<String> = missing.iter().map(|p| p.display().to_string()).collect();
        return Err(format!(
            "this state directory has no node in it yet — missing:\n  {}\n\n  \
             create one: choir init",
            names.join("\n  ")
        ));
    }
    if let Some(seed) = layout.seed() {
        return plan_seed(program, layout, &seed, create, extra);
    }
    let mut args = vec![
        layout.repos.display().to_string(),
        layout.port.to_string(),
        "--auth-file".to_string(),
        layout.auth.display().to_string(),
        "--keys-file".to_string(),
        layout.keys.display().to_string(),
    ];
    // Both derived from a file's existence rather than from a flag, so
    // that a certificate arriving later, or an accounts file being
    // created later, changes what the node serves without the
    // supervision file being re-rendered. That is the failure mode of
    // the shell installers this replaces: a unit that lints clean and
    // restarts cleanly while launching yesterday's arguments.
    if let Some((cert, key)) = layout.tls() {
        for path in [&cert, &key] {
            if std::fs::File::open(path).is_err() {
                return Err(format!(
                    "{} names {}, which this user cannot read\n\n  \
                     re-issue and re-project the pair: sudo choir node tls <domain> \
                     --user $(id -un)",
                    layout.tls_marker.display(),
                    path.display()
                ));
            }
        }
        args.push("--bind".to_string());
        args.push(layout.bind().to_string());
        args.push("--tls-cert".to_string());
        args.push(cert.display().to_string());
        args.push("--tls-key".to_string());
        args.push(key.display().to_string());
    }
    if layout.acl.exists() {
        args.push("--acl-file".to_string());
        args.push(layout.acl.display().to_string());
    }
    if layout.accounts.exists() {
        // Refused here rather than left to the daemon, which exits on it
        // at startup and is therefore discovered by a supervisor
        // restarting every two seconds into the same failure.
        if !layout.acl.exists() {
            return Err(format!(
                "{} turns on invite-only credentials, and there is no {} to grade the\n  \
                 grants against — an issued grant with no table is a grant to every\n  \
                 repository. Write one, or remove the accounts file.",
                layout.accounts.display(),
                layout.acl.display()
            ));
        }
        args.push("--accounts-file".to_string());
        args.push(layout.accounts.display().to_string());
    }
    for repo in create {
        args.push("--create".to_string());
        args.push(repo.clone());
    }
    if layout.followers_marker.exists() {
        args.push("--followers".to_string());
    }
    args.extend_from_slice(extra);
    Ok(Invocation { program, args })
}

/// The daemon invocation for a seed (D80).
///
/// The same layout, three flags different: `--seed` and
/// `--seed-credential` instead of `--keys-file`, and no port at all for
/// an archival seed. The daemon refuses `--keys-file` and `--create` on
/// a seed; both are refused here first, so the sentence names the reason
/// rather than a supervisor restarting into it.
fn plan_seed(
    program: PathBuf,
    layout: &Layout,
    seed: &Seed,
    create: &[String],
    extra: &[String],
) -> Result<Invocation, String> {
    if !create.is_empty() {
        return Err(format!(
            "a seed takes no --create: its repositories come from {}",
            seed.home
        ));
    }
    if extra.iter().any(|a| a == "--keys-file") {
        return Err(
            "a seed takes no --keys-file: it learns every key from its home's /api/signers"
                .to_string(),
        );
    }
    let mut args = vec![layout.repos.display().to_string()];
    if seed.serve {
        args.push(layout.port.to_string());
    }
    args.push("--seed".to_string());
    args.push(seed.home.clone());
    args.push("--seed-credential".to_string());
    args.push(seed.credential.display().to_string());
    if !seed.serve {
        // An archival seed takes only its own flags; the daemon refuses
        // the rest, and there is nothing to bind them to.
        args.extend_from_slice(extra);
        return Ok(Invocation { program, args });
    }
    if layout.auth.exists() {
        args.push("--auth-file".to_string());
        args.push(layout.auth.display().to_string());
    }
    if let Some((cert, key)) = layout.tls() {
        for path in [&cert, &key] {
            if std::fs::File::open(path).is_err() {
                return Err(format!(
                    "{} names {}, which this user cannot read\n\n  \
                     re-issue and re-project the pair: sudo choir node tls <domain> \
                     --user $(id -un)",
                    layout.tls_marker.display(),
                    path.display()
                ));
            }
        }
        args.push("--bind".to_string());
        args.push(layout.bind().to_string());
        args.push("--tls-cert".to_string());
        args.push(cert.display().to_string());
        args.push("--tls-key".to_string());
        args.push(key.display().to_string());
    }
    if layout.acl.exists() {
        args.push("--acl-file".to_string());
        args.push(layout.acl.display().to_string());
    }
    args.extend_from_slice(extra);
    Ok(Invocation { program, args })
}

/// Whether something is already listening on this port.
///
/// Asked before the daemon is launched only to make the refusal
/// legible: a bind that fails inside the daemon reports an errno
/// against an address, and the useful sentence — that this is probably
/// the node you already started — is one this side can write and that
/// side cannot. It is a race in principle, and harmless in practice:
/// losing the race produces exactly the message not checking would have.
#[must_use]
pub fn port_taken(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_err()
}

/// Replaces this process with the daemon.
///
/// # Errors
///
/// Only returns if the `exec` itself failed; a successful call never
/// returns at all.
#[cfg(unix)]
pub fn exec(invocation: &Invocation) -> String {
    use std::os::unix::process::CommandExt;
    let error = std::process::Command::new(&invocation.program)
        .args(&invocation.args)
        .exec();
    format!("could not run {}: {error}", invocation.program.display())
}

/// Runs the daemon as a child and waits for it.
///
/// The non-unix fallback: without `exec` the wrapper has to stay, so it
/// at least forwards the exit code rather than inventing one.
///
/// # Errors
///
/// Returns a description when the daemon could not be started.
#[cfg(not(unix))]
pub fn exec(invocation: &Invocation) -> String {
    match std::process::Command::new(&invocation.program)
        .args(&invocation.args)
        .status()
    {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(error) => format!("could not run {}: {error}", invocation.program.display()),
    }
}

/// The port named by a node URL, if it names one.
///
/// `http://127.0.0.1:8417` is the shape `choir init` writes, and the
/// port in it is the one the daemon should bind — reading it back means
/// `serve` and every client command agree without the port being
/// written down twice.
#[must_use]
pub fn port_of(url: &str) -> Option<u16> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = rest.split('/').next()?;
    host.rsplit_once(':')?.1.parse().ok()
}
