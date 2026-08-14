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
//! The file is not the only source of grants. Credential self-service
//! (D36) renders what it issued in this same grammar, and the node
//! enforces the two as one table ([`Acl::merged`]) — so an issued grant
//! and a hand-written one are the same kind of fact, checked in the same
//! place. What self-service may not issue is [`Scope::Node`]: node-wide
//! authority stays in the file the operator writes.
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

    /// A key identifying everything a filtered response depends on:
    /// the reader and the grants they hold, rendered canonically.
    ///
    /// Two requests with the same key produce the same filtered payload,
    /// which is what makes the browser page cacheable per reader. Editing
    /// the ACL file changes the key, so a hot reload invalidates the
    /// cached page without anything having to notice the reload happened.
    ///
    /// The username is part of the key rather than the grants alone,
    /// because [`filter_response`] also keeps reviews the reader is
    /// assigned to. Two readers holding identical grants can therefore
    /// see different pages, and a key covering only the grants would
    /// serve one of them the other's assignments.
    #[must_use]
    pub fn cache_key(&self, user: &str) -> String {
        let mut held: Vec<String> = self
            .grants
            .get(user)
            .map(|grants| {
                grants
                    .iter()
                    .map(|(scope, level)| {
                        let target = match scope {
                            Scope::Repo(repo) => repo.as_str(),
                            Scope::AllRepos => "*",
                            Scope::Node => "@node",
                        };
                        let level = match level {
                            Level::Read => "r",
                            Level::Write => "w",
                        };
                        format!("{target}={level}")
                    })
                    .collect()
            })
            .unwrap_or_default();
        held.sort();
        held.dedup();
        // A unit separator cannot occur in a username the auth file can
        // express, so the two halves of the key cannot be confused for
        // one another however they are spelled.
        format!("{user}\u{1f}{}", held.join(","))
    }

    /// This table plus `other`'s grants, as one table.
    ///
    /// The union, never an intersection: the operator's file and the
    /// self-service store (D36) each answer for the grants they issued,
    /// and neither can withdraw the other's. Written so that every
    /// enforcement point keeps consulting exactly one [`Acl`] — the two
    /// sources are a detail of where grants come from, not a second
    /// decision anybody has to remember to make.
    #[must_use]
    pub fn merged(&self, other: &Self) -> Self {
        let mut grants = self.grants.clone();
        for (user, held) in &other.grants {
            grants.entry(user.clone()).or_default().extend(held.clone());
        }
        Self { grants }
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
        // A comment authorizes against the repository under review, the
        // same as a verdict on the same review: discussion is part of the
        // review surface, not a node-wide fact.
        OpKind::PostVerdict { id, .. }
        | OpKind::ArchiveReview { id, .. }
        | OpKind::SlashApproval { id, .. }
        | OpKind::PostComment { id, .. }
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
        // Credential self-service (D36). Issuing and revoking are
        // node-wide writes and reading the roster is the node-wide read,
        // because an account is a fact about the node rather than about
        // one repository — and because `@node` is the grant this store is
        // forbidden to issue, so the authority to issue can only have
        // come from the operator's own file.
        ("POST", "/api/accounts/invite" | "/api/accounts/revoke") => {
            vec![(Scope::Node, Level::Write)]
        }
        ("GET", "/api/accounts") => vec![(Scope::Node, Level::Read)],
        // Redemption is reached by a principal that holds no grant at
        // all — an unredeemed invite — so there is nothing here to check.
        // What keeps it from being an open door is that the invite is
        // itself a credential, and that an invite principal is refused
        // every other route before this table is consulted.
        ("POST", "/api/accounts/redeem") => Vec::new(),
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

/// How one top-level section of a read response may be disclosed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Disclosure {
    /// Served to anyone who reaches the endpoint. It names no repository,
    /// and a writer cannot bind a scoped submission without it.
    Public,
    /// Served whole to a reader holding [`Scope::Node`] and omitted from
    /// everyone else. This is the "gate, never filter" rule `/api/log`
    /// and `/api/ref-agreement` follow, applied one level down: the
    /// section counts, attributes or times events across every
    /// repository at once, so there is no honest way to narrow it.
    NodeWide,
    /// Narrowed entry by entry to the reader's repository grants.
    PerRepo,
}

/// Every top-level section `/api/view` and `/api/reviews` serve.
///
/// [`filter_response`] drives off this table and drops any section that
/// has no row here, so a section added to the view without being
/// classified is withheld from readers lacking a node-wide grant rather
/// than served to all of them.
///
/// The omission is the failure mode worth designing against, because it
/// has already happened once: `changes` was added to the view while the
/// filter's section list was maintained by hand, and every authenticated
/// reader received every change record — owner channel, workspace and
/// revisions — for repositories they held no grant on. Failing closed
/// keeps that quiet, so
/// `every_section_the_view_serves_is_classified` in `tests/it/acl.rs`
/// makes it loud, comparing this table against a view a real node
/// served rather than against a sample written from memory.
pub const SECTIONS: [(&str, Disclosure); 15] = [
    ("log", Disclosure::Public),
    ("build", Disclosure::Public),
    ("snapshot", Disclosure::NodeWide),
    ("bindings", Disclosure::NodeWide),
    ("concentration", Disclosure::NodeWide),
    ("view_growth", Disclosure::NodeWide),
    ("newcomer_harm", Disclosure::NodeWide),
    ("new_actor_review_outcomes", Disclosure::NodeWide),
    ("sequencer_lag", Disclosure::NodeWide),
    ("refs", Disclosure::PerRepo),
    ("workspaces", Disclosure::PerRepo),
    ("provenance", Disclosure::PerRepo),
    ("reviews", Disclosure::PerRepo),
    ("changes", Disclosure::PerRepo),
    // `/api/reviews` rather than `/api/view`, narrowed by the same rule.
    ("pending", Disclosure::PerRepo),
];

/// The disclosure rule for `section`, or `None` when it has no row and
/// must therefore be withheld.
#[must_use]
pub fn disclosure(section: &str) -> Option<Disclosure> {
    SECTIONS
        .iter()
        .find(|(name, _)| *name == section)
        .map(|(_, rule)| *rule)
}

/// A read response narrowed to what `user` may see (D29 phase B).
///
/// `/api/view` and `/api/reviews` are the two endpoints that answer with
/// other repositories' contents, so they are the two this rewrites; every
/// other path is already gated by [`api_denial`] and passes through. A
/// body that is not the JSON object this expects is returned untouched
/// rather than emptied, because a filter that silently blanks an
/// unrecognized payload hides the mismatch instead of showing it.
///
/// `log` and `build` survive for every reader: they name the node, its
/// log head and the binary serving them. A writer needs the head to bind
/// a scoped submission, and neither says anything about a repository.
#[must_use]
pub fn filter_response(acl: &Acl, user: &str, path: &str, body: &str) -> String {
    if !(path.starts_with("/api/view") || path.starts_with("/api/reviews")) {
        return body.to_string();
    }
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.to_string();
    };
    let Some(object) = value.as_object_mut() else {
        return body.to_string();
    };
    let node_wide = acl.allows(user, &Scope::Node, Level::Read);
    let readable = |repo: Option<String>| -> bool {
        // A key naming no repository is node-scoped by the same rule that
        // sends a repo-less op to `Scope::Node`: fail closed, and let a
        // node-wide grant see it.
        match repo {
            Some(repo) => acl.allows(user, &Scope::Repo(repo), Level::Read),
            None => node_wide,
        }
    };
    if !node_wide {
        // Unclassified sections leave with the node-wide ones. A section
        // this build does not know about cannot be narrowed, and serving
        // it whole is the disclosure this table exists to prevent.
        object.retain(|section, _| {
            matches!(
                disclosure(section),
                Some(Disclosure::Public | Disclosure::PerRepo)
            )
        });
    }
    retain_keys(object.get_mut("refs"), |key| readable(ref_repo(key)));
    for section in ["workspaces", "provenance"] {
        retain_keys(object.get_mut(section), |key| readable(subject_repo(key)));
    }
    for section in ["reviews", "pending"] {
        retain_entries(object.get_mut(section), |_, review| {
            readable(review_repo_of(review)) || assigned_to(user, review)
        });
    }
    // A change is keyed by its own stable id rather than by a repository,
    // so it narrows on the workspace it is bound to. `workspace_id` and
    // not `active_workspace`: the binding outlives archival, and reading
    // the cleared field would send every archived change to the repo-less
    // branch, where a node-wide reader would still see it but the record
    // would no longer be attributable to the repository it came from.
    retain_entries(object.get_mut("changes"), |_, change| {
        readable(
            change
                .get("workspace_id")
                .and_then(serde_json::Value::as_str)
                .and_then(subject_repo),
        )
    });
    value.to_string()
}

/// Repository a serialized review names through its target ref, if any.
fn review_repo_of(review: &serde_json::Value) -> Option<String> {
    review
        .get("target_ref")
        .and_then(serde_json::Value::as_str)
        .and_then(ref_repo)
}

/// Whether `user` is one of a review's assigned reviewers.
///
/// A reviewer is a channel name, so both the bare name and the operator
/// half of `operator/agent` count: `reviewer_operator` is what the review
/// rules themselves treat as one actor, and matching only the full string
/// would drop a reader's own assignments the moment they run two agents.
/// Without this the ACL would silently break the review fan-out — the
/// reviews a reader most needs are exactly the ones on repositories they
/// were invited into rather than granted.
fn assigned_to(user: &str, review: &serde_json::Value) -> bool {
    review
        .get("reviewers")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|reviewers| {
            reviewers
                .iter()
                .filter_map(serde_json::Value::as_str)
                .any(|name| name == user || choir_view::reviewer_operator(name) == user)
        })
}

/// Drops map entries whose key fails `keep`. A non-map is left alone.
fn retain_keys(section: Option<&mut serde_json::Value>, keep: impl Fn(&str) -> bool) {
    retain_entries(section, |key, _| keep(key));
}

/// Drops map entries whose key and value fail `keep`. A non-map is left
/// alone: every section this is applied to is a JSON object, and one that
/// is not has already stopped meaning what the filter thinks it means.
fn retain_entries(
    section: Option<&mut serde_json::Value>,
    keep: impl Fn(&str, &serde_json::Value) -> bool,
) {
    if let Some(map) = section.and_then(serde_json::Value::as_object_mut) {
        map.retain(|key, value| keep(key, value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sections gated whole on a node-wide grant.
    fn node_wide_sections() -> impl Iterator<Item = &'static str> {
        SECTIONS
            .iter()
            .filter(|(_, rule)| *rule == Disclosure::NodeWide)
            .map(|(name, _)| *name)
    }

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

        // A comment resolves the same way (D38). Scoping it to the node
        // instead would mean a reader with write on one repository could
        // not answer a review there without a grant over everything.
        let comment = OpKind::PostComment {
            id: "r1".into(),
            comment: "c1".into(),
            author: "bob".into(),
            body: "why this base?".into(),
        };
        assert_eq!(
            op_scopes(&comment, resolves),
            vec![Scope::Repo("owner/project".into())]
        );
        assert_eq!(op_scopes(&comment, none), vec![Scope::Node]);
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

    /// A view payload shaped like the one `/api/view` serves: two
    /// repositories in every per-repository section, plus the node-wide
    /// sections and the two that survive for everybody.
    fn sample_view() -> String {
        serde_json::json!({
            "log": { "node": "b3-node", "head": "b3-head", "scope_required": false },
            "build": { "commit": "abc" },
            "refs": {
                "owner/mine.git:refs/heads/main": "git-1111",
                "owner/theirs.git:refs/heads/main": "git-2222",
            },
            "workspaces": {
                "owner/mine/feature-a": "git-3333",
                "owner/theirs/feature-b": "git-4444",
            },
            "provenance": {
                "owner/mine/feature-a": { "plan": "mine" },
                "owner/theirs/feature-b": { "plan": "theirs" },
            },
            "reviews": {
                "r-mine": { "target_ref": "owner/mine.git:refs/heads/main", "reviewers": [] },
                "r-theirs": { "target_ref": "owner/theirs.git:refs/heads/main", "reviewers": [] },
                "r-invited": {
                    "target_ref": "owner/theirs.git:refs/heads/main",
                    "reviewers": ["alice/bot"],
                },
                "r-unbound": { "target_ref": null, "reviewers": [] },
            },
            "changes": {
                "c-mine": {
                    "owner": "owner/agent",
                    "workspace_id": "owner/mine/feature-a",
                    "active_workspace": "owner/mine/feature-a",
                },
                "c-theirs": {
                    "owner": "other/agent",
                    "workspace_id": "owner/theirs/feature-b",
                    "active_workspace": "owner/theirs/feature-b",
                },
                // Archived: the active workspace is gone, but the binding
                // it was created under still names the repository.
                "c-theirs-archived": {
                    "owner": "other/agent",
                    "workspace_id": "owner/theirs/feature-c",
                    "active_workspace": null,
                },
            },
            "snapshot": { "id": "b3-snap" },
            "bindings": { "k1": { "operator": "someone" } },
            "concentration": { "as_of_seq": 9 },
            "view_growth": { "entries": 9 },
            "newcomer_harm": {},
            "new_actor_review_outcomes": {},
            "sequencer_lag": {},
        })
        .to_string()
    }

    /// The whole point of phase B: a reader granted one repository sees
    /// that repository, and cannot enumerate the other one through any
    /// of the four sections that name it.
    #[test]
    fn a_reader_granted_one_repository_sees_only_that_repository() {
        let acl = Acl::parse("alice owner/mine read").expect("parses");
        let filtered = filter_response(&acl, "alice", "/api/view", &sample_view());
        let value: serde_json::Value = serde_json::from_str(&filtered).expect("json");
        let keys = |section: &str| -> Vec<String> {
            value[section]
                .as_object()
                .unwrap_or_else(|| panic!("{section} object"))
                .keys()
                .cloned()
                .collect()
        };
        assert_eq!(keys("refs"), ["owner/mine.git:refs/heads/main"]);
        assert_eq!(keys("workspaces"), ["owner/mine/feature-a"]);
        assert_eq!(keys("provenance"), ["owner/mine/feature-a"]);
        // `r-invited` targets the ungranted repository and still reaches
        // this reader, because they were asked to review it: an invitation
        // discloses the ref it is about, and withholding it would silently
        // break the fan-out. `r-unbound` names no repository at all, so it
        // is node-scoped and fails closed like every repo-less subject.
        assert_eq!(keys("reviews"), ["r-invited", "r-mine"]);
        // A change record names its owner channel, its workspace and its
        // revisions, so leaking one enumerates both the repository and who
        // is working in it. The archived change narrows on the binding it
        // kept, not on the workspace it no longer has.
        assert_eq!(keys("changes"), ["c-mine"]);
    }

    /// Node-wide sections are gated, not narrowed, and the two a writer
    /// needs to keep working are not gated at all.
    #[test]
    fn the_node_sections_need_the_node_grant_and_the_log_head_never_does() {
        let repo_only = Acl::parse("alice owner/mine write").expect("parses");
        let narrowed = filter_response(&repo_only, "alice", "/api/view", &sample_view());
        for section in node_wide_sections() {
            assert!(
                !narrowed.contains(section),
                "{section} reached a reader with no node grant"
            );
        }
        // Without these a writer cannot bind a scoped submission, and
        // neither says anything about a repository.
        assert!(narrowed.contains("b3-head") && narrowed.contains("\"build\""));

        let auditor = Acl::parse("carol @node auditor").expect("parses");
        let whole = filter_response(&auditor, "carol", "/api/view", &sample_view());
        for section in node_wide_sections() {
            assert!(whole.contains(section), "{section} was withheld from an auditor");
        }
        // An auditor holds no repository grant, so the repository
        // sections are empty for them — `@node` is not a way around `*`.
        let value: serde_json::Value = serde_json::from_str(&whole).expect("json");
        assert!(value["refs"].as_object().expect("refs").is_empty());
    }

    /// The pending-review queue is the same data reached by a different
    /// path, so it is filtered by the same rule. Asking for somebody
    /// else's queue must not become a way to read the reviews the view
    /// would have withheld.
    #[test]
    fn the_pending_queue_is_filtered_like_the_view() {
        let acl = Acl::parse("alice owner/mine read").expect("parses");
        let body = serde_json::json!({
            "pending": {
                "r-mine": { "target_ref": "owner/mine.git:refs/heads/main", "reviewers": ["x"] },
                "r-theirs": { "target_ref": "owner/theirs.git:refs/heads/main", "reviewers": ["x"] },
            }
        })
        .to_string();
        let filtered = filter_response(&acl, "alice", "/api/reviews?reviewer=x", &body);
        assert!(filtered.contains("r-mine"));
        assert!(!filtered.contains("r-theirs"), "{filtered}");
    }

    /// Two readers with the same grants share a rendered page only if
    /// their keys match; a grant edit and a different reader must both
    /// change the key, or a stale page outlives the reload that should
    /// have invalidated it.
    #[test]
    fn the_cache_key_moves_with_the_reader_and_with_their_grants() {
        let before = Acl::parse("alice owner/mine read\nbob owner/mine read").expect("parses");
        assert_ne!(before.cache_key("alice"), before.cache_key("bob"));
        let after = Acl::parse("alice owner/mine write\nbob owner/mine read").expect("parses");
        assert_ne!(
            before.cache_key("alice"),
            after.cache_key("alice"),
            "an ACL edit left the cache key unchanged"
        );
        // Order in the file is not identity: the same grants written the
        // other way round are the same key.
        let reordered = Acl::parse("alice owner/b read\nalice owner/a read").expect("parses");
        let forward = Acl::parse("alice owner/a read\nalice owner/b read").expect("parses");
        assert_eq!(reordered.cache_key("alice"), forward.cache_key("alice"));
    }

    /// A body the filter does not recognize is passed through rather than
    /// blanked: an empty response would read as "nothing here" and hide
    /// the mismatch that produced it.
    #[test]
    fn an_unrecognized_body_or_path_is_left_alone() {
        let acl = Acl::parse("alice owner/mine read").expect("parses");
        assert_eq!(filter_response(&acl, "alice", "/api/view", "not json"), "not json");
        assert_eq!(filter_response(&acl, "alice", "/api/view", "[1,2]"), "[1,2]");
        let other = r#"{"refs":{"owner/theirs.git:refs/heads/main":"git-2222"}}"#;
        assert_eq!(filter_response(&acl, "alice", "/api/submit", other), other);
    }
}
