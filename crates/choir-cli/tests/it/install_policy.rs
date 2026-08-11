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
        // The leg runs detached now. Without these, an ssh that meets a
        // prompt or a black-holed route waits forever, no receipt is
        // ever written, and that is indistinguishable from a run still
        // in progress.
        "BatchMode=yes",
        "ConnectTimeout=",
    ] {
        assert!(
            script.contains(option),
            "mirror push dropped {option}; every VM round trip pays a full handshake again"
        );
    }

    // The stage timings are the point: this script was once tuned
    // against a model of where its time went, the model was wrong, and
    // nothing in the output could have revealed that. A run that
    // reports connect/transfer/push separately settles it.
    for stage in [
        "connect %.1fs",
        "git push %.1fs",
        "box-local push %.1fs",
        "oplog %.1fs",
        "total %.1fs",
    ] {
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

    // rsync walked 707 files and 37.5 MB to move a delta git already
    // knows how to compute — 1.74 s even with nothing changed, because
    // almost every one of those files is an immutable content-addressed
    // object. Going back to it also resurrects two workarounds it
    // needed: recreating the mirror remote after .git/config was
    // clobbered, and a .gitignore filter to keep local-only files off
    // the VM, which git gives for free by only pushing commits.
    // Non-comment lines only: the comments above explain why rsync went
    // away and would otherwise match themselves.
    for line in script.lines().filter(|l| !l.trim_start().starts_with('#')) {
        assert!(
            !line.contains("rsync "),
            "the mirror transfer went back to rsync; git sends only the missing objects: {line}"
        );
    }

    // Two syncs in quick succession would otherwise push into the same
    // repo concurrently. flock is not stock on macOS, so mkdir is the
    // atomic primitive; it must wait rather than skip, or the newest
    // commit is the one that gets dropped.
    assert!(
        script.contains("mkdir \"$LOCK\""),
        "mirror push lost its lock; concurrent syncs race on the receiving repo"
    );

    // Both trips must go through the same option set, or the second one
    // opens its own connection and the multiplexing buys nothing.
    let rsync = script
        .lines()
        .find(|line| line.trim_start().starts_with("export GIT_SSH_COMMAND"))
        .expect("mirror push sends objects over the shared ssh connection");
    let ssh = script
        .lines()
        .find(|line| line.trim_start().starts_with("ssh \""))
        .expect("mirror push runs the box-local push over ssh");
    assert!(
        rsync.contains("ssh_opts") && ssh.contains("ssh_opts"),
        "the git transfer and the box-local push must share one option set:\n  {rsync}\n  {ssh}"
    );
}

/// The mirror leg runs detached, so nothing blocks on it — which means
/// its failures are invisible unless the receipt is both written and
/// read. Grepping for the reporting code would only prove it exists;
/// this runs it, because the interesting failure is a receipt-reader
/// that returns the wrong verdict rather than one that is missing.
#[test]
fn the_mirror_receipt_is_read_not_merely_written() {
    let driver = std::fs::read_to_string(repo_root().join("scripts/choirctl"))
        .expect("choirctl source");

    // Detached, and only after the canonical push returns: D21 ordering
    // survives backgrounding precisely because `set -e` stops before
    // this line when the canonical half fails.
    assert!(
        driver.contains("nohup sh \"$HERE/push_mirror.sh\""),
        "choirctl sync no longer backgrounds the mirror leg"
    );
    let sync = driver
        .find("sync)")
        .and_then(|start| driver[start..].find("nohup").map(|n| start + n))
        .expect("sync backgrounds the mirror");
    let canonical = driver[..sync]
        .rfind("push_canonical.sh")
        .expect("sync pushes canonical first");
    assert!(
        canonical < sync,
        "the mirror leg must start after the canonical push, or the follower can lead"
    );
    assert!(
        driver.contains("mirror_line"),
        "nothing surfaces the receipt; a detached failure would be invisible"
    );

    // Execute the verdict function itself, under the shell that runs it.
    let body: String = driver
        .lines()
        .skip_while(|l| !l.starts_with("mirror_outcome()"))
        .take_while(|l| *l != "}")
        .chain(std::iter::once("}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        body.starts_with("mirror_outcome()"),
        "choirctl no longer defines mirror_outcome"
    );

    let work = std::env::temp_dir().join(format!("choir-receipt-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let receipt = work.join("mirror.receipt");

    let verdict = |case: &str| -> String {
        let script = format!(
            "STATE={state}; RECEIPT={receipt}; {body}; mirror_outcome",
            state = work.display(),
            receipt = receipt.display(),
        );
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .output()
            .unwrap_or_else(|e| panic!("{case}: {e}"));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    assert_eq!(verdict("absent"), "NONE");
    std::fs::write(&receipt, "started 1 abc\nmirror: ...\nmirror updated\n").unwrap();
    assert_eq!(verdict("complete"), "OK");
    // A truncated receipt is the shape a killed or timed-out run leaves.
    std::fs::write(&receipt, "started 1 abc\nssh: connect timed out\n").unwrap();
    assert_eq!(verdict("truncated"), "FAILED");
    // The lock, not the receipt, is what separates running from dead.
    std::fs::create_dir_all(work.join("mirror.lock")).unwrap();
    assert_eq!(verdict("in flight"), "RUNNING");

    std::fs::remove_dir_all(work).ok();
}

/// The op log is the only state in the system with exactly one copy:
/// git bundles carry commits, and `ops.jsonl` has never been a git
/// object. This asserts the backup leg exists, that it verifies rather
/// than assumes, and — the property that actually matters — that it
/// never carries the signing key off the node that owns it.
#[test]
fn the_oplog_backup_carries_the_log_and_the_pin_but_never_the_key() {
    let script = std::fs::read_to_string(repo_root().join("scripts/push_mirror.sh"))
        .expect("scripts/push_mirror.sh");
    let code: String = script
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        code.contains("ops.jsonl"),
        "the mirror stopped backing up the op log; it exists in exactly one place again"
    );
    assert!(
        code.contains("node.fingerprint"),
        "the pin must travel with the log, or a restore silently appends under a new identity"
    );
    // The whole point of splitting key from log. A backup holding the
    // key lets whoever holds the backup keep signing as this node.
    assert!(
        !code.contains("node.key"),
        "the op-log backup must never carry node.key off the node that owns it"
    );
    // Verified, not hoped: a silent truncation reads exactly like a
    // successful backup until the day it is restored.
    assert!(
        code.contains("OP LOG BACKUP MISMATCH"),
        "the backup stopped comparing checksums; a truncated copy now looks like a good one"
    );
    // Atomic publish: an interrupted transfer must leave the previous
    // good backup, not a half-written log that still parses.
    assert!(
        code.contains("ops.jsonl.part") && code.contains("mv "),
        "the backup stopped writing .part then renaming; an interrupted run truncates the backup"
    );

    // Rehearsing the restore showed the log alone is not enough: a node
    // rebuilt from ops.jsonl refused to boot without --reviewers-file,
    // so the policy files have to travel or the backup restores a
    // ledger onto a node that will not start.
    let list = code
        .lines()
        .find(|l| l.contains("for f in") && l.contains("reviewers"))
        .expect("the policy-file backup list");
    for needed in ["keys", "reviewers", "protected-refs", "newcomer-audit.jsonl"] {
        assert!(
            list.contains(needed),
            "the policy backup dropped {needed}; a restore stops booting again"
        );
    }
    // Same rule as the key: a token or a private key in the backup turns
    // an availability measure into a credential-distribution channel.
    for forbidden in ["auth", ".pem", ".key"] {
        assert!(
            !list.contains(forbidden),
            "the policy backup list names {forbidden}; secrets must not travel with it"
        );
    }
    assert!(
        code.contains("policy.part"),
        "the policy backup stopped staging into .part; a failed extract leaves a partial policy set"
    );

    // The script is run as `sh`, never as the zsh in its shebang, and a
    // runtime-only failure here would skip the backup while the receipt
    // still ended in success. `sh -n` catches at least the syntax half.
    let syntax = std::process::Command::new("sh")
        .arg("-n")
        .arg(repo_root().join("scripts/push_mirror.sh"))
        .output()
        .expect("run sh -n");
    assert!(
        syntax.status.success(),
        "push_mirror.sh is not valid sh: {}",
        String::from_utf8_lossy(&syntax.stderr)
    );
}
