//! The unit files, and the commands that would load them.
//!
//! Nothing here runs `launchctl` or `systemctl`. A test that actually
//! installed would load a job on the machine running the suite, under a
//! label a real node already uses — so the boundary is drawn at the
//! rendering and the argv, and the refusals are exercised through the
//! binary, which exits before it reaches a service manager.

use choir_cli::supervise::{in_build_directory, Action, Supervisor, LABEL};
use std::path::{Path, PathBuf};

fn exe() -> PathBuf {
    PathBuf::from("/home/example/.local/bin/choir")
}

fn state() -> PathBuf {
    PathBuf::from("/home/example/.choir")
}

/// The point of the whole rewrite: the unit names a command, so a daemon
/// flag changing later never means re-rendering supervision. If a path
/// like `--auth-file` ever appears in here again, that property is gone.
#[test]
fn the_unit_names_a_command_not_a_configuration() {
    for supervisor in [Supervisor::Launchd, Supervisor::Systemd] {
        let unit = supervisor.render(&exe(), &state(), 8417, &[]);
        assert!(unit.contains("node"), "{supervisor:?}");
        assert!(unit.contains("serve"), "{supervisor:?}");
        assert!(!unit.contains("--auth-file"), "{supervisor:?}: {unit}");
        assert!(!unit.contains("--keys-file"), "{supervisor:?}: {unit}");
        assert!(!unit.contains("repos"), "{supervisor:?}: {unit}");
    }
}

#[test]
fn both_supervisors_run_the_same_argv() {
    let launchd = Supervisor::Launchd.argv(&exe(), &state(), 8417, &[]);
    let systemd = Supervisor::Systemd.argv(&exe(), &state(), 8417, &[]);
    assert_eq!(launchd, systemd);
    assert_eq!(
        launchd,
        vec![
            "/home/example/.local/bin/choir",
            "node",
            "serve",
            "--state",
            "/home/example/.choir",
            "--port",
            "8417",
        ]
    );
}

/// Daemon flags reach the supervised node the same way they reach a
/// hand-started one, so the two are the same command.
#[test]
fn extra_daemon_flags_go_after_a_double_dash() {
    let extra = ["--rate-limit-api".to_string(), "60".to_string()];
    let argv = Supervisor::Launchd.argv(&exe(), &state(), 8417, &extra);
    let at = argv.iter().position(|a| a == "--").expect("a separator");
    assert_eq!(&argv[at + 1..], ["--rate-limit-api", "60"]);
}

#[test]
fn both_write_the_log_where_node_logs_reads_it() {
    for supervisor in [Supervisor::Launchd, Supervisor::Systemd] {
        let unit = supervisor.render(&exe(), &state(), 8417, &[]);
        assert!(
            unit.contains("/home/example/.choir/node.log"),
            "{supervisor:?}: {unit}"
        );
    }
}

#[test]
fn both_ask_to_be_restarted() {
    assert!(Supervisor::Launchd
        .render(&exe(), &state(), 8417, &[])
        .contains("<key>KeepAlive</key><true/>"));
    assert!(Supervisor::Systemd
        .render(&exe(), &state(), 8417, &[])
        .contains("Restart=always"));
}

/// `bootout` then `bootstrap`, never `kickstart -k`: a kick relaunches
/// the definition launchd cached, so a unit whose arguments changed
/// restarts cleanly into the old ones.
#[test]
fn installing_tears_the_launchd_job_down_before_loading_it() {
    let steps = Supervisor::Launchd.commands(Action::Install, Path::new("/home/example"));
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0][1], "bootout");
    assert_eq!(steps[1][1], "bootstrap");
    assert!(!steps.iter().any(|s| s.contains(&"kickstart".to_string())));
}

#[test]
fn units_land_where_each_service_manager_looks() {
    let home = Path::new("/home/example");
    assert_eq!(
        Supervisor::Launchd.unit_path(home),
        home.join(format!("Library/LaunchAgents/{LABEL}.plist"))
    );
    assert_eq!(
        Supervisor::Systemd.unit_path(home),
        home.join(".config/systemd/user/choir-node.service")
    );
}

/// A plist containing a raw `&` is one launchctl refuses to parse, with
/// an error naming the file rather than the character.
#[test]
fn a_path_with_xml_in_it_is_escaped() {
    let odd = PathBuf::from("/home/a&b/.choir");
    let unit = Supervisor::Launchd.render(&exe(), &odd, 8417, &[]);
    assert!(unit.contains("/home/a&amp;b/.choir"), "{unit}");
    assert!(!unit.contains("/home/a&b/"), "{unit}");
}

/// systemd splits `ExecStart` on whitespace, so a state directory with a
/// space in it would otherwise become two arguments and the node would
/// start against a root that does not exist.
#[test]
fn a_path_with_a_space_survives_execstart() {
    let odd = PathBuf::from("/home/some one/.choir");
    let unit = Supervisor::Systemd.render(&exe(), &odd, 8417, &[]);
    let exec = unit
        .lines()
        .find(|l| l.starts_with("ExecStart="))
        .expect("an ExecStart");
    assert!(exec.contains("\"/home/some one/.choir\""), "{exec}");
}

/// `CARGO_TARGET_DIR` renames `target`, so the check is the profile
/// directory cargo actually writes into.
#[test]
fn a_build_directory_is_recognised_however_target_is_named() {
    assert!(in_build_directory(Path::new("/w/target/release/choir")));
    assert!(in_build_directory(Path::new("/w/target/debug/choir")));
    assert!(in_build_directory(Path::new("/tmp/anything/debug/choir")));
    assert!(!in_build_directory(Path::new("/usr/local/bin/choir")));
    assert!(!in_build_directory(Path::new(
        "/home/example/.local/bin/choir"
    )));
}
