//! The dogfood installer must not silently drop an explicitly enabled
//! review gate. Test the pure plist renderer rather than touching launchd.

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Writes the repos file both renderers read in place of the old single
/// positional repo. Unique per call: these tests run on parallel threads
/// inside one process, so a pid-keyed name would collide.
fn repos_file(entries: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("choir-repos-list-{}-{n}", std::process::id()));
    std::fs::write(&path, entries).unwrap();
    path
}

fn render_output(repos: &str, protected: Option<&str>) -> std::process::Output {
    let script = repo_root().join("scripts/flip/render_node_plist.sh");
    let repos_path = repos_file(repos);
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
    ]);
    command.arg(&repos_path).args([
        "/state/newcomer-audit.jsonl",
        "/state/newcomer-adjudications.jsonl",
    ]);
    if let Some(path) = protected {
        command.arg(path);
    }
    let output = command.output().expect("render plist");
    std::fs::remove_file(repos_path).ok();
    output
}

fn render(protected: Option<&str>) -> String {
    let output = render_output("owner/repo.git\n", protected);
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

/// The Linux sibling of [`render_output`], fed byte-identical arguments.
fn render_unit_output(repos: &str, protected: Option<&str>) -> std::process::Output {
    let script = repo_root().join("scripts/flip/render_node_service.sh");
    let repos_path = repos_file(repos);
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
    ]);
    command.arg(&repos_path).args([
        "/state/newcomer-audit.jsonl",
        "/state/newcomer-adjudications.jsonl",
    ]);
    if let Some(path) = protected {
        command.arg(path);
    }
    let output = command.output().expect("render unit");
    std::fs::remove_file(repos_path).ok();
    output
}

fn render_unit(protected: Option<&str>) -> String {
    let output = render_unit_output("owner/repo.git\n", protected);
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

/// The repos file is what lets a new repo land as an appended line plus
/// a reinstall instead of a renderer signature change. Every entry must
/// reach both supervisors, identically ordered — and an empty list must
/// refuse to render, because a node with no `--create` installs no
/// pre-receive hook and its pushes are silently never sequenced.
#[test]
fn the_repos_file_renders_every_entry_and_refuses_an_empty_list() {
    let repos = "# comment\n\nowner/repo.git\nsecond/other.git\n";
    let plist_out = render_output(repos, Some("/state/protected-refs"));
    assert!(plist_out.status.success());
    let unit_out = render_unit_output(repos, Some("/state/protected-refs"));
    assert!(unit_out.status.success());

    let plist = plist_argv(&String::from_utf8(plist_out.stdout).expect("UTF-8 plist"));
    let unit = unit_argv(&String::from_utf8(unit_out.stdout).expect("UTF-8 unit"));
    assert_eq!(
        plist, unit,
        "a multi-repo list must reach both supervisors identically"
    );
    let created: Vec<&str> = plist
        .windows(2)
        .filter(|pair| pair[0] == "--create")
        .map(|pair| pair[1].as_str())
        .collect();
    assert_eq!(
        created,
        ["owner/repo.git", "second/other.git"],
        "comments and blank lines are skipped; entry order is preserved"
    );

    for empty in ["", "# only a comment\n"] {
        assert!(
            !render_output(empty, None).status.success(),
            "the plist renderer must refuse a repos list with no entries"
        );
        assert!(
            !render_unit_output(empty, None).status.success(),
            "the unit renderer must refuse a repos list with no entries"
        );
    }

    // Both installers own the file's lifecycle: seed it once, and append
    // only a repo named explicitly, as an exact whole line — a bare
    // re-run must never resurrect a line the operator deleted.
    for name in [
        "scripts/flip/install_node.sh",
        "scripts/flip/install_node_linux.sh",
    ] {
        let installer = std::fs::read_to_string(repo_root().join(name)).expect(name);
        assert!(
            installer.contains("repos.list"),
            "{name} must wire the repos list"
        );
        assert!(
            installer.contains("grep -qxF"),
            "{name} must append only an exact missing line"
        );
    }
}

/// The follower feed must mirror every repo the node serves, not just
/// the canonical one: after the first imported repo, the node host was
/// the only holder of that repo's git objects. The remote command is
/// extracted from choirctl and executed here with real git against a
/// scratch HOME, because a loop that is merely grepped for could still
/// skip everything and read like success.
#[test]
fn the_follower_feed_pushes_every_listed_repo_and_names_the_unmirrored() {
    let driver = std::fs::read_to_string(repo_root().join("scripts/choirctl"))
        .expect("choirctl source");

    // Both remote-mode call sites go through the one function; a stray
    // hardcoded single-repo push would silently shrink the follower.
    assert!(
        driver.matches("follower_feed").count() >= 3,
        "choirctl must define follower_feed and call it from mirror and sync"
    );
    assert!(
        !driver.contains("repos/choir/choir.git\" push"),
        "a hardcoded single-repo follower push survives in choirctl"
    );
    // D21 ordering inside the remote sync branch: canonical, follower,
    // then the backup pull.
    let sync = driver.find("sync)").expect("sync branch");
    let canonical = driver[sync..].find("push_canonical.sh").expect("canonical leg") + sync;
    let follower = driver[canonical..].find("follower_feed").expect("follower leg") + canonical;
    let backup = driver[follower..].find("pull_backup.sh").expect("backup leg") + follower;
    assert!(canonical < follower && follower < backup);

    // The command that actually runs on the node host.
    let def = driver.find("follower_feed()").expect("follower_feed defined");
    let body = &driver[def..];
    let start = body.find("node_ssh '").expect("one remote command") + "node_ssh '".len();
    let end = body[start..].find("'\n}").expect("remote command closes") + start;
    let remote = &body[start..end];

    let home = std::env::temp_dir().join(format!("choir-follower-{}", std::process::id()));
    std::fs::remove_dir_all(&home).ok();
    std::fs::create_dir_all(home.join(".choir/repos/agents")).unwrap();
    let sh = |cmd: &str| {
        std::process::Command::new("sh")
            .args(["-c", cmd])
            .env("HOME", &home)
            .output()
            .expect("sh runs")
    };

    // No repos.list: refuse, loudly.
    let out = sh(remote);
    assert!(!out.status.success(), "must refuse without a repos.list");
    assert!(String::from_utf8_lossy(&out.stderr).contains("no repos.list"));

    // A served repo with no forgejo remote is named but not fatal, and
    // comments are skipped.
    std::fs::write(
        home.join(".choir/repos.list"),
        "# comment\nagents/demo.git\n",
    )
    .unwrap();
    let git = |cmd: &str| assert!(sh(cmd).status.success(), "fixture git failed: {cmd}");
    git("git init -q --bare \"$HOME/.choir/repos/agents/demo.git\"");
    let out = sh(remote);
    assert!(out.status.success(), "an unconfigured follower must not fail the run");
    assert!(String::from_utf8_lossy(&out.stderr).contains("NO forgejo remote"));

    // Configured: the push happens for real, into a second bare repo.
    git("git init -q \"$HOME/work\" && cd \"$HOME/work\" \
         && git -c user.name=t -c user.email=t@t commit -q --allow-empty -m one \
         && git push -q \"$HOME/.choir/repos/agents/demo.git\" HEAD:refs/heads/main");
    git("git init -q --bare \"$HOME/follower.git\" \
         && git --git-dir \"$HOME/.choir/repos/agents/demo.git\" remote add forgejo \"$HOME/follower.git\"");
    let out = sh(remote);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("follower updated: agents/demo.git"));
    let shown = sh("git --git-dir \"$HOME/follower.git\" rev-parse refs/heads/main");
    assert!(shown.status.success(), "the follower never received the ref");

    // A configured push that fails is the one thing that fails the run.
    std::fs::write(
        home.join(".choir/repos.list"),
        "agents/demo.git\nagents/bad.git\n",
    )
    .unwrap();
    git("git init -q --bare \"$HOME/.choir/repos/agents/bad.git\" \
         && git --git-dir \"$HOME/.choir/repos/agents/bad.git\" remote add forgejo \"$HOME/absent.git\"");
    let out = sh(remote);
    assert!(!out.status.success(), "a failed configured push must fail the run");
    assert!(String::from_utf8_lossy(&out.stderr).contains("push FAILED for agents/bad.git"));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("follower updated: agents/demo.git"),
        "one bad repo must not stop the others from being pushed"
    );

    std::fs::remove_dir_all(&home).ok();
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

/// Both installers stop the running node before installing the new one,
/// so both must prove a binary exists first. The Linux one always has.
/// The macOS one did not, and because cargo resolves `target-dir` from
/// the working directory rather than from `--manifest-path`, running it
/// from a worktree built into `shared-target` and pointed launchd at a
/// path that was never written — taking the canonical node down with no
/// binary for KeepAlive to restart.
#[test]
fn both_installers_refuse_before_stopping_a_running_node() {
    for (name, stop_verb) in [
        ("scripts/flip/install_node.sh", "launchctl bootout"),
        ("scripts/flip/install_node_linux.sh", "systemctl --user restart"),
    ] {
        let raw = std::fs::read_to_string(repo_root().join(name)).expect(name);
        // Comments mention both the guard and the stop verb, and a
        // comment above the guard explaining what it protects would
        // otherwise read as the stop happening first.
        let src: String = raw
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        let guard = src
            .find("-x \"$BIN\"")
            .or_else(|| src.find("-x \"$f\""))
            .unwrap_or_else(|| panic!("{name} must test the binary is executable"));
        let stop = src
            .find(stop_verb)
            .unwrap_or_else(|| panic!("{name} must stop the service"));
        assert!(
            guard < stop,
            "{name} runs `{stop_verb}` before proving a binary exists; \
             that is a node stopped with nothing to restart it with"
        );
    }

    // The macOS installer additionally must not assume the target dir.
    let mac = std::fs::read_to_string(repo_root().join("scripts/flip/install_node.sh"))
        .expect("macos installer");
    assert!(
        mac.contains("cargo metadata") && mac.contains("target_directory"),
        "the macOS installer must ask cargo where it built, not assume $REPO_DIR/target"
    );
    let code: String = mac
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !code.contains("$REPO_DIR/target/release"),
        "the macOS installer still hardcodes $REPO_DIR/target/release somewhere"
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

/// Writing a backup and being able to restore one are different claims,
/// and only the second matters. This pins the checks that separate them.
#[test]
fn the_backup_is_verified_by_pulling_it_back_not_by_having_written_it() {
    let script = std::fs::read_to_string(repo_root().join("scripts/verify_backup.sh"))
        .expect("scripts/verify_backup.sh");
    let code: String = script
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");

    // A restore that boots needs all five; the reviewers file is the one
    // whose absence stops the daemon outright.
    for needed in ["reviewers", "protected-refs", "newcomer-audit.jsonl"] {
        assert!(
            code.contains(needed),
            "verify-backup stopped checking for {needed}; a restore can stop booting again"
        );
    }
    // An assertion about the backup itself, so it holds even if someone
    // copies a file up by hand rather than through push_mirror.sh.
    assert!(
        code.contains("SECRETS IN THE BACKUP"),
        "verify-backup must fail loudly if a token or key reached the backup"
    );
    // Prefix, not equality: the live log grows between syncs.
    assert!(
        code.contains("prefix") && code.contains("cmp"),
        "verify-backup must compare the backup as a prefix of the live log"
    );
    assert!(
        code.contains("seq gap"),
        "verify-backup must localise a gap; a whole-file checksum cannot"
    );

    let driver = std::fs::read_to_string(repo_root().join("scripts/choirctl")).expect("choirctl");
    assert!(
        driver.contains("verify-backup)") && driver.contains("verify_backup.sh"),
        "choirctl must expose verify-backup, or nothing ever runs it"
    );

    let syntax = std::process::Command::new("sh")
        .arg("-n")
        .arg(repo_root().join("scripts/verify_backup.sh"))
        .output()
        .expect("run sh -n");
    assert!(
        syntax.status.success(),
        "verify_backup.sh is not valid sh: {}",
        String::from_utf8_lossy(&syntax.stderr)
    );
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

/// After the D20 host move the backup direction inverts: the node host
/// holds the live log and this machine pulls the offsite copy. Same
/// non-negotiables as the push direction, pinned the same way: the log,
/// the pin, and the policy travel; the signing key and the token never
/// do; and every copy is verified rather than hoped.
#[test]
fn the_pulled_backup_carries_the_log_and_the_pin_but_never_the_key() {
    let script = std::fs::read_to_string(repo_root().join("scripts/pull_backup.sh"))
        .expect("scripts/pull_backup.sh");
    let code: String = script
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        code.contains("ops.jsonl") && code.contains("node.fingerprint"),
        "the pull must carry the log and its pin, or a restore appends under a new identity"
    );
    assert!(
        !code.contains("node.key"),
        "the pulled backup must never carry node.key off the node that owns it"
    );
    assert!(
        code.contains("OP LOG BACKUP MISMATCH"),
        "the pull stopped comparing checksums; a truncated copy now looks like a good one"
    );
    assert!(
        code.contains("ops.jsonl.part") && code.contains("mv "),
        "the pull stopped writing .part then renaming; an interrupted run truncates the backup"
    );
    // Append-only is the backup's integrity model: each pull must extend
    // the previous one, never rewrite it.
    assert!(
        code.contains("cmp") && code.contains("prefix"),
        "the pull stopped checking the previous copy is a prefix of the new one"
    );
    assert!(
        code.contains("seq gap"),
        "the pull must localise a gap; a whole-file checksum cannot"
    );
    // Pull by explicit name, never by directory: a directory inherits
    // whatever lands in it, including a key copied there by accident.
    let list = code
        .lines()
        .find(|l| l.contains("for f in") && l.contains("reviewers"))
        .expect("the policy-file pull list");
    for needed in ["keys", "reviewers", "protected-refs", "newcomer-audit.jsonl"] {
        assert!(
            list.contains(needed),
            "the policy pull dropped {needed}; a restore stops booting again"
        );
    }
    for forbidden in ["auth", ".pem", ".key"] {
        assert!(
            !list.contains(forbidden),
            "the policy pull list names {forbidden}; secrets must not travel with it"
        );
    }
    // Direction guard: run where the live log lives, a pull would
    // overwrite the real backup relation with a vacuous self-copy.
    assert!(
        code.contains("node-remote"),
        "the pull lost its direction guard; run on the node host it clobbers the backup"
    );

    let driver = std::fs::read_to_string(repo_root().join("scripts/choirctl")).expect("choirctl");
    assert!(
        driver.contains("pull-backup)") && driver.contains("pull_backup.sh"),
        "choirctl must expose pull-backup, or nothing ever runs it"
    );
    // And the old direction must refuse to run after the move: its oplog
    // leg would overwrite the historical backup with a frozen stale log.
    let push = std::fs::read_to_string(repo_root().join("scripts/push_mirror.sh"))
        .expect("scripts/push_mirror.sh");
    assert!(
        push.contains("node-remote"),
        "push_mirror.sh lost its direction guard; run after the host move it clobbers the backup"
    );

    let syntax = std::process::Command::new("sh")
        .arg("-n")
        .arg(repo_root().join("scripts/pull_backup.sh"))
        .output()
        .expect("run sh -n");
    assert!(
        syntax.status.success(),
        "pull_backup.sh is not valid sh: {}",
        String::from_utf8_lossy(&syntax.stderr)
    );
}
