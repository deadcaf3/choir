//! Per-repository authorization (D29).
//!
//! The node's basic-auth check answers *who is this* and returns a
//! username. This module answers the other question — *may this actor do
//! this to this repository* — which until D29 nothing asked. Without it
//! any valid credential reaches every repository on the node: safe while
//! one operator holds the only token, and the first thing that matters
//! when a second one is issued.
//!
//! The grammar is three whitespace-separated columns, `#` comments, blank
//! lines ignored — the same shape as `--auth-file`, so the operator
//! learns one file format rather than two:
//!
//! ```text
//! # <user>   <repo|*|@node>   <level>
//! alice      owner/project    write
//! alice      owner/notes      read
//! bob        *                read
//! carol      @node            auditor
//! ```
//!
//! Two levels, `read` < `write`. There is deliberately no `admin`: no
//! endpoint performs a repository-scoped administrative action today
//! (repository creation is the `--create` flag, ref protection is
//! `--protected-refs`, both operator-side files), and a level with no
//! operation behind it only invites a meaningless grant.
//!
//! `*` covers every repository and never covers [`Scope::Node`]: the op
//! log and the ref-state attestation describe the whole node, not a
//! repository, and are gated rather than filtered because filtering a
//! hash chain or a complete-ref-state snapshot destroys the property each
//! exists to provide.

use std::collections::HashMap;
use std::path::Path;

use choir_view::{OpKind, ViewOp};

/// What a grant applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// One repository, in the canonical spelling produced by
    /// [`normalize_repo`].
    Repo(String),
    /// Every repository on the node, written `*` in the file.
    ///
    /// Never matches [`Scope::Node`]. `*` is a statement about
    /// repositories, and the node's own log, attestation and
    /// repository-less ops are not a repository.
    AllRepos,
    /// The node itself, written `@node` in the file: the op log, the
    /// ref-state attestation, and ops that name no repository.
    Node,
}

/// Grant strength. [`Level::Read`] is implied by [`Level::Write`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Clone and fetch a repository; read the node-scoped endpoints.
    /// Spelled `read` on a repository and `auditor` on `@node`.
    Read,
    /// Everything [`Level::Read`] allows, plus push, provision a
    /// workspace, and submit ops.
    Write,
}

/// A refused request: the status to answer with, and the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial {
    /// HTTP status. `404` when the actor may not even read the target,
    /// so a missing grant does not confirm that the repository exists;
    /// `403` when they can read it but not perform this operation.
    pub status: u16,
    /// Human-readable reason, safe to return to the caller.
    pub reason: String,
}

/// A parsed ACL file: which users hold which grants.
///
/// Empty means nobody holds anything, which under a configured ACL denies
/// every request. That is the intended failure mode, and the reason a
/// malformed file is never partially applied.
#[derive(Debug, Default, Clone)]
pub struct Acl {
    grants: HashMap<String, Vec<(Scope, Level)>>,
}

impl Acl {
    /// Parses the ACL grammar described in the module documentation.
    ///
    /// # Errors
    ///
    /// Returns a message naming the offending line number. A file with
    /// one bad line does not parse at all: a partially applied ACL would
    /// silently revoke somebody's access.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut grants: HashMap<String, Vec<(Scope, Level)>> = HashMap::new();
        for (index, raw) in text.lines().enumerate() {
            let number = index + 1;
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut columns = line.split_whitespace();
            let (Some(user), Some(target), Some(level), None) = (
                columns.next(),
                columns.next(),
                columns.next(),
                columns.next(),
            ) else {
                return Err(format!(
                    "line {number}: expected three columns, `<user> <repo|*|@node> <read|write>`"
                ));
            };
            let scope = parse_scope(target).map_err(|e| format!("line {number}: {e}"))?;
            let level = parse_level(&scope, level).map_err(|e| format!("line {number}: {e}"))?;
            grants.entry(user.to_string()).or_default().push((scope, level));
        }
        Ok(Self { grants })
    }

    /// Reads and parses the file at `path`.
    ///
    /// # Errors
    ///
    /// Returns a message when the file cannot be read, or when it does
    /// not parse.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text)
    }

    /// Number of grants across all users.
    #[must_use]
    pub fn len(&self) -> usize {
        self.grants.values().map(Vec::len).sum()
    }

    /// Whether the table holds no grants at all, in which case a
    /// configured ACL denies everyone.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether `user` holds at least `level` over `scope`.
    #[must_use]
    pub fn allows(&self, user: &str, scope: &Scope, level: Level) -> bool {
        let Some(held) = self.grants.get(user) else {
            return false;
        };
        held.iter().any(|(granted, at)| *at >= level && covers(granted, scope))
    }

    /// Whether `user` holds at least `level` over repository `repo`,
    /// given in either spelling (`owner/repo` or `owner/repo.git`).
    #[must_use]
    pub fn allows_repo(&self, user: &str, repo: &str, level: Level) -> bool {
        self.allows(user, &Scope::Repo(normalize_repo(repo)), level)
    }

    /// The [`Denial`] for `user` over `scope` at `level`, or `None` when
    /// the request is allowed.
    #[must_use]
    pub fn check(&self, user: &str, scope: &Scope, level: Level) -> Option<Denial> {
        if self.allows(user, scope, level) {
            return None;
        }
        // Withholding read from a repository means withholding the fact
        // that it exists, so an unreadable one is "not found" rather than
        // "forbidden" — a `403` would confirm the name. A readable one
        // has already been disclosed, so the honest answer is `403`.
        //
        // The node is not hidden this way: the caller is authenticated to
        // it and is looking straight at it, so pretending its endpoints
        // do not exist buys nothing and only obscures the fix.
        if matches!(scope, Scope::Node) || self.allows(user, scope, Level::Read) {
            Some(Denial {
                status: 403,
                reason: match scope {
                    Scope::Node => "requires a node-wide grant".to_string(),
                    other => format!("no write grant for {}", describe(other)),
                },
            })
        } else {
            Some(Denial {
                status: 404,
                reason: "no such repository".to_string(),
            })
        }
    }
}

/// Whether a granted scope covers a requested one.
fn covers(granted: &Scope, requested: &Scope) -> bool {
    match (granted, requested) {
        (Scope::Node, Scope::Node) => true,
        (Scope::AllRepos, Scope::Repo(_)) => true,
        (Scope::Repo(held), Scope::Repo(want)) => held == want,
        _ => false,
    }
}

/// Phrase naming a scope in a denial message.
fn describe(scope: &Scope) -> String {
    match scope {
        Scope::Repo(repo) => format!("repository {repo}"),
        Scope::AllRepos => "every repository".to_string(),
        Scope::Node => "this node".to_string(),
    }
}

/// Canonical ACL spelling of a repository name: one trailing `.git`
/// removed, so `owner/repo` and `owner/repo.git` are the same grant.
#[must_use]
pub fn normalize_repo(repo: &str) -> String {
    repo.strip_suffix(".git").unwrap_or(repo).to_string()
}

/// Parses the repository column.
fn parse_scope(target: &str) -> Result<Scope, String> {
    if target == "*" {
        return Ok(Scope::AllRepos);
    }
    if target == "@node" {
        return Ok(Scope::Node);
    }
    if target.starts_with('@') {
        return Err(format!("`{target}` is not a pseudo-repository; the only one is `@node`"));
    }
    let repo = normalize_repo(target);
    let mut segments = repo.split('/');
    let (Some(owner), Some(name), None) = (segments.next(), segments.next(), segments.next())
    else {
        return Err(format!("`{target}` is not a repository name; write `owner/repo`"));
    };
    if owner.is_empty() || name.is_empty() {
        return Err(format!("`{target}` is not a repository name; write `owner/repo`"));
    }
    Ok(Scope::Repo(repo))
}

/// Parses the level column, which is spelled differently on `@node`
/// because the node-wide read grant is a named role rather than a
/// repository permission.
fn parse_level(scope: &Scope, level: &str) -> Result<Level, String> {
    match (scope, level) {
        (_, "write") => Ok(Level::Write),
        (Scope::Node, "auditor") => Ok(Level::Read),
        (Scope::Node, "read") => {
            Err("the node-wide read grant is spelled `auditor`, not `read`".to_string())
        }
        (_, "auditor") => {
            Err("`auditor` is a node-wide role; on a repository write `read`".to_string())
        }
        (_, "read") => Ok(Level::Read),
        (Scope::Node, other) => Err(format!("`{other}` is not a level; write `auditor` or `write`")),
        (_, other) => Err(format!("`{other}` is not a level; write `read` or `write`")),
    }
}

/// Repository and level a git smart-HTTP request needs.
///
/// `None` means the request names no repository, or names an operation
/// outside the smart-HTTP surface. Both are denied when an ACL is
/// configured, rather than falling through to the CGI handler.
#[must_use]
pub fn git_requirement(method: &str, url: &str) -> Option<(String, Level)> {
    let path = url.split('?').next().unwrap_or(url);
    let query = url.split_once('?').map_or("", |(_, q)| q);
    // A traversal segment would let a path authorized against the
    // repository named first reach a different one inside the CGI, since
    // `git http-backend` resolves PATH_INFO itself. Refusing it here does
    // not depend on what that resolution happens to do. Percent-encoded
    // dots need no separate rule: the path is handed to the CGI
    // undecoded, so `%2e%2e` is a literal directory name, not a segment.
    if path.split('/').any(|segment| segment == "..") {
        return None;
    }
    let repo = crate::repo_from_path(url)?;
    // Everything after `/<repo>`; a non-char-boundary index cannot
    // happen for an ASCII repo name, and yields a denial if it somehow
    // does.
    let tail = path.get(1 + repo.len()..).unwrap_or("").trim_start_matches('/');
    let level = match (method, tail) {
        ("POST", "git-receive-pack") => Level::Write,
        ("POST", "git-upload-pack") => Level::Read,
        // The ref advertisement is the first request of both directions,
        // and the service parameter is the only thing distinguishing a
        // clone from a push.
        ("GET", t) if t.starts_with("info/refs") => {
            if query.split('&').any(|p| p == "service=git-receive-pack") {
                Level::Write
            } else {
                Level::Read
            }
        }
        ("GET", "HEAD") => Level::Read,
        ("GET", t) if t.starts_with("info/") || t.starts_with("objects/") => Level::Read,
        _ => return None,
    };
    Some((normalize_repo(&repo), level))
}

/// Repository a workspace name or provenance subject belongs to: its
/// first two `/`-separated segments.
///
/// `None` for anything with fewer than two segments — a submission
/// channel like `git/<user>` has two, so it resolves to a repository
/// name that will simply not be granted, while a bare subject resolves
/// to nothing and falls to [`Scope::Node`].
fn subject_repo(subject: &str) -> Option<String> {
    let mut segments = subject.split('/');
    let (owner, name) = (segments.next()?, segments.next()?);
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(normalize_repo(&format!("{owner}/{name}")))
}

/// Repository named by a view ref key in its `<repo>:<refname>` form.
fn ref_repo(name: &str) -> Option<String> {
    name.split_once(':').map(|(repo, _)| normalize_repo(repo))
}

/// Scopes an op must be authorized against, never empty.
///
/// `review_repo` resolves a review id to the repository its target ref
/// names, so posting a verdict needs write on the repository under
/// review rather than a node-wide grant. An op that resolves to no
/// repository is authorized against [`Scope::Node`]: that is the
/// fail-closed rule, and being an exhaustive match, a new [`OpKind`]
/// variant will not compile until somebody classifies it.
#[must_use]
pub fn op_scopes(kind: &OpKind, review_repo: impl Fn(&str) -> Option<String>) -> Vec<Scope> {
    let repos: Vec<String> = match kind {
        OpKind::SetRef { name, .. } | OpKind::DeleteRef { name, .. } => {
            ref_repo(name).into_iter().collect()
        }
        OpKind::SetWorkspaceHead { workspace, .. } | OpKind::DeleteWorkspace { workspace } => {
            subject_repo(workspace).into_iter().collect()
        }
        // Every change op names the workspace it is bound to, and a
        // workspace's leading two segments are its repository. So the
        // lifecycle authorizes against the same repository a legacy
        // workspace move does, rather than falling to a node-wide grant.
        OpKind::CreateChange { workspace, .. }
        | OpKind::CheckpointChange { workspace, .. }
        | OpKind::ArchiveChange { workspace, .. } => {
            subject_repo(workspace).into_iter().collect()
        }
        OpKind::RequestReview { target_ref, .. } => {
            target_ref.as_deref().and_then(ref_repo).into_iter().collect()
        }
        OpKind::PostVerdict { id, .. }
        | OpKind::ArchiveReview { id, .. }
        | OpKind::SlashApproval { id, .. }
        | OpKind::AssignReviewers { id, .. } => review_repo(id).into_iter().collect(),
        OpKind::RecordProvenance { subject, .. } => subject_repo(subject).into_iter().collect(),
        // Node-scoped by nature: these name keys, or the whole ref
        // state, and never one repository.
        OpKind::BindKey { .. } | OpKind::RevokeKey { .. } | OpKind::RecordRefSnapshot { .. } => {
            Vec::new()
        }
    };
    if repos.is_empty() {
        vec![Scope::Node]
    } else {
        repos.into_iter().map(Scope::Repo).collect()
    }
}

/// Scopes a `/api/submit` body must be authorized against.
///
/// A body that cannot be decoded far enough to name a repository falls
/// to [`Scope::Node`] rather than being waved through, so a caller
/// without a node-wide grant gets a denial and one with it gets the
/// handler's own `400`.
fn submission_scopes(body: &serde_json::Value, review_repo: &impl Fn(&str) -> Option<String>) -> Vec<Scope> {
    let decoded = body
        .get("payload_hex")
        .and_then(serde_json::Value::as_str)
        .and_then(crate::platform::hex_decode)
        .and_then(|bytes| ViewOp::from_payload(&bytes).ok());
    match decoded {
        Some(op) => op_scopes(&op.kind, review_repo),
        None => vec![Scope::Node],
    }
}

/// The ACL decision for one platform-API request, or `None` when it is
/// allowed.
///
/// Unknown `/api/` paths are denied. They reach no handler today, so the
/// only behaviour this changes is for an endpoint added later without a
/// row here — which should fail closed rather than ship unauthorized.
#[must_use]
pub fn api_denial(
    acl: &Acl,
    user: &str,
    method: &str,
    path: &str,
    body: &[u8],
    review_repo: impl Fn(&str) -> Option<String>,
) -> Option<Denial> {
    let json = || serde_json::from_slice::<serde_json::Value>(body).ok();
    let required: Vec<(Scope, Level)> = match (method, path) {
        ("POST", "/api/workspace") => {
            let repo = json()
                .as_ref()
                .and_then(|v| v.get("repo"))
                .and_then(serde_json::Value::as_str)
                .map(normalize_repo);
            match repo {
                Some(repo) => vec![(Scope::Repo(repo), Level::Write)],
                None => vec![(Scope::Node, Level::Write)],
            }
        }
        // Archiving detaches a repository's workspace, so it needs the
        // same repo-scoped write as creating one. Without an arm here the
        // fail-closed default below would answer 404 and the workspace
        // lifecycle would end at create.
        ("POST", "/api/workspace/archive") => {
            let repo = json()
                .as_ref()
                .and_then(|v| v.get("repo"))
                .and_then(serde_json::Value::as_str)
                .map(normalize_repo);
            match repo {
                Some(repo) => vec![(Scope::Repo(repo), Level::Write)],
                None => vec![(Scope::Node, Level::Write)],
            }
        }
        ("POST", "/api/submit") => match json() {
            Some(value) => submission_scopes(&value, &review_repo)
                .into_iter()
                .map(|scope| (scope, Level::Write))
                .collect(),
            None => vec![(Scope::Node, Level::Write)],
        },
        ("POST", "/api/submit-batch") => {
            let ops = json();
            let entries = ops
                .as_ref()
                .and_then(|v| v.get("ops"))
                .and_then(serde_json::Value::as_array);
            match entries {
                Some(entries) => {
                    let mut scopes: Vec<Scope> = Vec::new();
                    for entry in entries {
                        for scope in submission_scopes(entry, &review_repo) {
                            if !scopes.contains(&scope) {
                                scopes.push(scope);
                            }
                        }
                    }
                    if scopes.is_empty() {
                        scopes.push(Scope::Node);
                    }
                    scopes.into_iter().map(|scope| (scope, Level::Write)).collect()
                }
                None => vec![(Scope::Node, Level::Write)],
            }
        }
        // Whole-node reads: gated rather than filtered, because the log
        // is a hash chain and the attestation covers the complete ref
        // state. Narrowing either would destroy what it is for.
        ("GET", p) if p.starts_with("/api/log") => vec![(Scope::Node, Level::Read)],
        ("GET", "/api/ref-agreement") => vec![(Scope::Node, Level::Read)],
        // Phase A leaves the aggregate view readable by any authenticated
        // actor: filtering it, and the page rendered from it, is phase B.
        // Until then a credential can enumerate ref names and oids of
        // repositories it cannot clone.
        ("GET", p) if p.starts_with("/api/view") || p.starts_with("/api/reviews") => Vec::new(),
        // An appeal names an attempt id the caller was handed by their own
        // rejection, not a repository, so there is nothing repo-scoped to
        // check here.
        ("POST", "/api/appeal") => Vec::new(),
        _ => {
            return Some(Denial {
                status: 404,
                reason: "no such endpoint".to_string(),
            })
        }
    };
    required
        .into_iter()
        .find_map(|(scope, level)| acl.check(user, &scope, level))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grammar_accepts_the_documented_file_and_nothing_else() {
        let acl = Acl::parse(
            "# a comment\n\
             alice   owner/project   write\n\
             \n\
             bob     *               read   # trailing comment\n\
             carol   @node           auditor\n",
        )
        .expect("the documented example must parse");
        assert_eq!(acl.len(), 3);

        for (bad, why) in [
            ("alice owner/project", "two columns"),
            ("alice owner/project write extra", "four columns"),
            ("alice owner/project admin", "a level that does not exist"),
            ("alice @nope write", "an invented pseudo-repository"),
            ("alice owner write", "a repo name with no owner"),
            ("alice owner/a/b write", "a three-segment repo name"),
            ("alice @node read", "`read` where the role is spelled `auditor`"),
            ("alice owner/project auditor", "a node role on a repository"),
        ] {
            assert!(Acl::parse(bad).is_err(), "accepted {why}: {bad:?}");
        }
    }

    /// The error has to name the line, or an operator with a 40-line file
    /// is bisecting it by hand while the node refuses to start.
    #[test]
    fn a_parse_error_names_its_line() {
        let error = Acl::parse("alice o/r read\nbob o/r sideways\n").expect_err("line 2 is bad");
        assert!(error.contains("line 2"), "{error}");
    }

    #[test]
    fn both_spellings_of_a_repository_are_one_grant() {
        let acl = Acl::parse("alice owner/project.git write").expect("parses");
        assert!(acl.allows_repo("alice", "owner/project", Level::Write));
        assert!(acl.allows_repo("alice", "owner/project.git", Level::Write));
    }

    #[test]
    fn write_implies_read_and_read_does_not_imply_write() {
        let acl = Acl::parse("alice o/r write\nbob o/r read").expect("parses");
        assert!(acl.allows_repo("alice", "o/r", Level::Read));
        assert!(acl.allows_repo("bob", "o/r", Level::Read));
        assert!(!acl.allows_repo("bob", "o/r", Level::Write));
    }

    /// `*` is a statement about repositories. If it also covered the node
    /// it would hand every repository-granted actor the op log and the
    /// key-management ops, which is the escalation this separation exists
    /// to prevent.
    #[test]
    fn the_wildcard_does_not_reach_the_node() {
        let acl = Acl::parse("bob * write").expect("parses");
        assert!(acl.allows_repo("bob", "anything/at-all", Level::Write));
        assert!(!acl.allows("bob", &Scope::Node, Level::Read));
    }

    #[test]
    fn an_unreadable_repository_is_not_found_and_a_readable_one_is_forbidden() {
        let acl = Acl::parse("bob o/r read").expect("parses");
        let unreadable = acl
            .check("bob", &Scope::Repo("o/other".into()), Level::Read)
            .expect("denied");
        assert_eq!(unreadable.status, 404);
        let unwritable = acl
            .check("bob", &Scope::Repo("o/r".into()), Level::Write)
            .expect("denied");
        assert_eq!(unwritable.status, 403);
        // The node is never hidden: the caller is authenticated to it.
        let no_role = acl.check("bob", &Scope::Node, Level::Read).expect("denied");
        assert_eq!(no_role.status, 403);
    }

    #[test]
    fn the_smart_http_surface_maps_to_the_level_it_actually_needs() {
        let cases = [
            ("GET", "/o/r.git/info/refs?service=git-upload-pack", Some(Level::Read)),
            ("GET", "/o/r.git/info/refs?service=git-receive-pack", Some(Level::Write)),
            ("GET", "/o/r.git/info/refs", Some(Level::Read)),
            ("GET", "/o/r.git/HEAD", Some(Level::Read)),
            ("GET", "/o/r.git/objects/info/packs", Some(Level::Read)),
            ("POST", "/o/r.git/git-upload-pack", Some(Level::Read)),
            ("POST", "/o/r.git/git-receive-pack", Some(Level::Write)),
            // Not a repository path, and not a smart-HTTP operation:
            // both refused rather than handed to the CGI.
            ("GET", "/not-a-repo/file", None),
            ("DELETE", "/o/r.git/git-receive-pack", None),
            // Authorized against `o/r`, resolved by the CGI against
            // `o/other`: refused before it can be either.
            ("GET", "/o/r.git/objects/../../o/other.git/info/refs", None),
        ];
        for (method, url, want) in cases {
            let got = git_requirement(method, url).map(|(_, level)| level);
            assert_eq!(got, want, "{method} {url}");
        }
        assert_eq!(
            git_requirement("POST", "/o/r.git/git-receive-pack").map(|(repo, _)| repo),
            Some("o/r".to_string()),
            "the repository must reach the ACL in its canonical spelling"
        );
    }

    /// A push whose advertisement was read-gated would fail late and
    /// confusingly. This is the arm that gets that right, so it is worth
    /// its own assertion rather than one row in the table above.
    #[test]
    fn a_push_advertisement_needs_write_not_read() {
        let acl = Acl::parse("bob o/r read").expect("parses");
        let (repo, level) =
            git_requirement("GET", "/o/r.git/info/refs?service=git-receive-pack").expect("maps");
        assert!(acl.check("bob", &Scope::Repo(repo), level).is_some());
    }

    #[test]
    fn an_op_naming_no_repository_falls_to_the_node() {
        let none = |_: &str| None;
        let bound = OpKind::SetRef {
            name: "owner/project.git:refs/heads/main".into(),
            commit: choir_oplog::ContentHash::blake3(b"c"),
            prev: None,
        };
        assert_eq!(
            op_scopes(&bound, none),
            vec![Scope::Repo("owner/project".into())]
        );

        let unbound = OpKind::RequestReview {
            id: "r1".into(),
            target: choir_oplog::ContentHash::blake3(b"c"),
            reviewers: Vec::new(),
            target_ref: None,
        };
        assert_eq!(op_scopes(&unbound, none), vec![Scope::Node]);

        // A verdict resolves through the review it settles, so a reviewer
        // needs write on the repository under review — not the node-wide
        // grant that would also hand them key management.
        let verdict = OpKind::PostVerdict {
            id: "r1".into(),
            reviewer: "bob".into(),
            verdict: choir_view::Verdict::Approve,
            note: String::new(),
        };
        let resolves = |_: &str| Some("owner/project".to_string());
        assert_eq!(
            op_scopes(&verdict, resolves),
            vec![Scope::Repo("owner/project".into())]
        );
        assert_eq!(op_scopes(&verdict, none), vec![Scope::Node]);
    }

    #[test]
    fn a_workspace_op_is_scoped_to_the_repository_it_sits_in() {
        let none = |_: &str| None;
        let op = OpKind::SetWorkspaceHead {
            workspace: "owner/project/feature-x".into(),
            commit: choir_oplog::ContentHash::blake3(b"c"),
            prev: None,
        };
        assert_eq!(op_scopes(&op, none), vec![Scope::Repo("owner/project".into())]);
    }
}
