//! An executor that is not in this process (D18).
//!
//! [`LocalRunner`](crate::local::LocalRunner) proves the seam can run
//! real work. It cannot prove the part a microVM, a container, or a
//! build farm actually adds, which is not virtualization but
//! *distance*: the jobs have to leave this address space and the
//! verdicts have to come back, aligned, including when the far side
//! stops talking halfway through.
//!
//! So the third conformance backend is the process boundary itself.
//! [`ProtocolRunner`] spawns a helper, hands it a batch over stdin as
//! JSON lines, and reads verdicts back from stdout. `choir-ci-local` is
//! the reference helper and runs the batch with `LocalRunner` on the
//! far side, which means the conformance suite runs the same assertions
//! against the same execution engine with a pipe in the middle -- and
//! anything that only passes in-process shows up as a difference rather
//! than as a story about pipes.
//!
//! A Firecracker or Cloud Hypervisor driver is a different helper
//! behind the same three lines of protocol. Neither can be built or
//! gated here: both require KVM, which is Linux-only, so a version
//! written on this machine would be exactly the untested single
//! implementation this seam exists to forbid.
//!
//! # Protocol
//!
//! One JSON object per line, both directions, and the count is agreed
//! before any work is described so neither side has to guess where the
//! batch ends.
//!
//! ```text
//! host   -> {"protocol":2,"jobs":2}
//! helper -> {"name":"choir-ci-local","protocol":2}
//! host   -> {"subject":"...","label":"1","command":["true"],"directory":null,...}
//! host   -> {"subject":"...","label":"2","command":["false"],"directory":"/w",...}
//! helper -> {"verdict":"passed"}
//! helper -> {"verdict":"failed","exit_code":1}
//! ```
//!
//! # Examples
//!
//! ```no_run
//! use choir_queue::executor::CiExecutor;
//! use choir_queue::remote::ProtocolRunner;
//!
//! let mut ci = ProtocolRunner::new(vec!["choir-ci-local".to_string()]);
//! let info = ci.info().expect("the helper answers");
//! assert_eq!(info.protocol, choir_queue::executor::PROTOCOL);
//! ```

use crate::executor::{CiExecutor, ExecutorError, ExecutorInfo, Job, Verdict, PROTOCOL};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Renders one job as the line a helper reads.
///
/// # Errors
///
/// The job names a directory that is not UTF-8, which JSON cannot
/// carry. Refused rather than lossily converted: a helper that runs the
/// job in a path spelled differently returns a verdict about a
/// different tree, and it would be index-aligned and therefore believed.
pub fn job_to_json(job: &Job) -> Result<serde_json::Value, String> {
    let directory = match &job.directory {
        None => serde_json::Value::Null,
        Some(dir) => serde_json::Value::String(
            dir.to_str()
                .ok_or_else(|| {
                    format!("job directory {dir:?} is not UTF-8 and cannot be sent as JSON")
                })?
                .to_string(),
        ),
    };
    Ok(serde_json::json!({
        "subject": job.subject.to_hex(),
        "label": job.label,
        "command": job.command,
        "environment": job.environment,
        "directory": directory,
        "deadline_ms": u64::try_from(job.deadline.as_millis()).unwrap_or(u64::MAX),
        "may_write_cache": job.may_write_cache,
    }))
}

/// Parses one job line, the inverse of [`job_to_json`].
///
/// # Errors
///
/// A message naming the first field that was missing or the wrong type.
/// Helpers report this rather than guessing: a job decoded with a
/// defaulted command is a job that tests something other than what was
/// asked, and the verdict would still be index-aligned and therefore
/// believed.
pub fn job_from_json(value: &serde_json::Value) -> Result<Job, String> {
    let subject = value["subject"]
        .as_str()
        .and_then(choir_hash::ContentHash::from_hex)
        .ok_or("job needs a hex `subject`")?;
    let command = value["command"]
        .as_array()
        .ok_or("job needs an array `command`")?
        .iter()
        .map(|a| {
            a.as_str()
                .map(str::to_string)
                .ok_or_else(|| "every `command` element must be a string".to_string())
        })
        .collect::<Result<Vec<String>, String>>()?;
    let mut job = Job::new(subject, command);
    job.label = value["label"].as_str().unwrap_or_default().to_string();
    if let Some(env) = value["environment"].as_object() {
        let mut map = BTreeMap::new();
        for (k, v) in env {
            let v = v
                .as_str()
                .ok_or("every environment value must be a string")?;
            map.insert(k.clone(), v.to_string());
        }
        job.environment = map;
    }
    if let Some(dir) = value["directory"].as_str() {
        job.directory = Some(std::path::PathBuf::from(dir));
    }
    if let Some(ms) = value["deadline_ms"].as_u64() {
        job.deadline = Duration::from_millis(ms);
    }
    job.may_write_cache = value["may_write_cache"].as_bool().unwrap_or(false);
    Ok(job)
}

/// Renders one verdict as the line a host reads.
#[must_use]
pub fn verdict_to_json(verdict: &Verdict) -> serde_json::Value {
    match verdict {
        Verdict::Passed => serde_json::json!({ "verdict": "passed" }),
        Verdict::Failed { exit_code } => {
            serde_json::json!({ "verdict": "failed", "exit_code": exit_code })
        }
        Verdict::Errored { provider, detail } => serde_json::json!({
            "verdict": "errored",
            "provider": provider,
            "detail": detail,
        }),
        Verdict::TimedOut => serde_json::json!({ "verdict": "timed_out" }),
    }
}

/// Parses one verdict line, the inverse of [`verdict_to_json`].
///
/// # Errors
///
/// A message naming the unknown or missing tag. An unrecognized verdict
/// is never coerced to a neighbour: `Passed` would land untested work
/// and `Failed` would eject a change on our own bug, so the only safe
/// answer is to refuse the line.
pub fn verdict_from_json(value: &serde_json::Value) -> Result<Verdict, String> {
    match value["verdict"].as_str() {
        Some("passed") => Ok(Verdict::Passed),
        Some("failed") => Ok(Verdict::Failed {
            exit_code: value["exit_code"]
                .as_i64()
                .and_then(|c| i32::try_from(c).ok()),
        }),
        Some("errored") => Ok(Verdict::Errored {
            provider: value["provider"].as_str().unwrap_or("remote").to_string(),
            detail: value["detail"].as_str().unwrap_or_default().to_string(),
        }),
        Some("timed_out") => Ok(Verdict::TimedOut),
        Some(other) => Err(format!("unknown verdict `{other}`")),
        None => Err("verdict line needs a string `verdict`".to_string()),
    }
}

/// An executor reached by spawning a helper and talking JSON lines.
pub struct ProtocolRunner {
    command: Vec<String>,
}

impl ProtocolRunner {
    /// A runner that spawns `command` (argv) once per batch.
    ///
    /// Once per batch, not once per job and not once for the runner's
    /// lifetime: it is the shape a VM supervisor already has, it gives
    /// the far side a natural place to tear down whatever it built, and
    /// a helper that dies takes one batch with it rather than every
    /// batch after it.
    #[must_use]
    pub fn new(command: Vec<String>) -> Self {
        Self { command }
    }

    /// Runs `jobs`, or says why it could not.
    fn talk(&self, jobs: &[Job]) -> Result<(ExecutorInfo, Vec<Verdict>), ExecutorError> {
        let Some((program, args)) = self.command.split_first() else {
            return Err(ExecutorError::Unavailable(
                "no helper command configured".into(),
            ));
        };
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| ExecutorError::Unavailable(format!("could not start `{program}`: {e}")))?;

        let mut stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");

        // The write runs on its own thread. A helper that answers as it
        // goes fills the stdout pipe while we are still filling its
        // stdin, and two processes each blocked on the other's full
        // buffer is a hang with no error and no timeout attached to it.
        let hello = serde_json::json!({ "protocol": PROTOCOL, "jobs": jobs.len() });
        let lines: Vec<String> = jobs
            .iter()
            .map(|j| job_to_json(j).map(|v| v.to_string()))
            .collect::<Result<Vec<String>, String>>()
            .map_err(ExecutorError::Protocol)?;
        let writer = std::thread::spawn(move || {
            let _ = writeln!(stdin, "{hello}");
            for line in lines {
                if writeln!(stdin, "{line}").is_err() {
                    break;
                }
            }
            drop(stdin);
        });

        let mut reader = BufReader::new(stdout);
        let mut first = String::new();
        let read = reader.read_line(&mut first).map_err(|e| {
            ExecutorError::Unavailable(format!("could not read from `{program}`: {e}"))
        })?;
        if read == 0 {
            let _ = writer.join();
            let _ = child.wait();
            return Err(ExecutorError::Unavailable(format!(
                "`{program}` closed without answering the handshake"
            )));
        }
        let hello: serde_json::Value = serde_json::from_str(first.trim()).map_err(|e| {
            ExecutorError::Protocol(format!(
                "`{program}` sent a handshake that is not JSON: {e}"
            ))
        })?;
        let name = hello["name"].as_str().unwrap_or(program).to_string();
        let spoken = hello["protocol"]
            .as_u64()
            .and_then(|p| u32::try_from(p).ok())
            .ok_or_else(|| {
                ExecutorError::Protocol(format!("`{program}` did not name a protocol version"))
            })?;
        if spoken != PROTOCOL {
            let _ = writer.join();
            let _ = child.kill();
            let _ = child.wait();
            return Err(ExecutorError::Protocol(format!(
                "`{name}` speaks protocol {spoken}, this build speaks {PROTOCOL}"
            )));
        }

        let mut verdicts = Vec::with_capacity(jobs.len());
        let mut protocol_error = None;
        for line in reader.lines() {
            if verdicts.len() == jobs.len() {
                break;
            }
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    protocol_error = Some(format!("`{name}` stopped mid-batch: {e}"));
                    break;
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str(&line)
                .map_err(|e| e.to_string())
                .and_then(|v: serde_json::Value| verdict_from_json(&v))
            {
                Ok(v) => verdicts.push(v),
                Err(why) => {
                    protocol_error = Some(format!("`{name}` sent an unusable verdict: {why}"));
                    break;
                }
            }
        }
        let _ = writer.join();
        let _ = child.kill();
        let _ = child.wait();

        if let Some(why) = protocol_error {
            return Err(ExecutorError::Protocol(why));
        }
        // A helper that stopped early judged the jobs it answered for
        // and nobody else. Filling the tail with `Errored` keeps the
        // batch index-aligned and says the true thing about the rest;
        // returning a short list instead would be refused wholesale by
        // the queue, throwing away answers we actually have.
        let short = jobs.len() - verdicts.len();
        if short > 0 {
            let answered = verdicts.len();
            for _ in 0..short {
                verdicts.push(Verdict::Errored {
                    provider: name.clone(),
                    detail: format!("stopped after {answered} of {} verdicts", jobs.len()),
                });
            }
        }
        Ok((
            ExecutorInfo {
                name,
                protocol: spoken,
            },
            verdicts,
        ))
    }
}

impl CiExecutor for ProtocolRunner {
    fn info(&mut self) -> Result<ExecutorInfo, ExecutorError> {
        self.talk(&[]).map(|(info, _)| info)
    }

    fn run(&mut self, jobs: &[Job]) -> Result<Vec<Verdict>, ExecutorError> {
        self.talk(jobs).map(|(_, verdicts)| verdicts)
    }
}
