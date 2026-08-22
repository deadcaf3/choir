//! SSH transport for git (D31): the host's `sshd`, a forced command, and
//! this shim.
//!
//! The node speaks git over HTTPS with basic auth, which is what every
//! agent uses and what no human expects. `git@host:owner/repo.git` is the
//! address a person types, and the way to serve it here is the way Gitea
//! and gitolite serve it: one OS account, one line per registered key in
//! its `authorized_keys`, and a forced command that never gives out a
//! shell.
//!
//! ```text
//! command="choir-ssh --root /srv/repos --user alice --acl-file /etc/choir/acl \
//!   --handoff /srv/repos/.choir/ssh-handoff",restrict ssh-ed25519 AAAA... alice
//! ```
//!
//! There is deliberately no SSH server inside the daemon. Every Rust SSH
//! library within reach is tokio-based, and this workspace is synchronous
//! threads everywhere except `choir-actor`; an in-process server would
//! make an async runtime the largest dependency the project has taken, to
//! terminate a protocol the operating system already terminates.
//!
//! The three questions stay split exactly as they are on the HTTP path:
//!
//! * *which key* — sshd answers it, by matching the presented public key
//!   against `authorized_keys`;
//! * *which actor* — the matched line answers it, because `--user` is
//!   written into that key's forced command by the operator. The client
//!   cannot influence it: sshd runs the forced command and puts whatever
//!   the client asked for in `SSH_ORIGINAL_COMMAND` instead;
//! * *which repository* — [`Acl`] answers it, the same file and the same
//!   [`crate::acl::git_requirement`] mapping the HTTP route consults, so
//!   the two paths cannot drift into granting different things.
//!
//! A push then runs the repository's `pre-receive` hook like any other
//! push, because [`Shim::decide`] hands the sequencer callback to git in
//! the environment. That is the point of the whole exercise: an SSH push
//! joins the same total order as an HTTPS one, rather than being a second,
//! unsequenced way in. A shim with no `--handoff` refuses pushes outright
//! rather than let one through unsequenced.
//!
//! The operator's guide to every way a client reaches a node:
//!
#![doc = include_str!("../../../docs/operating/transports.md")]

use std::path::{Path, PathBuf};

use crate::acl::{self, Acl, Level};

/// The git service a client asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    /// Serving a clone or fetch: `git-upload-pack`.
    UploadPack,
    /// Receiving a push: `git-receive-pack`.
    ReceivePack,
}

impl Service {
    /// git's subcommand name, as passed to the real binary.
    #[must_use]
    pub fn verb(self) -> &'static str {
        match self {
            Self::UploadPack => "upload-pack",
            Self::ReceivePack => "receive-pack",
        }
    }

    /// The smart-HTTP path doing the same thing, used to borrow the HTTP
    /// route's own grant decision rather than restate it. See
    /// [`Shim::required_level`].
    fn http_endpoint(self) -> &'static str {
        match self {
            Self::UploadPack => "git-upload-pack",
            Self::ReceivePack => "git-receive-pack",
        }
    }
}

/// A parsed `SSH_ORIGINAL_COMMAND`: what the client asked to run, and on
/// which repository, before any of it is believed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Which of the two services was named.
    pub service: Service,
    /// The repository argument exactly as the client spelled it, still
    /// unvalidated. [`canonical_repo`] is what turns it into a name this
    /// node will act on.
    pub repo: String,
}

/// The git invocation a resolved request becomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exec {
    /// git subcommand: `upload-pack` or `receive-pack`.
    pub verb: &'static str,
    /// Absolute path of the bare repository to serve.
    pub dir: PathBuf,
    /// Environment to add for the child, which is how the `pre-receive`
    /// hook learns where to send this push. Empty for a fetch, which runs
    /// no hook.
    pub env: Vec<(String, String)>,
}

/// Where the shim sends the sequencer callback, and the loopback secret
/// that callback authenticates with.
///
/// Written by the daemon at startup ([`crate::Node::write_ssh_handoff`])
/// and read by the shim on each invocation. It exists because both values
/// are runtime state: the port may be ephemeral and the secret is minted
/// per process, so neither can be written into an `authorized_keys` line
/// that has to survive restarts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handoff {
    /// Base URL of the running daemon, e.g. `http://127.0.0.1:8417`.
    pub api: String,
    /// The daemon's loopback secret, sent as `X-Choir-Internal`.
    pub secret: String,
    /// The ACL file the daemon itself is enforcing, when it has one.
    ///
    /// A forced command whose `--acl-file` was forgotten would otherwise
    /// be an authenticated key reaching every repository on a node that
    /// authorizes every HTTP request — the ACL bypassed by the transport
    /// rather than by a grant. [`Shim::decide`] falls back to this path,
    /// so forgetting the flag costs an operator nothing and grants
    /// nobody anything.
    pub acl: Option<String>,
    /// The self-service credential store the daemon is enforcing (D36),
    /// when it has one.
    ///
    /// The grants a token was issued with live there rather than in the
    /// ACL file, so a shim that read only the file would refuse every
    /// self-served account — the transport disagreeing with the daemon
    /// about who holds what. Read-only here: the shim never writes an
    /// account, and takes only the grants.
    pub accounts: Option<String>,
}

impl Handoff {
    /// Parses the two-key file format: `<key> <value>` lines, `#`
    /// comments, blank lines ignored.
    ///
    /// # Errors
    ///
    /// Returns a message when a line is malformed, a key is unknown, or
    /// either key is missing. A half-read handoff is refused rather than
    /// used, because the failure it produces downstream is a push that
    /// silently misses the sequencer.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (mut api, mut secret, mut acl, mut accounts) = (None, None, None, None);
        for (index, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let number = index + 1;
            let Some((key, value)) = line.split_once(char::is_whitespace) else {
                return Err(format!("line {number}: expected `<key> <value>`"));
            };
            match key {
                "api" => api = Some(value.trim().to_string()),
                "secret" => secret = Some(value.trim().to_string()),
                "acl" => acl = Some(value.trim().to_string()),
                "accounts" => accounts = Some(value.trim().to_string()),
                other => return Err(format!("line {number}: unknown key `{other}`")),
            }
        }
        match (api, secret) {
            (Some(api), Some(secret)) => Ok(Self {
                api,
                secret,
                acl,
                accounts,
            }),
            (None, _) => Err("no `api` line".to_string()),
            (_, None) => Err("no `secret` line".to_string()),
        }
    }

    /// Reads and parses the file at `path`.
    ///
    /// # Errors
    ///
    /// Returns a message when the file cannot be read or does not parse.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// Writes a [`Handoff`] file, readable only by the user that wrote it.
///
/// # Errors
///
/// Any I/O error creating, writing or chmod-ing the file.
pub fn write_handoff(
    path: &Path,
    api: &str,
    secret: &str,
    acl: Option<&Path>,
    accounts: Option<&Path>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let acl = acl.map_or(String::new(), |path| format!("acl {}\n", path.display()));
    let accounts = accounts.map_or(String::new(), |path| {
        format!("accounts {}\n", path.display())
    });
    std::fs::write(
        path,
        format!(
            "# choir ssh handoff, rewritten on every start. Not a file to \
             share: the secret line authenticates the sequencer callback.\n\
             api {api}\nsecret {secret}\n{acl}{accounts}"
        ),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Splits a command line the way sshd's client wrote it: single quotes,
/// no expansion, no operators.
///
/// git builds this string itself (`git-upload-pack '<path>'`, quoting by
/// the same rules `sq_quote` uses), so the grammar accepted here is
/// deliberately the smallest one that reads what git writes. Anything
/// carrying shell punctuation is refused rather than interpreted: the
/// shim never runs a shell, but a string it cannot fully account for is
/// one it should not act on either.
fn split_words(command: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quoted = false;
    for c in command.chars() {
        if quoted {
            if c == '\'' {
                quoted = false;
            } else {
                word.push(c);
            }
            continue;
        }
        match c {
            '\'' => {
                quoted = true;
                started = true;
            }
            c if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            ';' | '&' | '|' | '<' | '>' | '`' | '$' | '(' | ')' | '{' | '}' | '\\' | '"' | '*'
            | '?' | '#' | '~' | '=' | '!' | '\n' => {
                return Err(format!("`{c}` is not allowed in a git command over ssh"));
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if quoted {
        return Err("unterminated quote".to_string());
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

/// Parses the command sshd put in `SSH_ORIGINAL_COMMAND`.
///
/// Accepts exactly what a git client sends — `git-upload-pack '<repo>'`,
/// `git-receive-pack '<repo>'`, and the dashless `git upload-pack
/// '<repo>'` spelling — with exactly one argument. Everything else,
/// `git-upload-archive` included, is refused: this account exists to
/// serve two services and a surface it does not serve is a surface it
/// cannot get wrong.
///
/// # Errors
///
/// Returns a message safe to print to whoever ran the command.
pub fn parse_command(original: &str) -> Result<Request, String> {
    let mut parts = split_words(original)?;
    // `git upload-pack <repo>` is the same request as `git-upload-pack
    // <repo>`; join the two spellings before looking at the verb.
    if parts.first().map(String::as_str) == Some("git") && parts.len() > 1 {
        let joined = format!("git-{}", parts[1]);
        parts.splice(0..2, [joined]);
    }
    let (verb, rest) = parts
        .split_first()
        .ok_or_else(|| "no command given".to_string())?;
    let service = match verb.as_str() {
        "git-upload-pack" => Service::UploadPack,
        "git-receive-pack" => Service::ReceivePack,
        other => {
            return Err(format!(
                "`{other}` is not served here; this account serves git-upload-pack and \
                 git-receive-pack"
            ))
        }
    };
    match rest {
        [repo] => Ok(Request {
            service,
            repo: repo.clone(),
        }),
        [] => Err(format!("{verb} needs a repository")),
        _ => Err(format!("{verb} takes exactly one repository")),
    }
}

/// Canonical repository name for a path a client sent: `owner/repo.git`,
/// the same spelling the smart-HTTP path derives from a URL.
///
/// The spelling matters beyond tidiness. Ref names in the op log are
/// `<repo>:<refname>` with `<repo>` in exactly this form, so a shim that
/// normalized differently would push into a second, parallel namespace
/// that looks fine in isolation and never converges with the HTTP one.
///
/// Both `owner/repo` and `owner/repo.git` are accepted, because both are
/// spellings humans type. Two segments are required: a grant cannot be
/// written for any other shape (see [`crate::acl`]), so a deeper path
/// could never be authorized anyway.
///
/// # Errors
///
/// Returns a message naming what is wrong with the path.
pub fn canonical_repo(raw: &str) -> Result<String, String> {
    let path = raw.trim_start_matches('/');
    let segments: Vec<&str> = path.split('/').collect();
    let [owner, name] = segments.as_slice() else {
        return Err(format!(
            "`{raw}` is not a repository; write `owner/repo.git`"
        ));
    };
    for segment in [owner, name] {
        if segment.is_empty() {
            return Err(format!(
                "`{raw}` is not a repository; write `owner/repo.git`"
            ));
        }
        // A leading dot would put `<root>/.choir` — the node's key, its
        // log, this very handoff file — one well-chosen path away from a
        // fetch. `.` and `..` fall out of the same rule, so traversal
        // needs no separate check.
        if segment.starts_with('.') {
            return Err(format!("`{segment}` may not start with a dot"));
        }
        if let Some(bad) = segment
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
        {
            return Err(format!("`{bad}` is not allowed in a repository name"));
        }
    }
    Ok(format!("{owner}/{}.git", acl::normalize_repo(name)))
}

/// One invocation's configuration, all of it from the flags the operator
/// wrote into the forced command.
#[derive(Debug, Clone)]
pub struct Shim {
    /// Repository root, the same directory the daemon serves.
    pub root: PathBuf,
    /// The choir user this key belongs to. Written by the operator into
    /// the key's own `authorized_keys` line; the client cannot reach it.
    pub user: String,
    /// The ACL file (D29), re-read on every invocation so an edited grant
    /// takes effect on the next command rather than on a restart. `None`
    /// falls back to the ACL the daemon named in the [`Handoff`], and
    /// only when that is absent too does this become the daemon's
    /// authenticate-only behaviour: any registered key reaches every
    /// repository.
    pub acl_file: Option<PathBuf>,
    /// The daemon's [`Handoff`] file: where a push reports itself, and
    /// which ACL the daemon is enforcing. `None` refuses pushes.
    pub handoff: Option<PathBuf>,
}

impl Shim {
    /// The grant this request needs, obtained by asking the HTTP route's
    /// own mapping about the URL that would do the same thing.
    ///
    /// Restating the mapping here would work today and drift later: the
    /// day a level changes in [`crate::acl::git_requirement`], an SSH
    /// client would keep getting the old answer. Borrowing it means there
    /// is one mapping on the node and both transports read it.
    fn required_level(repo: &str, service: Service) -> Result<Level, String> {
        let url = format!("/{repo}/{}", service.http_endpoint());
        acl::git_requirement("POST", &url)
            .map(|(_, level)| level)
            .ok_or_else(|| "no such repository".to_string())
    }

    /// Resolves one `SSH_ORIGINAL_COMMAND` into the git invocation to
    /// become, or the message to fail with.
    ///
    /// # Errors
    ///
    /// Every refusal path: an unparseable or unserved command, a name
    /// that is not a repository, a missing grant, a repository that is
    /// not there, and a push with nowhere to send the sequencer callback.
    pub fn decide(&self, original: &str) -> Result<Exec, String> {
        let request = parse_command(original)?;
        let repo = canonical_repo(&request.repo)?;
        let level = Self::required_level(&repo, request.service)?;
        // Read before the ACL decision and for fetches too, not only for
        // pushes: the daemon's own ACL path lives in here, so a handoff
        // that cannot be read is a request whose authorization cannot be
        // established. That fails the fetch rather than serving it
        // ungated.
        let handoff = self.handoff.as_deref().map(Handoff::load).transpose()?;
        // The flag if the operator wrote one, otherwise whatever the
        // daemon says it is enforcing. Forgetting `--acl-file` on one
        // `authorized_keys` line should not be the difference between a
        // gated repository and an open one.
        let acl_file = self.acl_file.clone().or_else(|| {
            handoff
                .as_ref()
                .and_then(|h| h.acl.as_deref())
                .map(PathBuf::from)
        });
        // Read the table per invocation rather than caching it: a shim
        // process handles one command and exits, so "hot reload" here is
        // just not holding a stale copy.
        //
        // Both halves of the daemon's table, for the same reason the
        // daemon merges them (D36): a grant issued through self-service
        // lives in the store rather than in the file, and a transport
        // that consulted only one of them would answer a different
        // question than the HTTP route answers about the same user.
        let file_table = acl_file.as_deref().map(Acl::load).transpose()?;
        let store_table = handoff
            .as_ref()
            .and_then(|h| h.accounts.as_deref())
            .map(|path| crate::accounts::grants_acl(Path::new(path)))
            .transpose()?;
        let table = match (file_table, store_table) {
            (Some(file), Some(store)) => Some(file.merged(&store)),
            (Some(one), None) | (None, Some(one)) => Some(one),
            (None, None) => None,
        };
        if let Some(table) = table {
            // Dated here rather than at load, the same rule the HTTP
            // chokepoint follows (D66): a grant with a lapsed deadline
            // must not reach a check, and ssh gets one process per
            // connection so there is nothing cached to go stale.
            if let Some(denial) = table.at(crate::accounts::now_secs()).check(
                &self.user,
                &acl::Scope::Repo(acl::normalize_repo(&repo)),
                level,
            ) {
                return Err(denial.reason);
            }
        }
        let dir = self.root.join(&repo);
        // Deliberately the same words the ACL uses for a repository the
        // caller may not read: whether a name is unreadable or absent is
        // not something an unauthorized caller gets to learn.
        if !dir.join("objects").is_dir() {
            return Err("no such repository".to_string());
        }
        let mut env = Vec::new();
        if request.service == Service::ReceivePack {
            let Some(handoff) = handoff else {
                return Err(
                    "this account cannot accept pushes: it was installed without --handoff, \
                     so a push would bypass the sequencer"
                        .to_string(),
                );
            };
            let api = handoff.api.trim_end_matches('/');
            env = vec![
                ("CHOIR_API".to_string(), format!("{api}/api/git-update")),
                ("CHOIR_ABORT".to_string(), format!("{api}/api/git-abort")),
                ("CHOIR_REPO".to_string(), repo),
                ("CHOIR_USER".to_string(), self.user.clone()),
                ("CHOIR_INTERNAL".to_string(), handoff.secret),
            ];
        }
        Ok(Exec {
            verb: request.service.verb(),
            dir,
            env,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_what_git_sends() {
        assert_eq!(
            parse_command("git-upload-pack 'owner/repo.git'").unwrap(),
            Request {
                service: Service::UploadPack,
                repo: "owner/repo.git".to_string()
            }
        );
        assert_eq!(
            parse_command("git-receive-pack 'owner/repo.git'")
                .unwrap()
                .service,
            Service::ReceivePack
        );
        // The dashless spelling, and the leading slash an `ssh://` URL
        // produces, are the same request.
        assert_eq!(
            parse_command("git upload-pack '/owner/repo.git'")
                .unwrap()
                .repo,
            "/owner/repo.git"
        );
        // Unquoted is legal too; some clients do not quote a plain name.
        assert_eq!(
            parse_command("git-upload-pack owner/repo.git")
                .unwrap()
                .repo,
            "owner/repo.git"
        );
    }

    #[test]
    fn refuses_anything_but_the_two_services() {
        for command in [
            "git-upload-archive 'owner/repo.git'",
            "scp -f /etc/passwd",
            "sh",
            "",
        ] {
            assert!(parse_command(command).is_err(), "accepted {command:?}");
        }
    }

    #[test]
    fn refuses_shell_punctuation_and_extra_arguments() {
        for command in [
            "git-upload-pack 'owner/repo.git'; rm -rf /",
            "git-upload-pack 'owner/repo.git' && curl evil",
            "git-upload-pack `whoami`",
            "git-upload-pack $(whoami)",
            "git-upload-pack 'a/b.git' 'c/d.git'",
            "git-upload-pack 'owner/repo.git",
        ] {
            assert!(parse_command(command).is_err(), "accepted {command:?}");
        }
    }

    #[test]
    fn punctuation_is_refused_where_the_rule_lives() {
        // Not what stops an injection — the one-argument rule and the
        // name charset do that, and both are tested above. This rule is
        // tested here because it is otherwise invisible: every string it
        // rejects is also rejected further along, so a mutation that
        // deleted it would leave every other test green.
        for command in [
            "git-upload-pack 'a/b.git' > /tmp/x",
            "git-upload-pack `id`",
            "git-upload-pack $(id)",
            "git-upload-pack a/b.git | tee /tmp/x",
        ] {
            assert!(split_words(command).is_err(), "accepted {command:?}");
        }
    }

    #[test]
    fn canonicalizes_to_the_spelling_the_op_log_uses() {
        for spelling in ["owner/repo", "owner/repo.git", "/owner/repo.git"] {
            assert_eq!(canonical_repo(spelling).unwrap(), "owner/repo.git");
        }
    }

    #[test]
    fn refuses_paths_that_are_not_two_plain_segments() {
        for path in [
            "../etc/passwd",
            "owner/../../etc/passwd",
            "a/b/c.git",
            "owner",
            "",
            "/",
            "owner/",
            ".choir/node.key",
            "owner/.choir",
            "own*er/repo",
            "owner/re po",
        ] {
            assert!(canonical_repo(path).is_err(), "accepted {path:?}");
        }
    }

    #[test]
    fn asks_for_the_level_the_http_route_asks_for() {
        assert_eq!(
            Shim::required_level("owner/repo.git", Service::UploadPack).unwrap(),
            Level::Read
        );
        assert_eq!(
            Shim::required_level("owner/repo.git", Service::ReceivePack).unwrap(),
            // `propose` since D60, and this test is the proof the
            // borrowing works: nothing in this file changed to say so.
            // The refname half is transport-agnostic too, because an SSH
            // push runs the same `pre-receive` hook and reaches the same
            // `/api/git-update`.
            Level::Propose
        );
    }

    // A push with no `--handoff` is refused against a real repository in
    // `tests/it/ssh.rs`; here there is nothing on disk to refuse for the
    // right reason, and the wrong reason ("no such repository") would
    // have passed a laxer assertion.

    #[test]
    fn handoff_round_trips_and_partial_files_are_refused() {
        let parsed = Handoff::parse("# comment\napi http://127.0.0.1:1/\n\nsecret abc\n").unwrap();
        assert_eq!(parsed.api, "http://127.0.0.1:1/");
        assert_eq!(parsed.secret, "abc");
        assert_eq!(parsed.acl, None);
        // A daemon enforcing an ACL says so, and the shim inherits it.
        let gated = Handoff::parse("api http://x\nsecret abc\nacl /etc/choir/acl\n").unwrap();
        assert_eq!(gated.acl.as_deref(), Some("/etc/choir/acl"));
        assert!(Handoff::parse("api http://x\n").is_err());
        assert!(Handoff::parse("secret abc\n").is_err());
        assert!(Handoff::parse("port 22\n").is_err());
        assert!(Handoff::parse("api\n").is_err());
    }
}
