//! Followers (D21): after a ref lands, push it to the remotes the
//! operator added to the bare repository.
//!
//! A follower is any remote a bare repository under the root carries. A
//! bare repository on a node has no `origin`, it *is* the origin, so
//! every remote on one was added on purpose, and enumerating them is
//! what keeps this list-free: adding a mirror is `git remote add` on the
//! node host (`choir repo follower add`), and nothing here to edit.
//!
//! # The node is canonical, and this only ever pushes
//!
//! D21's single-canonical invariant: upstream is the node, a follower
//! copies it, and nothing flows back. So this pushes `refs/heads/*` and
//! `refs/tags/*`, never fetches, and **never forces**. A follower that
//! diverged from the node is a fault to surface, not a state to
//! overwrite, and a push that refuses is the whole of that surfacing. A
//! deleted ref is not deleted on the follower either: a follower is a
//! copy that may hold more, and pruning is a policy nobody has asked for.
//!
//! # Invariant 5, the same way webhooks keep it
//!
//! A push goes to a host somebody else runs, so its latency is unbounded.
//! The writer thread therefore does one thing with a landed ref:
//! [`Followers::offer`], a non-blocking `try_send` of the repository's
//! name into a bounded queue. The pushes run on the thread this module
//! spawns, and a full queue drops the offer and counts it. Dropping is
//! safe here in a way it is not for a webhook: the next landing on the
//! same repository pushes everything, so a dropped offer is delay, not
//! loss.
//!
//! # Consent is the flag
//!
//! The daemon pushes only when started with `--followers`. Without it a
//! remote on a bare repository is inert, which matters for a mirror
//! `choir-bridge` keeps: that repository carries an `origin` pointing
//! *upstream*, and a node that pushed to every remote it found would
//! push a mirror back at the thing it mirrors.
//!
//! # Examples
//!
//! ```
//! use choir_node::followers::repository_of;
//!
//! // A landed ref names its repository ahead of the colon.
//! assert_eq!(repository_of("owner/repo.git:refs/heads/main"), Some("owner/repo.git"));
//! assert_eq!(repository_of("refs/heads/main"), None);
//! ```

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::Arc;

/// Offers waiting to be pushed. Small on purpose: every offer for a
/// repository is the same work, so a deep queue would hold duplicates.
const QUEUE_CAPACITY: usize = 64;

/// The repository half of a view ref key, `<repo>:<refname>`.
#[must_use]
pub fn repository_of(key: &str) -> Option<&str> {
    let (repo, rest) = key.split_once(':')?;
    (!repo.is_empty() && rest.starts_with("refs/")).then_some(repo)
}

/// The push handle the writer thread holds.
pub struct Followers {
    tx: SyncSender<String>,
    dropped: Arc<AtomicU64>,
}

impl Followers {
    /// Starts the push thread over the bare repositories under `root`,
    /// appending one record per push to `log`.
    ///
    /// # Errors
    ///
    /// Returns a message when the thread cannot be started.
    pub fn start(root: PathBuf, log: PathBuf) -> Result<Self, String> {
        let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let worker = Worker {
            rx,
            root,
            log,
            dropped: Arc::clone(&dropped),
        };
        std::thread::Builder::new()
            .name("choir-followers".to_string())
            .spawn(move || worker.run())
            .map_err(|e| format!("could not start the follower push thread: {e}"))?;
        eprintln!("followers enabled: every landing is pushed to each remote of its repository");
        Ok(Self { tx, dropped })
    }

    /// Offers a landed ref's repository to the push thread. **Never
    /// blocks**; a full queue drops the offer and counts it, and the next
    /// landing on the same repository pushes everything anyway.
    pub fn offer(&self, key: &str) {
        let Some(repo) = repository_of(key) else {
            return;
        };
        match self.tx.try_send(repo.to_string()) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Offers dropped because the queue was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

struct Worker {
    rx: Receiver<String>,
    root: PathBuf,
    log: PathBuf,
    dropped: Arc<AtomicU64>,
}

impl Worker {
    fn run(self) {
        let mut reported = 0;
        while let Ok(first) = self.rx.recv() {
            // Everything queued behind it, once each: twenty landings on
            // one repository while a slow push ran are one push after it.
            let mut pending = BTreeSet::from([first]);
            while let Ok(repo) = self.rx.try_recv() {
                pending.insert(repo);
            }
            for repo in pending {
                for outcome in push_all(&self.root, &repo) {
                    record(&self.log, &outcome);
                }
            }
            let dropped = self.dropped.load(Ordering::Relaxed);
            if dropped > reported {
                record(
                    &self.log,
                    &Outcome {
                        repo: String::new(),
                        remote: String::new(),
                        event: "dropped",
                        detail: format!("{} offers dropped, queue full", dropped - reported),
                    },
                );
                reported = dropped;
            }
        }
    }
}

/// One push, as it went.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Outcome {
    /// The repository, as `repos.list` spells it.
    pub repo: String,
    /// The remote's name; empty when the repository has none.
    pub remote: String,
    /// `pushed`, `failed`, `no_remote`, `no_repository` or `dropped`.
    pub event: &'static str,
    /// git's own words, or what was missing.
    pub detail: String,
}

impl Outcome {
    /// Whether this is a push that succeeded or a repository with
    /// nothing to push to; the latter is named, never counted as failure,
    /// because "no second copy yet" should stay visible without being an
    /// error.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.event != "failed" && self.event != "no_repository"
    }
}

/// The remotes a bare repository carries, `(name, url)` each.
#[must_use]
pub fn remotes(bare: &Path) -> Vec<(String, String)> {
    let Some(names) = git(bare, &["remote"]) else {
        return Vec::new();
    };
    names
        .lines()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|name| {
            let url = git(bare, &["remote", "get-url", name])
                .map(|u| u.trim().to_string())
                .unwrap_or_default();
            (name.to_string(), url)
        })
        .collect()
}

/// Pushes every branch and tag of one bare repository to one remote.
/// No force, no prune, no fetch.
///
/// # Errors
///
/// Returns git's stderr when the push was refused or could not run.
pub fn push(bare: &Path, remote: &str) -> Result<(), String> {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(bare)
        .args([
            "push",
            "-q",
            remote,
            "refs/heads/*:refs/heads/*",
            "refs/tags/*:refs/tags/*",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Pushes one repository to each of its remotes, one outcome per remote,
/// or one `no_remote` outcome when it has none.
#[must_use]
pub fn push_all(root: &Path, repo: &str) -> Vec<Outcome> {
    let bare = root.join(repo);
    if !bare.is_dir() {
        return vec![Outcome {
            repo: repo.to_string(),
            remote: String::new(),
            event: "no_repository",
            detail: format!("{} is not a repository", bare.display()),
        }];
    }
    let remotes = remotes(&bare);
    if remotes.is_empty() {
        return vec![Outcome {
            repo: repo.to_string(),
            remote: String::new(),
            event: "no_remote",
            detail: "no remote: its objects have no second copy yet".to_string(),
        }];
    }
    remotes
        .into_iter()
        .map(|(name, url)| match push(&bare, &name) {
            Ok(()) => Outcome {
                repo: repo.to_string(),
                remote: name,
                event: "pushed",
                detail: url,
            },
            Err(why) => Outcome {
                repo: repo.to_string(),
                remote: name,
                event: "failed",
                detail: why,
            },
        })
        .collect()
}

/// Appends one outcome to the push log as a JSON line.
fn record(log: &Path, outcome: &Outcome) {
    let line = serde_json::json!({
        "at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "repo": outcome.repo,
        "remote": outcome.remote,
        "event": outcome.event,
        "detail": outcome.detail,
    });
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        use std::io::Write;
        let _ = writeln!(file, "{line}");
    }
}

fn git(bare: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(bare)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).to_string())
}
