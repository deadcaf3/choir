//! The release shelf, from a stranger's side of it (D79).
//!
//! The claim this route makes is narrow and easy to state wrongly: the
//! bytes a reader installs come from the node they typed, and nothing in
//! the path names another host. A test that only checked the status code
//! would pass just as happily against a script that redirected to a
//! forge, so the assertion here is on the script's `BASE` and, in the
//! last test, on a binary that actually arrives.
//!
//! The end-to-end one builds a release-shaped archive and runs the
//! served installer against the served node. It is the only test in this
//! file that would notice if the archive layout, the digest format, the
//! target triple or the substituted `BASE` stopped agreeing with each
//! other, which is the whole surface this feature has.

use choir_node::{AuthTable, Node};

struct Served {
    base: String,
    work: std::path::PathBuf,
    shelf: std::path::PathBuf,
}

/// A node with an auth file, so an unauthenticated request is refused
/// everywhere the shelf has not deliberately opened.
fn served(tag: &str, shelf_files: &[(&str, &[u8])]) -> Served {
    let work = std::env::temp_dir().join(format!("choir-node-downloads-{tag}"));
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");

    let shelf = work.join("shelf");
    std::fs::create_dir_all(&shelf).expect("shelf");
    for (name, bytes) in shelf_files {
        std::fs::write(shelf.join(name), bytes).expect("shelf file");
    }

    let mut table = AuthTable::new();
    table.insert("op".into(), "o".into());

    let root = work.join("repos");
    let mut node = Node::bind_with_auth(&root, 0, Some(table)).expect("node binds a free port");
    let port = node.port();
    node.publish_downloads(shelf.clone()).expect("shelf serves");
    std::thread::spawn(move || node.serve_forever());

    Served {
        base: format!("http://127.0.0.1:{port}"),
        work,
        shelf,
    }
}

impl Served {
    /// Status and body of a `curl` carrying no credential at all.
    fn anon(&self, path: &str) -> (u16, String) {
        let out = std::process::Command::new("curl")
            .args(["-s", "-w", "\n%{http_code}"])
            .arg(format!("{}{path}", self.base))
            .output()
            .expect("curl runs");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let (body, status) = text.rsplit_once('\n').expect("status is appended");
        (status.trim().parse().expect("numeric status"), body.into())
    }

    /// The same, with the path sent exactly as written.
    ///
    /// `curl` resolves `..` in a URL before it sends anything, so
    /// without this a traversal probe never reaches the node and the
    /// test proves only that `curl` can do arithmetic.
    fn anon_raw(&self, path: &str) -> u16 {
        let out = std::process::Command::new("curl")
            .args([
                "-s",
                "--path-as-is",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
            ])
            .arg(format!("{}{path}", self.base))
            .output()
            .expect("curl runs");
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("numeric status")
    }

    fn done(self) {
        std::fs::remove_dir_all(&self.work).ok();
    }
}

/// The triple the release archives are named for, on whichever machine
/// is running this. Mirrors the `uname` mapping in `install.sh`; if the
/// two ever disagree the end-to-end test below stops finding its
/// archive, which is the point of computing it here rather than
/// hard-coding one.
fn target() -> String {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        other => panic!("no release target for {other}"),
    };
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu",
        other => panic!("no release target for {other}"),
    };
    format!("{arch}-{os}")
}

/// The one thing a stranger must be able to do, and the one claim the
/// script makes about where its bytes come from.
#[test]
fn the_installer_a_stranger_is_handed_points_at_the_node_that_handed_it_over() {
    let s = served("installer", &[]);
    let (status, body) = s.anon("/download/install.sh");
    assert_eq!(status, 200, "a stranger could not fetch the installer");
    assert!(
        body.contains(&format!("BASE=\"{}/download\"", s.base)),
        "the installer does not name the node that served it: {body}"
    );
    assert!(
        !body.contains("github.com"),
        "the installer names a forge: {body}"
    );
    assert!(
        !body.contains("__CHOIR_DOWNLOAD_BASE__"),
        "the placeholder survived into a served script: {body}"
    );
    s.done();
}

/// A shelf is a public directory, so the interesting cases are the names
/// that are not files on it.
#[test]
fn the_shelf_hands_over_a_file_and_nothing_around_it() {
    let s = served("files", &[("choir-cli-x.tar.xz", b"archive bytes")]);

    let (status, body) = s.anon("/download/choir-cli-x.tar.xz");
    assert_eq!(status, 200);
    assert_eq!(body, "archive bytes");

    for probe in [
        "/download/../../etc/passwd",
        "/download/%2e%2e%2f%2e%2e%2fetc%2fpasswd",
        "/download/.hidden",
        "/download/sub/file",
        "/download/nothing-here.tar.xz",
        "/download/install.sh.sha256",
    ] {
        assert_eq!(s.anon_raw(probe), 404, "{probe} was not a 404");
    }

    // A symlink on the shelf is a link, not the thing it points at. The
    // target is real and readable, so a route that followed links would
    // answer 200 here.
    let secret = s.work.join("secret");
    std::fs::write(&secret, "not for readers").expect("secret file");
    std::os::unix::fs::symlink(&secret, s.shelf.join("escape.tar.xz")).expect("symlink");
    let (status, _) = s.anon("/download/escape.tar.xz");
    assert_eq!(status, 404, "the shelf followed a symlink off itself");

    s.done();
}

/// The index exists so the address is not a guessing game, and it must
/// name the node in the command it prints for the same reason the script
/// does.
#[test]
fn the_index_lists_the_shelf_and_prints_a_command_that_works() {
    let s = served("index", &[("choir-cli-x.tar.xz", b"bytes")]);
    for path in ["/download", "/download/"] {
        let (status, body) = s.anon(path);
        assert_eq!(status, 200, "{path} did not serve the index");
        assert!(body.contains("choir-cli-x.tar.xz"), "{path} listed nothing");
        assert!(
            body.contains(&format!("curl -fsSL {}/download/install.sh", s.base)),
            "{path} printed a command naming another host"
        );
    }
    s.done();
}

/// Absent the flag there is no route, and in particular no route that
/// answers differently from every other unauthenticated path. A node
/// that publishes no binaries must be exactly the node it was before.
#[test]
fn a_node_with_no_shelf_offers_no_shelf() {
    let work = std::env::temp_dir().join("choir-node-downloads-absent");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");
    let mut table = AuthTable::new();
    table.insert("op".into(), "o".into());
    let node =
        Node::bind_with_auth(&work.join("repos"), 0, Some(table)).expect("node binds a free port");
    let port = node.port();
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    for path in ["/download/install.sh", "/download/", "/download"] {
        let out = std::process::Command::new("curl")
            .args(["-s", "-o", "/dev/null", "-w", "%{http_code}"])
            .arg(format!("{base}{path}"))
            .output()
            .expect("curl runs");
        let status: u16 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("numeric status");
        assert_ne!(
            status, 200,
            "{path} served something on a node with no shelf"
        );
    }
    std::fs::remove_dir_all(&work).ok();
}

/// The assertion the rest of this file is scaffolding for: a release
/// archive on the shelf, the served installer run against the served
/// node, and a binary on `PATH` at the end of it.
///
/// Nothing here reaches the network. The archive is built in the test to
/// the layout a real one has — one directory named after the archive,
/// with the executables in it — and the digest is written in the format
/// the release publishes, `<hex> *<name>`.
#[test]
fn the_served_installer_installs_from_the_node_that_served_it() {
    let work = std::env::temp_dir().join("choir-node-downloads-e2e");
    std::fs::remove_dir_all(&work).ok();
    std::fs::create_dir_all(&work).expect("temp root");

    // Two release-shaped archives, because the installer given no
    // arguments installs both packages -- the property this test is
    // here for. Each holds one directory named after itself with an
    // executable and a licence in it, so the "anything executable in
    // there" rule has both cases to sort.
    let target = target();
    let shelf = work.join("shelf");
    std::fs::create_dir_all(&shelf).expect("shelf");
    for (package, binary) in [("choir-cli", "choir"), ("choir-node", "choir-node")] {
        let stage = work.join(format!("{package}-{target}"));
        std::fs::create_dir_all(&stage).expect("stage");
        let fake = stage.join(binary);
        std::fs::write(&fake, format!("#!/bin/sh\necho i-am-{binary}\n")).expect("fake binary");
        std::process::Command::new("chmod")
            .args(["+x"])
            .arg(&fake)
            .status()
            .expect("chmod runs");
        std::fs::write(stage.join("LICENSE-MIT"), "a licence").expect("licence");

        let archive = format!("{package}-{target}.tar.xz");
        let tarred = std::process::Command::new("tar")
            .args(["-cJf", &archive, &format!("{package}-{target}")])
            .current_dir(&work)
            .status()
            .expect("tar runs");
        assert!(tarred.success(), "could not build a test archive");
        std::fs::rename(work.join(&archive), shelf.join(&archive)).expect("onto the shelf");

        // The digest, in the format the release publishes it in.
        let sum = std::process::Command::new("shasum")
            .args(["-a", "256"])
            .arg(&archive)
            .current_dir(&shelf)
            .output()
            .expect("shasum runs");
        let hex = String::from_utf8_lossy(&sum.stdout)
            .split_whitespace()
            .next()
            .expect("a digest")
            .to_string();
        std::fs::write(
            shelf.join(format!("{archive}.sha256")),
            format!("{hex} *{archive}\n"),
        )
        .expect("digest file");
    }

    let mut table = AuthTable::new();
    table.insert("op".into(), "o".into());
    let mut node =
        Node::bind_with_auth(&work.join("repos"), 0, Some(table)).expect("node binds a free port");
    let port = node.port();
    node.publish_downloads(shelf).expect("shelf serves");
    std::thread::spawn(move || node.serve_forever());
    let base = format!("http://127.0.0.1:{port}");

    // Exactly the documented command, with `CARGO_HOME` pointed
    // somewhere this test owns.
    let home = work.join("cargo-home");
    let script = std::process::Command::new("curl")
        .args(["-fsSL", &format!("{base}/download/install.sh")])
        .output()
        .expect("curl runs");
    assert!(script.status.success(), "the installer did not serve");
    let script_path = work.join("install.sh");
    std::fs::write(&script_path, &script.stdout).expect("save the script");

    // No arguments. `choir host` is the second line of the documented
    // quick start and it execs the daemon, so an installer whose default
    // leaves the daemon out hands the reader `command not found` one
    // step later. That was the first version's default.
    let run = std::process::Command::new("sh")
        .arg(&script_path)
        .env("CARGO_HOME", &home)
        .output()
        .expect("sh runs");
    assert!(
        run.status.success(),
        "the installer failed: {}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );

    for binary in ["choir", "choir-node"] {
        let installed = home.join("bin").join(binary);
        assert!(
            installed.is_file(),
            "{binary} was not installed by default: {}{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
        let ran = std::process::Command::new(&installed)
            .output()
            .expect("the installed binary runs");
        assert_eq!(
            String::from_utf8_lossy(&ran.stdout).trim(),
            format!("i-am-{binary}")
        );
    }
    assert!(
        !home.join("bin/LICENSE-MIT").exists(),
        "the installer copied a licence in beside the binaries"
    );

    std::fs::remove_dir_all(&work).ok();
}

/// A digest that does not match must stop the install, not warn about
/// it. Asserted by corrupting the archive after the digest was written.
#[test]
fn a_mismatched_digest_stops_the_install() {
    let target = target();
    let archive = format!("choir-cli-{target}.tar.xz");
    let s = served(
        "digest",
        &[
            (&archive, b"not the bytes the digest describes"),
            (
                &format!("{archive}.sha256"),
                b"0000000000000000000000000000000000000000000000000000000000000000 *x\n",
            ),
        ],
    );

    let script = std::process::Command::new("curl")
        .args(["-fsSL", &format!("{}/download/install.sh", s.base)])
        .output()
        .expect("curl runs");
    let script_path = s.work.join("install.sh");
    std::fs::write(&script_path, &script.stdout).expect("save the script");

    let home = s.work.join("cargo-home");
    let run = std::process::Command::new("sh")
        .arg(&script_path)
        .env("CARGO_HOME", &home)
        .output()
        .expect("sh runs");
    assert!(!run.status.success(), "a bad digest installed anyway");
    assert!(
        String::from_utf8_lossy(&run.stderr).contains("checksum mismatch"),
        "the failure did not say why: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        !home.join("bin/choir").exists(),
        "something was installed despite the mismatch"
    );
    s.done();
}
