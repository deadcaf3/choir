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
    /// The port to bind on loopback.
    pub port: u16,
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
            port,
        }
    }

    /// What must already exist for a node to start, and does not.
    ///
    /// Checked before the daemon is launched so the answer names
    /// `choir init` rather than arriving as whatever the daemon says
    /// about a path it could not read. The repository root is not in
    /// this list: the daemon creates it, and an empty node is a valid
    /// one.
    #[must_use]
    pub fn missing(&self) -> Vec<&Path> {
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
    let mut args = vec![
        layout.repos.display().to_string(),
        layout.port.to_string(),
        "--auth-file".to_string(),
        layout.auth.display().to_string(),
        "--keys-file".to_string(),
        layout.keys.display().to_string(),
    ];
    for repo in create {
        args.push("--create".to_string());
        args.push(repo.clone());
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
