//! Exporting a node root, importing one, and settling what an export
//! claims (§E).
//!
//! The plan's standing rule for a one-way door is that no persisted
//! format ships without three things: a version field, a written
//! evolution policy, and export/import tooling. The op log has had the
//! first two since its first byte — every entry carries
//! `format_version`, and [`crate::platform`] documents the additive
//! rule. This is the third, and it is deliberately a *format* tool
//! rather than a deployment one.
//!
//! That distinction is the reason this exists beside `scripts/pull_backup.sh`
//! instead of replacing it. That script is disaster recovery for one
//! deployment: it pulls over ssh from a fixed remote path, refuses to run
//! on the machine that holds the log, takes its repository list from a
//! policy file, needs python3 to project a hash, and checks the bundles
//! against the D25 attestation. All of that is right for the job it does
//! and none of it travels. Here there is no network, no ssh key, no
//! `repos.list`, and no interpreter: a directory goes in, a directory
//! comes out, and the log itself says what the export must contain.
//!
//! # What an export claims, and how the claim is settled
//!
//! An export is not a pile of files, it is an assertion with two halves
//! that can disagree: *this op log* and *these repositories* describe the
//! same node. [`verify`] settles it by folding the log into a
//! [`choir_view::View`] and requiring every ref the view names to be
//! present in that repository's bundle at the same oid.
//!
//! The check is deliberately one-directional. A bundle may carry refs the
//! log does not name; a log may not name refs no bundle carries. That
//! asymmetry is what makes exporting a *running* node meaningful: the log
//! is read first and the bundles after, so a push landing mid-export puts
//! the bundles ahead, which is a state the export can honestly describe.
//! The reverse — the log naming a commit no bundle holds — is the failure
//! that matters, because restoring it produces a node whose view points
//! at objects it does not have, and startup reconciliation answers that
//! by appending signed retractions of exactly the refs being restored.
//! [`Report::ahead`] counts the benign direction rather than hiding it,
//! so an operator can see drift instead of inferring it.
//!
//! # What an export does not contain
//!
//! Secrets, by construction and then by inspection. Only four names are
//! ever copied out of `.choir`, and the finished directory is walked
//! again afterwards and refused if it holds anything matching
//! [`is_secret`] — so the guarantee is a property of the output rather
//! than of the care taken while writing it. The node's signing key stays
//! with the node that owns it, and `docs/runbook-restore.md` covers what
//! its absence means.
//!
//! Policy files are also absent, and that is not an oversight worth
//! quietly tolerating: `--acl-file`, `--auth-file`, `--keys-file` and the
//! rest name paths anywhere on the host, so a root does not know where
//! they are and an export taken from one cannot honestly claim to hold
//! them. The manifest says so in a field rather than leaving the reader
//! to notice. Provisioned workspaces and checkouts under `.choir` are
//! left behind too, being derived from refs the export does carry.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Version of the export directory's own layout, carried in the manifest.
///
/// Separate from the op log's `format_version`, which the manifest also
/// records: a change to how this directory is arranged is not a change to
/// the entries inside it, and conflating the two would make either
/// version bump imply the other.
pub const FORMAT_VERSION: u32 = 1;

/// Whether a file name is one an export must never carry.
///
/// Named rather than enumerated: `--tls-key`, a per-actor key and a
/// GitHub App PEM are all secrets that no list of literal file names
/// would keep up with, so the rule is the shape of the name. `auth` is
/// the one literal, being the bearer-token table.
#[must_use]
pub fn is_secret(name: &str) -> bool {
    name == "auth" || name.ends_with(".key") || name.ends_with(".pem")
}

/// What an export holds, counted from the files rather than claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Op-log records the export carries.
    pub records: u64,
    /// The log's head hash as hex, or `None` for an empty log.
    pub head: Option<String>,
    /// Repositories the export carries a bundle for.
    pub bundles: usize,
    /// Refs the log names that a bundle carries at the same oid.
    pub refs: usize,
    /// Refs a bundle carries that the log does not name.
    ///
    /// Benign by itself — see the module docs on why the check runs in
    /// one direction — and reported so that drift is visible.
    pub ahead: usize,
}

/// Writes an export of `root` into `dest`, then verifies its own output.
///
/// `dest` must not already exist. An export never writes into a
/// directory it did not create, for the same reason
/// `scripts/restore_from_backup.sh` never writes over a log: the thing
/// being overwritten is the fallback.
///
/// # Errors
///
/// Returns a description when `root` holds no op log, when that log does
/// not verify, when a repository the log names refs for cannot be
/// bundled, or when the finished directory fails [`verify`].
pub fn export(root: &Path, dest: &Path) -> Result<Report, String> {
    let state = root.join(".choir");
    let log = state.join("ops.jsonl");
    if !log.is_file() {
        return Err(format!(
            "{} holds no op log at .choir/ops.jsonl; a node started without \
             --keys-file runs no sequencer and writes none",
            root.display()
        ));
    }
    if dest.exists() {
        return Err(format!(
            "{} already exists; an export writes a new directory rather than into one",
            dest.display()
        ));
    }

    // The log is read before a single bundle is taken, which is what
    // puts a concurrent push in the benign direction rather than the
    // failing one. See the module docs.
    let chain = chain_report(&log)?;
    let refs = log_refs(&log)?;

    std::fs::create_dir_all(dest.join("repos")).map_err(|e| format!("create {dest:?}: {e}"))?;
    copy(&log, &dest.join("ops.jsonl"))?;
    for name in ["node.fingerprint", "refs.snapshot"] {
        let from = state.join(name);
        if from.is_file() {
            copy(&from, &dest.join(name))?;
        }
    }

    let mut rows = Vec::new();
    for repo in repos(root)? {
        let dir = root.join(&repo);
        let bundle = dest.join("repos").join(format!("{repo}.bundle"));
        if let Some(parent) = bundle.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create {parent:?}: {e}"))?;
        }
        // A repository with no refs cannot be bundled: git refuses
        // rather than writing a zero-ref bundle. Recording it as an
        // empty row keeps the export's repository list complete, which
        // is what lets an import recreate a repo somebody made and has
        // not pushed to yet.
        let bundled = !git(&["for-each-ref", "--format=%(refname)"], &dir)?
            .trim()
            .is_empty();
        if bundled {
            let path = bundle.to_str().ok_or("bundle path is not utf-8")?;
            git(&["bundle", "create", path, "--all"], &dir)?;
        }
        rows.push((repo, bundled));
    }

    let manifest = serde_json::json!({
        "format_version": FORMAT_VERSION,
        "log_format_version": choir_oplog::FORMAT_VERSION,
        "records": chain.0,
        "head": chain.1,
        "repos": rows.iter().map(|(name, bundled)| serde_json::json!({
            "name": name,
            "bundle": bundled.then(|| format!("repos/{name}.bundle")),
            "refs": refs.get(name).map_or(0, BTreeMap::len),
        })).collect::<Vec<_>>(),
        "policy_files": "not included: named by flags and stored outside the root",
        "secrets": "not included: see docs/runbook-restore.md",
    });
    std::fs::write(
        dest.join("manifest.json"),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?
        ),
    )
    .map_err(|e| format!("write manifest: {e}"))?;

    // An export that does not verify is not an export. Running the
    // reader over the writer's output is the only thing that makes the
    // two agree about the format a year from now.
    verify(dest)
}

/// Settles what an export at `dir` claims: the log verifies, the manifest
/// describes it, and every ref the log names is in a bundle at that oid.
///
/// # Errors
///
/// Returns a description naming the first disagreement found.
pub fn verify(dir: &Path) -> Result<Report, String> {
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.join("manifest.json"))
            .map_err(|e| format!("{}: no manifest.json ({e})", dir.display()))?,
    )
    .map_err(|e| format!("manifest.json does not parse: {e}"))?;
    let version = manifest
        .get("format_version")
        .and_then(serde_json::Value::as_u64);
    if version != Some(u64::from(FORMAT_VERSION)) {
        return Err(format!(
            "export format_version is {}, this build reads {FORMAT_VERSION}",
            version.map_or_else(|| "absent".to_string(), |v| v.to_string())
        ));
    }

    let log = dir.join("ops.jsonl");
    if !log.is_file() {
        return Err("export holds no ops.jsonl".to_string());
    }
    let (records, head) = chain_report(&log)?;
    if manifest.get("records").and_then(serde_json::Value::as_u64) != Some(records) {
        return Err(format!(
            "manifest claims {} records, the log verifies {records}",
            manifest["records"]
        ));
    }
    if manifest.get("head").and_then(serde_json::Value::as_str) != head.as_deref() {
        return Err(format!(
            "manifest claims head {}, the log ends at {}",
            manifest["head"],
            head.as_deref().unwrap_or("nothing")
        ));
    }

    // Every name in the manifest, so that a repository dropped from the
    // export is caught even when the log names no ref for it.
    let listed = rows(&manifest)?;

    let (mut matched, mut ahead, mut bundles) = (0usize, 0usize, 0usize);
    let refs = log_refs(&log)?;
    for (repo, wanted) in &refs {
        if !listed.contains_key(repo) {
            return Err(format!(
                "the log names {} refs for {repo}, which the manifest does not list",
                wanted.len()
            ));
        }
    }
    for (repo, bundled) in &listed {
        let empty = BTreeMap::new();
        let wanted = refs.get(repo).unwrap_or(&empty);
        if !bundled {
            if wanted.is_empty() {
                continue;
            }
            return Err(format!(
                "the log names {} refs for {repo}, which the export carries no bundle for",
                wanted.len()
            ));
        }
        bundles += 1;
        let path = dir.join("repos").join(format!("{repo}.bundle"));
        if !path.is_file() {
            return Err(format!(
                "{repo}: the manifest names a bundle that is not here"
            ));
        }
        let held = bundle_heads(&path)?;
        for (name, oid) in wanted {
            match held.get(name) {
                Some(found) if found == oid => matched += 1,
                Some(found) => {
                    return Err(format!(
                        "{repo}: the log has {name} at {oid}, the bundle has it at {found}"
                    ))
                }
                None => {
                    return Err(format!(
                        "{repo}: the log names {name} at {oid}, the bundle does not carry it"
                    ))
                }
            }
        }
        ahead += held
            .keys()
            .filter(|name| !wanted.contains_key(*name))
            .count();
    }

    for found in walk(dir)? {
        let name = found
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        if is_secret(&name) {
            return Err(format!(
                "{} is a secret and an export must not carry one",
                found.display()
            ));
        }
    }

    Ok(Report {
        records,
        head,
        bundles,
        refs: matched,
        ahead,
    })
}

/// Places the export at `dir` into a fresh node `root`.
///
/// Verifies before writing a byte, then refuses a root that already
/// holds a log or any repository the manifest names — the thing being
/// written over is the fallback, which is the same reason
/// `scripts/restore_from_backup.sh` refuses both.
///
/// What this does *not* do is claim the result works. A restored node
/// that serves is not a restored node; what settles it is one that
/// accepts a write, and the secrets that boot needs are deliberately not
/// here. `docs/runbook-restore.md` covers them. Hooks and git config are
/// not written either: the daemon adopts a repository it finds under its
/// root, so writing them here would be a second copy of that rule, drifting.
///
/// # Errors
///
/// Returns a description when the export does not verify, when `root`
/// already holds a log or a named repository, or when git refuses to
/// unbundle one.
pub fn import(dir: &Path, root: &Path) -> Result<Report, String> {
    let report = verify(dir)?;
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.join("manifest.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let listed = rows(&manifest)?;

    let state = root.join(".choir");
    if state.join("ops.jsonl").exists() {
        return Err(format!(
            "{} already holds a log: import into a fresh root, or move the existing \
             log aside first, because nothing here overwrites one",
            state.display()
        ));
    }
    for name in listed.keys() {
        if root.join(name).exists() {
            return Err(format!(
                "{} already exists: import into a fresh root",
                root.join(name).display()
            ));
        }
    }

    std::fs::create_dir_all(&state).map_err(|e| format!("create {state:?}: {e}"))?;
    copy(&dir.join("ops.jsonl"), &state.join("ops.jsonl"))?;
    for name in ["node.fingerprint", "refs.snapshot"] {
        let from = dir.join(name);
        if from.is_file() {
            copy(&from, &state.join(name))?;
        }
    }

    for (name, bundled) in &listed {
        let target = root.join(name);
        let target = target.to_str().ok_or("repository path is not utf-8")?;
        if *bundled {
            let bundle = dir.join("repos").join(format!("{name}.bundle"));
            let bundle = bundle.to_str().ok_or("bundle path is not utf-8")?;
            git(
                &["clone", "--bare", "--quiet", bundle, target],
                Path::new("."),
            )?;
            // A bundle clone leaves an `origin` pointing at the bundle
            // file, which will not be there once the export is gone.
            git(&["remote", "remove", "origin"], Path::new(target))?;
        } else {
            git(&["init", "--bare", "--quiet", target], Path::new("."))?;
        }
    }
    Ok(report)
}

/// The manifest's repository list as `name -> has a bundle`, with every
/// name checked before it is ever joined to a path.
///
/// The check is here rather than at each use because both halves take
/// this list from a directory somebody else produced: a name of `../..`
/// would otherwise write outside the root being imported into, and read
/// outside the export being verified.
fn rows(manifest: &serde_json::Value) -> Result<BTreeMap<String, bool>, String> {
    let mut out = BTreeMap::new();
    for row in manifest
        .get("repos")
        .and_then(serde_json::Value::as_array)
        .ok_or("manifest names no repository list")?
    {
        let name = row
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or("a manifest row names no repository")?;
        if !name.ends_with(".git")
            || name.starts_with('/')
            || name.split('/').any(|part| part.is_empty() || part == "..")
        {
            return Err(format!(
                "manifest names `{name}`, which is not a repository path"
            ));
        }
        out.insert(
            name.to_string(),
            !row.get("bundle").is_none_or(serde_json::Value::is_null),
        );
    }
    Ok(out)
}

/// Verifies the chain and returns `(records, head-hex)`.
fn chain_report(log: &Path) -> Result<(u64, Option<String>), String> {
    let report =
        choir_oplog::repair::verify(log).map_err(|e| format!("{}: {e:?}", log.display()))?;
    if let Some(fault) = report.fault {
        return Err(format!(
            "{}: op log refused at record {}: {fault}",
            log.display(),
            fault.position()
        ));
    }
    if report.torn_tail_bytes != 0 {
        return Err(format!(
            "{}: op log has an unterminated {}-byte tail; an export needs a complete record boundary",
            log.display(),
            report.torn_tail_bytes
        ));
    }
    Ok((
        report.intact_records,
        report.head.as_ref().map(choir_hash::ContentHash::to_hex),
    ))
}

/// Folds the log and groups its refs by repository, as git oids.
fn log_refs(log: &Path) -> Result<BTreeMap<String, BTreeMap<String, String>>, String> {
    let backend = choir_oplog::FileLog::open(log).map_err(|e| format!("open log: {e:?}"))?;
    let view = choir_view::View::materialize(&backend).map_err(|e| format!("fold log: {e:?}"))?;
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (key, hash) in &view.refs {
        let (repo, name) = key
            .split_once(':')
            .ok_or_else(|| format!("view ref key `{key}` names no repository"))?;
        let oid = hash
            .git_oid()
            .ok_or_else(|| format!("view ref `{key}` does not hold a git oid"))?;
        out.entry(repo.to_string())
            .or_default()
            .insert(name.to_string(), oid);
    }
    Ok(out)
}

/// Bare repositories under `root`, as `owner/name.git` paths.
///
/// Public because the export is not the only caller that needs to know
/// what is actually on disk. Startup needs it too: a repository can
/// arrive without ever being named in `--create` — restored from a
/// bundle, or created against a running node — and one that is served
/// without being adopted is served with no `pre-receive` hook, which
/// means every push into it bypasses the sequencer.
///
/// A directory counts as a repository when its name ends `.git` and it
/// holds a `HEAD` file, so a half-written directory is not mistaken for
/// one. `.choir` is skipped, and symlinks are never followed: a link out
/// of the root would otherwise let an export copy, or a startup adopt,
/// whatever it pointed at.
///
/// # Errors
///
/// Returns a description when a directory under `root` cannot be read.
pub fn repos(root: &Path) -> Result<Vec<String>, String> {
    let mut found = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|e| format!("read {dir:?}: {e}"))? {
            let entry = entry.map_err(|e| format!("read {dir:?}: {e}"))?;
            // `DirEntry::file_type` does not follow symlinks, so a link
            // out of the root is skipped rather than followed. An export
            // that chased one would copy whatever it pointed at.
            if !entry
                .file_type()
                .map_err(|e| format!("stat: {e}"))?
                .is_dir()
            {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".choir" {
                continue;
            }
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if name.ends_with(".git") && entry.path().join("HEAD").is_file() {
                found.push(rel);
            } else {
                stack.push((entry.path(), rel));
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Every file under `dir`, recursively.
fn walk(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|e| format!("read {dir:?}: {e}"))? {
            let entry = entry.map_err(|e| format!("read {dir:?}: {e}"))?;
            if entry
                .file_type()
                .map_err(|e| format!("stat: {e}"))?
                .is_dir()
            {
                stack.push(entry.path());
            } else {
                found.push(entry.path());
            }
        }
    }
    Ok(found)
}

/// `refname -> oid` for a bundle, read from its header alone.
fn bundle_heads(bundle: &Path) -> Result<BTreeMap<String, String>, String> {
    let path = bundle.to_str().ok_or("bundle path is not utf-8")?;
    let out = git(&["bundle", "list-heads", path], Path::new("."))?;
    let mut heads = BTreeMap::new();
    for line in out.lines() {
        // `HEAD` is listed beside the refs and is a symbolic pointer, not
        // a ref any op ever names.
        if let Some((oid, name)) = line.split_once(' ') {
            if name.starts_with("refs/") {
                heads.insert(name.to_string(), oid.to_string());
            }
        }
    }
    Ok(heads)
}

/// Copies one file, reporting which one failed.
fn copy(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::copy(from, to)
        .map(|_| ())
        .map_err(|e| format!("copy {} to {}: {e}", from.display(), to.display()))
}

/// Runs git in `dir`, returning stdout or a failure description.
fn git(args: &[&str], dir: &Path) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "git {} in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}
