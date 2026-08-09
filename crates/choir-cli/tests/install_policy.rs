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
