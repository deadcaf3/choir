//! Durable, advisory calibration receipts for D23 differential runs.
//!
//! One process owns a state directory. Every observation and adjudication is
//! an append-only, versioned JSONL row. The derived receipt is replaceable and
//! can always be rebuilt from those rows. No timestamp is used as evidence:
//! observation ids are the local total order, and exact integer counts drive
//! the `< 1/1000` calculation.
//!
//! Reproducibility is frozen along two axes, not one. The command file's raw
//! bytes are hashed so a ledger cannot silently mix command versions, and the
//! effective environment the runs execute in — the command file's declared
//! `env` plus the small pass-through list in [`effective_environment`] — is
//! hashed into `activation.json` the same way, so a ledger cannot silently
//! mix environments either. A state directory activated before environment
//! hashing existed adopts the current environment on its next observation and
//! enforces it from then on; its earlier rows predate enforcement and cannot
//! be retroactively attested.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use choir_oplog::ContentHash;

use crate::differential::{Calibration, DifferentialReport, Verdict};

const FORMAT_VERSION: u64 = 1;
static REPLACE_TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// Explicit argv loaded from a versioned JSON file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// Executable name or path. It is never interpreted by a shell.
    pub program: String,
    /// Exact argv applied in all three worktrees.
    pub args: Vec<String>,
    /// Environment variables declared by the command file. The runs see
    /// these plus the pass-through list in [`effective_environment`], and
    /// nothing else.
    pub env: BTreeMap<String, String>,
    /// BLAKE3 content address of the command file bytes.
    pub snapshot_hash: String,
}

/// Commit identities recorded with one three-worktree observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revisions {
    /// First-parent commit as a canonical full Git object id.
    pub parent_a: String,
    /// Second-parent commit as a canonical full Git object id.
    pub parent_b: String,
    /// Speculative merge commit as a canonical full Git object id.
    pub merged: String,
}

fn target_dir_stays_inside_worktree(value: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && !path.is_absolute()
        && path.components().all(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::Normal(_)
            )
        })
}

fn cargo_target_dirs_are_isolated(program: &str, args: &[String]) -> bool {
    if Path::new(program)
        .file_stem()
        .and_then(|name| name.to_str())
        != Some("cargo")
    {
        return true;
    }
    args.iter().enumerate().all(|(index, arg)| {
        if arg == "--target-dir" {
            args.get(index + 1)
                .is_some_and(|value| target_dir_stays_inside_worktree(value))
        } else if let Some(value) = arg.strip_prefix("--target-dir=") {
            target_dir_stays_inside_worktree(value)
        } else {
            true
        }
    })
}

/// Result of durably appending one observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedObservation {
    /// Monotonic id within this state directory.
    pub observation_id: u64,
    /// Exact report that was appended.
    pub report: DifferentialReport,
    /// Receipt rebuilt after the append.
    pub calibration: serde_json::Value,
}

/// Reads and validates a command specification.
///
/// The file schema is
/// `{"format_version":1,"program":"cargo","args":["test"],"env":{"NAME":"value"}}`
/// with `env` optional and empty by default. Its raw bytes are hashed so a
/// ledger cannot silently mix command versions; because `env` lives in those
/// bytes, a declared-environment change is a command change.
///
/// # Errors
///
/// The file is unreadable, malformed, has the wrong version, has an empty
/// program/non-string argument, declares an environment entry whose name is
/// empty or contains `=` or NUL, or gives Cargo a target directory outside
/// the current revision worktree.
pub fn load_command(path: &Path) -> Result<CommandSpec, String> {
    let bytes = fs::read(path).map_err(|error| format!("read differential command: {error}"))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse differential command: {error}"))?;
    if value["format_version"].as_u64() != Some(FORMAT_VERSION) {
        return Err("differential command needs format_version 1".to_string());
    }
    let program = value["program"]
        .as_str()
        .filter(|program| !program.is_empty())
        .ok_or("differential command needs a non-empty program")?
        .to_string();
    let args = value["args"]
        .as_array()
        .ok_or("differential command args must be an array")?
        .iter()
        .map(|arg| {
            arg.as_str()
                .map(str::to_string)
                .ok_or("differential command arguments must be strings".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let env = match &value["env"] {
        serde_json::Value::Null => BTreeMap::new(),
        serde_json::Value::Object(entries) => entries
            .iter()
            .map(|(name, value)| {
                if name.is_empty() || name.contains(['=', '\0']) {
                    return Err("differential command env names must be non-empty and free of = and NUL".to_string());
                }
                value
                    .as_str()
                    .map(|value| (name.clone(), value.to_string()))
                    .ok_or("differential command env values must be strings".to_string())
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?,
        _ => return Err("differential command env must be an object".to_string()),
    };
    if !cargo_target_dirs_are_isolated(&program, &args) {
        return Err("cargo target directory must stay inside each revision worktree".to_string());
    }
    Ok(CommandSpec {
        program,
        args,
        env,
        snapshot_hash: ContentHash::blake3(&bytes).to_hex(),
    })
}

/// The complete environment a differential run executes in.
///
/// Starts from the pass-through list — `PATH`, `HOME`, `TMPDIR`, taken from
/// this process when present, because subprocess commands are unrunnable
/// without them — and lets the command file's declared `env` override. The
/// result is exactly what [`crate::differential::run_merged_vs_parents`]
/// should be given, and exactly what [`environment_hash`] attests.
#[must_use]
pub fn effective_environment(declared: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for name in ["PATH", "HOME", "TMPDIR"] {
        if let Ok(value) = std::env::var(name) {
            env.insert(name.to_string(), value);
        }
    }
    env.extend(declared.iter().map(|(k, v)| (k.clone(), v.clone())));
    env
}

/// BLAKE3 content address of one effective environment.
///
/// The preimage is the canonical JSON encoding of the map; `BTreeMap` order
/// makes it deterministic, the same discipline every hashed struct in the
/// workspace relies on.
#[must_use]
pub fn environment_hash(env: &BTreeMap<String, String>) -> String {
    ContentHash::blake3(&serde_json::to_vec(env).expect("string map always encodes")).to_hex()
}

fn chmod(path: &Path, mode: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|error| format!("set calibration permissions: {error}"))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

fn secure_append(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("open calibration stream: {error}"))?;
    chmod(path, 0o600)?;
    Ok(file)
}

fn append_row(path: &Path, row: &serde_json::Value) -> Result<(), String> {
    let mut file = secure_append(path)?;
    serde_json::to_writer(&mut file, row)
        .map_err(|error| format!("encode calibration row: {error}"))?;
    file.write_all(b"\n")
        .map_err(|error| format!("append calibration row: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("sync calibration row: {error}"))
}

fn read_rows(path: &Path) -> Result<Vec<serde_json::Value>, String> {
    let file = File::open(path).map_err(|error| format!("open calibration stream: {error}"))?;
    BufReader::new(file)
        .lines()
        .enumerate()
        .filter_map(|(index, line)| match line {
            Ok(line) if line.trim().is_empty() => None,
            other => Some((index, other)),
        })
        .map(|(index, line)| {
            let line = line.map_err(|error| format!("read calibration row: {error}"))?;
            serde_json::from_str(&line)
                .map_err(|error| format!("parse calibration row {}: {error}", index + 1))
        })
        .collect()
}

fn state_paths(state: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    (
        state.join("activation.json"),
        state.join("observations.jsonl"),
        state.join("adjudications.jsonl"),
        state.join("receipt.json"),
    )
}

fn is_canonical_git_oid(revision: &str) -> bool {
    matches!(revision.len(), 40 | 64)
        && revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_revisions(revisions: &Revisions) -> Result<(), String> {
    if [
        revisions.parent_a.as_str(),
        revisions.parent_b.as_str(),
        revisions.merged.as_str(),
    ]
    .into_iter()
    .all(is_canonical_git_oid)
    {
        Ok(())
    } else {
        Err("observation revisions must use canonical Git object ids".to_string())
    }
}

fn write_new(path: &Path, body: &[u8]) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("create calibration file: {error}"))?;
    file.write_all(body)
        .map_err(|error| format!("write calibration file: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("sync calibration file: {error}"))?;
    chmod(path, 0o600)
}

/// `environment_hash` is `Some` only on the run path: recording an
/// observation must pin the environment, while adjudication and refresh
/// neither run the command nor should be refused because the operator's
/// shell has since changed.
fn prepare_state(
    state: &Path,
    command_hash: &str,
    environment_hash: Option<&str>,
) -> Result<(), String> {
    fs::create_dir_all(state).map_err(|error| format!("create calibration directory: {error}"))?;
    chmod(state, 0o700)?;
    let (activation, observations, adjudications, _) = state_paths(state);
    if activation.exists() {
        let mut value: serde_json::Value = serde_json::from_slice(
            &fs::read(&activation)
                .map_err(|error| format!("read calibration activation: {error}"))?,
        )
        .map_err(|error| format!("parse calibration activation: {error}"))?;
        if value["format_version"].as_u64() != Some(FORMAT_VERSION)
            || value["command_snapshot_hash"].as_str() != Some(command_hash)
        {
            return Err("calibration state belongs to a different command snapshot".to_string());
        }
        if let Some(environment_hash) = environment_hash {
            match value["environment_hash"].as_str() {
                Some(recorded) if recorded == environment_hash => {}
                Some(_) => {
                    return Err(
                        "calibration state belongs to a different environment".to_string()
                    );
                }
                // Activated before environments were hashed: adopt the
                // current one and enforce it from here on. The rows already
                // in the ledger predate enforcement, which the module doc
                // says out loud rather than pretending to attest them.
                None => {
                    value["environment_hash"] =
                        serde_json::Value::String(environment_hash.to_string());
                    let body = serde_json::to_vec(&value).map_err(|error| {
                        format!("encode calibration activation: {error}")
                    })?;
                    replace_file(state, &activation, &body)?;
                }
            }
        }
        chmod(&activation, 0o600)?;
    } else {
        let mut fields = serde_json::json!({
            "format_version": FORMAT_VERSION,
            "command_snapshot_hash": command_hash,
        });
        if let Some(environment_hash) = environment_hash {
            fields["environment_hash"] = serde_json::Value::String(environment_hash.to_string());
        }
        let body = serde_json::to_vec(&fields)
            .map_err(|error| format!("encode calibration activation: {error}"))?;
        write_new(&activation, &body)?;
    }
    drop(secure_append(&observations)?);
    drop(secure_append(&adjudications)?);
    File::open(state)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync calibration directory: {error}"))
}

#[derive(Debug)]
struct Folded {
    next_id: u64,
    reports: BTreeMap<u64, DifferentialReport>,
    adjudicated_ids: BTreeSet<u64>,
    receipt: serde_json::Value,
}

fn fold(state: &Path, command_hash: &str) -> Result<Folded, String> {
    let (activation_path, observations_path, adjudications_path, _) = state_paths(state);
    let mut reports = BTreeMap::new();
    let mut unique_merge_commits = BTreeSet::new();
    let rows = read_rows(&observations_path)?;
    for (index, row) in rows.iter().enumerate() {
        if row["format_version"].as_u64() != Some(FORMAT_VERSION) {
            return Err("observation row needs format_version 1".to_string());
        }
        let expected = u64::try_from(index + 1).map_err(|_| "too many observations")?;
        if row["observation_id"].as_u64() != Some(expected) {
            return Err("observation ids must be contiguous from 1".to_string());
        }
        for field in ["parent_a", "parent_b", "merged"] {
            if row["revisions"][field]
                .as_str()
                .filter(|revision| is_canonical_git_oid(revision))
                .is_none()
            {
                return Err(
                    "observation row needs three canonical Git object ids".to_string(),
                );
            }
        }
        unique_merge_commits.insert(
            row["revisions"]["merged"]
                .as_str()
                .expect("validated above")
                .to_string(),
        );
        reports.insert(expected, DifferentialReport::from_json(&row["report"])?);
    }

    let mut adjudications = BTreeMap::new();
    for row in read_rows(&adjudications_path)? {
        if row["format_version"].as_u64() != Some(FORMAT_VERSION) {
            return Err("adjudication row needs format_version 1".to_string());
        }
        let id = row["observation_id"]
            .as_u64()
            .ok_or("adjudication row needs an observation_id")?;
        let real = row["real_interaction"]
            .as_bool()
            .ok_or("adjudication row needs boolean real_interaction")?;
        if adjudications.insert(id, real).is_some() {
            return Err("an observation may be adjudicated only once".to_string());
        }
    }

    let adjudicated_ids = adjudications.keys().copied().collect();
    let mut calibration = Calibration::default();
    for (id, report) in &reports {
        match (report.verdict, adjudications.remove(id)) {
            (Verdict::InteractionFailure, Some(real)) => calibration.record(report, Some(real))?,
            (Verdict::InteractionFailure, None) => calibration.record_pending(report)?,
            (_, Some(_)) => {
                return Err("only an interaction failure may be adjudicated".to_string());
            }
            (_, None) => calibration.record(report, None)?,
        }
    }
    if !adjudications.is_empty() {
        return Err("adjudication refers to an unknown observation".to_string());
    }

    let mut receipt = calibration.receipt();
    let receipt_object = receipt
        .as_object_mut()
        .ok_or("calibration receipt must be an object")?;
    receipt_object.insert(
        "command_snapshot_hash".to_string(),
        serde_json::Value::String(command_hash.to_string()),
    );
    // Null means this state directory has never had its environment pinned,
    // which is itself worth seeing in the receipt.
    let activation: serde_json::Value = serde_json::from_slice(
        &fs::read(&activation_path)
            .map_err(|error| format!("read calibration activation: {error}"))?,
    )
    .map_err(|error| format!("parse calibration activation: {error}"))?;
    receipt_object.insert(
        "environment_hash".to_string(),
        activation["environment_hash"].clone(),
    );
    receipt_object.insert("observations".to_string(), serde_json::json!(reports.len()));
    receipt_object.insert(
        "unique_merge_commits".to_string(),
        serde_json::json!(unique_merge_commits.len()),
    );
    receipt_object.insert(
        "has_conclusive_observation".to_string(),
        serde_json::json!(receipt_object["evaluated_merges"].as_u64().unwrap_or(0) != 0),
    );
    receipt_object.insert(
        "all_flags_adjudicated".to_string(),
        serde_json::json!(receipt_object["pending_interactions"].as_u64() == Some(0)),
    );
    Ok(Folded {
        next_id: u64::try_from(reports.len())
            .map_err(|_| "too many observations")?
            .saturating_add(1),
        reports,
        adjudicated_ids,
        receipt,
    })
}

fn replace_file(state: &Path, path: &Path, body: &[u8]) -> Result<(), String> {
    let temp = state.join(format!(
        ".replace-{}-{}",
        std::process::id(),
        REPLACE_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    write_new(&temp, body)?;
    fs::rename(&temp, path)
        .map_err(|error| format!("replace calibration file: {error}"))?;
    chmod(path, 0o600)?;
    File::open(state)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync calibration directory: {error}"))
}

fn write_receipt(state: &Path, receipt: &serde_json::Value) -> Result<(), String> {
    let (_, _, _, receipt_path) = state_paths(state);
    let mut body = serde_json::to_vec_pretty(receipt)
        .map_err(|error| format!("encode calibration receipt: {error}"))?;
    body.push(b'\n');
    replace_file(state, &receipt_path, &body)
}

/// Appends a report and rebuilds the advisory receipt.
///
/// `environment_hash` is the [`environment_hash`] of the
/// [`effective_environment`] the report's three runs actually executed in.
/// The first observation pins it in `activation.json`; later observations
/// must match it or are refused, exactly as a changed command snapshot is.
///
/// # Errors
///
/// A revision is not a canonical full Git object id, state cannot be created,
/// belongs to another command or another environment, contains an invalid
/// row, or cannot be durably updated.
pub fn record_observation(
    state: &Path,
    command_hash: &str,
    environment_hash: &str,
    revisions: &Revisions,
    report: &DifferentialReport,
) -> Result<RecordedObservation, String> {
    validate_revisions(revisions)?;
    prepare_state(state, command_hash, Some(environment_hash))?;
    let before = fold(state, command_hash)?;
    let (_, observations, _, _) = state_paths(state);
    let row = serde_json::json!({
        "format_version": FORMAT_VERSION,
        "observation_id": before.next_id,
        "revisions": {
            "parent_a": revisions.parent_a,
            "parent_b": revisions.parent_b,
            "merged": revisions.merged,
        },
        "report": report.to_json(),
    });
    append_row(&observations, &row)?;
    let after = fold(state, command_hash)?;
    write_receipt(state, &after.receipt)?;
    Ok(RecordedObservation {
        observation_id: before.next_id,
        report: report.clone(),
        calibration: after.receipt,
    })
}

/// Appends one ground-truth decision for a flagged observation and rebuilds
/// the receipt. `true` means a real interaction; `false` means spurious.
///
/// # Errors
///
/// The id is absent, unflagged, already adjudicated, or state cannot be
/// durably updated.
pub fn adjudicate(
    state: &Path,
    command_hash: &str,
    observation_id: u64,
    real_interaction: bool,
) -> Result<serde_json::Value, String> {
    prepare_state(state, command_hash, None)?;
    let before = fold(state, command_hash)?;
    match before.reports.get(&observation_id) {
        Some(report) if report.verdict == Verdict::InteractionFailure => {}
        Some(_) => return Err("only an interaction failure may be adjudicated".to_string()),
        None => return Err("adjudication refers to an unknown observation".to_string()),
    }
    if before.adjudicated_ids.contains(&observation_id) {
        return Err("an observation may be adjudicated only once".to_string());
    }
    let (_, _, adjudications, _) = state_paths(state);
    let row = serde_json::json!({
        "format_version": FORMAT_VERSION,
        "observation_id": observation_id,
        "real_interaction": real_interaction,
    });
    append_row(&adjudications, &row)?;
    let after = fold(state, command_hash)?;
    write_receipt(state, &after.receipt)?;
    Ok(after.receipt)
}

/// Rebuilds a receipt from the append-only source rows.
///
/// # Errors
///
/// The command snapshot does not match or any source row is invalid.
pub fn refresh(state: &Path, command_hash: &str) -> Result<serde_json::Value, String> {
    prepare_state(state, command_hash, None)?;
    let folded = fold(state, command_hash)?;
    write_receipt(state, &folded.receipt)?;
    Ok(folded.receipt)
}
