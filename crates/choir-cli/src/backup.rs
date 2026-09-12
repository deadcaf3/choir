//! `choir backup verify` — is this copy restorable?
//!
//! The mirror leg reports that it *wrote* a backup. This is the only
//! thing that reports the backup can be restored from, which is a
//! different claim and the one that matters on the day it is needed.
//!
//! Every check is local. It opens no connection, reads nothing from the
//! node, and does not care which machine it is run on — a backup you can
//! only verify by asking the thing it is a backup of is not a backup.
//!
//! # Why this is not the shell script it replaces
//!
//! `docs/runbook-restore.md` told a reader to run `./choirctl
//! verify-backup`, and the release ships `choir` and `choir-node` and
//! nothing else. The restore runbook — the page reached for on the worst
//! day — named a command that was not in the tarball.
//!
//! # Examples
//!
//! ```
//! use choir_cli::backup::REQUIRED_POLICY;
//!
//! // A backup without these cannot start the node it came from.
//! assert!(REQUIRED_POLICY.contains(&"keys"));
//! assert!(REQUIRED_POLICY.contains(&"repos.list"));
//! ```

use crate::doctor::{Check, Status};
use std::path::{Path, PathBuf};

/// The four files every backup has, and cannot be restored without.
pub const REQUIRED_FILES: &[&str] = &["ops.jsonl", "node.fingerprint", "policy.tar", "manifest"];

/// Policy files a restored node cannot start without.
pub const REQUIRED_POLICY: &[&str] = &["keys", "reviewers", "repos.list"];

/// Policy files a node may or may not have been started with.
///
/// Absent, they are reported and not fatal: a node with no ACL is a node
/// with no ACL, and a backup of it is complete without one. Reported at
/// all because "the ACL is missing from the backup" and "there was no
/// ACL" look identical in a restored directory, and only one of them is
/// a disaster.
pub const OPTIONAL_POLICY: &[&str] = &[
    "protected-refs",
    "newcomer-audit.jsonl",
    "newcomer-adjudications.jsonl",
    "review-adjudications.jsonl",
    "acl",
    "private-beta.manifest",
];

/// Names that must never be inside a backup.
///
/// A backup carries the log, the policy and the git objects. It does not
/// carry the node's key or anybody's credential, and one that does is a
/// copy of the node's identity sitting on whatever disk the backup lives
/// on. Fatal rather than a warning for that reason: the fix is to remove
/// it and re-take the backup, not to note it.
#[must_use]
pub fn is_secret(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    base == "auth" || base == "node.key" || base.ends_with(".key") || base.ends_with(".pem")
}

/// Runs `program` and returns its stdout when it succeeded.
fn output(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// The SHA-256 of a file, as lowercase hex.
///
/// `openssl`, not a crate: the manifest is written in SHA-256 by the
/// script that takes the backup, this workspace adds dependencies
/// reluctantly, and `openssl` is already required by `choir doctor` for
/// every other digest and signature it handles.
fn sha256(path: &Path) -> Option<String> {
    let text = output(
        "openssl",
        &["dgst", "-sha256", "-r", &path.display().to_string()],
    )?;
    // `-r` is the coreutils-shaped form: `<hex> *<path>`.
    text.split_whitespace().next().map(str::to_string)
}

/// One `key value` line out of the manifest.
#[must_use]
pub fn manifest_value(manifest: &str, key: &str) -> Option<String> {
    manifest.lines().find_map(|line| {
        let (found, value) = line.trim().split_once(char::is_whitespace)?;
        (found == key).then(|| value.trim().to_string())
    })
}

/// The members of a tar archive, by name.
fn tar_members(path: &Path) -> Option<Vec<String>> {
    let text = output("tar", &["tf", &path.display().to_string()])?;
    Some(
        text.lines()
            .map(|line| line.trim_start_matches("./").to_string())
            .filter(|line| !line.is_empty())
            .collect(),
    )
}

/// Checks one backup directory.
///
/// `daemon` is the `choir-node` that walks the hash chain. Without one
/// the chain check warns rather than failing: every other check here is
/// still worth having, and a machine holding a backup is not necessarily
/// a machine that runs nodes.
#[must_use]
pub fn verify(dir: &Path, daemon: Option<&Path>) -> Vec<Check> {
    let mut checks = Vec::new();

    // 1. The four files, present and not empty. A zero-byte ops.jsonl is
    //    a backup that ran and captured nothing, which reads as success
    //    everywhere except here.
    let mut missing = Vec::new();
    for name in REQUIRED_FILES {
        let path = dir.join(name);
        match std::fs::metadata(&path) {
            Ok(meta) if meta.len() > 0 => {}
            Ok(_) => missing.push(format!("{name} (empty)")),
            Err(_) => missing.push((*name).to_string()),
        }
    }
    if missing.is_empty() {
        checks.push(Check::pass(
            "files",
            format!("{} present", REQUIRED_FILES.len()),
        ));
    } else {
        checks.push(
            Check::fail("files", missing.join(", "))
                .with_fix("this is not a complete backup; take another"),
        );
        // Everything below reads these. Stopping here says one true
        // thing instead of eight consequences of it.
        return checks;
    }

    let manifest = std::fs::read_to_string(dir.join("manifest")).unwrap_or_default();
    let ops = dir.join("ops.jsonl");

    // 2. The log is the bytes the manifest says it is.
    match (manifest_value(&manifest, "ops_sha256"), sha256(&ops)) {
        (Some(want), Some(got)) if want == got => {
            checks.push(Check::pass("checksum", format!("ops.jsonl {}", &got[..16])));
        }
        (Some(want), Some(got)) => checks.push(
            Check::fail(
                "checksum",
                format!(
                    "manifest says {}, file is {}",
                    &want[..16.min(want.len())],
                    &got[..16]
                ),
            )
            .with_fix("the log was modified or truncated after it was written"),
        ),
        (None, _) => checks.push(Check::fail("checksum", "no ops_sha256 in the manifest")),
        (_, None) => checks.push(Check::warn("checksum", "openssl could not read the log")),
    }

    // 3. The chain, from the binary that defines it.
    match daemon {
        Some(daemon) => {
            let ok = std::process::Command::new(daemon)
                .args(["--verify-log", &ops.display().to_string()])
                .output()
                .is_ok_and(|out| out.status.success());
            if ok {
                checks.push(Check::pass("chain", "format, sequence and hashes verify"));
            } else {
                checks.push(
                    Check::fail("chain", "choir-node --verify-log refused this log")
                        .with_fix("choir repair <log> --verify says where it breaks"),
                );
            }
        }
        None => checks.push(
            Check::warn("chain", "no choir-node to walk it with")
                .with_fix("cargo build --release -p choir-node"),
        ),
    }

    // 4. The attestation, if this node kept one.
    if dir.join("refs.snapshot").is_file() {
        checks.push(Check::pass("attestation", "refs.snapshot present"));
    } else {
        checks.push(Check::warn(
            "attestation",
            "no refs.snapshot — a restore cannot check the refs it serves against one",
        ));
    }

    // 5-7. The policy archive.
    match tar_members(&dir.join("policy.tar")) {
        None => checks.push(Check::fail("policy", "policy.tar cannot be listed")),
        Some(members) => {
            let held = |name: &str| members.iter().any(|m| m == name);
            let absent: Vec<&str> = REQUIRED_POLICY
                .iter()
                .copied()
                .filter(|n| !held(n))
                .collect();
            if absent.is_empty() {
                checks.push(Check::pass("policy", format!("{} files", members.len())));
            } else {
                checks.push(
                    Check::fail("policy", format!("missing {}", absent.join(", ")))
                        .with_fix("a node restored without these cannot start"),
                );
            }
            let optional: Vec<&str> = OPTIONAL_POLICY
                .iter()
                .copied()
                .filter(|n| !held(n))
                .collect();
            if !optional.is_empty() {
                checks.push(Check::warn(
                    "policy (optional)",
                    format!(
                        "not in this backup: {} — either the node had none, or they were lost",
                        optional.join(", ")
                    ),
                ));
            }
            let leaked: Vec<&String> = members.iter().filter(|m| is_secret(m)).collect();
            if leaked.is_empty() {
                checks.push(Check::pass("secrets", "no key or credential in the backup"));
            } else {
                checks.push(
                    Check::fail(
                        "secrets",
                        format!(
                            "{} in policy.tar",
                            leaked
                                .iter()
                                .map(|s| s.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    )
                    .with_fix("remove it and take the backup again; this copy holds an identity"),
                );
            }
        }
    }

    // 8. Every bundle git will actually open.
    checks.push(bundle_check(dir));

    // 9. What this backup is, so a stale one is visible as stale.
    let age = std::fs::metadata(dir.join("ops.jsonl"))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|d| format!("{} hours old", d.as_secs() / 3600))
        .unwrap_or_else(|| "age unknown".to_string());
    let seq = manifest_value(&manifest, "next_seq").unwrap_or_else(|| "?".to_string());
    checks.push(Check::pass("taken", format!("{age}, next seq {seq}")));

    checks
}

/// A throwaway empty repository to verify bundles against.
///
/// `git bundle verify` refuses to run outside a repository, so without
/// one the answer depends on where the reader happened to be standing —
/// the check passed from a checkout and failed from a backup directory,
/// which is the one place somebody verifying a backup actually stands.
///
/// Empty on purpose, and not merely as a convenience. Verifying against
/// a repository that already holds the history proves nothing about the
/// bundle; verifying against one that holds nothing proves the bundle
/// records a complete history, which is exactly what a restore needs
/// and what the shell version never checked.
fn scratch_repo() -> Option<PathBuf> {
    // Process id *and* a counter. The pid alone is unique between
    // `choir` invocations and not within one, and the test harness runs
    // every module in a single process on parallel threads — so two
    // verifications shared a directory and one deleted it while the
    // other was still verifying against it.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let at = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("choir-bundle-check-{}-{at}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let ok = std::process::Command::new("git")
        .args(["init", "--bare", "-q", &dir.display().to_string()])
        .output()
        .is_ok_and(|out| out.status.success());
    ok.then_some(dir)
}

/// Every `.bundle` under `repos/`, as git sees it.
///
/// A backup with no bundle is a backup of the log alone: the operations
/// are all there and the git objects they name are not, so a restore
/// produces a node that agrees about history and can serve none of it.
fn bundle_check(dir: &Path) -> Check {
    let repos = dir.join("repos");
    let mut bundles: Vec<PathBuf> = Vec::new();
    let mut stack = vec![repos];
    while let Some(at) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&at) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "bundle") {
                bundles.push(path);
            }
        }
    }
    if bundles.is_empty() {
        return Check::fail("bundles", "no git bundle in repos/")
            .with_fix("the log would restore, but every repository would be empty");
    }
    let Some(scratch) = scratch_repo() else {
        return Check::warn(
            "bundles",
            format!("{} found, git could not check them", bundles.len()),
        );
    };
    let bad: Vec<String> = bundles
        .iter()
        .filter(|path| {
            !std::process::Command::new("git")
                .args([
                    "--git-dir",
                    &scratch.display().to_string(),
                    "bundle",
                    "verify",
                    &path.display().to_string(),
                ])
                .output()
                .is_ok_and(|out| out.status.success())
        })
        .map(|path| {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        })
        .collect();
    std::fs::remove_dir_all(&scratch).ok();
    if bad.is_empty() {
        Check::pass(
            "bundles",
            format!("{} verify, complete history", bundles.len()),
        )
    } else {
        Check::fail("bundles", format!("git refuses {}", bad.join(", ")))
            .with_fix("an incomplete bundle restores a repository missing its own history")
    }
}

/// Whether the whole backup is restorable.
///
/// A warning never makes it unrestorable — that is the difference
/// between "this node had no ACL" and "this backup lost the ACL", and
/// only the second is a failure.
#[must_use]
pub fn restorable(checks: &[Check]) -> bool {
    Status::worst(checks) != Status::Fail
}

// ---------------------------------------------------------------- taking one

/// Every policy file a backup carries, required and optional.
fn policy_names() -> Vec<&'static str> {
    REQUIRED_POLICY
        .iter()
        .chain(OPTIONAL_POLICY.iter())
        .copied()
        .collect()
}

/// The SHA-256 of some bytes, as lowercase hex, through `openssl`'s stdin.
fn sha256_bytes(bytes: &[u8]) -> Option<String> {
    use std::io::Write;
    let mut child = std::process::Command::new("openssl")
        .args(["dgst", "-sha256", "-r"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(bytes).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .map(str::to_string)
}

/// `YYYYMMDDTHHMMSSZ`, now, from the system clock and arithmetic.
///
/// No date crate: the manifest wants one sortable stamp, and the civil
/// date from a day count is twenty lines that have not changed since
/// 1582.
#[must_use]
pub fn utc_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
}

/// What `take` did, for the caller to print.
#[derive(Debug, Default)]
pub struct Taken {
    /// Where the backup now is.
    pub dest: PathBuf,
    /// Entries in the log copied.
    pub ops: usize,
    /// The position the log will fill next.
    pub next_seq: u64,
    /// Bytes the log grew by since the previous backup here, if there was one.
    pub grew: Option<(u64, u64)>,
    /// One line per repository: kept unchanged, or bundled afresh.
    pub bundles: Vec<String>,
    /// Things worth a line that did not stop the backup.
    pub warnings: Vec<String>,
}

/// Runs git against one bare repository and returns its stdout.
fn git_in(bare: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(bare)
        .args(args)
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

/// Takes a backup of the node whose state directory is `layout`, into
/// `dest`, in the shape [`verify`] reads and `choir backup restore`
/// unpacks.
///
/// The same checks the flip-era pull script made, on the node's own
/// disk: the copied log must be a prefix-extension of the one already
/// held here, contiguous from seq 0, and pass `choir-node --verify-log`
/// when a daemon is there to run it; the policy archive must carry no
/// key or credential; every bundle must verify. Everything is written
/// into a sibling directory first and moved into place last, so a run
/// cut short leaves the previous backup as it was.
///
/// Live is fine: the log is append-only, and a line the daemon was
/// still writing fails the contiguity check and is refused, which is
/// the right answer for a copy taken a moment too early.
///
/// # Errors
///
/// Returns the sentence to print when the node's files are missing, the
/// log shrank or diverged from the backup already here, a secret is
/// among the policy files, or a subprocess refused.
pub fn take(
    layout: &crate::serve::Layout,
    dest: &Path,
    daemon: Option<&Path>,
) -> Result<Taken, String> {
    let node_state = layout.repos.join(".choir");
    let ops_src = node_state.join("ops.jsonl");
    let fingerprint_src = node_state.join("node.fingerprint");
    for (what, path) in [("op log", &ops_src), ("node fingerprint", &fingerprint_src)] {
        match std::fs::metadata(path) {
            Ok(meta) if meta.len() > 0 => {}
            Ok(_) => return Err(format!("the {what} at {} is empty", path.display())),
            Err(_) => {
                return Err(format!(
                    "no {what} at {}\n\n  is {} a node's state directory? choir node status",
                    path.display(),
                    layout.state.display()
                ))
            }
        }
    }

    let stamp = utc_stamp();
    let incoming = dest.join(format!(".incoming-{stamp}"));
    std::fs::create_dir_all(incoming.join("repos"))
        .map_err(|e| format!("create {}: {e}", incoming.display()))?;
    // Removed on every exit from here, success included: on success the
    // files have been moved out of it.
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }
    let _cleanup = Cleanup(incoming.clone());
    let mut taken = Taken {
        dest: dest.to_path_buf(),
        ..Taken::default()
    };

    // 1. The log, and whether it extends what is held here.
    let ops = std::fs::read(&ops_src).map_err(|e| format!("read {}: {e}", ops_src.display()))?;
    let held = std::fs::read(dest.join("ops.jsonl")).ok();
    if let Some(held) = &held {
        if ops.len() < held.len() {
            return Err(format!(
                "the node's log is SHORTER than the copy held here: held {} bytes, node has {}\n  \
                 the log was rewritten, not appended to; keeping the existing backup",
                held.len(),
                ops.len()
            ));
        }
        if &ops[..held.len()] != held.as_slice() {
            return Err(
                "the copy held here is NOT a prefix of the node's log: the history diverged\n  \
                 keeping the existing backup and refusing this one"
                    .to_string(),
            );
        }
        taken.grew = Some((held.len() as u64, ops.len() as u64));
    }
    // 2. Contiguous from 0, every line a record with a seq.
    let mut expected = 0u64;
    for (at, line) in ops.split(|b| *b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let seq = serde_json::from_slice::<serde_json::Value>(line)
            .ok()
            .and_then(|v| v["seq"].as_u64())
            .ok_or_else(|| {
                format!(
                    "line {} of the log carries no seq; taken mid-write? try again",
                    at + 1
                )
            })?;
        if seq != expected {
            return Err(format!(
                "the log is not contiguous: seq {expected} expected, {seq} found"
            ));
        }
        expected += 1;
        taken.ops += 1;
    }
    taken.next_seq = expected;
    let ops_copy = incoming.join("ops.jsonl");
    std::fs::write(&ops_copy, &ops).map_err(|e| format!("write {}: {e}", ops_copy.display()))?;
    let ops_sha = sha256(&ops_copy).ok_or("openssl could not digest the log")?;
    // 3. The chain, from the binary that defines it.
    match daemon {
        Some(daemon) => {
            let out = std::process::Command::new(daemon)
                .args(["--verify-log", &ops_copy.display().to_string()])
                .output()
                .map_err(|e| format!("run {}: {e}", daemon.display()))?;
            if !out.status.success() {
                return Err(format!(
                    "choir-node --verify-log refused the log:\n  {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
        }
        None => taken
            .warnings
            .push("chain unverified: no choir-node beside choir to walk it with".to_string()),
    }

    // 4. Identity and attestation.
    std::fs::copy(&fingerprint_src, incoming.join("node.fingerprint"))
        .map_err(|e| format!("copy node.fingerprint: {e}"))?;
    let snapshot_src = node_state.join("refs.snapshot");
    let has_snapshot = match std::fs::metadata(&snapshot_src) {
        Ok(meta) if meta.len() > 0 => {
            std::fs::copy(&snapshot_src, incoming.join("refs.snapshot"))
                .map_err(|e| format!("copy refs.snapshot: {e}"))?;
            true
        }
        _ => {
            taken.warnings.push(
                "no attestation on the node: a restore from this backup cannot check the view it replays into".to_string(),
            );
            false
        }
    };

    // 5. Policy, and nothing that is not policy.
    let present: Vec<&str> = policy_names()
        .into_iter()
        .filter(|name| layout.state.join(name).exists())
        .collect();
    if present.is_empty() {
        return Err(format!(
            "no policy files in {}: nothing to ship",
            layout.state.display()
        ));
    }
    for name in policy_names() {
        if !present.contains(&name) {
            taken.warnings.push(format!(
                "no {name} on the node: a restore starts without it"
            ));
        }
    }
    let leaked: Vec<&&str> = present.iter().filter(|n| is_secret(n)).collect();
    if !leaked.is_empty() {
        return Err(format!("refusing: a policy name is a secret: {leaked:?}"));
    }
    let policy_tar = incoming.join("policy.tar");
    let ok = std::process::Command::new("tar")
        .arg("cf")
        .arg(&policy_tar)
        .arg("-C")
        .arg(&layout.state)
        .args(&present)
        .status()
        .map_err(|e| format!("run tar: {e}"))?
        .success();
    if !ok {
        return Err("tar could not build the policy archive".to_string());
    }

    // 6. One bundle per repository, kept when its refs have not moved.
    let listed = std::fs::read_to_string(layout.state.join("repos.list")).unwrap_or_default();
    let repos: Vec<&str> = listed
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    for repo in &repos {
        let bare = layout.repos.join(repo);
        let name = Path::new(repo)
            .file_name()
            .map(|n| n.to_string_lossy().trim_end_matches(".git").to_string())
            .unwrap_or_else(|| (*repo).to_string());
        let refs = git_in(&bare, &["show-ref"]).unwrap_or_default();
        let refs_hash = sha256_bytes(&refs).ok_or("openssl could not digest the refs")?;
        let held_hash = std::fs::read_to_string(dest.join("repos").join(format!("{name}.refs")))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let held_bundle = dest.join("repos").join(format!("{name}.bundle"));
        let bundle = incoming.join("repos").join(format!("{name}.bundle"));
        if held_hash == refs_hash && held_bundle.is_file() {
            std::fs::copy(&held_bundle, &bundle).map_err(|e| format!("keep {name}.bundle: {e}"))?;
            taken.bundles.push(format!("{name}: unchanged, kept"));
        } else {
            if git_in(
                &bare,
                &["bundle", "create", &bundle.display().to_string(), "--all"],
            )
            .is_none()
            {
                return Err(format!("git could not bundle {repo} at {}", bare.display()));
            }
            let size = std::fs::metadata(&bundle).map(|m| m.len()).unwrap_or(0);
            taken.bundles.push(format!("{name}: bundled, {size} bytes"));
        }
        std::fs::write(
            incoming.join("repos").join(format!("{name}.refs")),
            format!("{refs_hash}\n"),
        )
        .map_err(|e| format!("write {name}.refs: {e}"))?;
    }
    if repos.is_empty() {
        taken
            .warnings
            .push("repos.list names no repository: no bundle taken".to_string());
    }

    // 7. The manifest, then everything into place, the log last so a
    //    backup is never a new manifest over an old log.
    std::fs::write(
        incoming.join("manifest"),
        format!(
            "format_version 1\npulled_at {stamp}\nops_sha256 {ops_sha}\nops_bytes {}\nnext_seq {}\nrepos {}\n",
            ops.len(),
            taken.next_seq,
            repos.len()
        ),
    )
    .map_err(|e| format!("write manifest: {e}"))?;
    std::fs::create_dir_all(dest.join("repos"))
        .map_err(|e| format!("create {}: {e}", dest.display()))?;
    for entry in
        std::fs::read_dir(incoming.join("repos")).map_err(|e| format!("list bundles: {e}"))?
    {
        let entry = entry.map_err(|e| format!("list bundles: {e}"))?;
        std::fs::rename(entry.path(), dest.join("repos").join(entry.file_name()))
            .map_err(|e| format!("move {}: {e}", entry.path().display()))?;
    }
    for name in ["node.fingerprint", "policy.tar", "manifest", "ops.jsonl"] {
        std::fs::rename(incoming.join(name), dest.join(name))
            .map_err(|e| format!("move {name}: {e}"))?;
    }
    if has_snapshot {
        std::fs::rename(incoming.join("refs.snapshot"), dest.join("refs.snapshot"))
            .map_err(|e| format!("move refs.snapshot: {e}"))?;
    } else {
        std::fs::remove_file(dest.join("refs.snapshot")).ok();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o700)).ok();
    }
    Ok(taken)
}
