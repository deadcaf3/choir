//! `choir backup restore` — turn a backup back into a node, and refuse
//! to say it worked until the restored node has accepted a write.
//!
//! The other half of a backup leg. That one proves a copy arrived; this
//! proves the copy is a node. A backup nobody has restored is a
//! hypothesis, and the only thing that settles it is a running daemon
//! appending to the log it was handed.
//!
//! # Nothing here mints a secret
//!
//! Backups exclude them by design, so a restore has a hole in it only a
//! person can fill: the daemon's signing key, the credential, and any
//! TLS material. This stops and names them rather than inventing
//! replacements, because a minted node key is a *new node* wearing the
//! old node's log.
//!
//! # The ordering is the safety property
//!
//! Read, refuse, then write. A restore that fails halfway has already
//! destroyed the thing an operator would fall back to, so everything
//! checkable about the backup is checked before a byte reaches the
//! target — and the policy archive is unpacked into a work directory
//! rather than into the node's root for the same reason.
//!
//! # Examples
//!
//! ```
//! use choir_cli::restore::Refusal;
//!
//! // Exit 3 is its own code: an operator decision, not a failed check.
//! let decision = Refusal::decide("the signing key is not here");
//! assert_eq!(decision.code, 3);
//! ```

use std::path::{Path, PathBuf};

/// Why a restore stopped.
///
/// `code` is the process exit code, and the three are genuinely
/// different outcomes: 1 is "this backup or this target is wrong", 3 is
/// "the backup is fine and something only you can supply is missing".
/// Collapsing them would make the documented recovery path — stop,
/// supply the key, re-run — indistinguishable from a corrupt backup.
#[derive(Debug)]
pub struct Refusal {
    /// Process exit code: 1 a check failed, 2 usage, 3 a decision.
    pub code: i32,
    /// What to print, already addressed to a person.
    pub message: String,
}

impl Refusal {
    /// A check failed: exit 1.
    #[must_use]
    pub fn fail(message: impl Into<String>) -> Refusal {
        Refusal {
            code: 1,
            message: message.into(),
        }
    }

    /// An operator decision is required: exit 3.
    #[must_use]
    pub fn decide(message: impl Into<String>) -> Refusal {
        Refusal {
            code: 3,
            message: message.into(),
        }
    }
}

/// What the backup turned out to hold, after every read-side check.
#[derive(Debug)]
pub struct Backup {
    /// The backup directory.
    pub src: PathBuf,
    /// Where the policy files are readable from — the backup's own
    /// `policy/`, or a work directory the tar was unpacked into.
    pub policy: PathBuf,
    /// Repository names from `repos.list`, in file order.
    pub repos: Vec<String>,
    /// Lines in the backed-up log, which is also the seq the canary
    /// will land at.
    pub ops: usize,
    /// Bytes in the backed-up log, for the prefix check at the end.
    pub bytes: u64,
}

/// What a completed restore proved.
#[derive(Debug)]
pub struct Restored {
    /// Operations replayed.
    pub ops: usize,
    /// Repositories unbundled.
    pub repos: usize,
    /// The canary ref, left in place as evidence.
    pub canary: String,
    /// The repository it landed in.
    pub landed_in: String,
}

/// Policy a node cannot boot or serve restored refs without.
pub const REQUIRED: &[&str] = &["keys", "reviewers", "repos.list"];

/// Policy whose absence changes what the restored node enforces.
///
/// Named rather than refused: a restore that demanded all of them would
/// refuse every backup from a node that protects no ref, and one that
/// stayed quiet would hand back a node whose policy is weaker than the
/// one it replaced.
pub const OPTIONAL: &[&str] = &[
    "protected-refs",
    "newcomer-audit.jsonl",
    "newcomer-adjudications.jsonl",
    "review-adjudications.jsonl",
    "acl",
    "private-beta.manifest",
];

/// Reads and checks everything about the backup, writing nothing.
///
/// # Errors
///
/// Refuses, with exit code 1, on any of: no log, an empty log, a log the
/// daemon will not verify, no policy in either shape, a missing required
/// policy file, a secret anywhere in the backup, a `repos.list` naming
/// nothing, or a repository with no bundle.
pub fn read(src: &Path, work: &Path, daemon: &Path) -> Result<Backup, Refusal> {
    let ops_path = src.join("ops.jsonl");
    let text = std::fs::read_to_string(&ops_path).map_err(|_| {
        Refusal::fail(format!(
            "no ops.jsonl in {}: that is not a backup",
            src.display()
        ))
    })?;
    let ops = text.lines().count();
    if ops == 0 {
        return Err(Refusal::fail("the backed-up log is empty"));
    }
    let bytes = text.len() as u64;

    // Before a byte reaches the target: unsupported formats, sequence
    // gaps, broken parent links, recomputed-hash mismatches and torn
    // tails, all rejected by the same code the daemon ships.
    let verified = std::process::Command::new(daemon)
        .args(["--verify-log", &ops_path.display().to_string()])
        .output()
        .is_ok_and(|out| out.status.success());
    if !verified {
        return Err(Refusal::fail(
            "the backup failed format/sequence/parent/hash verification",
        ));
    }

    // Either shape of the same thing. The tar goes to the work
    // directory, so a backup that fails a check below has still written
    // nothing where a node would read it.
    let policy = if src.join("policy").is_dir() {
        src.join("policy")
    } else if src.join("policy.tar").is_file() {
        let into = work.join("policy");
        std::fs::create_dir_all(&into)
            .map_err(|e| Refusal::fail(format!("create {}: {e}", into.display())))?;
        let ok = std::process::Command::new("tar")
            .arg("-xf")
            .arg(src.join("policy.tar"))
            .arg("-C")
            .arg(&into)
            .output()
            .is_ok_and(|out| out.status.success());
        if !ok {
            return Err(Refusal::fail(format!(
                "could not read {}",
                src.join("policy.tar").display()
            )));
        }
        into
    } else {
        return Err(Refusal::fail(format!(
            "no policy in {} (neither policy/ nor policy.tar): a node restored \
             without reviewers does not boot",
            src.display()
        )));
    };

    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|f| !policy.join(f).is_file())
        .collect();
    if !missing.is_empty() {
        return Err(Refusal::fail(format!(
            "policy files missing from the backup: {} (a node restored without \
             reviewers does not boot)",
            missing.join(" ")
        )));
    }

    // The same assertion a backup leg makes about its own output, made
    // again here about its input: it holds whoever put the file there.
    let mut leaked = Vec::new();
    for dir in [src, policy.as_path()] {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if crate::backup::is_secret(&name) {
                    leaked.push(name);
                }
            }
        }
    }
    if !leaked.is_empty() {
        leaked.sort();
        leaked.dedup();
        return Err(Refusal::fail(format!(
            "SECRETS IN THE BACKUP: {} (a backup holding a token or key is a \
             credential channel, and this restore will not spread it)",
            leaked.join(" ")
        )));
    }

    let listed = std::fs::read_to_string(policy.join("repos.list")).unwrap_or_default();
    let repos: Vec<String> = listed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect();
    if repos.is_empty() {
        return Err(Refusal::fail(
            "repos.list names no repositories: there is nothing to serve",
        ));
    }
    for repo in &repos {
        if bundle_for(src, repo).is_none() {
            return Err(Refusal::fail(format!(
                "no bundle for {repo}: the log's refs for it name commits nothing here holds"
            )));
        }
    }

    Ok(Backup {
        src: src.to_path_buf(),
        policy,
        repos,
        ops,
        bytes,
    })
}

/// The bundle for one repository, under either backup leg's naming.
///
/// The full repository path (`owner/name.git.bundle`) and the basename
/// the flip-era leg writes (`name.bundle`). Resolved in one place so the
/// existence check and the unbundle loop can never disagree about which
/// file they mean.
#[must_use]
pub fn bundle_for(src: &Path, repo: &str) -> Option<PathBuf> {
    let full = src.join("repos").join(format!("{repo}.bundle"));
    if full.is_file() {
        return Some(full);
    }
    let base = repo.rsplit('/').next().unwrap_or(repo);
    let base = base.strip_suffix(".git").unwrap_or(base);
    let short = src.join("repos").join(format!("{base}.bundle"));
    short.is_file().then_some(short)
}

/// Whether this target is a resumed placement rather than a fresh one.
///
/// A log byte-identical to the backup's is this command's own earlier
/// placement, from a run that stopped for a decision. Refusing it would
/// make the documented recovery path unreachable: the first run places
/// the files, and the second could never get past this check. Anything
/// else is somebody's node, and which of the two logs is real is not a
/// decision available here.
///
/// # Errors
///
/// Refuses when the target holds a different log, or already holds one
/// of the repositories.
pub fn resuming(root: &Path, backup: &Backup) -> Result<bool, Refusal> {
    let placed = root.join(".choir/ops.jsonl");
    let resume = if placed.exists() {
        let same = std::fs::read(&placed).ok() == std::fs::read(backup.src.join("ops.jsonl")).ok();
        if !same {
            return Err(Refusal::fail(format!(
                "{} already exists and is not this backup: restore into an empty \
                 root, or move the existing log aside first (it is never \
                 overwritten here)",
                placed.display()
            )));
        }
        true
    } else {
        false
    };
    if !resume {
        for repo in &backup.repos {
            if root.join(repo).exists() {
                return Err(Refusal::fail(format!(
                    "{} already exists: restore into an empty root",
                    root.join(repo).display()
                )));
            }
        }
    }
    Ok(resume)
}

/// Places the log, the policy and the git objects.
///
/// Objects go in before the first boot, never after. Startup
/// reconciliation reads the log against what git holds, and a ref naming
/// a commit the repository does not have is classed as unbackable — so
/// the daemon appends a retraction and the log now agrees with the
/// emptiness. Restoring into repositories the daemon made for itself
/// would therefore erase, in signed ops, exactly the ref state being
/// restored.
///
/// # Errors
///
/// Refuses on any filesystem error, or a bundle git will not clone.
pub fn place(root: &Path, backup: &Backup, resume: bool) -> Result<Vec<String>, Refusal> {
    let choir = root.join(".choir");
    std::fs::create_dir_all(choir.join("policy"))
        .map_err(|e| Refusal::fail(format!("create {}: {e}", choir.display())))?;
    std::fs::copy(backup.src.join("ops.jsonl"), choir.join("ops.jsonl"))
        .map_err(|e| Refusal::fail(format!("place the log: {e}")))?;

    // Not on a resume. Deleting the fingerprint is the operator
    // accepting that the log changes author, and it is done between two
    // runs of this command — re-placing it would put back the pin they
    // just removed, and the re-run they were told to make would stop at
    // the same refusal forever.
    if !resume && backup.src.join("node.fingerprint").is_file() {
        std::fs::copy(
            backup.src.join("node.fingerprint"),
            choir.join("node.fingerprint"),
        )
        .map_err(|e| Refusal::fail(format!("place the fingerprint: {e}")))?;
    }
    if backup.src.join("refs.snapshot").is_file() {
        std::fs::copy(
            backup.src.join("refs.snapshot"),
            choir.join("refs.snapshot"),
        )
        .map_err(|e| Refusal::fail(format!("place the attestation: {e}")))?;
    }

    let mut absent = Vec::new();
    for name in REQUIRED.iter().chain(OPTIONAL) {
        let from = backup.policy.join(name);
        if from.is_file() {
            std::fs::copy(&from, choir.join("policy").join(name))
                .map_err(|e| Refusal::fail(format!("place {name}: {e}")))?;
        } else if OPTIONAL.contains(name) {
            absent.push((*name).to_string());
        }
    }
    private(&choir)?;

    for repo in &backup.repos {
        let into = root.join(repo);
        if into.exists() {
            continue;
        }
        if let Some(parent) = into.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Refusal::fail(format!("create {}: {e}", parent.display())))?;
        }
        let Some(bundle) = bundle_for(&backup.src, repo) else {
            return Err(Refusal::fail(format!("no bundle for {repo}")));
        };
        let ok = std::process::Command::new("git")
            .args(["clone", "--bare", "--quiet"])
            .arg(&bundle)
            .arg(&into)
            .output()
            .is_ok_and(|out| out.status.success());
        if !ok {
            return Err(Refusal::fail(format!("could not unbundle {repo}")));
        }
        // A bundle clone leaves an `origin` pointing at the bundle file,
        // which would make the restored repository fetch from a path
        // that is about to be a temp directory on somebody's laptop.
        let _ = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(&into)
            .args(["remote", "remove", "origin"])
            .output();
    }
    Ok(absent)
}

/// 0700 on the state directory, 0600 on every policy file.
fn private(choir: &Path) -> Result<(), Refusal> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let set = |path: &Path, mode: u32| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                .map_err(|e| Refusal::fail(format!("chmod {}: {e}", path.display())))
        };
        set(choir, 0o700)?;
        if let Ok(entries) = std::fs::read_dir(choir.join("policy")) {
            for entry in entries.flatten() {
                set(&entry.path(), 0o600)?;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = choir;
    Ok(())
}

/// The two holes a backup deliberately does not fill.
///
/// Both are refusals rather than warnings: the rehearsal cannot run
/// without them, and a restore that has not been rehearsed has not been
/// done.
///
/// # Errors
///
/// Exit 3 when the signing key is absent and a fingerprint pins the
/// identity that wrote the log, or when there is no credential to serve
/// with. Returns the warning to print when there is neither key nor pin.
pub fn secrets(root: &Path, auth: &Path, ops: usize) -> Result<Option<String>, Refusal> {
    let choir = root.join(".choir");
    if !choir.join("node.key").is_file() {
        if choir.join("node.fingerprint").is_file() {
            return Err(Refusal::decide(format!(
                "the node's signing key is not here, and {} pins the identity that \
                 wrote this log.\n  Files are in place; the daemon will refuse to \
                 start until you choose:\n    (a) put the original 32-byte key at \
                 {} (chmod 600) and re-run this, or\n    (b) accept that the log \
                 changes author at this point:\n          rm {}\n        and \
                 re-run. Every op after the seam is signed by a different actor.\n  \
                 Option (b) is not reversible and not invisible: see \
                 docs/runbook-restore.md.",
                choir.join("node.fingerprint").display(),
                choir.join("node.key").display(),
                choir.join("node.fingerprint").display(),
            )));
        }
        // No key and no pin: either this log never had an author to
        // keep, or the operator has taken option (b) by deleting the
        // fingerprint. Nothing here can tell those apart, and refusing
        // both would leave (b) with no way forward at all — the key it
        // asks for is the one that is gone. So it proceeds, loudly.
        let warning = format!(
            "NO SIGNING KEY AND NO PIN — the daemon will mint a fresh key on the \
             start below.\n  Every op from seq {ops} on is signed by a different \
             actor than seq 0..{}.\n  Anyone holding the old fingerprint should be \
             told (docs/runbook-restore.md).",
            ops.saturating_sub(1)
        );
        if !auth.is_file() {
            return Err(no_credential(auth));
        }
        return Ok(Some(warning));
    }
    if !auth.is_file() {
        return Err(no_credential(auth));
    }
    Ok(None)
}

fn no_credential(auth: &Path) -> Refusal {
    Refusal::decide(format!(
        "no auth file at {}. Backups carry no credentials, so mint one now:\n    \
         printf '<operator>:%s\\n' \"$(openssl rand -hex 32)\" > {} && chmod 600 {}\n  \
         Replace <operator> with a username the restored ACL grants ownership of a \
         restored repository, then re-run. Reuse of the old token is not possible \
         and not wanted: it was last seen on a host you are restoring away from.",
        auth.display(),
        auth.display(),
        auth.display(),
    ))
}

/// A daemon started for the rehearsal, killed when this is dropped.
///
/// A `Drop` rather than a `kill` at each exit: every refusal below
/// returns early, and a rehearsal daemon left running holds the port and
/// keeps appending to the log an operator is about to inspect.
struct Rehearsal {
    child: std::process::Child,
    port: u16,
}

impl Drop for Rehearsal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts the restored node on a port it picks itself.
///
/// Port 0, so a rehearsal never collides with the node it is rehearsing
/// to replace. The wait is for the daemon's own marker rather than for a
/// duration: a slow machine is not a failed restore, and a dead process
/// is not a slow one.
fn boot(
    root: &Path,
    auth: &Path,
    daemon: &Path,
    work: &Path,
    backup: &Backup,
) -> Result<Rehearsal, Refusal> {
    let policy = root.join(".choir/policy");
    let mut args: Vec<String> = vec![
        root.display().to_string(),
        "0".to_string(),
        "--bind".to_string(),
        "127.0.0.1".to_string(),
        "--auth-file".to_string(),
        auth.display().to_string(),
        "--keys-file".to_string(),
        policy.join("keys").display().to_string(),
        "--reviewers-file".to_string(),
        policy.join("reviewers").display().to_string(),
        "--require-assignment".to_string(),
        "--require-scope".to_string(),
        "--read-only-browser".to_string(),
        "--journal".to_string(),
        root.join(".choir/journal.jsonl").display().to_string(),
        "--request-log".to_string(),
        root.join(".choir/requests.jsonl").display().to_string(),
        "--request-log-max-bytes".to_string(),
        "33554432".to_string(),
        "--rate-limit-api".to_string(),
        "120".to_string(),
        "--rate-limit-git".to_string(),
        "60".to_string(),
        "--quota-push-bytes".to_string(),
        "536870912".to_string(),
        "--quota-workspaces".to_string(),
        "8".to_string(),
        "--api-body-limit".to_string(),
        "1048576".to_string(),
        "--batch-limit".to_string(),
        "256".to_string(),
        "--ready-min-free-bytes".to_string(),
        "1073741824".to_string(),
    ];
    // Only where the file exists. A flag naming a path that is not there
    // is not a smaller policy, it is a daemon that does not start.
    //
    // Both halves or neither for the review gate: `--require-review`
    // without `--protected-refs` is refused by the daemon, because a
    // gate over nothing is worse than no gate. A backup from a node that
    // protects no ref restores into a node that protects no ref.
    for (name, flag, also) in [
        (
            "protected-refs",
            "--protected-refs",
            Some("--require-review"),
        ),
        ("newcomer-audit.jsonl", "--newcomer-audit", None),
        (
            "newcomer-adjudications.jsonl",
            "--newcomer-adjudications",
            None,
        ),
        ("review-adjudications.jsonl", "--review-adjudications", None),
        ("acl", "--acl-file", None),
    ] {
        let path = policy.join(name);
        if !path.is_file() {
            continue;
        }
        args.push(flag.to_string());
        args.push(path.display().to_string());
        if let Some(also) = also {
            args.push(also.to_string());
        }
    }
    for repo in &backup.repos {
        args.push("--create".to_string());
        args.push(repo.clone());
    }

    let err_path = work.join("node.err");
    let err = std::fs::File::create(&err_path)
        .map_err(|e| Refusal::fail(format!("create {}: {e}", err_path.display())))?;
    let out = std::fs::File::create(work.join("node.out"))
        .map_err(|e| Refusal::fail(format!("create node.out: {e}")))?;
    let mut child = std::process::Command::new(daemon)
        .args(&args)
        .stdout(out)
        .stderr(err)
        .spawn()
        .map_err(|e| Refusal::fail(format!("could not start {}: {e}", daemon.display())))?;

    let mut port = None;
    for _ in 0..600 {
        let text = std::fs::read_to_string(&err_path).unwrap_or_default();
        if let Some(found) = serving_port(&text) {
            port = Some(found);
            break;
        }
        // A dead process is not a slow one.
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
    let Some(port) = port else {
        let _ = child.kill();
        return Err(Refusal::fail(format!(
            "the restored node did not start. Its output:\n{stderr}"
        )));
    };
    let rehearsal = Rehearsal { child, port };

    // A retraction during a restore is the failure this whole ordering
    // exists to prevent, and it is loud rather than fatal-by-accident:
    // the log has already been appended to by the time it is printed.
    let retracted: Vec<&str> = stderr
        .lines()
        .filter(|l| l.starts_with("choir: retracted"))
        .collect();
    if !retracted.is_empty() {
        return Err(Refusal::fail(format!(
            "the restored node RETRACTED refs on start:\n{}\n  The log named \
             commits the repos do not hold. The restored log now has compensating \
             ops in it and is no longer the backup. Start again from the backup \
             into a clean root.",
            retracted.join("\n")
        )));
    }
    Ok(rehearsal)
}

/// The port out of the daemon's own start line.
///
/// Parsed rather than assumed because the rehearsal asks for port 0 and
/// only the daemon knows what it got.
#[must_use]
pub fn serving_port(stderr: &str) -> Option<u16> {
    stderr.lines().find_map(|line| {
        let rest = line.strip_prefix("choir-node serving ")?;
        let at = rest.rfind("http://")?;
        let host = rest[at + "http://".len()..]
            .split(|c: char| c == '/' || c.is_whitespace())
            .next()?;
        host.rsplit_once(':')?.1.parse().ok()
    })
}

/// One authenticated GET against the rehearsal node.
fn get(api: &str, credential: &str, path: &str) -> Option<serde_json::Value> {
    let out = std::process::Command::new("curl")
        .args(["-sS", "-u", credential])
        .arg(format!("{api}{path}"))
        .output()
        .ok()?;
    serde_json::from_slice(&out.stdout).ok()
}

/// The refs an attestation says this log ends at, in display form.
///
/// The attestation holds canonical [`choir_hash::ContentHash`] values
/// and the served view holds their display form, so the first is
/// projected into the second — through `ContentHash::to_hex` itself
/// rather than through a format string, because a comparison written
/// against imagined hex compares nothing at all.
///
/// # Errors
///
/// Returns a description when the file is not an attestation.
pub fn attested_refs(
    snapshot: &serde_json::Value,
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let refs = snapshot
        .get("refs")
        .and_then(serde_json::Value::as_object)
        .ok_or("no `refs` in the attestation")?;
    let mut out = std::collections::BTreeMap::new();
    for (name, value) in refs {
        let hash: choir_hash::ContentHash = serde_json::from_value(value.clone())
            .map_err(|e| format!("{name} is not a content hash: {e}"))?;
        out.insert(name.clone(), hash.to_hex());
    }
    Ok(out)
}

/// The refs a served view reports, in the same form.
#[must_use]
pub fn served_refs(view: &serde_json::Value) -> std::collections::BTreeMap<String, String> {
    view.get("refs")
        .and_then(serde_json::Value::as_object)
        .map(|refs| {
            refs.iter()
                .filter_map(|(name, value)| {
                    value.as_str().map(|hex| (name.clone(), hex.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Every difference between what was attested and what is served.
#[must_use]
pub fn ref_mismatches(
    attested: &std::collections::BTreeMap<String, String>,
    served: &std::collections::BTreeMap<String, String>,
) -> Vec<String> {
    let mut names: Vec<&String> = attested.keys().chain(served.keys()).collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter(|name| attested.get(*name) != served.get(*name))
        .map(|name| {
            format!(
                "  {name}: attested {}, serving {}",
                attested.get(name).map_or("nothing", String::as_str),
                served.get(name).map_or("nothing", String::as_str)
            )
        })
        .collect()
}

/// The whole restore: read, refuse, place, rehearse, prove.
///
/// # Errors
///
/// Every refusal above, plus the rehearsal's own: a node that serves no
/// log head, a view that disagrees with the attestation, a canary the
/// node refuses, a log that did not grow, an appended entry that does
/// not chain onto what the node replayed to, or a restored log that is
/// not a byte-exact continuation of the backup.
pub fn run(
    src: &Path,
    root: &Path,
    daemon: &Path,
    auth: &Path,
    say: &mut dyn FnMut(&str),
) -> Result<Restored, Refusal> {
    let work = scratch()?;
    let backup = read(src, &work, daemon)?;
    let resume = resuming(root, &backup)?;
    if resume {
        say(&format!(
            "resuming — {} is this backup, placed and not appended to",
            root.join(".choir/ops.jsonl").display()
        ));
    }
    let absent = place(root, &backup, resume)?;
    for name in &absent {
        say(&format!(
            "{name} is not in this backup — the restored node starts without it"
        ));
    }
    if let Some(warning) = secrets(root, auth, backup.ops)? {
        say(&warning);
    }

    let rehearsal = boot(root, auth, daemon, &work, &backup)?;
    let api = format!("http://127.0.0.1:{}", rehearsal.port);
    let credential = std::fs::read_to_string(auth)
        .map_err(|e| Refusal::fail(format!("read {}: {e}", auth.display())))?
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();

    // What the node replayed to, before anything is written. The claim
    // is checked below, where the canary's own `parent` has to be this
    // hash — an entry naming it is the log itself agreeing, rather than
    // the node being asked to confirm its own arithmetic.
    let view = get(&api, &credential, "/api/view")
        .ok_or_else(|| Refusal::fail("the restored node served no view"))?;
    let head_before = view["log"]["head"].as_str().unwrap_or_default().to_string();
    if head_before.is_empty() {
        return Err(Refusal::fail(
            "the restored node serves no log head; it did not replay the log it was given",
        ));
    }

    // The half of a restore a checksum cannot reach: the bytes can
    // arrive perfectly and still be replayed into a different view.
    let snapshot_path = root.join(".choir/refs.snapshot");
    if snapshot_path.is_file() {
        let snapshot: serde_json::Value = std::fs::read_to_string(&snapshot_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .ok_or_else(|| Refusal::fail("refs.snapshot is not readable JSON"))?;
        let attested = attested_refs(&snapshot).map_err(Refusal::fail)?;
        let served = served_refs(&view);
        let differences = ref_mismatches(&attested, &served);
        if !differences.is_empty() {
            return Err(Refusal::fail(format!(
                "the restored view does not match the backup's ref attestation:\n{}",
                differences.join("\n")
            )));
        }
        say(&format!(
            "view matches the attestation at seq {} ({} refs)",
            snapshot["at_seq"],
            attested.len()
        ));
    }

    let landed_in = canary(&backup, &work, &credential, rehearsal.port)?;
    let canary_ref = landed_in.1;
    let landed_in = landed_in.0;

    // The append happened, and it happened on top of what the node
    // replayed. An entry whose parent is the head read above is the
    // log's own statement that the restored bytes were folded to
    // exactly that point.
    let placed = root.join(".choir/ops.jsonl");
    let after = std::fs::read_to_string(&placed).unwrap_or_default();
    let lines: Vec<&str> = after.lines().collect();
    if lines.len() <= backup.ops {
        return Err(Refusal::fail(
            "the canary push returned success but the log did not grow. The repo \
             is being served without its pre-receive hook, so pushes bypass the \
             sequencer.",
        ));
    }
    let entry: serde_json::Value = serde_json::from_str(lines[backup.ops])
        .map_err(|e| Refusal::fail(format!("the appended entry is not JSON: {e}")))?;
    let parent: choir_hash::ContentHash = serde_json::from_value(entry["parent"].clone())
        .map_err(|e| Refusal::fail(format!("the appended entry has no parent hash: {e}")))?;
    if parent.to_hex() != head_before {
        return Err(Refusal::fail(format!(
            "the first appended entry chains onto {}, but the node served head \
             {head_before}: the replay and the log do not agree",
            parent.to_hex()
        )));
    }

    // Append-only, asserted rather than assumed: everything restored is
    // still byte-for-byte where it was, with the canary after it. A
    // restore that rewrote history would pass every other check here.
    let restored_bytes = std::fs::read(&placed).unwrap_or_default();
    let backup_bytes = std::fs::read(backup.src.join("ops.jsonl")).unwrap_or_default();
    if restored_bytes.len() < backup_bytes.len()
        || restored_bytes[..backup_bytes.len()] != backup_bytes[..]
    {
        return Err(Refusal::fail(
            "the restored log is not a byte-exact continuation of the backup: \
             something rewrote history",
        ));
    }

    drop(rehearsal);
    std::fs::remove_dir_all(&work).ok();
    Ok(Restored {
        ops: backup.ops,
        repos: backup.repos.len(),
        canary: canary_ref,
        landed_in,
    })
}

/// A real push over the real transport.
///
/// http-backend, the `pre-receive` hook, the sequencer, and an append to
/// the log that was just restored. Nothing short of that distinguishes a
/// node from a directory of files.
///
/// Returns the repository it landed in and the ref it created.
fn canary(
    backup: &Backup,
    work: &Path,
    credential: &str,
    port: u16,
) -> Result<(String, String), Refusal> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let canary = format!("refs/heads/restore-canary-{stamp}");
    for repo in &backup.repos {
        let url = format!("http://{credential}@127.0.0.1:{port}/{repo}");
        let clone = work.join("canary");
        std::fs::remove_dir_all(&clone).ok();
        let cloned = std::process::Command::new("git")
            .args(["clone", "--quiet", &url])
            .arg(&clone)
            .output()
            .is_ok_and(|out| out.status.success());
        if !cloned {
            continue;
        }
        // Any commit will do, and HEAD is not reliably one: a repository
        // rebuilt from a bundle keeps whatever HEAD the original had, so
        // one whose default branch is not among the restored refs clones
        // with an unborn HEAD and nothing checked out. Falling back to a
        // fetched branch is what keeps a restore that worked from
        // reporting that it proved nothing.
        let tip = git_line(&clone, &["rev-parse", "--verify", "--quiet", "HEAD"]).or_else(|| {
            git_line(
                &clone,
                &[
                    "for-each-ref",
                    "--count=1",
                    "--format=%(objectname)",
                    "refs/remotes/origin/",
                ],
            )
        });
        // An empty repository clones fine and has nothing to push. Not a
        // failure of this repository, only a reason to try the next.
        let Some(tip) = tip else { continue };
        let pushed = std::process::Command::new("git")
            .arg("-C")
            .arg(&clone)
            .args(["push", "--quiet", &url, &format!("{tip}:{canary}")])
            .output()
            .is_ok_and(|out| out.status.success());
        if !pushed {
            return Err(Refusal::fail(format!(
                "the canary push to {repo} was refused. The restored node serves, \
                 but it does not accept writes."
            )));
        }
        return Ok((repo.clone(), canary));
    }
    Err(Refusal::fail(
        "no restored repo has a commit to push, so nothing proved the node accepts \
         writes. This is not a pass.",
    ))
}

/// One line of git output, or nothing when git failed or said nothing.
fn git_line(dir: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!line.is_empty()).then_some(line)
}

/// A private work directory for the tar, the clone and the daemon's output.
fn scratch() -> Result<PathBuf, Refusal> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let at = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("choir-restore-{}-{at}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir)
        .map_err(|e| Refusal::fail(format!("create {}: {e}", dir.display())))?;
    Ok(dir)
}
