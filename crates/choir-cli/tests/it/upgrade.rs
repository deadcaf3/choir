//! `choir node upgrade`: the decisions without a node, and the copy.
//!
//! The build and the shelf need a toolchain or a network; what is pinned
//! here is everything before and after them: which source was asked
//! for, where the binaries go, how a version line becomes a stamp, and
//! that placing binaries is a rename that leaves nothing half-written.

use choir_cli::upgrade::{self, cargo_home_for, place, stamp_of, Source, BINARIES};
use std::path::{Path, PathBuf};

fn parse(args: &[&str]) -> Result<upgrade::Options, String> {
    upgrade::parse(args, PathBuf::from("/tmp/choir-upgrade-default"))
}

#[test]
fn one_source_is_required_and_two_are_refused() {
    let error = parse(&[]).expect_err("none");
    assert!(
        error.contains("--from") && error.contains("--source"),
        "{error}"
    );
    let error = parse(&["--from", "https://n", "--source", "."]).expect_err("both");
    assert!(error.contains("one question"), "{error}");
    let error = parse(&["--from", "n.example"]).expect_err("not a url");
    assert!(error.contains("http://"), "{error}");
}

#[test]
fn the_source_and_the_target_are_recorded_absolutely() {
    let options =
        parse(&["--from", "https://n.example/", "--into", "bin", "--dry-run"]).expect("parses");
    assert_eq!(options.source, Source::Shelf("https://n.example".into()));
    assert!(options.into.as_deref().is_some_and(Path::is_absolute));
    assert!(options.dry_run);
    let options = parse(&["--source", "checkout", "--state", "st"]).expect("parses");
    assert!(matches!(&options.source, Source::Checkout(dir) if dir.is_absolute()));
    assert!(options.state.ends_with("st"));
}

#[test]
fn a_version_line_names_its_commit_or_nothing() {
    assert_eq!(
        stamp_of("choir build 0123456789ab (stamp source: git)").as_deref(),
        Some("0123456789ab")
    );
    assert_eq!(
        stamp_of("choir build 0123456789ab+dirty (stamp source: git)").as_deref(),
        Some("0123456789ab")
    );
    assert_eq!(stamp_of("choir build unknown (stamp source: none)"), None);
    assert_eq!(stamp_of("something else"), None);
}

#[test]
fn the_installer_is_told_the_parent_of_a_bin_directory_and_nothing_else() {
    assert_eq!(
        cargo_home_for(Path::new("/home/x/.cargo/bin")).expect("bin"),
        PathBuf::from("/home/x/.cargo")
    );
    let error = cargo_home_for(Path::new("/home/x/tools")).expect_err("not bin");
    assert!(error.contains("--source"), "{error}");
}

#[test]
fn placing_copies_what_was_built_by_rename_and_keeps_the_mode() {
    let root = std::env::temp_dir().join(format!("choir-cli-upgrade-place-{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    let built = root.join("release");
    let into = root.join("bin");
    std::fs::create_dir_all(&built).expect("built");
    std::fs::create_dir_all(&into).expect("into");
    std::fs::write(into.join("choir"), "old\n").expect("old");
    for name in ["choir", "choir-node"] {
        std::fs::write(built.join(name), format!("new {name}\n")).expect("new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(built.join(name), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
    }
    let placed = place(&built, &into).expect("placed");
    assert_eq!(placed, vec!["choir".to_string(), "choir-node".to_string()]);
    assert_eq!(
        std::fs::read_to_string(into.join("choir")).unwrap(),
        "new choir\n"
    );
    assert!(!into.join("choir-mcp").exists(), "only what was built");
    assert!(
        !into.join(".choir.new").exists(),
        "nothing staged is left behind"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(into.join("choir-node"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "executable bits survive the copy");
    }
    assert_eq!(BINARIES.len(), 4);

    let error = place(&root.join("empty"), &into).expect_err("nothing built");
    assert!(error.contains("nothing to place"), "{error}");
}

#[test]
fn the_command_dry_runs_without_touching_anything() {
    let root = std::env::temp_dir().join(format!("choir-cli-upgrade-dry-{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    std::fs::create_dir_all(root.join("bin")).expect("bin");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_choir"))
        .args([
            "node",
            "upgrade",
            "--source",
            root.to_str().unwrap(),
            "--into",
            root.join("bin").to_str().unwrap(),
            "--state",
            root.join("state").to_str().unwrap(),
            "--dry-run",
        ])
        .output()
        .expect("runs");
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("cargo build --release"), "{text}");
    assert!(text.contains("nothing installed"), "{text}");
    assert!(
        std::fs::read_dir(root.join("bin"))
            .unwrap()
            .next()
            .is_none(),
        "nothing placed"
    );
}
