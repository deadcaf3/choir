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
