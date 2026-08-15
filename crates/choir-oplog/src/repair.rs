//! Inspecting and repairing a log file that a daemon is not holding.
//!
//! Three things a log can be, and they want three different answers:
//!
//! 1. **Intact.** Every record decodes, every `parent` names the record
//!    before it, and `seq` counts from zero without a gap.
//! 2. **Torn at the tail.** The final record was still being written
//!    when the machine stopped. Nothing was lost — the sequencer
//!    acknowledges only after [`crate::OpLog::sync`] returns — so this is
//!    repairable, and [`crate::FileLog::open`] already repairs it in place.
//! 3. **Damaged in the middle.** A record that was once written whole no
//!    longer decodes, or the chain does not link. **Not repairable
//!    here.** Anything that made the file consistent again would do it
//!    by dropping ops that were acknowledged to somebody, and a log that
//!    silently loses acknowledged ops is worse than one that refuses to
//!    open. The answer is a restore from backup, and this module's job
//!    is to say so precisely rather than to improvise.
//!
//! # Why this does not call `FileLog::open`
//!
//! Because opening a log *repairs* it: a torn tail is truncated as a
//! side effect of the constructor. A verify that ran through `open`
//! would change the thing it was asked to inspect, and would report
//! "intact" about a file it had just altered. Everything here reads the
//! file with its own reader and writes nothing unless asked to.
//!
//! # What `open` does not check, and this does
//!
//! [`crate::FileLog::open`] validates that each record *decodes*. It does not
//! validate the chain: the `parent`/head comparison lives in
//! [`crate::OpLog::append`], on the write path, and has no counterpart on
//! replay. So a log whose hash chain is broken opens perfectly well
//! today. That gap is the reason [`verify`] exists.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::{ContentHash, LogError, OpEntry};

/// The first thing found wrong with a log, if anything was.
///
/// One fault, not a list, and deliberately so: after the first break
/// every later record is being judged against a chain that is already
/// wrong, so a list would be one real finding followed by noise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// A newline-terminated record did not decode. It was written whole
    /// once, so this is damage, not a torn write.
    Undecodable {
        /// Position in the file, counting records from zero.
        position: u64,
        /// What the decoder said.
        detail: String,
    },
    /// A record's `parent` is not the hash of the record before it.
    BrokenChain {
        /// Position in the file, counting records from zero.
        position: u64,
        /// The hash the previous record actually has.
        expected: Option<ContentHash>,
        /// The hash this record claims its parent is.
        found: Option<ContentHash>,
    },
    /// A record's `seq` is not its position in the file.
    ///
    /// Separate from [`Fault::BrokenChain`] because the two fail
    /// independently: a log can renumber without breaking its hashes
    /// (the hash does not cover position) and can break its hashes while
    /// staying numbered correctly.
    SeqMismatch {
        /// Position in the file, counting records from zero.
        position: u64,
        /// The `seq` the record carries.
        found: u64,
    },
}

impl Fault {
    /// Where the log first goes wrong.
    #[must_use]
    pub fn position(&self) -> u64 {
        match self {
            Fault::Undecodable { position, .. }
            | Fault::BrokenChain { position, .. }
            | Fault::SeqMismatch { position, .. } => *position,
        }
    }
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fault::Undecodable { position, detail } => write!(
                f,
                "record {position} does not decode: {detail}"
            ),
            Fault::BrokenChain {
                position,
                expected,
                found,
            } => write!(
                f,
                "record {position} names parent {} but the record before it hashes to {}",
                found.as_ref().map_or("none".to_string(), ContentHash::to_hex),
                expected
                    .as_ref()
                    .map_or("none".to_string(), ContentHash::to_hex)
            ),
            Fault::SeqMismatch { position, found } => write!(
                f,
                "record at position {position} carries seq {found}"
            ),
        }
    }
}

/// What a read-only walk of the log found.
#[derive(Debug, Clone)]
pub struct ChainReport {
    /// Records that decoded and linked correctly, counted from the
    /// start until the first fault (or to the end, if there was none).
    pub intact_records: u64,
    /// The first thing wrong, if anything was.
    pub fault: Option<Fault>,
    /// Bytes of unterminated final record, or 0 if the file ends on a
    /// record boundary. Non-zero is repairable; see [`truncate_tail`].
    pub torn_tail_bytes: u64,
    /// Hash of the last intact record, which is what a restore has to
    /// agree with.
    pub head: Option<ContentHash>,
}

impl ChainReport {
    /// Whether the log can be opened and used as it stands.
    ///
    /// A torn tail does not make this false: [`crate::FileLog::open`] repairs
    /// that on its own, and the bytes it discards were never
    /// acknowledged.
    #[must_use]
    pub fn is_usable(&self) -> bool {
        self.fault.is_none()
    }

    /// Whether the only thing wrong is a tail that was still being
    /// written, which [`truncate_tail`] can repair.
    #[must_use]
    pub fn is_repairable(&self) -> bool {
        self.fault.is_none() && self.torn_tail_bytes > 0
    }
}

/// Walks the log and reports the first fault. Changes nothing.
///
/// Reads the file directly rather than through [`crate::FileLog::open`],
/// because opening repairs a torn tail as a side effect and would make
/// this report describe a file it had already altered.
///
/// # Errors
///
/// [`LogError::Io`] if the file cannot be read. A damaged *record* is
/// reported in the [`ChainReport`], not returned as an error: the
/// caller asked what is wrong with the log, and answering with a
/// failure would make the answer indistinguishable from not being able
/// to look.
pub fn verify(path: &Path) -> Result<ChainReport, LogError> {
    let file = std::fs::File::open(path).map_err(LogError::Io)?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut head: Option<ContentHash> = None;
    let mut position = 0u64;
    let mut torn_tail_bytes = 0u64;
    let mut fault = None;

    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line).map_err(LogError::Io)?;
        if read == 0 {
            break;
        }
        let Some(body) = line.strip_suffix(b"\n") else {
            // No terminator: the write was interrupted. Not a fault --
            // it is the one repairable state -- so it is reported in its
            // own field rather than as damage.
            torn_tail_bytes = read as u64;
            break;
        };
        let entry: OpEntry = match serde_json::from_slice(body) {
            Ok(entry) => entry,
            Err(error) => {
                fault = Some(Fault::Undecodable {
                    position,
                    detail: error.to_string(),
                });
                break;
            }
        };
        if entry.parent != head {
            fault = Some(Fault::BrokenChain {
                position,
                expected: head.clone(),
                found: entry.parent,
            });
            break;
        }
        if entry.seq != position {
            fault = Some(Fault::SeqMismatch {
                position,
                found: entry.seq,
            });
            break;
        }
        head = Some(entry.content_hash());
        position += 1;
    }

    Ok(ChainReport {
        intact_records: position,
        fault,
        torn_tail_bytes,
        head,
    })
}

/// What a tail repair did.
#[derive(Debug, Clone)]
pub struct Repaired {
    /// Where the removed bytes were written before the file was cut.
    pub quarantine: PathBuf,
    /// How many bytes were moved there.
    pub bytes: u64,
    /// The log's length afterwards.
    pub length: u64,
}

/// Moves an unterminated final record into a sidecar and truncates the
/// log to the last complete one.
///
/// The bytes are **copied out before the file is cut**, in that order,
/// and the sidecar is synced before the truncation is issued. If the
/// machine stops midway the worst case is a sidecar with no truncation
/// -- a spare copy of bytes that are still in the log -- rather than a
/// truncation with no sidecar, which would be the deletion this is
/// written to avoid.
///
/// # Errors
///
/// [`LogError::Corrupt`] if the log has a fault that is not a torn tail.
/// Refusing is the whole point: a mid-log break cannot be repaired by
/// removing the end of the file, and doing it anyway would silently drop
/// acknowledged ops. [`LogError::Io`] on filesystem failure.
pub fn truncate_tail(path: &Path) -> Result<Option<Repaired>, LogError> {
    let report = verify(path)?;
    if let Some(fault) = report.fault {
        return Err(LogError::Corrupt(format!(
            "refusing to truncate: the damage is not a torn tail ({fault}). \
             Truncating would drop acknowledged ops. Restore from backup."
        )));
    }
    if report.torn_tail_bytes == 0 {
        return Ok(None);
    }

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(LogError::Io)?;
    let length = file.metadata().map_err(LogError::Io)?.len();
    let keep = length - report.torn_tail_bytes;

    let mut torn = vec![0u8; usize::try_from(report.torn_tail_bytes).unwrap_or(usize::MAX)];
    file.seek(SeekFrom::Start(keep)).map_err(LogError::Io)?;
    file.read_exact(&mut torn).map_err(LogError::Io)?;

    let quarantine = quarantine_path(path, keep);
    let mut sidecar = std::fs::File::create(&quarantine).map_err(LogError::Io)?;
    sidecar.write_all(&torn).map_err(LogError::Io)?;
    // Durable before the log is cut, or a crash here loses the only
    // remaining copy.
    sidecar.sync_all().map_err(LogError::Io)?;
    drop(sidecar);
    crate::sync_parent_dir(path)?;

    file.set_len(keep).map_err(LogError::Io)?;
    file.sync_all().map_err(LogError::Io)?;

    Ok(Some(Repaired {
        quarantine,
        bytes: report.torn_tail_bytes,
        length: keep,
    }))
}

/// Writes `bytes` to the sidecar for a tail cut at `offset`, syncing it
/// before returning.
///
/// Shared with [`crate::FileLog::open`], which does the same repair
/// automatically on startup. Automatic *truncation* is a deliberate
/// availability choice — a node has to come back after a power cut
/// without a human — but automatic *deletion* is not, and this is the
/// difference. The caller must not truncate until this returns.
///
/// # Errors
///
/// [`LogError::Io`] if the sidecar cannot be written or synced. Failing
/// here fails the open, which is correct: the alternative is truncating
/// with nowhere to put the bytes.
pub(crate) fn quarantine_tail(
    path: &Path,
    bytes: &[u8],
    offset: u64,
) -> Result<PathBuf, LogError> {
    let quarantine = quarantine_path(path, offset);
    let mut sidecar = std::fs::File::create(&quarantine).map_err(LogError::Io)?;
    sidecar.write_all(bytes).map_err(LogError::Io)?;
    sidecar.sync_all().map_err(LogError::Io)?;
    drop(sidecar);
    crate::sync_parent_dir(path)?;
    Ok(quarantine)
}

/// Sidecar name for bytes cut from `path` at offset `offset`.
///
/// The offset is in the name rather than a timestamp: it makes the file
/// say where it came from, and repeating the same repair produces the
/// same name instead of a new file each run.
fn quarantine_path(path: &Path, offset: u64) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".torn-{offset}"));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OpLog, FORMAT_VERSION};

    /// Builds a valid log file of `count` linked records and returns its
    /// path. Generated inline rather than kept as a fixture, per the
    /// house rule, and built through the real writer so the bytes are
    /// exactly what a node would have produced.
    fn valid_log(dir: &Path, count: u64) -> PathBuf {
        let path = dir.join("ops.jsonl");
        let mut log = crate::FileLog::open(&path).expect("fresh log opens");
        for seq in 0..count {
            let entry = OpEntry {
                format_version: FORMAT_VERSION,
                parent: log.head(),
                seq,
                channel: "ws".into(),
                payload: format!("op-{seq}").into_bytes(),
                witnesses: Vec::new(),
                author_sig: None,
            };
            log.append(entry).expect("append");
        }
        log.sync().expect("sync");
        path
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "choir-repair-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn an_intact_log_reports_no_fault() {
        let dir = scratch("intact");
        let path = valid_log(&dir, 5);
        let report = verify(&path).expect("readable");
        assert_eq!(report.intact_records, 5);
        assert!(report.fault.is_none(), "{:?}", report.fault);
        assert_eq!(report.torn_tail_bytes, 0);
        assert!(report.is_usable());
        assert!(!report.is_repairable(), "nothing to repair");
    }

    #[test]
    fn a_torn_tail_is_reported_as_repairable_not_as_damage() {
        let dir = scratch("torn");
        let path = valid_log(&dir, 3);
        // A partial record with no terminator, exactly what an
        // interrupted append leaves behind.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("reopen");
        file.write_all(br#"{"format_version":1,"seq":3,"chan"#)
            .expect("partial write");
        file.sync_all().expect("sync");
        drop(file);

        let report = verify(&path).expect("readable");
        assert!(report.fault.is_none(), "a torn tail is not damage");
        assert_eq!(report.intact_records, 3);
        assert!(report.torn_tail_bytes > 0);
        assert!(report.is_repairable());
    }

    #[test]
    fn a_broken_chain_is_found_and_located() {
        let dir = scratch("chain");
        let path = valid_log(&dir, 4);
        // Rewrite record 2 with a parent that names nothing real. The
        // record still decodes, so only a chain check can catch it --
        // which is precisely what `FileLog::open` does not do.
        let text = std::fs::read_to_string(&path).expect("read");
        let mut lines: Vec<String> = text.lines().map(ToString::to_string).collect();
        // Give record 2 record 1's parent: a correctly *shaped*
        // ContentHash lifted from the file itself, pointing at the wrong
        // record. Handing it a hex string instead produced an
        // `Undecodable` fault -- the check fired, but on the wrong
        // thing, which would have made this test a false witness for
        // chain verification.
        let earlier: serde_json::Value = serde_json::from_str(&lines[1]).expect("record 1 decodes");
        let mut entry: serde_json::Value =
            serde_json::from_str(&lines[2]).expect("record 2 decodes");
        entry["parent"] = earlier["parent"].clone();
        lines[2] = serde_json::to_string(&entry).expect("re-encode");
        std::fs::write(&path, lines.join("\n") + "\n").expect("write back");

        let report = verify(&path).expect("readable");
        assert!(!report.is_usable());
        assert!(!report.is_repairable(), "this is not a tail problem");
        assert_eq!(report.intact_records, 2, "records before it are intact");
        let fault = report.fault.expect("the break is found");
        assert_eq!(fault.position(), 2, "and located exactly: {fault}");
        assert!(matches!(fault, Fault::BrokenChain { .. }), "{fault:?}");
    }

    #[test]
    fn an_undecodable_record_is_found_and_located() {
        let dir = scratch("garbage");
        let path = valid_log(&dir, 4);
        let text = std::fs::read_to_string(&path).expect("read");
        let mut lines: Vec<String> = text.lines().map(ToString::to_string).collect();
        lines[1] = "{not json at all".to_string();
        std::fs::write(&path, lines.join("\n") + "\n").expect("write back");

        let report = verify(&path).expect("readable");
        let fault = report.fault.expect("the damage is found");
        assert_eq!(fault.position(), 1);
        assert!(matches!(fault, Fault::Undecodable { .. }), "{fault:?}");
    }

    #[test]
    fn verifying_does_not_repair() {
        // The trap this module's doc warns about: a verify implemented
        // over `FileLog::open` would truncate the tail and then report a
        // clean log, having caused the change it failed to mention.
        let dir = scratch("readonly");
        let path = valid_log(&dir, 2);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("reopen");
        file.write_all(b"partial").expect("partial write");
        drop(file);

        let before = std::fs::metadata(&path).expect("stat").len();
        let report = verify(&path).expect("readable");
        let after = std::fs::metadata(&path).expect("stat").len();
        assert_eq!(before, after, "verify must not change the file");
        assert_eq!(report.torn_tail_bytes, 7);
    }

    #[test]
    fn repairing_a_tail_quarantines_the_bytes_before_cutting() {
        let dir = scratch("quarantine");
        let path = valid_log(&dir, 3);
        let intact_len = std::fs::metadata(&path).expect("stat").len();
        let torn = br#"{"format_version":1,"seq":3,"chan"#;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("reopen");
        file.write_all(torn).expect("partial write");
        drop(file);

        let repaired = truncate_tail(&path)
            .expect("repairable")
            .expect("something was repaired");
        assert_eq!(repaired.bytes, torn.len() as u64);
        assert_eq!(repaired.length, intact_len, "cut back to the last record");

        // The bytes still exist, byte for byte. This is the difference
        // between quarantine and deletion, and the only way to tell the
        // two apart is to read them back.
        let saved = std::fs::read(&repaired.quarantine).expect("sidecar exists");
        assert_eq!(saved, torn, "the removed bytes are recoverable");

        // And the log is now usable.
        let after = verify(&path).expect("readable");
        assert!(after.is_usable(), "{:?}", after.fault);
        assert_eq!(after.intact_records, 3);
        assert_eq!(after.torn_tail_bytes, 0);
    }

    /// After a tail repair the log is not merely readable — it takes new
    /// appends that chain onto the surviving head.
    ///
    /// Verifying the repaired file only proves it parses. What a node
    /// does next is *write*, and a truncation that left `write_pos` or
    /// the head wrong would pass a verify and corrupt the log on the
    /// first append after boot.
    #[test]
    fn a_node_can_boot_and_keep_writing_after_a_tail_repair() {
        let dir = scratch("boots");
        let path = valid_log(&dir, 3);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("reopen");
        file.write_all(br#"{"format_version":1,"seq":3,"wor"#)
            .expect("partial write");
        drop(file);

        truncate_tail(&path).expect("repairable").expect("repaired");

        let mut log = crate::FileLog::open(&path).expect("the node opens the repaired log");
        assert_eq!(log.len(), 3, "the surviving records are all there");
        assert_eq!(log.torn_tail_bytes(), 0, "and nothing was left torn");

        let head_before = log.head().expect("three records means a head");
        log.append(OpEntry {
            format_version: FORMAT_VERSION,
            parent: Some(head_before),
            seq: 3,
            channel: "ws".into(),
            payload: b"after the repair".to_vec(),
            witnesses: Vec::new(),
            author_sig: None,
        })
        .expect("the append chains onto the surviving head");
        log.sync().expect("sync");
        drop(log);

        let report = verify(&path).expect("readable");
        assert!(report.is_usable(), "{:?}", report.fault);
        assert_eq!(report.intact_records, 4, "the new record joined the chain");
    }

    #[test]
    fn repairing_refuses_mid_log_damage_and_says_why() {
        let dir = scratch("refuse");
        let path = valid_log(&dir, 4);
        let text = std::fs::read_to_string(&path).expect("read");
        let mut lines: Vec<String> = text.lines().map(ToString::to_string).collect();
        lines[1] = "{not json at all".to_string();
        std::fs::write(&path, lines.join("\n") + "\n").expect("write back");
        let before = std::fs::read(&path).expect("read");

        let error = truncate_tail(&path).expect_err("must refuse");
        let message = format!("{error:?}");
        assert!(
            message.contains("restore") || message.contains("Restore"),
            "the refusal must point at the way out: {message}"
        );

        // Refusing means changing nothing at all, including not
        // quarantining anything.
        assert_eq!(
            std::fs::read(&path).expect("read"),
            before,
            "a refusal must leave the file untouched"
        );
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .expect("listing")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".torn-"))
            .collect();
        assert!(strays.is_empty(), "nothing was quarantined either");
    }

    #[test]
    fn repairing_an_intact_log_does_nothing_rather_than_something() {
        let dir = scratch("noop");
        let path = valid_log(&dir, 3);
        let before = std::fs::read(&path).expect("read");
        assert!(
            truncate_tail(&path).expect("succeeds").is_none(),
            "an intact log has no tail to repair"
        );
        assert_eq!(std::fs::read(&path).expect("read"), before);
    }

    #[test]
    fn a_renumbered_record_is_caught_even_though_it_decodes() {
        // `seq` is not covered by the parent chain, so a log can link
        // correctly and still be misnumbered. Checked separately for
        // exactly that reason.
        let dir = scratch("renumber");
        let path = valid_log(&dir, 3);
        let text = std::fs::read_to_string(&path).expect("read");
        let mut lines: Vec<String> = text.lines().map(ToString::to_string).collect();
        let mut entry: serde_json::Value =
            serde_json::from_str(&lines[1]).expect("decodes");
        entry["seq"] = serde_json::json!(99);
        lines[1] = serde_json::to_string(&entry).expect("re-encode");
        std::fs::write(&path, lines.join("\n") + "\n").expect("write back");

        let report = verify(&path).expect("readable");
        let fault = report.fault.expect("caught");
        assert!(
            matches!(fault, Fault::SeqMismatch { found: 99, .. }),
            "{fault:?}"
        );
    }
}
