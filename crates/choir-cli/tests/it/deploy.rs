//! Receipt 5's atomic deployment and one-command rollback, exercised.
//!
//! `deploy_private_beta.sh` and `rollback_private_beta.sh` had no tests
//! at all, which is not an accident: they call `systemctl` and GNU `mv
//! -T`, so neither runs to completion on a developer machine. They are
//! also the two scripts an operator reaches for when something has
//! already gone wrong, which is the worst place for the first execution
//! to be the real one.
//!
//! So both are driven here against stubs placed ahead of the real tools
//! on `PATH`: a `systemctl` that succeeds or fails on command, and an
//! `mv` that understands `-T`. **The stubbed `mv` is not atomic**, and
//! this therefore proves nothing about the swap being atomic -- that
//! rests on `rename(2)` and is the one property a test on this machine
//! cannot observe. What it does prove is the symlink bookkeeping around
//! the swap, which is where every defect these tests were written
//! against actually lived.

/// A directory holding stub `systemctl` and `mv`, to be prepended to
/// `PATH`. `systemctl` fails whenever the file `fail-restart` exists
/// beside it, so a test can break the service mid-script.
fn stubs(dir: &std::path::Path) -> String {
    std::fs::create_dir_all(dir).expect("stub directory");
    let systemctl = dir.join("systemctl");
    std::fs::write(
        &systemctl,
        "#!/bin/sh\n\
         here=$(cd \"$(dirname \"$0\")\" && pwd)\n\
         [ -e \"$here/fail-restart\" ] && exit 1\n\
         echo \"$@\" >> \"$here/calls\"\n\
         exit 0\n",
    )
    .expect("systemctl stub");
    // BSD `mv` has no `-T`. The real script wants it for an atomic
    // replace of a symlink; here it only has to mean "replace it".
    let mv = dir.join("mv");
    std::fs::write(
        &mv,
        "#!/bin/sh\n\
         args=\"\"\n\
         for a in \"$@\"; do\n\
           case $a in -Tf|-T|-f) ;; *) args=\"$args $a\" ;; esac\n\
         done\n\
         set -- $args\n\
         rm -rf \"$2\"\n\
         exec /bin/mv \"$1\" \"$2\"\n",
    )
    .expect("mv stub");
    for path in [&systemctl, &mv] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("stub is executable");
    }
    format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// Writes an artifact directory the deploy script will accept, at
/// `version`. `omit` names a file to leave out.
fn artifact(dir: &std::path::Path, version: &str, omit: Option<&str>) {
    std::fs::create_dir_all(dir).expect("artifact directory");
    let files = [
        "choir-node",
        "choir",
        "private-beta.manifest",
        "release-manifest.json",
        "choir-node.cdx.json",
        "choir.cdx.json",
    ];
    for file in files {
        if omit == Some(file) {
            continue;
        }
        let contents = if file == "release-manifest.json" {
            format!("{{\"version\":\"{version}\"}}")
        } else {
            format!("{file} for {version}\n")
        };
        std::fs::write(dir.join(file), contents).expect("artifact file");
    }
    // SHA256SUMS has to match, because the script verifies it and a test
    // that disabled that check would be testing a different script.
    let listing = std::process::Command::new("sh")
        .arg("-c")
        .arg("sha256sum * > SHA256SUMS")
        .current_dir(dir)
        .status()
        .expect("run sha256sum");
    assert!(listing.success(), "could not checksum the fixture artifact");
}

struct Deployment {
    root: std::path::PathBuf,
    artifacts: std::path::PathBuf,
    path: String,
}

impl Deployment {
    fn new(tag: &str) -> Self {
        let work = std::env::temp_dir().join(format!(
            "choir-deploy-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&work).ok();
        std::fs::create_dir_all(&work).expect("work directory");
        let path = stubs(&work.join("bin"));
        Self {
            root: work.join("install"),
            artifacts: work.join("artifacts"),
            path,
        }
    }

    fn script(&self, name: &str, args: &[&str]) -> std::process::Output {
        std::process::Command::new("sh")
            .arg(super::install_policy::repo_root().join(format!("scripts/{name}")))
            .args(args)
            .env("PATH", &self.path)
            .output()
            .unwrap_or_else(|e| panic!("run {name}: {e}"))
    }

    fn deploy(&self, version: &str, omit: Option<&str>) -> std::process::Output {
        let dir = self.artifacts.join(version);
        artifact(&dir, version, omit);
        self.script(
            "deploy_private_beta.sh",
            &[
                dir.to_str().expect("utf-8 path"),
                self.root.to_str().expect("utf-8 path"),
                "choir-node",
            ],
        )
    }

    fn rollback(&self) -> std::process::Output {
        self.script(
            "rollback_private_beta.sh",
            &[self.root.to_str().expect("utf-8 path"), "choir-node"],
        )
    }

    /// The version `link` resolves to, or `None` if it is absent.
    fn link(&self, link: &str) -> Option<String> {
        let target = std::fs::read_link(self.root.join(link)).ok()?;
        Some(
            target
                .file_name()?
                .to_string_lossy()
                .trim_end_matches(".git")
                .to_string(),
        )
    }

    /// Makes the stub service fail, or stop failing.
    fn break_service(&self, broken: bool) {
        let flag = self.root.parent().unwrap().join("bin/fail-restart");
        if broken {
            std::fs::write(&flag, "").expect("break the service");
        } else {
            std::fs::remove_file(&flag).ok();
        }
    }
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_deploy_promotes_the_release_and_names_the_one_it_replaced() {
    let d = Deployment::new("promote");

    let first = d.deploy("1.0.0", None);
    assert!(first.status.success(), "{}", stderr(&first));
    assert_eq!(d.link("current").as_deref(), Some("1.0.0"));
    assert_eq!(d.link("previous"), None, "a first deploy replaces nothing");

    let second = d.deploy("1.1.0", None);
    assert!(second.status.success(), "{}", stderr(&second));
    assert_eq!(d.link("current").as_deref(), Some("1.1.0"));
    assert_eq!(d.link("previous").as_deref(), Some("1.0.0"));
}

#[test]
fn rolling_back_twice_returns_to_where_it_started() {
    let d = Deployment::new("involution");
    assert!(d.deploy("1.0.0", None).status.success());
    assert!(d.deploy("1.1.0", None).status.success());

    let back = d.rollback();
    assert!(back.status.success(), "{}", stderr(&back));
    assert_eq!(d.link("current").as_deref(), Some("1.0.0"));
    // The release just left is the one to return to. Without this the
    // second rollback below moves nothing and reports success, and an
    // operator who rolled back by mistake has no one-command way back.
    assert_eq!(d.link("previous").as_deref(), Some("1.1.0"));

    let forward = d.rollback();
    assert!(forward.status.success(), "{}", stderr(&forward));
    assert_eq!(d.link("current").as_deref(), Some("1.1.0"));
    assert_eq!(d.link("previous").as_deref(), Some("1.0.0"));
}

#[test]
fn a_rollback_with_nowhere_to_go_refuses_rather_than_reporting_success() {
    let d = Deployment::new("nowhere");
    assert!(d.deploy("1.0.0", None).status.success());

    let back = d.rollback();
    assert!(
        !back.status.success(),
        "a rollback with no previous release reported success"
    );
    assert!(
        stderr(&back).contains("no previous release"),
        "{}",
        stderr(&back)
    );
    assert_eq!(d.link("current").as_deref(), Some("1.0.0"));
}

#[test]
fn a_failed_deploy_leaves_the_running_release_and_its_history_untouched() {
    let d = Deployment::new("failed-deploy");
    assert!(d.deploy("1.0.0", None).status.success());
    assert!(d.deploy("1.1.0", None).status.success());

    d.break_service(true);
    let failed = d.deploy("1.2.0", None);
    d.break_service(false);
    assert!(!failed.status.success(), "a dead service deployed anyway");

    assert_eq!(d.link("current").as_deref(), Some("1.1.0"));
    // `previous` is repointed before the swap, so a failed deploy that
    // restored `current` and left `previous` alone made the two equal --
    // and a rollback against that state changes nothing and says it
    // worked.
    assert_eq!(
        d.link("previous").as_deref(),
        Some("1.0.0"),
        "a failed deploy rewrote the rollback target"
    );
    assert!(
        !d.root.join("releases/1.2.0").exists(),
        "the half-deployed release was left behind, so the version can never be retried"
    );

    // And the rollback that follows a failed deploy still goes somewhere.
    let back = d.rollback();
    assert!(back.status.success(), "{}", stderr(&back));
    assert_eq!(d.link("current").as_deref(), Some("1.0.0"));
}

#[test]
fn an_incomplete_artifact_is_refused_before_anything_is_installed() {
    let d = Deployment::new("incomplete");
    assert!(d.deploy("1.0.0", None).status.success());

    // The SBOM was validated by nobody and installed by the loop, so a
    // missing one used to fail after the release directory existed --
    // and the "release already exists" guard then refused that version
    // forever.
    let broken = d.deploy("1.1.0", Some("choir-node.cdx.json"));
    assert!(!broken.status.success(), "an incomplete artifact deployed");
    assert!(
        stderr(&broken).contains("artifact is incomplete"),
        "{}",
        stderr(&broken)
    );
    assert_eq!(d.link("current").as_deref(), Some("1.0.0"));
    assert!(
        !d.root.join("releases/1.1.0").exists(),
        "a refused artifact left a release directory behind"
    );
}
