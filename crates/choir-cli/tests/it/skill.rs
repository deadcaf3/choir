//! `choir skill install` (internal/oak.md item 5): the installed skill is
//! rendered from the running binary's surface table, lands under a
//! directory matching its frontmatter `name:`, and re-installs are
//! idempotent.

use choir_cli::surface;

fn choir(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_choir"))
        .args(args)
        .output()
        .expect("choir runs")
}

fn json(out: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!("json stdout, got {:?}", String::from_utf8_lossy(&out.stdout))
    })
}

#[test]
fn install_is_versioned_with_the_binary_and_idempotent() {
    let work = std::env::temp_dir().join(format!("choir-skill-{}", std::process::id()));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).unwrap();
    let into = work.to_str().unwrap();

    let first = choir(&["skill", "install", "--into", into]);
    assert!(first.status.success(), "{:?}", String::from_utf8_lossy(&first.stderr));
    let first = json(&first);
    assert_eq!(first["wrote"], true);
    let path = std::path::PathBuf::from(first["path"].as_str().unwrap());
    assert_eq!(path.file_name().unwrap(), "SKILL.md");
    // Skill loaders resolve by directory, so the frontmatter name and the
    // directory must agree — Oak's own frontmatter lesson.
    assert_eq!(path.parent().unwrap().file_name().unwrap(), surface::SKILL_DIR);
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.starts_with(&format!("---\nname: {}\n", surface::SKILL_DIR)), "{body}");
    // Rendered from the live table: a command added to the surface is in
    // the installed skill with no separate file to forget.
    assert!(body.contains("choir triage"), "{body}");
    assert!(body.contains("choir state"), "{body}");

    // Unchanged content is left alone; stale content is refreshed.
    let second = json(&choir(&["skill", "install", "--into", into]));
    assert_eq!(second["wrote"], false);
    std::fs::write(&path, "stale\n").unwrap();
    let third = json(&choir(&["skill", "install", "--into", into]));
    assert_eq!(third["wrote"], true);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), surface::skill_md());

    std::fs::remove_dir_all(&work).ok();
}
