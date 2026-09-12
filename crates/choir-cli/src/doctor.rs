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
        return Check::warn("auth file", "none given, and no default found").with_fix(
            "choir join '<link>' to be issued one, or name yours: choir --auth-file <path> …",
        );
    };
    let Ok(meta) = std::fs::metadata(path) else {
        return Check::fail("auth file", format!("{path} cannot be read"))
            .with_fix("choir join '<link>'");
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
        return Check::warn("node", "no node configured").with_fix(
            "choir join '<link>' writes one to ~/.choir/config; or write `node = <url>` \
             to .choir/config here, or pass the URL",
        );
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
    let mut checks = vec![self_check()];
    checks.extend(tool_checks());
    let curl = checks
        .iter()
        .any(|c| c.name == "curl" && c.status == Status::Pass);
    checks.push(daemon_check());
    checks.push(auth_check(auth));
    checks.push(node_check(api, auth, curl));
    checks
}

/// How many attestations a seed's statement may trail the home's current
/// one before the fork check says so (D80).
///
/// A seed polls every few seconds and a home attests after every ref it
/// moves, so a handful behind is a seed between rounds; more than this
/// is a seed that has stopped, which is worth a warning and is not a
/// fork.
pub const FAR_BEHIND: u64 = 8;

/// What the fork check is called in the report.
const FORK_CHECK: &str = "fork check";

/// The fork rule applied to one seed's statement (D80), with nothing
/// fetched: the statement, the node id and current attestation the home
/// showed this reader, and the chain read from the home's own log.
///
/// Pure so the one judgement that can call a home a liar is testable
/// against a forged statement without a node that would forge one.
#[must_use]
pub fn fork_check(
    seed: &str,
    statement: &choir_node::replica::SignedStatement,
    home_node: &str,
    current: &str,
    chain: &choir_node::replica::SnapshotChain,
) -> Check {
    use choir_node::replica::Relation;
    if let Err(why) = statement.verify() {
        return Check::fail(
            FORK_CHECK,
            format!("{seed}: its statement does not verify: {why}"),
        );
    }
    let said = &statement.statement;
    if said.home_node_id != home_node {
        return Check::fail(
            FORK_CHECK,
            format!(
                "{seed}: witnesses the home {}, and this node is {home_node}",
                said.home_node_id
            ),
        )
        .with_fix("name in `seeds =` only seeds of the node `node =` names");
    }
    match chain.relate(&said.snapshot, current) {
        Relation::Same => Check::pass(
            FORK_CHECK,
            format!(
                "{seed}: agrees; it witnessed the home's current attestation (seq {})",
                said.at_seq
            ),
        ),
        Relation::Behind(steps) if steps > FAR_BEHIND => Check::warn(
            FORK_CHECK,
            format!(
                "{seed}: agrees, {steps} attestations behind the home's current one \
                 (it last witnessed seq {})",
                said.at_seq
            ),
        )
        .with_fix("read `replica` in the seed's GET /api/view: it says where replication stopped"),
        Relation::Behind(steps) => Check::pass(
            FORK_CHECK,
            format!(
                "{seed}: agrees; its statement is {steps} attestation(s) behind the home's \
                 current one"
            ),
        ),
        Relation::Ahead(steps) => Check::warn(
            FORK_CHECK,
            format!(
                "{seed}: agrees, but it has witnessed {steps} attestation(s) past the one the \
                 home is showing you, so the home may be serving you an older view"
            ),
        ),
        Relation::Fork => Check::fail(
            FORK_CHECK,
            format!(
                "{seed}: fork: the seed witnessed {} at seq {}, which is not on the \
                 attestation chain the home shows you (current {current}); the home has \
                 shown two histories",
                said.snapshot, said.at_seq
            ),
        )
        .with_fix(
            "stop acting on this home's answers; an export from the home and one from the \
             seed (choir-node --export) show which history each holds",
        ),
    }
}

/// `GET` through the crate's one credential path.
fn get(
    url: &str,
    auth: Option<&str>,
    user: Option<&str>,
    path: &str,
) -> Result<(u16, serde_json::Value), String> {
    let client = crate::mcp::HttpClient::new(url, auth.map(std::path::Path::new), user)?;
    let (status, body) = client.get(path)?;
    let value = serde_json::from_str(&body)
        .map_err(|_| format!("{url}{path} answered {status} with a body that is not JSON"))?;
    Ok((status, value))
}

/// What the seed says about its own copy, from its `/api/witness` body: a
/// gap, and a halt with the seq and reason. Both are warnings and not
/// failures, because neither says the home lied; a halt on a page that
/// failed a check is the reader's cue to look, which is why it is named
/// here rather than left to show as a statement falling behind.
fn copy_rows(seed: &str, witness: &serde_json::Value) -> Vec<Check> {
    let mut rows = Vec::new();
    if witness["gap"] == true {
        rows.push(
            Check::warn(
                "seed gap",
                format!("{seed}: its copy has a hole, so it is a copy and not a witness"),
            )
            .with_fix("re-seed it from an export of the home (choir-node --import)"),
        );
    }
    if let (Some(seq), Some(reason)) = (
        witness["halted"]["seq"].as_u64(),
        witness["halted"]["reason"].as_str(),
    ) {
        rows.push(
            Check::warn(
                "seed halted",
                format!("{seed}: stopped replicating at seq {seq}: {reason}"),
            )
            .with_fix("read that page from the home yourself; a seed never skips an entry"),
        );
    }
    rows
}

/// One seed's rows: what it says about its copy, and its fork check.
///
/// The seed is read **before** the home. An honest home's current
/// attestation is then never older than the one the seed folded, so a
/// statement the home's chain cannot reach is two histories rather than
/// a race between two reads.
fn seed_checks(
    home: &str,
    seed: &str,
    auth_for: &dyn Fn(&str) -> Option<String>,
    user: Option<&str>,
) -> Vec<Check> {
    let warn = |detail: String| vec![Check::warn(FORK_CHECK, format!("{seed}: {detail}"))];
    let seed_auth = auth_for(seed);
    let witness = match get(seed, seed_auth.as_deref(), user, "/api/witness") {
        Ok((200, witness)) => witness,
        Ok((status, body)) => {
            return warn(format!(
                "answered {status} on /api/witness, so its statement cannot be checked: {}",
                body["error"].as_str().unwrap_or("")
            ))
        }
        Err(why) => return warn(format!("cannot be read: {why}")),
    };
    let mut rows = copy_rows(seed, &witness);
    let Some(statement) = choir_node::replica::SignedStatement::from_json(&witness["latest"])
    else {
        rows.extend(warn(
            "has signed no statement yet: it has folded no attestation".to_string(),
        ));
        return rows;
    };
    let home_auth = auth_for(home);
    let view = match get(home, home_auth.as_deref(), user, "/api/view") {
        Ok((200, view)) => view,
        Ok((status, _)) => {
            rows.extend(warn(format!("the home answered {status} on /api/view")));
            return rows;
        }
        Err(why) => {
            rows.extend(warn(format!("the home cannot be read: {why}")));
            return rows;
        }
    };
    let home_node = view["log"]["node"].as_str().unwrap_or_default().to_string();
    let (Some(current), Some(current_at)) = (
        view["snapshot"]["id"].as_str().map(str::to_string),
        view["snapshot"]["at_seq"].as_u64(),
    ) else {
        rows.extend(warn(
            "the home has made no attestation to compare".to_string(),
        ));
        return rows;
    };
    // The chain from the lower of the two positions to the head, which is
    // everything the walk between them can pass through.
    let mut chain = choir_node::replica::SnapshotChain::default();
    let mut from = statement.statement.at_seq.min(current_at);
    loop {
        match get(
            home,
            home_auth.as_deref(),
            user,
            &format!("/api/log?from={from}"),
        ) {
            Ok((200, page)) => {
                let entries = page["entries"].as_array().cloned().unwrap_or_default();
                let Some(last) = entries.last().and_then(|e| e["seq"].as_u64()) else {
                    break;
                };
                chain.read_page(&entries);
                from = last + 1;
            }
            Ok((status, _)) => {
                rows.extend(warn(format!(
                    "the home answered {status} on /api/log; walking its attestation chain \
                     needs the node-wide read grant (`@node auditor`)"
                )));
                return rows;
            }
            Err(why) => {
                rows.extend(warn(format!("the home's log cannot be read: {why}")));
                return rows;
            }
        }
    }
    rows.push(fork_check(seed, &statement, &home_node, &current, &chain));
    rows
}

/// The fork check for every seed `.choir/config` names (D80), against the
/// node `home` is.
///
/// `auth_for` is the caller's own credential rule, so a seed is handed a
/// credential exactly when every other command would hand it one.
pub fn seeds(
    home: Option<&str>,
    seeds: &[String],
    auth_for: &dyn Fn(&str) -> Option<String>,
    user: Option<&str>,
) -> Vec<Check> {
    let Some(home) = home else {
        return vec![Check::warn(
            FORK_CHECK,
            "seeds are configured and no node is, so there is no home to compare them with",
        )
        .with_fix("write `node = <url>` beside `seeds =` in .choir/config")];
    };
    seeds
        .iter()
        .flat_map(|seed| seed_checks(home, seed, auth_for, user))
        .collect()
}

/// What this `choir` is: its version, the commit it was built from, and
/// the file it is running as.
///
/// First in the report because every other line describes the machine
/// and this one describes the thing doing the reporting. The common
/// support question after there are two ways to obtain the binary is
/// "which one am I running" — an old copy in `~/.cargo/bin` shadowing a
/// new one in a build tree answers every other check identically.
///
/// The stamp source is carried through rather than summarised: `env`
/// means something set `CHOIR_GIT_HEAD` at build time, which is the
/// release workflow and the on-box installer; `git` means a build in a
/// checkout; `unavailable` means neither, and the commit reads
/// `unknown`. See `crates/choir-node/build.rs`.
///
/// Deliberately not claimed: *which* installer put it there. The shell
/// installer writes no receipt (the self-updater that would consume one
/// is off, on purpose), so a binary in `$CARGO_HOME/bin` could equally
/// have come from `cargo install` or `cargo binstall`. Naming one of
/// them would be a guess printed as a fact, in the one command whose
/// whole job is to stop people guessing.
fn self_check() -> Check {
    let Ok(exe) = std::env::current_exe() else {
        // Not a failure: the binary plainly ran. It just cannot say
        // which file it is, which is a curiosity rather than a fault.
        return Check::warn(
            "choir",
            format!(
                "{} {} (this process cannot name its own path)",
                env!("CARGO_PKG_VERSION"),
                choir_node::build_line()
            ),
        );
    };
    let where_ = if crate::supervise::in_build_directory(&exe) {
        "a build directory"
    } else if in_cargo_home(&exe) {
        "an install directory"
    } else {
        "not an install or build directory"
    };
    Check::pass(
        "choir",
        format!(
            "{} {}, {} ({where_})",
            env!("CARGO_PKG_VERSION"),
            choir_node::build_line(),
            exe.display()
        ),
    )
}

/// Whether a path sits in the `bin` directory both installers write to.
///
/// `CARGO_HOME` before `HOME/.cargo`, in that order, because that is the
/// order the shell installer resolves it in and this has to agree with
/// the thing it is describing.
fn in_cargo_home(exe: &std::path::Path) -> bool {
    let bin = match std::env::var_os("CARGO_HOME") {
        Some(home) => PathBuf::from(home).join("bin"),
        None => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(".cargo").join("bin"),
            None => return false,
        },
    };
    exe.parent() == Some(bin.as_path())
}

/// The six facts that describe a machine *hosting* a node, as opposed
/// to one talking to somebody else's.
///
/// Appended to [`run`] rather than folded into it, because they are only
/// findings on a machine that has a node: reporting "linger is off" to a
/// laptop that only ever clones would be six rows of noise on a report
/// whose value is that every row means something.
///
/// The last of them is a request to this node's *public* name, made from
/// the node itself. That is a hairpin — the packet may never leave the
/// box — and it still answers the two questions that go wrong most
/// often: whether the name resolves, and whether the certificate
/// presented matches it. A firewall closed to the outside is the case it
/// cannot see, and the firewall row is what covers that.
#[must_use]
pub fn host(state: &std::path::Path, auth: Option<&str>) -> Vec<Check> {
    let layout = crate::serve::Layout::new(state, port_of_state(state));
    let tls = layout.tls();
    let mut checks = vec![Check::pass(
        "bind",
        match tls {
            Some(_) => format!("{}:{} (reachable from outside)", layout.bind(), layout.port),
            None => format!("127.0.0.1:{} (loopback only)", layout.port),
        },
    )];

    checks.push(match &tls {
        Some((cert, _)) => Check::pass("tls", format!("on, {}", cert.display())),
        // A pass, not a warning. A loopback node without a certificate
        // is correct rather than degraded — invariant 9 is what makes it
        // so — and a report that cries "degraded" at every laptop is a
        // report people stop reading.
        // A pass, not a warning. A loopback node without a certificate
        // is correct rather than degraded — invariant 9 is what makes it
        // so — and a report that cries "degraded" at every laptop is a
        // report people stop reading. The pointer goes in the detail
        // rather than in a fix, because a fix on a passing row is a fix
        // for a problem the row just said there isn't.
        None => Check::pass(
            "tls",
            "off — not needed on loopback; `choir host --domain <name>` publishes it",
        ),
    });

    checks.push(match &tls {
        None => Check::pass("certificate", "none needed for a loopback node"),
        Some((cert, _)) => match crate::tls::expiry(cert) {
            Err(error) => Check::fail("certificate", error)
                .with_fix("sudo choir node tls <domain> --user $(id -un)"),
            Ok(when) => {
                // `openssl -checkend` rather than parsing that date and
                // doing the arithmetic here: the question is "is it
                // still good", openssl answers it with an exit code, and
                // a date parser written for one report is a date parser
                // that is wrong in one time zone.
                if !checkend(cert, 0) {
                    Check::fail("certificate", format!("EXPIRED — was valid until {when}"))
                        .with_fix("sudo certbot renew --force-renewal && choir node restart")
                } else if !checkend(cert, 21 * 24 * 3600) {
                    Check::warn("certificate", format!("expires within 21 days — {when}"))
                        .with_fix("sudo certbot renew --dry-run   (checks the renewal path)")
                } else {
                    Check::pass("certificate", format!("valid until {when}"))
                }
            }
        },
    });

    let user = crate::host::username();
    checks.push(match crate::host::linger(&user) {
        None => Check::pass("linger", "not a systemd --user machine"),
        Some(true) => Check::pass("linger", format!("on for {user}")),
        Some(false) => Check::fail(
            "linger",
            format!("off — the node dies when {user} logs out"),
        )
        .with_fix(format!("sudo loginctl enable-linger {user}")),
    });

    checks.push(unit_check());

    checks.push(match layout.public() {
        None => Check::pass("public url", "none; this node is not published"),
        Some(url) => {
            match crate::mcp::HttpClient::new(&url, auth.map(std::path::Path::new), None) {
                Err(error) => Check::fail("public url", format!("{url}: {error}")),
                Ok(client) => match client.get("/healthz") {
                    Ok((200 | 401, _)) => Check::pass("public url", format!("{url} answers")),
                    Ok((code, _)) => Check::warn("public url", format!("{url} answered {code}")),
                    Err(error) => Check::fail("public url", format!("{url}: {error}")).with_fix(
                        "check DNS points here, and that the port is open in the firewall",
                    ),
                },
            }
        }
    });

    checks
}

/// Whether a certificate is still valid `seconds` from now.
fn checkend(cert: &std::path::Path, seconds: u64) -> bool {
    std::process::Command::new("openssl")
        .args(["x509", "-noout", "-checkend", &seconds.to_string(), "-in"])
        .arg(cert)
        .output()
        .is_ok_and(|out| out.status.success())
}

/// Whether the service manager has this node loaded and running.
fn unit_check() -> Check {
    let Some(supervisor) = crate::supervise::Supervisor::detect() else {
        return Check::warn(
            "unit",
            format!("no service manager on {}", std::env::consts::OS),
        )
        .with_fix("run it in the foreground: choir node serve");
    };
    let (program, args) = match supervisor {
        crate::supervise::Supervisor::Launchd => (
            "launchctl",
            vec![
                "print".to_string(),
                format!("gui/{}/{}", crate::tls::uid(), crate::supervise::LABEL),
            ],
        ),
        crate::supervise::Supervisor::Systemd => (
            "systemctl",
            vec![
                "--user".to_string(),
                "is-active".to_string(),
                "choir-node.service".to_string(),
            ],
        ),
    };
    match std::process::Command::new(program).args(&args).output() {
        Ok(out) if out.status.success() => Check::pass("unit", "loaded and running"),
        // A warning, not a failure. An unsupervised node is a real
        // state and a supported one — `choir node serve` in a terminal,
        // and `choir host --foreground` in a container, where the
        // runtime is the supervisor and there is no unit to find. That
        // the node is *answering* is the `node` row's question, and it
        // is the one that fails when nothing does.
        Ok(_) => Check::warn("unit", "not loaded — this node is not supervised")
            .with_fix("choir node install, to survive a logout and a reboot"),
        Err(error) => Check::warn("unit", format!("could not ask {program}: {error}")),
    }
}

/// The port a state directory's node was installed on.
///
/// Read back out of the public URL when there is one, so a published
/// node reports the port it actually serves rather than the default.
fn port_of_state(state: &std::path::Path) -> u16 {
    let layout = crate::serve::Layout::new(state, 8417);
    layout
        .public()
        .as_deref()
        .and_then(crate::serve::port_of)
        .unwrap_or(8417)
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

/// The line above the checks: what this machine is set up as.
///
/// Separate from [`report`] rather than folded into it because the role
/// is read off paths and `report` is handed findings. Keeping the two
/// apart is what lets the caller's own resolution — the `.choir/config`
/// walk, the `--auth-file` flag — be the thing reported on, which is the
/// same reason [`run`] takes its inputs rather than discovering them.
#[must_use]
pub fn heading(role: crate::join::Role, style: Style) -> String {
    format!("\n  {}\n\n", style.bold(role.line()))
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

#[cfg(test)]
mod tests {
    /// The one judgement here that can call a home a liar: a statement,
    /// well signed, over an attestation the home's chain does not reach.
    #[test]
    fn a_statement_off_the_homes_chain_is_a_fork() {
        use choir_node::replica::{SignedStatement, SnapshotChain, WitnessStatement};
        let key = choir_identity::ActorKey::generate();
        let statement = |snapshot: &str| {
            SignedStatement::sign(
                &key,
                WitnessStatement {
                    format_version: 1,
                    witness: key.actor_id().to_hex(),
                    home_node_id: "1e-home".into(),
                    snapshot: snapshot.into(),
                    at_seq: 2,
                    seen_at_seq: 2,
                },
            )
        };
        let mut chain = SnapshotChain::default();
        chain.insert("s1".into(), None);
        chain.insert("s2".into(), Some("s1".into()));

        let same = super::fork_check("seed", &statement("s2"), "1e-home", "s2", &chain);
        assert_eq!(same.status, super::Status::Pass, "{}", same.detail);
        let behind = super::fork_check("seed", &statement("s1"), "1e-home", "s2", &chain);
        assert_eq!(behind.status, super::Status::Pass, "{}", behind.detail);
        let forged = super::fork_check("seed", &statement("elsewhere"), "1e-home", "s2", &chain);
        assert_eq!(forged.status, super::Status::Fail);
        assert!(forged.detail.contains("fork"), "{}", forged.detail);
        let other = super::fork_check("seed", &statement("s2"), "1e-other", "s2", &chain);
        assert_eq!(other.status, super::Status::Fail);

        let mut far = SnapshotChain::default();
        far.insert("a0".into(), None);
        for i in 1..=super::FAR_BEHIND + 1 {
            far.insert(format!("a{i}"), Some(format!("a{}", i - 1)));
        }
        let current = format!("a{}", super::FAR_BEHIND + 1);
        let stale = super::fork_check("seed", &statement("a0"), "1e-home", &current, &far);
        assert_eq!(stale.status, super::Status::Warn);
        assert!(
            stale.detail.contains("attestations behind"),
            "{}",
            stale.detail
        );
    }

    /// A halt is named by seq and reason, and a clean copy adds no row.
    #[test]
    fn a_halted_seed_is_named_with_the_seq_it_stopped_at() {
        let clean = serde_json::json!({"gap": false, "halted": null});
        assert!(super::copy_rows("seed", &clean).is_empty());
        let halted = serde_json::json!({
            "gap": false,
            "halted": {"seq": 3, "reason": "entry 3 does not hash to what the page claimed"},
        });
        let rows = super::copy_rows("seed", &halted);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, super::Status::Warn);
        assert_eq!(rows[0].name, "seed halted");
        assert!(rows[0].detail.contains("seq 3"), "{}", rows[0].detail);
        assert!(
            rows[0].detail.contains("does not hash"),
            "{}",
            rows[0].detail
        );
    }
}
