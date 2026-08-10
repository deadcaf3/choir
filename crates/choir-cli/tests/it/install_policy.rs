//! The dogfood installer must not silently drop an explicitly enabled
//! review gate. Test the pure plist renderer rather than touching launchd.

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn render(protected: Option<&str>) -> String {
    let script = repo_root().join("scripts/flip/render_node_plist.sh");
    let mut command = std::process::Command::new("sh");
    command.arg(script).args([
        "com.example.node",
        "/opt/choir-node",
        "/srv/repos",
        "8417",
        "/state/auth",
        "/state/keys",
        "/state/reviewers",
        "/state/node.log",
        "owner/repo.git",
        "/state/newcomer-audit.jsonl",
        "/state/newcomer-adjudications.jsonl",
    ]);
    if let Some(path) = protected {
        command.arg(path);
    }
    let output = command.output().expect("render plist");
    assert!(output.status.success());
    String::from_utf8(output.stdout).expect("UTF-8 plist")
}

#[test]
fn explicit_policy_renders_all_three_review_gates_or_none() {
    let open = render(None);
    for required in [
        "--newcomer-audit",
        "/state/newcomer-audit.jsonl",
        "--newcomer-adjudications",
        "/state/newcomer-adjudications.jsonl",
    ] {
        assert!(open.contains(required), "install omits {required}");
    }
    for flag in [
        "--require-assignment",
        "--protected-refs",
        "--require-review",
    ] {
        assert!(!open.contains(flag), "ungated install contains {flag}");
    }

    let protected = render(Some("/state/protected-refs"));
    let positions: Vec<usize> = [
        "--require-assignment",
        "--protected-refs",
        "/state/protected-refs",
        "--require-review",
    ]
    .iter()
    .map(|needle| protected.find(needle).expect("policy argument"))
    .collect();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "review policy arguments must stay complete and ordered"
    );

    let installer = std::fs::read_to_string(repo_root().join("scripts/flip/install_node.sh"))
        .expect("installer source");
    assert!(installer.contains("review-gates.enabled"));
    assert!(installer.contains("render_node_plist.sh"));
    assert!(installer.contains("validate_review_policy.sh"));
    let here = installer
        .find("HERE=")
        .expect("installer defines helper directory");
    for helper in ["validate_review_policy.sh", "render_node_plist.sh"] {
        assert!(
            here < installer.find(helper).expect("installer invokes helper"),
            "installer must define HERE before invoking {helper}"
        );
    }
}

/// The Linux sibling of [`render`], fed byte-identical arguments.
fn render_unit(protected: Option<&str>) -> String {
    let script = repo_root().join("scripts/flip/render_node_service.sh");
    let mut command = std::process::Command::new("sh");
    command.arg(script).args([
        "com.example.node",
        "/opt/choir-node",
        "/srv/repos",
        "8417",
        "/state/auth",
        "/state/keys",
        "/state/reviewers",
        "/state/node.log",
        "owner/repo.git",
        "/state/newcomer-audit.jsonl",
        "/state/newcomer-adjudications.jsonl",
    ]);
    if let Some(path) = protected {
        command.arg(path);
    }
    let output = command.output().expect("render unit");
    assert!(output.status.success());
    String::from_utf8(output.stdout).expect("UTF-8 unit")
}

/// The `ProgramArguments` array only — `StandardOutPath` and `Label` are
/// `<string>` elements too, and counting them would compare the plist's
/// supervision settings against the unit's argument list.
fn plist_argv(plist: &str) -> Vec<String> {
    let array = plist
        .split_once("<array>")
        .and_then(|(_, rest)| rest.split_once("</array>"))
        .map(|(inner, _)| inner)
        .expect("ProgramArguments array");
    let mut argv = Vec::new();
    let mut rest = array;
    while let Some(start) = rest.find("<string>") {
        let after = &rest[start + "<string>".len()..];
        let (value, tail) = after.split_once("</string>").expect("closed <string>");
        argv.push(value.to_string());
        rest = tail;
    }
    argv
}

/// Split on a single space deliberately: a double space yields an empty
/// element, which is how the empty-policy splice is caught below.
fn unit_argv(unit: &str) -> Vec<String> {
    let line = unit
        .lines()
        .find(|line| line.starts_with("ExecStart="))
        .expect("unit defines ExecStart");
    line["ExecStart=".len()..]
        .split(' ')
        .map(str::to_string)
        .collect()
}

#[test]
fn both_supervisors_launch_the_node_with_the_same_arguments() {
    for protected in [None, Some("/state/protected-refs")] {
        let plist = plist_argv(&render(protected));
        let unit = unit_argv(&render_unit(protected));

        // Without this the whole test passes vacuously when a renderer
        // rejects its arguments and prints usage to stderr — which is
        // exactly how the first version of this check reported success
        // while comparing nothing to nothing.
        assert!(
            plist.len() >= 10,
            "extracted {} arguments; the renderer did not run",
            plist.len()
        );
        assert!(
            !unit.iter().any(String::is_empty),
            "unit ExecStart carries an empty argument (a spliced-in empty \
             policy leaves a double space): {unit:?}"
        );
        assert_eq!(
            plist, unit,
            "launchd and systemd must start the node with identical \
             arguments; a flag added to one supervisor and not the other \
             is a node running without the gate its operator configured"
        );
    }
}

#[test]
fn the_linux_installer_carries_the_same_policy_wiring() {
    let installer = std::fs::read_to_string(repo_root().join("scripts/flip/install_node_linux.sh"))
        .expect("linux installer source");
    assert!(installer.contains("review-gates.enabled"));
    assert!(installer.contains("render_node_service.sh"));
    assert!(installer.contains("validate_review_policy.sh"));
    let here = installer
        .find("HERE=")
        .expect("installer defines helper directory");
    for helper in ["validate_review_policy.sh", "render_node_service.sh"] {
        assert!(
            here < installer.find(helper).expect("installer invokes helper"),
            "installer must define HERE before invoking {helper}"
        );
    }
    // The macOS installer builds in place; this one must not, because the
    // host it targets cannot compile the workspace.
    assert!(
        !installer.contains("cargo build"),
        "the Linux installer must not build on the node host"
    );
}

fn validate(keys: &str, reviewers: &str, protected: &str) -> std::process::ExitStatus {
    let work = std::env::temp_dir().join(format!("choir-policy-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("keys"), keys).unwrap();
    std::fs::write(work.join("reviewers"), reviewers).unwrap();
    std::fs::write(work.join("protected"), protected).unwrap();
    let status = std::process::Command::new("sh")
        .arg(repo_root().join("scripts/flip/validate_review_policy.sh"))
        .args([
            work.join("keys"),
            work.join("reviewers"),
            work.join("protected"),
        ])
        .stderr(std::process::Stdio::null())
        .status()
        .expect("validate review policy");
    std::fs::remove_dir_all(work).ok();
    status
}

#[test]
fn review_policy_validation_fails_closed() {
    let keys = "operator/writer aa\nreview-a/agent bb\nreview-a/second dd\nreview-b/agent cc\n";
    assert!(validate(
        keys,
        "review-a/agent\nreview-b/agent\n",
        "owner/repo.git:refs/heads/main\n"
    )
    .success());
    assert!(!validate(
        keys,
        "review-a/agent\nreview-a/second\n",
        "owner/repo.git:refs/heads/main\n"
    )
    .success());
    assert!(!validate(
        keys,
        "review-a/agent\nreview-b/missing\n",
        "owner/repo.git:refs/heads/main\n"
    )
    .success());
    assert!(!validate(keys, "review-a/agent\nreview-b/agent\n", "").success());
}

/// The mirror push makes two round trips to a VM ~275 ms away, and a
/// fresh SSH handshake to it measured 3.78 s against 0.55 s on a reused
/// connection. Losing the multiplexing options silently triples the
/// cost of `choirctl sync` — nothing fails, it just gets slow again,
/// which is exactly the kind of regression no other check would catch.
#[test]
fn the_mirror_push_reuses_one_ssh_connection() {
    let script = std::fs::read_to_string(repo_root().join("scripts/push_mirror.sh"))
        .expect("mirror push source");

    for option in [
        "ControlMaster=auto",
        "ControlPath=",
        // 60s was too short to ever hit: syncs are minutes apart, so
        // the master had always expired and every sync paid a full
        // handshake anyway. The window has to span a working session.
        "ControlPersist=600",
        // Without this ssh offers the agent key first and the server
        // refuses it: one wasted round trip before the real key.
        "IdentitiesOnly=yes",
    ] {
        assert!(
            script.contains(option),
            "mirror push dropped {option}; every VM round trip pays a full handshake again"
        );
    }

    // The stage timings are the point: this script was once tuned
    // against a model of where its time went, the model was wrong, and
    // nothing in the output could have revealed that. A run that
    // reports connect/rsync/push separately settles it.
    for stage in ["connect %.1fs", "rsync %.1fs", "box-local push %.1fs", "total %.1fs"] {
        assert!(
            script.contains(stage),
            "mirror push stopped reporting {stage}; the next slowdown gets guessed at again"
        );
    }

    // choirctl runs this as `sh push_mirror.sh`, so the #!/bin/zsh line
    // is never consulted and /bin/sh on macOS is bash 3.2. A zsh-only
    // builtin therefore fails at runtime, and under `set -e` that means
    // the mirror push is skipped while the canonical half still reports
    // success — which is how a sync once landed on the node and silently
    // never reached the mirror. `sh -n` cannot catch it: the syntax is
    // fine, the command just does not exist.
    let driver = std::fs::read_to_string(repo_root().join("scripts/choirctl"))
        .expect("choirctl source");
    assert!(
        driver.contains("sh \"$HERE/push_mirror.sh\""),
        "choirctl no longer runs the mirror push with sh; revisit the shell assumptions below"
    );
    for line in script.lines().filter(|l| !l.trim_start().starts_with('#')) {
        for zshism in ["zmodload", "EPOCHREALTIME", "setopt", "autoload"] {
            assert!(
                !line.contains(zshism),
                "mirror push uses the zsh-only {zshism} but is run with sh: {line}"
            );
        }
    }

    // Proving it parses is not proving it runs. Execute the script's own
    // clock under the shell that actually invokes it.
    let now_def = script
        .lines()
        .find(|line| line.starts_with("now()"))
        .expect("mirror push defines now()");
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{now_def}; now"))
        .output()
        .expect("run now() under sh");
    let stamp = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        stamp.parse::<f64>().map(|t| t > 1.0e9).unwrap_or(false),
        "now() did not produce a unix timestamp under sh, got {stamp:?} (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );

    // main and tags go to Forgejo in one push, not two. The repo has no
    // tags at all, so the second push was a round trip to a shared-core
    // VM to say nothing: 0.93s for the pair against 0.46s combined,
    // measured on the mirror.
    assert!(
        script.contains("git push -q mirror main --tags"),
        "the box-local push split main and tags into two Forgejo round trips again"
    );

    // Both trips must go through the same option set, or the second one
    // opens its own connection and the multiplexing buys nothing.
    let rsync = script
        .lines()
        .find(|line| line.trim_start().starts_with("rsync "))
        .expect("mirror push runs rsync");
    let ssh = script
        .lines()
        .find(|line| line.trim_start().starts_with("ssh \""))
        .expect("mirror push runs the box-local push over ssh");
    assert!(
        rsync.contains("ssh_opts") && ssh.contains("ssh_opts"),
        "rsync and the box-local push must share one option set:\n  {rsync}\n  {ssh}"
    );
}
