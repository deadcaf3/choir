//! The reference CI executor helper: runs a batch locally, over a pipe.
//!
//! Reads the D18 executor protocol on stdin, runs the batch with
//! [`choir_queue::local::LocalRunner`], and writes verdicts to stdout.
//! Two jobs at once, both real:
//!
//! It is the second party the protocol needs in order to be tested at
//! all -- a wire format with one implementation is a data structure with
//! extra steps. And it is the worked example for anyone writing a
//! driver for a real isolation boundary: a Firecracker or Cloud
//! Hypervisor helper differs from this file only in what it hands the
//! batch to, and this repository cannot build one because both require
//! KVM.
//!
//! Interaction failures are data, not a nonzero exit, the same rule
//! `choir-differential` follows: a helper that dies loudly tells the
//! host nothing it can attribute to a job.

use choir_queue::executor::{CiExecutor, Verdict, PROTOCOL};
use choir_queue::local::LocalRunner;
use choir_queue::remote::{job_from_json, verdict_to_json};
use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::BufReader::new(std::io::stdin());
    let mut lines = stdin.lines();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    // The handshake, before anything is read that could fail: a host
    // whose protocol we cannot speak must learn that from the version
    // rather than from a batch that half worked.
    let Some(Ok(hello)) = lines.next() else {
        return;
    };
    let hello: serde_json::Value = match serde_json::from_str(&hello) {
        Ok(v) => v,
        Err(_) => return,
    };
    let _ = writeln!(
        out,
        "{}",
        serde_json::json!({ "name": "choir-ci-local", "protocol": PROTOCOL })
    );
    let _ = out.flush();
    if hello["protocol"].as_u64() != Some(u64::from(PROTOCOL)) {
        return;
    }
    let count = hello["jobs"].as_u64().unwrap_or(0) as usize;

    // Collect the whole batch before running any of it. Streaming one
    // job at a time would answer sooner and cost the concurrency the
    // batch call exists for, which is the defect this seam was carved
    // out of `bool` to prevent.
    let mut jobs = Vec::with_capacity(count);
    let mut refusal = None;
    for _ in 0..count {
        let Some(Ok(line)) = lines.next() else {
            refusal = Some("host stopped sending jobs".to_string());
            break;
        };
        match serde_json::from_str(&line)
            .map_err(|e| e.to_string())
            .and_then(|v: serde_json::Value| job_from_json(&v))
        {
            Ok(job) => jobs.push(job),
            Err(why) => {
                refusal = Some(why);
                break;
            }
        }
    }

    // A job we could not read is a job we did not run, and saying so
    // for every remaining slot keeps the host's indices aligned.
    let verdicts = if let Some(why) = refusal {
        let mut answered = LocalRunner::new().run(&jobs).unwrap_or_default();
        while answered.len() < count {
            answered.push(Verdict::Errored {
                provider: "choir-ci-local".into(),
                detail: why.clone(),
            });
        }
        answered
    } else {
        match LocalRunner::new().run(&jobs) {
            Ok(v) => v,
            Err(e) => (0..count)
                .map(|_| Verdict::Errored {
                    provider: "choir-ci-local".into(),
                    detail: e.to_string(),
                })
                .collect(),
        }
    };

    for verdict in &verdicts {
        let _ = writeln!(out, "{}", verdict_to_json(verdict));
    }
    let _ = out.flush();
}
