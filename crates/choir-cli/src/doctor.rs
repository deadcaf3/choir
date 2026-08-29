//! `choir doctor` — one command that answers "why did that fail?".
//!
//! Every other command in this binary assumes its surroundings: that
//! `git` is on the path, that `curl` can reach a node, that the auth
//! file is readable and not world-readable, that the node it is about
//! to talk to is actually up. When one of those is false the failure
//! surfaces wherever it happens to be noticed — a subprocess exit
//! status, a `curl` error, a 401 — and the reader has to work backwards
//! from a symptom to a cause.
//!
//! This module checks all of them up front and reports each one with the
//! command that fixes it. It is the only place in the crate that treats
//! a missing dependency as a *finding* rather than an error: nothing
//! here aborts on the first failure, because "git is missing" and "the
//! node is unreachable" are independently useful and a reader who has
//! both wants both in one pass.
//!
//! # Examples
//!
//! ```
//! use choir_cli::doctor::{Check, Status};
//!
//! let checks = vec![
//!     Check::pass("git", "/usr/bin/git"),
//!     Check::fail("mergiraf", "not on PATH").with_fix("brew install mergiraf"),
//! ];
//! assert_eq!(Status::worst(&checks), Status::Fail);
//! assert_eq!(choir_cli::doctor::exit_code(&checks), 1);
//! ```

use crate::style::Style;
use std::path::PathBuf;

/// How a single check came out.
///
/// Three states rather than two because the difference matters to the
/// exit code: an absent `mergiraf` narrows the merge ladder to line
/// merge and first-class conflicts, which is a *worse* node and a
/// working one. Failing the command for it would teach operators to
/// ignore the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    /// Present and usable.
    Pass,
    /// Usable, but something is degraded or unset.
    Warn,
    /// Something that is required is absent or broken.
    Fail,
}

impl Status {
    /// The most severe status in a set, or [`Status::Pass`] for none.
    ///
    /// This is what decides the exit code, so it is ordering over the
    /// enum rather than a hand-written match: adding a status variant
    /// between the existing ones must not silently keep the old answer.
    pub fn worst(checks: &[Check]) -> Status {
        checks
            .iter()
            .map(|c| c.status)
            .max()
            .unwrap_or(Status::Pass)
    }

    /// The fixed-width label used in the report, without colour.
    pub fn label(self) -> &'static str {
        match self {
            Status::Pass => "ok  ",
            Status::Warn => "warn",
            Status::Fail => "FAIL",
        }
    }

    fn paint(self, style: Style) -> String {
        match self {
            Status::Pass => style.green(self.label()),
            Status::Warn => style.cyan(self.label()),
            Status::Fail => style.red(self.label()),
        }
    }
}

/// One thing that was checked, and what to do when it is wrong.
#[derive(Debug, Clone)]
pub struct Check {
    /// What was checked, e.g. `git` or `node reachable`.
    pub name: String,
    /// How it came out.
    pub status: Status,
    /// What was found: a path, a version, an error, an http status.
    pub detail: String,
    /// The command that fixes it, when there is one to name.
    ///
    /// `None` for a passing check and for a failure whose fix depends on
    /// something this process cannot know. A wrong fix is worse than no
    /// fix: it gets run.
    pub fix: Option<String>,
}

impl Check {
    /// A passing check.
    pub fn pass(name: &str, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            status: Status::Pass,
            detail: detail.into(),
            fix: None,
        }
    }

    /// A degraded-but-working check.
    pub fn warn(name: &str, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            status: Status::Warn,
            detail: detail.into(),
            fix: None,
        }
    }

    /// A check that failed.
    pub fn fail(name: &str, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            status: Status::Fail,
            detail: detail.into(),
            fix: None,
        }
    }

    /// Attaches the command that fixes this check.
    pub fn with_fix(mut self, fix: impl Into<String>) -> Check {
        self.fix = Some(fix.into());
        self
    }
}

/// The exit code for a set of checks: 0 when nothing failed, 1 when
/// something did.
///
/// A warning does not fail the command. This follows the binary's own
/// convention — 0 accepted, 1 rejected, 2 usage — so `choir doctor &&
/// choir propose …` means what it looks like it means.
pub fn exit_code(checks: &[Check]) -> i32 {
    match Status::worst(checks) {
        Status::Fail => 1,
        _ => 0,
    }
}

/// Finds an executable on `PATH`, the way exec does.
///
/// Reading `PATH` is not configuration — it is the environment
/// describing the machine, the same category as the `PATH`/`HOME`
/// pass-through `choir-queue` already relies on. Walking it here rather
/// than shelling out to `which` costs no subprocess and gives the same
/// answer the failing `Command::new` would have got.
pub fn on_path(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(tool))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

/// Runs `<tool> <args>` and returns its first line of output.
///
/// Both streams are read because the version banners disagree about
/// which one they belong on, and a tool that prints its version to
/// stderr is not thereby broken.
fn first_line(tool: &std::path::Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(tool).args(args).output().ok()?;
    let text = if out.stdout.is_empty() {
        out.stderr
    } else {
        out.stdout
    };
    String::from_utf8_lossy(&text)
        .lines()
        .next()
        .map(str::to_string)
}

/// One external binary this workspace shells out to.
struct Tool {
    name: &'static str,
    /// How to ask it for its version, or empty when it has no clean way
    /// to be asked. `ssh-keygen` is the empty case: every spelling of
    /// `--version` is an unknown flag it answers with usage.
    version: &'static [&'static str],
    /// What stops working without it. Empty means it is required.
    without: &'static str,
    fix: &'static str,
}

/// The tools every command here assumes, and what each is for.
///
/// Required and optional in one table rather than two, because the
/// difference is one field and two tables drift.
const TOOLS: &[Tool] = &[
    Tool {
        name: "git",
        version: &["--version"],
        without: "",
        fix: "install git (xcode-select --install, or your package manager)",
    },
    Tool {
        name: "curl",
        version: &["--version"],
        without: "",
        fix: "install curl",
    },
    Tool {
        name: "openssl",
        version: &["version"],
        without: "",
        fix: "brew install openssl@3 (macOS) or apt install openssl libssl-dev (Debian)",
    },
    Tool {
        name: "ssh-keygen",
        version: &[],
        without: "",
        fix: "install openssh-client",
    },
    Tool {
        name: "mergiraf",
        version: &["--version"],
        without: "structured merge; line merge and first-class conflicts still work",
        fix: "brew install mergiraf",
    },
    Tool {
        name: "mdbook",
        version: &["--version"],
        without: "`choir docs` cannot build the book",
        fix: "cargo install mdbook --locked",
    },
];

/// Checks every external binary in [`TOOLS`].
fn tool_checks() -> Vec<Check> {
    TOOLS
        .iter()
        .map(|tool| match on_path(tool.name) {
            Some(path) => {
                let detail = match tool.version.is_empty() {
                    true => path.display().to_string(),
                    false => first_line(&path, tool.version)
                        .unwrap_or_else(|| path.display().to_string()),
                };
                Check::pass(tool.name, detail)
            }
            None if tool.without.is_empty() => {
                Check::fail(tool.name, "not on PATH").with_fix(tool.fix)
            }
            None => Check::warn(
                tool.name,
                format!("not on PATH — without it, {}", tool.without),
            )
            .with_fix(tool.fix),
        })
        .collect()
}

/// Checks the auth file: that it exists, and that nobody else can read
/// it.
///
/// The mode check is not decoration. This file carries a bearer
/// credential for a node, and a group- or world-readable one is a
/// credential every account on the machine holds. It is reported as a
/// failure rather than a warning for that reason, even though every
/// command would keep working.
fn auth_check(path: Option<&str>) -> Check {
    let Some(path) = path else {
        return Check::warn("auth file", "none given, and no default found")
            .with_fix("choir --auth-file <path> …, or `choir join` to be issued one");
    };
    let Ok(meta) = std::fs::metadata(path) else {
        return Check::fail("auth file", format!("{path} cannot be read"))
            .with_fix("choir join <api> <invite-file> <key-file>");
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Check::fail("auth file", format!("{path} is mode {mode:04o}"))
                .with_fix(format!("chmod 600 {path}"));
        }
        Check::pass("auth file", format!("{path} ({mode:04o})"))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        Check::pass("auth file", path.to_string())
    }
}

/// Checks that a node answers, using the same credential the other
/// commands would.
///
/// `/healthz` rather than `/api/view`, because the question here is "is
/// anything serving" and `/api/view` conflates that with "may I read
/// it". The node authenticates `/healthz` like everything else, so this
/// goes through [`HttpClient`] rather than a bare `curl`: a doctor that
/// reached the node differently from the commands it is diagnosing
/// would be checking a path nobody runs, and would report every
/// correctly authenticated node as a 401.
///
/// A 401 *with* a credential is therefore a real finding — the token is
/// wrong or the node does not know it — and is reported as one.
///
/// [`HttpClient`]: crate::mcp::HttpClient
fn node_check(api: Option<&str>, auth: Option<&str>, curl: bool) -> Check {
    let Some(api) = api else {
        return Check::warn("node", "no node configured")
            .with_fix("write `node = <url>` to .choir/config, or pass the URL");
    };
    if !curl {
        return Check::fail("node", format!("{api}: cannot check without curl"));
    }
    let client = match crate::mcp::HttpClient::new(api, auth.map(std::path::Path::new), None) {
        Ok(client) => client,
        Err(error) => return Check::fail("node", format!("{api}: {error}")),
    };
    match client.get("/healthz") {
        Ok((200, _)) => Check::pass("node", format!("{api} is healthy")),
        // The node serves 503 here when its own durability check has
        // failed. That is the node telling the truth about itself, and
        // it is the one answer this command must not soften.
        Ok((503, body)) => Check::fail("node", format!("{api} reports unhealthy: {body}"))
            .with_fix("check the node's log; a failed durable append exits it 75"),
        Ok((401, _)) if auth.is_none() => {
            Check::warn("node", format!("{api} is up; needs a credential"))
                .with_fix("choir --auth-file <path> doctor")
        }
        Ok((401, _)) => Check::fail("node", format!("{api} refused the credential"))
            .with_fix("check --auth-file names a token this node knows"),
        Ok((code, _)) => Check::warn("node", format!("{api} answered {code} on /healthz")),
        Err(error) => Check::fail("node", format!("{api}: {error}")),
    }
}

/// Runs every check.
///
/// `api` and `auth` are passed in rather than discovered here so the
/// caller's own resolution — the `.choir/config` walk, the `--auth-file`
/// flag — is the one being reported on. A doctor that resolves its
/// inputs differently from the commands it is diagnosing is checking a
/// configuration nobody runs.
pub fn run(api: Option<&str>, auth: Option<&str>) -> Vec<Check> {
    let mut checks = tool_checks();
    let curl = checks
        .iter()
        .any(|c| c.name == "curl" && c.status == Status::Pass);
    checks.push(daemon_check());
    checks.push(auth_check(auth));
    checks.push(node_check(api, auth, curl));
    checks
}

/// Whether `choir node serve` has a daemon to exec.
///
/// A warning rather than a failure: a machine that only ever talks to
/// somebody else's node needs no `choir-node` at all, and failing there
/// would tell a perfectly healthy client install that it is broken. It
/// is here because `serve` and `install` both depend on it, and finding
/// out at install time — from a service manager that then retries every
/// two seconds — is the worst place to find out.
fn daemon_check() -> Check {
    match crate::serve::find_daemon() {
        Ok(path) => {
            let where_ = path.display().to_string();
            if crate::supervise::in_build_directory(&path) {
                // Not a failure either: running from a build tree is
                // exactly right in a checkout. It is only a unit
                // pointing at one that breaks, and `node install`
                // refuses that on its own.
                Check::warn("choir-node", format!("{where_} (a build directory)")).with_fix(
                    "fine for a checkout; `choir node install` refuses it, so install \
                     the pair before supervising one",
                )
            } else {
                Check::pass("choir-node", where_)
            }
        }
        Err(_) => Check::warn("choir-node", "not beside `choir` or on PATH")
            .with_fix("only needed to run a node yourself: cargo build --release -p choir-node"),
    }
}

/// Renders the checks as the report the command prints.
///
/// Fixes are gathered under the table rather than shown per row: the
/// table answers "what is wrong" at a glance, and a `fix` column would
/// wrap every long command and destroy that. Ordering is the check
/// order, not severity — the reader is looking for a name they
/// recognise.
pub fn report(checks: &[Check], style: Style) -> String {
    let width = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let mut out = String::new();
    for check in checks {
        let name = format!("{:width$}", check.name);
        out.push_str(&format!(
            "  {}  {}  {}\n",
            check.status.paint(style),
            style.dim(&name),
            check.detail
        ));
    }
    let fixes: Vec<&Check> = checks.iter().filter(|c| c.fix.is_some()).collect();
    if !fixes.is_empty() {
        out.push('\n');
        for check in fixes {
            let fix = check.fix.as_deref().unwrap_or_default();
            out.push_str(&format!(
                "  {} {}\n",
                style.dim(&format!("{}:", check.name)),
                fix
            ));
        }
    }
    out.push('\n');
    out.push_str(&match Status::worst(checks) {
        Status::Fail => format!("  {}\n", style.red("something required is missing")),
        Status::Warn => format!("  {}\n", style.cyan("usable; some things are degraded")),
        Status::Pass => format!("  {}\n", style.green("everything checks out")),
    });
    out
}
