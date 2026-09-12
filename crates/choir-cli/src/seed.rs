//! `choir seed` — a fresh machine to a running seed of somebody else's
//! log, in one command (D80).
//!
//! Everything this does was already possible: mint a node key where the
//! daemon will look for it, hand the home's operator three lines to
//! paste, take the credential they issue, start `choir-node` with
//! `--seed`, supervise it. The order is what nobody should have to know,
//! and the daemon flags are what nobody should have to retype, which is
//! the same reasoning as [`crate::host`].
//!
//! # Two runs, on purpose
//!
//! A seed is a *named* reader of its home. Its key has to be registered
//! and bound there, and a credential issued, before it can read a page,
//! and all of that happens on a machine this command is not running on.
//! So the first run stops after minting the identity and prints what the
//! home registers, exit 3, the same handover shape as `choir host`'s
//! certificate step; the second run, with `--credential`, finishes.
//!
//! # What it writes
//!
//! The marker [`crate::serve::Seed`] beside the state directory's other
//! markers, and the credential copied to 0600 inside it. After that
//! `choir node serve`, `node install`, `node status`, `node logs` and
//! `doctor` all work on a seed exactly as on a home, because the marker
//! is what they read.
//!
//! # Examples
//!
//! ```
//! use choir_cli::seed::registration;
//!
//! // The three things the home's operator pastes: a keys line, a
//! // binding, and the two grants a seed needs (the log grant does not
//! // reach git, so `read` on the repositories is the second one).
//! let lines = registration("seed-a", "ab".repeat(32).as_str(), "https://home.example");
//! assert!(lines.iter().any(|l| l.contains("@node auditor")));
//! assert!(lines.iter().any(|l| l.contains("choir bind https://home.example")));
//! ```

use std::path::{Path, PathBuf};

/// What `choir seed` was asked to do.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Options {
    /// The home whose log this machine will copy, without a trailing slash.
    pub home: String,
    /// The `user:token` file the home issued, once it has.
    pub credential: Option<PathBuf>,
    /// The state directory, `~/.choir` unless given.
    pub state: PathBuf,
    /// The port to serve on. Ignored by an archival seed.
    pub port: u16,
    /// Replicate and store, bind nothing.
    pub archival: bool,
    /// The principal's name at the home, as the keys line and the ACL
    /// will spell it.
    pub name: String,
    /// Become the daemon instead of installing a unit.
    pub foreground: bool,
    /// Daemon flags, passed through after `--`.
    pub extra: Vec<String>,
}

/// Parses `choir seed`'s arguments.
///
/// # Errors
///
/// Returns a usage sentence naming what was wrong: no home, a home that
/// is not an `http(s)` URL, a port on an archival seed, or an option this
/// does not know (daemon flags go after `--`).
pub fn parse(rest: &[&str], default_state: PathBuf) -> Result<Options, String> {
    let (mine, extra) = match rest.iter().position(|a| *a == "--") {
        Some(at) => (&rest[..at], &rest[at + 1..]),
        None => (rest, &rest[rest.len()..]),
    };
    let mut home: Option<String> = None;
    let mut credential: Option<PathBuf> = None;
    let mut state: Option<PathBuf> = None;
    let mut port: Option<u16> = None;
    let mut archival = false;
    let mut name: Option<String> = None;
    let mut foreground = false;

    let mut i = 0;
    while i < mine.len() {
        let arg = mine[i];
        let value = || -> Result<String, String> {
            mine.get(i + 1)
                .map(|v| (*v).to_string())
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg {
            "--archival" => {
                archival = true;
                i += 1;
                continue;
            }
            "--foreground" => {
                foreground = true;
                i += 1;
                continue;
            }
            "--credential" => credential = Some(PathBuf::from(value()?)),
            "--state" => state = Some(PathBuf::from(value()?)),
            "--name" => name = Some(value()?),
            "--port" => {
                let raw = value()?;
                port = Some(
                    raw.parse()
                        .map_err(|_| format!("--port needs a port number, not {raw:?}"))?,
                );
            }
            other if other.starts_with('-') => {
                return Err(format!(
                    "unknown option {other:?}\n\n  \
                     daemon flags go after `--`: choir seed <home-url> -- {other} ..."
                ));
            }
            positional => {
                if home.is_some() {
                    return Err(format!(
                        "one home, not two: {positional:?} came after {}",
                        home.as_deref().unwrap_or_default()
                    ));
                }
                home = Some(positional.to_string());
                i += 1;
                continue;
            }
        }
        i += 2;
    }

    let Some(home) = home else {
        return Err("which node's log is this a seed of?\n\n    \
                    choir seed https://<home> [--credential <file>]"
            .to_string());
    };
    if !(home.starts_with("http://") || home.starts_with("https://")) {
        return Err(format!(
            "the home is a node URL, http:// or https://, not {home:?}"
        ));
    }
    if archival && port.is_some() {
        return Err(
            "--archival binds nothing, so --port would never be used. Give one or the other."
                .to_string(),
        );
    }
    let name = name.unwrap_or_else(|| "seed".to_string());
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    {
        return Err(format!(
            "--name is the principal's name at the home: ASCII letters, digits, `-`, `_` \
             and `.`, not {name:?}"
        ));
    }

    Ok(Options {
        home: home.trim_end_matches('/').to_string(),
        credential,
        state: absolute(state.unwrap_or(default_state)),
        port: port.unwrap_or(8417),
        archival,
        name,
        foreground,
        extra: extra.iter().map(|a| (*a).to_string()).collect(),
    })
}

/// Resolves a state directory against the working directory.
///
/// The supervision file this ends up in is read by launchd or systemd,
/// neither of which is standing where you were; see [`crate::host`].
fn absolute(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}

/// Where the daemon keeps a node's key under `repos`.
///
/// The seed's own key is a node key like any other: the daemon reads
/// `<root>/.choir/node.key`, 32 secret bytes, and mints one if absent.
/// Minted *here* rather than left to the daemon because the home has to
/// know the public half before the daemon can read a page, which is
/// before it ever starts.
#[must_use]
pub fn key_path(repos: &Path) -> PathBuf {
    repos.join(".choir").join("node.key")
}

/// Reads the node key at `path`, or mints one there.
///
/// # Errors
///
/// Returns a description when the file exists and is not 32 bytes, or
/// cannot be written.
pub fn identity(path: &Path) -> Result<choir_identity::ActorKey, String> {
    if path.exists() {
        let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            format!(
                "{} is not a node key: it should be exactly 32 bytes",
                path.display()
            )
        })?;
        return Ok(choir_identity::ActorKey::from_secret_bytes(&bytes));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let key = choir_identity::ActorKey::generate();
    choir_fs::write_atomic_private(path, key.secret_bytes())
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(key)
}

/// The public half of a key, as the keys file spells it.
#[must_use]
pub fn public_hex(key: &choir_identity::ActorKey) -> String {
    key.public_key_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// What the home's operator pastes to admit this seed, one line each.
///
/// Nothing seed-specific on the home: a keys line, a binding, a
/// credential and two grants, all files it already has. Two grants
/// rather than one because the log grant (`@node auditor`, the
/// node-wide read `/api/log` needs) does not reach git, and a seed that
/// cannot fetch holds a history pointing at objects it does not have.
/// `*` is the whole node; name repositories instead to seed only some.
#[must_use]
pub fn registration(name: &str, hex: &str, home: &str) -> Vec<String> {
    vec![
        format!("keys file:   {name} {hex}"),
        format!("binding:     choir bind {home} <home-root>/.choir/node.key {name} {hex}"),
        format!("auth file:   {name}:<a token minted there, openssl rand -hex 32>"),
        format!("acl file:    {name} @node auditor"),
        format!("             {name} * read"),
    ]
}

/// Copies the issued credential into the state directory at 0600.
///
/// The unit that starts the seed must not point at a file in a download
/// folder or a pasted-into `/tmp`; it points inside the state directory,
/// where a backup, a `doctor` and a person looking for it will look. The
/// file is validated as one `user:token` line first, so a wrong file is
/// refused before anything is written.
///
/// # Errors
///
/// Returns a description when the file is not a credential or cannot be
/// copied.
pub fn place_credential(from: &Path, into: &Path) -> Result<String, String> {
    let credential = choir_node::replica::Credential::read(from)?;
    if from != into {
        let text =
            std::fs::read_to_string(from).map_err(|e| format!("read {}: {e}", from.display()))?;
        choir_fs::write_atomic_private(into, text)
            .map_err(|e| format!("write {}: {e}", into.display()))?;
    }
    Ok(credential.user().to_string())
}
