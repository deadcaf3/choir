//! Phase-0 integrated gate spike (DECISIONS.md).
//!
//! End to end on one machine: provision N workspaces from a base tree via
//! CoW clone (macOS `clonefile(2)` here; btrfs/ZFS on the Linux target),
//! have every workspace submit ops through the single-writer sequencer, then
//! push all concurrent edits of a shared file through the merge pipeline.
//! Prints the gate report: provisioning percentiles, decision-latency
//! percentiles, and merge outcomes (clean vs first-class conflict).
//!
//! # Where this sits
//!
//! `docs/architecture.md` is the map of the whole workspace.
//! This crate is the Phase-0 gate binary: provisioning and decision-latency percentiles, and merge outcomes.
//!
//! It builds on [`choir_merge`], [`choir_oplog`] and [`choir_sequencer`].

use choir_merge::{MergeOutcome, Pipeline};
use choir_oplog::MemLog;
use choir_sequencer::Sequencer;
use std::path::Path;
use std::time::{Duration, Instant};

const WORKSPACES: usize = 16;
const FILES_IN_BASE: usize = 500;

#[cfg(target_os = "macos")]
fn cow_clone(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::raw::{c_char, c_int, c_uint};
    extern "C" {
        fn clonefile(src: *const c_char, dst: *const c_char, flags: c_uint) -> c_int;
    }
    let s = std::ffi::CString::new(src.as_os_str().as_encoded_bytes()).unwrap();
    let d = std::ffi::CString::new(dst.as_os_str().as_encoded_bytes()).unwrap();
    if unsafe { clonefile(s.as_ptr(), d.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
fn cow_clone(src: &Path, dst: &Path) -> std::io::Result<()> {
    // Portability fallback only; the Linux target uses btrfs/ZFS reflinks.
    fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            let to = dst.join(e.file_name());
            if e.file_type()?.is_dir() {
                copy_dir(&e.path(), &to)?;
            } else {
                std::fs::copy(e.path(), to)?;
            }
        }
        Ok(())
    }
    copy_dir(src, dst)
}

fn percentile(sorted: &[Duration], q: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * q) as usize]
}

fn main() {
    let work = std::env::temp_dir().join(format!("choir-spike-{}", std::process::id()));
    let base = work.join("base");
    std::fs::create_dir_all(&base).unwrap();

    // shared.txt: widely spaced distinct regions, one per workspace (edits
    // must fold cleanly). hot.txt: a single line every workspace edits
    // (merges must refuse to resolve silently).
    let mut shared = Vec::new();
    for i in 0..(WORKSPACES * 4) {
        shared.push(format!("region {i}"));
    }
    let shared_base = shared.join("\n") + "\n";
    std::fs::write(base.join("shared.txt"), &shared_base).unwrap();
    let hot_base = "hot line\n".to_string();
    std::fs::write(base.join("hot.txt"), &hot_base).unwrap();
    for i in 0..FILES_IN_BASE {
        let d = base.join(format!("dir{}", i % 50));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(format!("f{i}.txt")), format!("file {i}\n")).unwrap();
    }

    // 1) Provision N workspaces via CoW clone.
    let mut provision_times = Vec::new();
    let mut ws_paths = Vec::new();
    for w in 0..WORKSPACES {
        let dst = work.join(format!("ws{w}"));
        let t = Instant::now();
        cow_clone(&base, &dst).expect("workspace provisioning");
        provision_times.push(t.elapsed());
        ws_paths.push(dst);
    }
    provision_times.sort();

    // 2) Every workspace edits its own region of shared.txt and the single
    //    line of hot.txt, then submits an op through the sequencer,
    //    concurrently.
    let sequencer = Sequencer::spawn(Box::new(MemLog::new()));
    let mut threads = Vec::new();
    for (w, path) in ws_paths.iter().enumerate() {
        let handle = sequencer.handle();
        let path = path.clone();
        let base_text = shared_base.clone();
        threads.push(std::thread::spawn(move || {
            let mut lines: Vec<String> = base_text.lines().map(String::from).collect();
            lines[w * 4 + 2] = format!("region {} edited by ws{w}", w * 4 + 2);
            let shared_edit = lines.join("\n") + "\n";
            let hot_edit = format!("hot line edited by ws{w}\n");
            std::fs::write(path.join("shared.txt"), &shared_edit).unwrap();
            std::fs::write(path.join("hot.txt"), &hot_edit).unwrap();
            let accepted = handle.submit(&format!("ws{w}"), shared_edit.clone().into_bytes());
            (shared_edit, hot_edit, accepted.decision_latency)
        }));
    }
    let mut shared_edits = Vec::new();
    let mut hot_edits = Vec::new();
    let mut latencies = Vec::new();
    for t in threads {
        let (shared_edit, hot_edit, lat) = t.join().unwrap();
        shared_edits.push(shared_edit);
        hot_edits.push(hot_edit);
        latencies.push(lat);
    }
    latencies.sort();
    let log = sequencer.shutdown();
    assert_eq!(log.len(), WORKSPACES as u64, "zero op loss");

    // 3a) Disjoint-region edits must fold cleanly through the pipeline.
    let pipeline = Pipeline::default_v1();
    let mut acc = shared_edits[0].clone();
    let mut clean = 0usize;
    let mut wrong_conflicts = 0usize;
    for edit in &shared_edits[1..] {
        match pipeline.merge(&shared_base, &acc, edit).outcome {
            MergeOutcome::Resolved(merged) => {
                clean += 1;
                acc = merged;
            }
            MergeOutcome::Conflict { .. } => wrong_conflicts += 1,
            MergeOutcome::Unavailable(_) => unreachable!(),
        }
    }
    let regions_merged = (0..WORKSPACES)
        .filter(|w| acc.contains(&format!("edited by ws{w}")))
        .count();

    // 3b) Same-line edits must surface as first-class conflicts, never a
    //     silent pick.
    let mut hot_conflicts = 0usize;
    let mut silent_hot_merges = 0usize;
    for pair in hot_edits.windows(2) {
        match pipeline.merge(&hot_base, &pair[0], &pair[1]).outcome {
            MergeOutcome::Conflict { .. } => hot_conflicts += 1,
            MergeOutcome::Resolved(_) => silent_hot_merges += 1,
            MergeOutcome::Unavailable(_) => unreachable!(),
        }
    }

    std::fs::remove_dir_all(&work).ok();

    println!("== Phase-0 integrated gate report ==");
    println!(
        "workspaces: {WORKSPACES} (gate: >10) | base tree: {FILES_IN_BASE} files + shared.txt"
    );
    println!(
        "provisioning (CoW clone): p50={:?} p90={:?} max={:?} (target p50 < 50 ms warm)",
        percentile(&provision_times, 0.5),
        percentile(&provision_times, 0.9),
        provision_times.last().unwrap()
    );
    println!(
        "sequencer decision latency: p50={:?} p99={:?} (gate: < 100 ms, CI excluded)",
        percentile(&latencies, 0.5),
        percentile(&latencies, 0.99)
    );
    println!(
        "disjoint-region fold: {clean}/{} clean merges, {regions_merged}/{WORKSPACES} regions present, {wrong_conflicts} spurious conflicts",
        WORKSPACES - 1
    );
    println!(
        "same-line (hot) merges: {hot_conflicts}/{} first-class conflicts, {silent_hot_merges} silent picks (must be 0)",
        WORKSPACES - 1
    );

    let pass = WORKSPACES > 10
        && percentile(&provision_times, 0.5) < Duration::from_millis(50)
        && percentile(&latencies, 0.99) < Duration::from_millis(100)
        && wrong_conflicts == 0
        && regions_merged == WORKSPACES
        && silent_hot_merges == 0;
    println!(
        "GATE ({} dev-machine): {}",
        std::env::consts::OS,
        if pass { "PASS" } else { "FAIL" }
    );
    std::process::exit(if pass { 0 } else { 1 });
}
