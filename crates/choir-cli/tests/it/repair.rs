//! `choir repair`, driven as the operator drives it: the real binary,
//! against real log files on disk.
//!
//! The unit tests in `choir-oplog::repair` cover the walk and the
//! quarantine. What is only testable here is the part an operator
//! actually depends on in the worst hour of their week: that the tool
//! refuses to do the destructive thing by default, and that when it
//! refuses to repair, it says what to do instead.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A private directory per test: this harness shares a process and runs
/// on parallel threads.
fn workdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "choir-cli-repair-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Runs the built binary and returns `(exit code, stdout, stderr)`.
fn choir(args: &[&str]) -> (i32, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_choir"))
        .args(args)
        .output()
        .expect("choir runs");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

/// A valid log of `count` linked records, written by the real writer so
/// the bytes are what a node would have produced.
fn valid_log(dir: &Path, count: u64) -> PathBuf {
    use choir_oplog::{OpEntry, OpLog, FORMAT_VERSION};
    let path = dir.join("ops.jsonl");
    let mut log = choir_oplog::FileLog::open(&path).expect("fresh log");
    for seq in 0..count {
        log.append(OpEntry {
            format_version: FORMAT_VERSION,
            parent: log.head(),
            seq,
            channel: "ws".into(),
            payload: format!("op-{seq}").into_bytes(),
            witnesses: Vec::new(),
            author_sig: None,
        })
        .expect("append");
    }
    log.sync().expect("sync");
    path
}

fn append_partial_record(path: &Path) -> usize {
    let torn = br#"{"format_version":1,"seq":9,"workspa"#;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("reopen");
    file.write_all(torn).expect("partial write");
    file.sync_all().expect("sync");
    torn.len()
}

/// No mode is a usage error, not a default action.
///
/// This is the constraint the whole command is shaped around: "repair"
/// with no further words must never be taken as permission to modify a
/// log. A tool that defaulted to fixing would eventually be run by
/// someone who only meant to look.
#[test]
fn a_bare_repair_refuses_to_guess_the_mode() {
    let dir = workdir("nomode");
    let path = valid_log(&dir, 2);
    let before = std::fs::read(&path).expect("read");

    let (code, _out, err) = choir(&["repair", path.to_str().expect("utf8")]);

    assert_eq!(code, 2, "no mode is a usage error: {err}");
    assert!(err.contains("--verify"), "and it lists the modes: {err}");
    assert!(err.contains("--truncate-tail"), "{err}");
    assert_eq!(
        std::fs::read(&path).expect("read"),
        before,
        "and nothing was touched"
    );
}

/// Both modes at once is also refused. It reads like "check, then fix",
/// which is exactly the compound the operator is meant to be choosing
/// between rather than sliding past.
#[test]
fn asking_for_both_modes_is_refused_rather_than_ordered() {
    let dir = workdir("bothmodes");
    let path = valid_log(&dir, 2);
    let (code, _out, err) = choir(&[
        "repair",
        path.to_str().expect("utf8"),
        "--verify",
        "--truncate-tail",
    ]);
    assert_eq!(code, 2, "{err}");
}

/// `--verify` reports and changes nothing, including when there is
/// something it could have repaired.
#[test]
fn verify_reports_a_torn_tail_without_repairing_it() {
    let dir = workdir("verify");
    let path = valid_log(&dir, 3);
    let torn_len = append_partial_record(&path);
    let before = std::fs::read(&path).expect("read");

    let (code, out, err) = choir(&["repair", path.to_str().expect("utf8"), "--verify"]);

    assert_eq!(code, 0, "a torn tail is usable, not a failure: {err}");
    assert!(out.contains("intact records: 3"), "{out}");
    assert!(
        out.contains(&format!("{torn_len} bytes")),
        "the tail is reported with its size: {out}"
    );
    assert_eq!(
        std::fs::read(&path).expect("read"),
        before,
        "--verify must not modify the log"
    );
    let sidecars = torn_sidecars(&dir);
    assert!(
        sidecars.is_empty(),
        "and must not quarantine anything either: {sidecars:?}"
    );
}

/// `--truncate-tail` moves the bytes to a sidecar and cuts back. The
/// bytes are read back and compared, because "quarantined" and "deleted"
/// look identical from the log's side.
#[test]
fn truncate_tail_quarantines_the_bytes_it_removes() {
    let dir = workdir("truncate");
    let path = valid_log(&dir, 3);
    let intact_len = std::fs::metadata(&path).expect("stat").len();
    let torn_len = append_partial_record(&path);

    let (code, out, err) = choir(&["repair", path.to_str().expect("utf8"), "--truncate-tail"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("quarantined"), "{out}");

    assert_eq!(
        std::fs::metadata(&path).expect("stat").len(),
        intact_len,
        "the log is back to its last complete record"
    );
    let sidecars = torn_sidecars(&dir);
    assert_eq!(sidecars.len(), 1, "exactly one sidecar: {sidecars:?}");
    assert_eq!(
        std::fs::read(&sidecars[0]).expect("sidecar readable").len(),
        torn_len,
        "holding every byte that was removed"
    );

    // And the repaired log verifies clean.
    let (code, out, _) = choir(&["repair", path.to_str().expect("utf8"), "--verify"]);
    assert_eq!(code, 0);
    assert!(out.contains("Intact."), "{out}");
}

/// Mid-log damage is refused, and the refusal names the way out.
///
/// The exit code matters as much as the text: a script that repairs a
/// fleet must be able to tell "fixed" from "do not touch this one"
/// without parsing prose.
#[test]
fn mid_log_damage_is_refused_with_restore_instructions() {
    let dir = workdir("middamage");
    let path = valid_log(&dir, 4);
    let text = std::fs::read_to_string(&path).expect("read");
    let mut lines: Vec<String> = text.lines().map(ToString::to_string).collect();
    lines[1] = "{this was once a whole record".to_string();
    std::fs::write(&path, lines.join("\n") + "\n").expect("write back");
    let before = std::fs::read(&path).expect("read");

    let (code, out, err) = choir(&["repair", path.to_str().expect("utf8"), "--truncate-tail"]);

    assert_eq!(code, 1, "damaged is a failure exit: {out}{err}");
    assert!(
        err.contains("Restore from backup") || out.contains("Restore from backup"),
        "the operator is told what to do instead: {out}{err}"
    );
    assert_eq!(
        std::fs::read(&path).expect("read"),
        before,
        "a refusal changes nothing"
    );
    assert!(
        torn_sidecars(&dir).is_empty(),
        "and quarantines nothing: there is nothing safe to remove"
    );
}

/// The same damage under `--verify` reports the position rather than
/// just failing.
#[test]
fn verify_locates_the_first_bad_record() {
    let dir = workdir("locate");
    let path = valid_log(&dir, 5);
    let text = std::fs::read_to_string(&path).expect("read");
    let mut lines: Vec<String> = text.lines().map(ToString::to_string).collect();
    lines[3] = "{damaged".to_string();
    std::fs::write(&path, lines.join("\n") + "\n").expect("write back");

    let (code, out, err) = choir(&["repair", path.to_str().expect("utf8"), "--verify"]);
    assert_eq!(code, 1);
    assert!(
        out.contains("record 3") || err.contains("record 3"),
        "the first bad record is named: {out}{err}"
    );
    assert!(
        out.contains("intact records: 3"),
        "and how much was good before it: {out}"
    );
}

fn torn_sidecars(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .expect("listing")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(".torn-"))
        })
        .collect()
}
