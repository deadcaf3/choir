//! `choir node upgrade` — newer binaries into the place the unit runs
//! them from, a restart, and the running node's own word that it took.
//!
//! Deploying used to be five commands retyped over ssh, and the product
//! half of them is the half that happens *on the node host*: get the
//! binaries, put them where the service manager's unit points, restart,
//! and read the build stamp back from the daemon rather than trusting
//! that the restart did anything. That half is this command. The ssh
//! hop that types it stays outside `choir`, because a hostname and an
//! identity are the operator's private layout, and nothing in this
//! binary ever runs `ssh`.
//!
//! # Two sources
//!
//! | you have | run | what happens |
//! |:--|:--|:--|
//! | a node with a shelf | `choir node upgrade --from https://<node>` | its `install.sh`, into the directory this `choir` lives in |
//! | a checkout | `choir node upgrade --source <dir>` | `cargo build --release` there, then the four binaries copied over |
//!
//! Either way the copy is a rename over the old file, so a running
//! daemon keeps the inode it was started from until the restart, and a
//! build that fails leaves the old binaries where they were.
//!
//! # Examples
//!
//! ```
//! use choir_cli::upgrade::stamp_of;
//!
//! // `choir --version` prints the build line; the stamp is its commit.
//! assert_eq!(
//!     stamp_of("choir build 0123456789ab (stamp source: git)"),
//!     Some("0123456789ab".to_string())
//! );
//! assert_eq!(stamp_of("choir build unknown (stamp source: none)"), None);
//! ```

use std::path::{Path, PathBuf};

/// The binaries a release ships, and the ones a checkout builds.
pub const BINARIES: &[&str] = &["choir", "choir-node", "choir-mcp", "choir-ssh"];

/// Where the newer binaries come from.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Source {
    /// A node's `/download/` shelf: its installer, run.
    Shelf(String),
    /// A checkout of this workspace: `cargo build --release` there.
    Checkout(PathBuf),
}

/// What `choir node upgrade` was asked to do.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Options {
    /// Where from.
    pub source: Source,
    /// Where to: the directory the unit's `choir` lives in, unless
    /// `--into` names another.
    pub into: Option<PathBuf>,
    /// The state directory, for the node's address and credential.
    pub state: PathBuf,
    /// Print the plan and do nothing.
    pub dry_run: bool,
}

/// Parses `choir node upgrade`'s arguments.
///
/// # Errors
///
/// Returns a usage sentence when no source is named, both are, or an
/// option is unknown.
pub fn parse(rest: &[&str], default_state: PathBuf) -> Result<Options, String> {
    let mut from: Option<String> = None;
    let mut source: Option<PathBuf> = None;
    let mut into: Option<PathBuf> = None;
    let mut state: Option<PathBuf> = None;
    let mut dry_run = false;
    let mut i = 0;
    while i < rest.len() {
        let arg = rest[i];
        let value = || -> Result<String, String> {
            rest.get(i + 1)
                .map(|v| (*v).to_string())
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg {
            "--dry-run" => {
                dry_run = true;
                i += 1;
                continue;
            }
            "--from" => from = Some(value()?),
            "--source" => source = Some(PathBuf::from(value()?)),
            "--into" => into = Some(PathBuf::from(value()?)),
            "--state" => state = Some(PathBuf::from(value()?)),
            other => return Err(format!("unknown option {other:?}")),
        }
        i += 2;
    }
    let source = match (from, source) {
        (Some(_), Some(_)) => {
            return Err(
                "--from and --source are two answers to one question. Give one.".to_string(),
            )
        }
        (Some(url), None) => {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(format!(
                    "--from is a node URL, http:// or https://, not {url:?}"
                ));
            }
            Source::Shelf(url.trim_end_matches('/').to_string())
        }
        (None, Some(dir)) => Source::Checkout(absolute(dir)),
        (None, None) => {
            return Err("where do the newer binaries come from?\n\n    \
                        choir node upgrade --from https://<node>     its shelf\n    \
                        choir node upgrade --source <checkout>      built there"
                .to_string())
        }
    };
    Ok(Options {
        source,
        into: into.map(absolute),
        state: absolute(state.unwrap_or(default_state)),
        dry_run,
    })
}

fn absolute(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}

/// The commit a `choir --version` line names, or `None` for an
/// unstamped build.
#[must_use]
pub fn stamp_of(version_line: &str) -> Option<String> {
    let word = version_line
        .split_whitespace()
        .skip_while(|w| *w != "build")
        .nth(1)?;
    let word = word.trim_end_matches("+dirty");
    (word.len() >= 7 && word.chars().all(|c| c.is_ascii_hexdigit())).then(|| word.to_string())
}

/// What a `choir` binary says it is: the whole line, and its commit.
#[must_use]
pub fn version_of(choir: &Path) -> Option<(String, Option<String>)> {
    let out = std::process::Command::new(choir)
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stamp = stamp_of(&line);
    Some((line, stamp))
}

/// The directory the installer will fill, for a shelf source.
///
/// The installer writes `$CARGO_HOME/bin`, so the target has to be a
/// directory *called* `bin`; its parent is what the installer is told.
///
/// # Errors
///
/// Names the directory when it is not a `bin`.
pub fn cargo_home_for(into: &Path) -> Result<PathBuf, String> {
    match (into.file_name(), into.parent()) {
        (Some(name), Some(parent)) if name == "bin" => Ok(parent.to_path_buf()),
        _ => Err(format!(
            "the installer places binaries in a directory called `bin`, and this \
             choir lives in {}\n\n  \
             build from a checkout instead: choir node upgrade --source <dir>\n  \
             or name a bin directory:        choir node upgrade --from <node> --into <dir>/bin",
            into.display()
        )),
    }
}

/// Copies each binary a build produced over the installed one, by
/// rename, so a running process keeps its inode and a copy cut short
/// leaves the old binary in place.
///
/// Returns the names placed.
///
/// # Errors
///
/// Returns a description when nothing was built, or a copy failed.
pub fn place(built: &Path, into: &Path) -> Result<Vec<String>, String> {
    std::fs::create_dir_all(into).map_err(|e| format!("create {}: {e}", into.display()))?;
    let mut placed = Vec::new();
    for name in BINARIES {
        let from = built.join(name);
        if !from.is_file() {
            continue;
        }
        let staged = into.join(format!(".{name}.new"));
        std::fs::copy(&from, &staged)
            .map_err(|e| format!("copy {} to {}: {e}", from.display(), staged.display()))?;
        std::fs::rename(&staged, into.join(name))
            .map_err(|e| format!("place {}: {e}", into.join(name).display()))?;
        placed.push((*name).to_string());
    }
    if placed.is_empty() {
        return Err(format!(
            "nothing to place: none of {} is in {}",
            BINARIES.join(", "),
            built.display()
        ));
    }
    Ok(placed)
}

/// Builds the two packages in a checkout with the stamp set from its
/// HEAD, and returns the release directory.
///
/// `cd` rather than `--manifest-path`: rustup resolves
/// `rust-toolchain.toml` from the working directory, and the other form
/// fails on a box whose account has no default toolchain, with an error
/// naming rustup rather than the flag that caused it.
///
/// # Errors
///
/// Returns cargo's or git's own words when either refuses.
pub fn build(checkout: &Path) -> Result<(PathBuf, String), String> {
    let head = std::process::Command::new("git")
        .args(["-C", &checkout.display().to_string(), "rev-parse", "HEAD"])
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if !head.status.success() {
        return Err(format!(
            "{} is not a git checkout: {}",
            checkout.display(),
            String::from_utf8_lossy(&head.stderr).trim()
        ));
    }
    let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "choir-node", "-p", "choir-cli"])
        .current_dir(checkout)
        .env("CHOIR_GIT_HEAD", &head)
        .status()
        .map_err(|e| format!("could not run cargo: {e}"))?;
    if !status.success() {
        return Err("cargo build failed; the old binaries are untouched".to_string());
    }
    Ok((checkout.join("target").join("release"), head))
}

/// Runs a node's installer for both packages, into `cargo_home/bin`.
///
/// The script is fetched to a file and run from it rather than piped,
/// so what ran is on disk to read afterwards.
///
/// # Errors
///
/// Returns the installer's own words when it refuses.
pub fn install_from_shelf(node: &str, cargo_home: &Path) -> Result<String, String> {
    let script = std::env::temp_dir().join(format!("choir-install-{}.sh", std::process::id()));
    let fetched = std::process::Command::new("curl")
        .args([
            "-fsSL",
            &format!("{node}/download/install.sh"),
            "-o",
            &script.display().to_string(),
        ])
        .output()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !fetched.status.success() {
        return Err(format!(
            "could not fetch {node}/download/install.sh: {}",
            String::from_utf8_lossy(&fetched.stderr).trim()
        ));
    }
    let out = std::process::Command::new("sh")
        .arg(&script)
        .args(["choir-cli", "choir-node"])
        .env("CARGO_HOME", cargo_home)
        .output()
        .map_err(|e| format!("could not run sh: {e}"))?;
    std::fs::remove_file(&script).ok();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !out.status.success() {
        return Err(format!("the installer refused:\n{}", text.trim()));
    }
    Ok(text.trim().to_string())
}
