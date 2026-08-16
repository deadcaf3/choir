//! Explicit D23 runner invoked by `choir-bridge queue`.
//!
//! The bridge supplies three isolated worktrees. This process runs one argv
//! specification in all three — under an explicit environment, never this
//! process's inherited one — appends the observation, and prints only a
//! structured result. Interaction failures are data, not a nonzero exit.

use std::path::Path;

use choir_queue::differential::run_merged_vs_parents;
use choir_queue::differential_ledger::{
    adjudicate, effective_environment, environment_hash, load_command, record_observation, refresh,
    Revisions,
};

fn usage() -> ! {
    eprintln!(
        "usage: choir-differential run <command-file> <state-dir> <parent-a-oid> <parent-a-dir> <parent-b-oid> <parent-b-dir> <merged-oid> <merged-dir>\n       choir-differential adjudicate <command-file> <state-dir> <observation-id> real|spurious\n       choir-differential refresh <command-file> <state-dir>"
    );
    std::process::exit(2);
}

fn run(args: &[String]) -> Result<serde_json::Value, String> {
    let [command_file, state_dir, parent_a_oid, parent_a_dir, parent_b_oid, parent_b_dir, merged_oid, merged_dir] =
        args
    else {
        usage();
    };
    let command = load_command(Path::new(command_file))?;
    let environment = effective_environment(&command.env);
    let report = run_merged_vs_parents(
        &command.program,
        &command.args,
        Path::new(parent_a_dir),
        Path::new(parent_b_dir),
        Path::new(merged_dir),
        &environment,
        command.timeout_seconds.map(std::time::Duration::from_secs),
    )?;
    let revisions = Revisions {
        parent_a: parent_a_oid.clone(),
        parent_b: parent_b_oid.clone(),
        merged: merged_oid.clone(),
    };
    let recorded = record_observation(
        Path::new(state_dir),
        &command.snapshot_hash,
        &environment_hash(&environment),
        &revisions,
        &report,
    )?;
    Ok(serde_json::json!({
        "format_version": 1,
        "observation_id": recorded.observation_id,
        "merge": merged_oid,
        "report": recorded.report.to_json(),
        "calibration": recorded.calibration,
    }))
}

fn adjudicate_one(args: &[String]) -> Result<serde_json::Value, String> {
    let [command_file, state_dir, observation_id, verdict] = args else {
        usage();
    };
    let command = load_command(Path::new(command_file))?;
    let observation_id = observation_id
        .parse::<u64>()
        .map_err(|_| "observation id must be an integer".to_string())?;
    let real = match verdict.as_str() {
        "real" => true,
        "spurious" => false,
        _ => return Err("adjudication must be real or spurious".to_string()),
    };
    adjudicate(
        Path::new(state_dir),
        &command.snapshot_hash,
        observation_id,
        real,
    )
}

fn refresh_one(args: &[String]) -> Result<serde_json::Value, String> {
    let [command_file, state_dir] = args else {
        usage();
    };
    let command = load_command(Path::new(command_file))?;
    refresh(Path::new(state_dir), &command.snapshot_hash)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((command, rest)) = args.split_first() else {
        usage();
    };
    let result = match command.as_str() {
        "run" => run(rest),
        "adjudicate" => adjudicate_one(rest),
        "refresh" => refresh_one(rest),
        _ => usage(),
    };
    match result {
        Ok(value) => println!("{value}"),
        Err(error) => {
            eprintln!("choir-differential: {error}");
            std::process::exit(1);
        }
    }
}
