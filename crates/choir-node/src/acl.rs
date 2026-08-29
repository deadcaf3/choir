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
//! learns one file format rather than two — and an optional fourth column
//! carrying a deadline (D66):
//!
//! ```text
//! # <user>   <repo|*|@node>   <level>   [until=<unix seconds>]
//! alice      owner/project    own
//! alice      owner/notes      read
//! bob        owner/project    write     until=1788000000
//! carol      @node            auditor
//! ```
//!
//! **A grant with a deadline stops mattering when the deadline passes,**
//! and nothing sweeps: the table is dated on every request, so a lapse
//! takes effect on the next one. It lapses *downward*, not to nothing —
//! `bob` above keeps whatever weaker grant another line gives him, which
//! is what makes a time-locked `write` over a permanent `read` a usable
//! way to lend a privilege rather than an account.
//!
//! This is the mechanism D24's T1 response names ("time-locks + bonds
//! only") and did not have. It is deliberately **not** an answer to T1's
//! *measurement* problem: a grant lives in this file and in the D36
//! store, never in the op log, so a replayer still cannot see one. That
//! is D29's design and [`choir_view::View::validate_submit`] says so —
//! the ACL grant is the single element replay cannot rederive.
//!
//! Four levels, `read` < `propose` < `write` < `own`. `propose` arrived
//! with D60 and is the one that lets a repository take contributions
//! from someone who is not trusted with its branches: it admits a push
//! to `refs/for/<branch>/<user>/<topic>`, where the pusher's own name is
//! what keeps two of them apart, and refuses every other ref. `own`
//! arrived with D42, which
//! is the first repository-scoped administrative action to exist: on a
//! protected ref an owner's assent authorizes the landing, and `write`
//! alone does not. Before that there was deliberately no `admin`, because
//! a level with no operation behind it only invites a meaningless grant.
//!
//! **`own` is granted in this file only.** Self-service (D36) renders its
//! issued grants in the same grammar, but the landing gate reads the
//! operator's file directly rather than the merged table, so ownership
//! cannot be self-issued.
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
//!
//! The operator's guide to every authorization question this module
//! and its neighbours answer:
//!
#![doc = include_str!("../../../docs/operating/authorization.md")]

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

/// Grant strength. [`Level::Read`] is implied by [`Level::Write`], which
/// is implied by [`Level::Own`].
///
/// The implication is the derived [`Ord`], which follows declaration
/// order, and [`Effective::allows`] compares with `>=`. A new level must
/// therefore be declared in strength order or every existing check
/// silently changes meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Clone and fetch a repository; read the node-scoped endpoints.
    /// Spelled `read` on a repository and `auditor` on `@node`.
    Read,
    /// Everything [`Level::Read`] allows, plus opening a proposal: a
    /// push to `refs/for/<branch>/<user>/<topic>` (D53) and no other
    /// ref. Spelled `propose` (D60).
    ///
    /// The pusher's own name is a required segment, and that is what
    /// keeps two holders of this level apart. Several people hold
    /// `propose` at once, by construction -- it is the grant given to
    /// contributors a repository does not trust -- so without it whoever
    /// pushed second would take over or delete the first one's proposal,
    /// and the log would record the takeover as an ordinary update by an
    /// authorized pusher. A `write` holder is not held to the rule,
    /// because a `write` holder can already reach every ref anyway.
    ///
    /// This is the grant for a contributor the operator does not trust
    /// with the repository's branches, which until it existed was not
    /// expressible: opening a review needed `write`, and `write` also
    /// reaches every unprotected ref. Taking a contribution from a
    /// stranger meant handing them the repository.
    ///
    /// **It is enforced in two places because it has to be.** The
    /// smart-HTTP boundary admits the push at this level, and cannot do
    /// better: git sends the ref list only after the server agrees to
    /// receive the pack, so no refname exists when [`git_requirement`]
    /// runs. The refname first exists when the `pre-receive` hook
    /// reports it, and that is where a proposal-only grant is held to
    /// proposals. Nothing is applied in between -- git applies no ref
    /// until the hook exits zero.
    Propose,
    /// Everything [`Level::Propose`] allows, plus pushing any other ref,
    /// provisioning a workspace, and submitting ops.
    Write,
    /// Everything [`Level::Write`] allows, plus authorizing a landing on
    /// a protected ref of this repository (D42). Spelled `own`.
    ///
    /// This is the repository-scoped administrative action the module
    /// documentation used to say did not exist. It is not a *stronger
    /// push*: on a protected ref an owner's assent is what the gate asks
    /// for, and `write` alone no longer answers it.
    Own,
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

/// One grant: what it covers, how strong it is, and when it stops (D66).
///
/// `until` is unix seconds and `None` means forever, which is every
/// grant written before D66 and every one written since without the
/// fourth column. It is absolute rather than a duration because a
/// duration has to be measured from something, and a file that is read
/// again on every reload has no issue time to measure from.
#[derive(Debug, Clone)]
struct Grant {
    scope: Scope,
    level: Level,
    until: Option<u64>,
}

impl Grant {
    /// Whether this grant is still in force at `now`, in unix seconds.
    ///
    /// Strict, so the grant is already dead in the second it names rather
    /// than in the one after. That is the comparison an invite's expiry
    /// makes in [`crate::accounts`], and the two are the same kind of
    /// statement about the same timeline; a deadline that meant one
    /// second more here than there would be a bug nobody could see.
    fn live_at(&self, now: u64) -> bool {
        self.until.is_none_or(|until| now < until)
    }
}

/// A parsed ACL file: which users hold which grants, and until when.
///
/// Empty means nobody holds anything, which under a configured ACL denies
/// every request. That is the intended failure mode, and the reason a
/// malformed file is never partially applied.
///
/// **This type answers no authorization question.** It is what the file
/// says; [`Acl::at`] turns it into the [`Effective`] table that holds at
/// one instant, and that is the only type with `allows` on it. The split
/// is the whole D66 mechanism: a grant with a deadline is only safe if
/// forgetting the deadline is impossible, and here forgetting it does not
/// compile.
#[derive(Debug, Default, Clone)]
pub struct Acl {
    grants: HashMap<String, Vec<Grant>>,
}

impl Acl {
    /// Parses the ACL grammar described in the module documentation.
    ///
    /// # Errors
    ///
    /// Returns a message naming the offending line number. A file with
    /// one bad line does not parse at all: a partially applied ACL would
    /// silently revoke somebody's access.
    ///
    /// A deadline already in the past is **not** an error. It parses, and
    /// then never matches: refusing the file would turn one stale line
    /// into a node-wide lockout, which is a worse failure than the one it
    /// would be reporting. [`Acl::expired`] is how the operator sees it.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut grants: HashMap<String, Vec<Grant>> = HashMap::new();
        for (index, raw) in text.lines().enumerate() {
            let number = index + 1;
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut columns = line.split_whitespace();
            let (Some(user), Some(target), Some(level), deadline, None) = (
                columns.next(),
                columns.next(),
                columns.next(),
                columns.next(),
                columns.next(),
            ) else {
                return Err(format!(
                    "line {number}: expected three columns, `<user> <repo|*|@node> <read|write>`, \
                     and optionally a fourth, `until=<unix seconds>`"
                ));
            };
            let scope = parse_scope(target).map_err(|e| format!("line {number}: {e}"))?;
            let level = parse_level(&scope, level).map_err(|e| format!("line {number}: {e}"))?;
            let until = deadline
                .map(parse_deadline)
                .transpose()
                .map_err(|e| format!("line {number}: {e}"))?;
            grants.entry(user.to_string()).or_default().push(Grant {
                scope,
                level,
                until,
            });
        }
        Ok(Self { grants })
    }

    /// Whether `user` is granted anything at [`Scope::Node`] here, at any
    /// level and whatever its deadline says.
    ///
    /// Deliberately **not** an [`Effective`] question. D36 forbids
    /// self-service from issuing node-wide authority at all, and a
    /// deadline must never be the thing that enforces that: a grant
    /// dated into the past would answer "no" today and "yes" to anyone
    /// who reads the same table with a different clock. The rule is
    /// about what may be written, so it is asked of what is written.
    #[must_use]
    pub fn grants_node(&self, user: &str) -> bool {
        self.grants
            .get(user)
            .is_some_and(|held| held.iter().any(|grant| matches!(grant.scope, Scope::Node)))
    }

    /// The grants that hold at `now`, in unix seconds — the only table
    /// that answers an authorization question.
    ///
    /// Evaluated per request rather than cached across one, so a caller
    /// holding this decides every question at a single instant. A grant
    /// that lapses mid-request therefore lapses at the next request, not
    /// between two checks of the same one.
    #[must_use]
    pub fn at(&self, now: u64) -> Effective {
        let mut grants: HashMap<String, Vec<(Scope, Level)>> = HashMap::new();
        for (user, held) in &self.grants {
            let live: Vec<(Scope, Level)> = held
                .iter()
                .filter(|grant| grant.live_at(now))
                .map(|grant| (grant.scope.clone(), grant.level))
                .collect();
            if !live.is_empty() {
                grants.insert(user.clone(), live);
            }
        }
        Effective { grants }
    }

    /// How many grants have a deadline that has already passed at `now`.
    ///
    /// Reported at startup and on reload so a line that is dead on
    /// arrival — a typo in the deadline, or a file that outlived what it
    /// was granting — is visible without an operator diffing behaviour
    /// against intent.
    #[must_use]
    pub fn expired(&self, now: u64) -> usize {
        self.grants
            .values()
            .flatten()
            .filter(|grant| !grant.live_at(now))
            .count()
    }

    /// Reads and parses the file at `path`.
    ///
    /// # Errors
    ///
    /// Returns a message when the file cannot be read, or when it does
    /// not parse.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
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
}

/// The grants that hold right now: an [`Acl`] with every lapsed deadline
/// already dropped (D66).
///
/// Produced only by [`Acl::at`], which is what makes the deadline
/// impossible to skip — there is no way to reach `allows` holding a
/// table nobody has dated. Every field of every method below behaves
/// exactly as it did before D66 for a grant with no deadline, which is
/// still most of them.
#[derive(Debug, Default, Clone)]
pub struct Effective {
    grants: HashMap<String, Vec<(Scope, Level)>>,
}

impl Effective {
    /// Whether `user` holds at least `level` over `scope`.
    #[must_use]
    pub fn allows(&self, user: &str, scope: &Scope, level: Level) -> bool {
        let Some(held) = self.grants.get(user) else {
            return false;
        };
        held.iter()
            .any(|(granted, at)| *at >= level && covers(granted, scope))
    }

    /// Whether `user` holds at least `level` over repository `repo`,
    /// given in either spelling (`owner/repo` or `owner/repo.git`).
    #[must_use]
    pub fn allows_repo(&self, user: &str, repo: &str, level: Level) -> bool {
        self.allows(user, &Scope::Repo(normalize_repo(repo)), level)
    }

    /// Whether anybody at all holds [`Level::Own`] over `repo` (D42).
    ///
    /// This is the switch between the two landing rules, not an
    /// authorization check: a repository with no owner keeps the
    /// approval-weight gate, and one with an owner asks for owner assent
    /// instead. It is deliberately a question about the repository rather
    /// than about a user, because the gate has to choose which rule
    /// applies before it knows whether the actor satisfies it.
    #[must_use]
    pub fn has_owner(&self, repo: &str) -> bool {
        let scope = Scope::Repo(normalize_repo(repo));
        self.grants.values().any(|held| {
            held.iter()
                .any(|(granted, at)| *at >= Level::Own && covers(granted, &scope))
        })
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
                            Level::Propose => "p",
                            Level::Write => "w",
                            Level::Own => "o",
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
        return Err(format!(
            "`{target}` is not a pseudo-repository; the only one is `@node`"
        ));
    }
    let repo = normalize_repo(target);
    let mut segments = repo.split('/');
    let (Some(owner), Some(name), None) = (segments.next(), segments.next(), segments.next())
    else {
        return Err(format!(
            "`{target}` is not a repository name; write `owner/repo`"
        ));
    };
    if owner.is_empty() || name.is_empty() {
        return Err(format!(
            "`{target}` is not a repository name; write `owner/repo`"
        ));
    }
    Ok(Scope::Repo(repo))
}

/// Parses the optional fourth column, `until=<unix seconds>`.
///
/// A `key=value` shape rather than a bare number so the column says what
/// it means in the file itself, and so a fifth thing to say about a grant
/// does not have to be positional. An unknown key is an error rather than
/// something ignored: a grant is the wrong place to be generous about
/// what a line might have meant.
///
/// # Errors
///
/// Returns a message when the key is not `until`, or when the value is
/// not a unix-seconds number.
fn parse_deadline(column: &str) -> Result<u64, String> {
    let Some(value) = column.strip_prefix("until=") else {
        let key = column.split('=').next().unwrap_or(column);
        return Err(format!(
            "`{key}` is not a grant option; the only fourth column is `until=<unix seconds>`"
        ));
    };
    value.parse().map_err(|_| {
        format!("`{value}` is not a unix-seconds deadline; `until=` takes a whole number")
    })
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
        // `propose` names a right over one repository's review surface.
        // `@node` is the log and the attestation, which hold no reviews,
        // so the spelling is refused there rather than quietly granted.
        (Scope::Node, "propose") => {
            Err("`propose` is a repository grant; `@node` takes `auditor` or `write`".to_string())
        }
        (_, "propose") => Ok(Level::Propose),
        // `own` names an owner *of a repository*. `@node` is the log and
        // the attestation, which no repository owns, so the spelling is
        // refused there rather than quietly granted over everything.
        (Scope::Node, "own") => {
            Err("`own` is a repository grant; `@node` takes `auditor` or `write`".to_string())
        }
        (_, "own") => Ok(Level::Own),
        (Scope::Node, other) => Err(format!(
            "`{other}` is not a level; write `auditor` or `write`"
        )),
        (_, other) => Err(format!(
            "`{other}` is not a level; write `read`, `propose`, `write` or `own`"
        )),
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
    let tail = path
        .get(1 + repo.len()..)
        .unwrap_or("")
        .trim_start_matches('/');
    let level = match (method, tail) {
        // `propose`, not `write`: no refname exists yet (see
        // [`Level::Propose`]). A pusher who reaches here holding only
        // `propose` has their refs checked when the hook reports them,
        // and git applies none of them before that.
        ("POST", "git-receive-pack") => Level::Propose,
        ("POST", "git-upload-pack") => Level::Read,
        // The ref advertisement is the first request of both directions,
        // and the service parameter is the only thing distinguishing a
        // clone from a push.
        ("GET", t) if t.starts_with("info/refs") => {
            if query.split('&').any(|p| p == "service=git-receive-pack") {
                Level::Propose
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
pub(crate) fn ref_repo(name: &str) -> Option<String> {
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
        // A landing authorizes against the repository whose ref it
        // moves, exactly as the bare ref move does. It needs no scope of
        // its own: the extra authority a `Submit` carries is the landing
        // gate's, and that gate is not this grant.
        OpKind::SetRef { name, .. }
        | OpKind::DeleteRef { name, .. }
        | OpKind::Submit { name, .. } => ref_repo(name).into_iter().collect(),
        OpKind::SetWorkspaceHead { workspace, .. } | OpKind::DeleteWorkspace { workspace } => {
            subject_repo(workspace).into_iter().collect()
        }
        // Every change op names the workspace it is bound to, and a
        // workspace's leading two segments are its repository. So the
        // lifecycle authorizes against the same repository a legacy
        // workspace move does, rather than falling to a node-wide grant.
        OpKind::CreateChange { workspace, .. }
        | OpKind::CheckpointChange { workspace, .. }
        | OpKind::ArchiveChange { workspace, .. } => subject_repo(workspace).into_iter().collect(),
        // A check reports on a commit, and a commit belongs to no
        // repository, so the destination ref is the only thing that can
        // scope it — the same reasoning, and the same fail-closed
        // fallback, as the review request above.
        OpKind::RequestReview { target_ref, .. } | OpKind::RecordCheck { target_ref, .. } => {
            target_ref
                .as_deref()
                .and_then(ref_repo)
                .into_iter()
                .collect()
        }
        // A comment authorizes against the repository under review, the
        // same as a verdict on the same review: discussion is part of the
        // review surface, not a node-wide fact.
        OpKind::PostVerdict { id, .. }
        | OpKind::ArchiveReview { id, .. }
        | OpKind::SlashApproval { id, .. }
        | OpKind::PostComment { id, .. }
        | OpKind::ViewedReview { id, .. }
        | OpKind::AssignReviewers { id, .. } => review_repo(id).into_iter().collect(),
        OpKind::RecordProvenance { subject, .. } => subject_repo(subject).into_iter().collect(),
        // Node-scoped by nature: these name keys, operators, or the
        // whole ref state, and never one repository. A vouch is about
        // who somebody is, which is not a fact about any repository even
        // when the only place the reader met them was one.
        OpKind::BindKey { .. }
        | OpKind::RevokeKey { .. }
        | OpKind::Vouch { .. }
        | OpKind::WithdrawVouch { .. }
        | OpKind::RecordRefSnapshot { .. }
        | OpKind::CountersignSnapshot { .. } => Vec::new(),
    };
    if repos.is_empty() {
        vec![Scope::Node]
    } else {
        repos.into_iter().map(Scope::Repo).collect()
    }
}

/// The grant strength an op needs over the scopes [`op_scopes`] names.
///
/// Write for everything that moves a ref or changes a review's shape,
/// and read for the ops that only report what their own signer thinks:
/// a verdict, a comment, a viewing receipt (D55), and a vouch or its
/// withdrawal (D65).
///
/// Those are exactly the ops admission binds to the signing channel — a
/// claimed attribution other than the channel is `reviewer_mismatch` —
/// and the fold refuses a verdict from anybody the review does not list.
/// Read is therefore the whole authority they need, and requiring write
/// would mean handing push rights to every reviewer drawn onto a
/// repository, which is the opposite of what asking for a review is for.
///
/// A vouch joins them for the same reason and one more. It is node-
/// scoped, so `write` here would mean node-wide write: the web of trust
/// would be authorable only by the handful of identities who can already
/// move any ref on the node, which is not a web. What stops it being
/// free is not this level but [`View::is_bound_operator`] — both ends of
/// an edge need a binding, and only the node authors those.
///
/// [`View::is_bound_operator`]: choir_view::View::is_bound_operator
///
/// Exhaustive on purpose, like [`op_scopes`]: a new [`OpKind`] variant
/// will not compile until somebody says which side of this line it
/// falls on, and the safe answer is write.
#[must_use]
pub fn op_level(kind: &OpKind) -> Level {
    match kind {
        OpKind::PostVerdict { .. }
        | OpKind::PostComment { .. }
        | OpKind::ViewedReview { .. }
        | OpKind::Vouch { .. }
        | OpKind::WithdrawVouch { .. }
        | OpKind::CountersignSnapshot { .. } => Level::Read,
        OpKind::SetRef { .. }
        | OpKind::DeleteRef { .. }
        | OpKind::Submit { .. }
        | OpKind::SetWorkspaceHead { .. }
        | OpKind::DeleteWorkspace { .. }
        | OpKind::CreateChange { .. }
        | OpKind::CheckpointChange { .. }
        | OpKind::ArchiveChange { .. }
        | OpKind::RequestReview { .. }
        | OpKind::RecordCheck { .. }
        | OpKind::ArchiveReview { .. }
        | OpKind::SlashApproval { .. }
        | OpKind::AssignReviewers { .. }
        | OpKind::RecordProvenance { .. }
        | OpKind::BindKey { .. }
        | OpKind::RevokeKey { .. }
        | OpKind::RecordRefSnapshot { .. } => Level::Write,
    }
}

/// Scopes a `/api/submit` body must be authorized against, each with the
/// level that op needs over it.
///
/// A body that cannot be decoded far enough to name a repository falls
/// to [`Scope::Node`] at [`Level::Write`] rather than being waved
/// through, so a caller without a node-wide grant gets a denial and one
/// with it gets the handler's own `400`.
fn submission_scopes(
    body: &serde_json::Value,
    review_repo: &impl Fn(&str) -> Option<String>,
) -> Vec<(Scope, Level)> {
    let decoded = body
        .get("payload_hex")
        .and_then(serde_json::Value::as_str)
        .and_then(crate::platform::hex_decode)
        .and_then(|bytes| ViewOp::from_payload(&bytes).ok());
    match decoded {
        Some(op) => {
            let level = op_level(&op.kind);
            op_scopes(&op.kind, review_repo)
                .into_iter()
                .map(|scope| (scope, level))
                .collect()
        }
        None => vec![(Scope::Node, Level::Write)],
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
    acl: &Effective,
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
        // D68. A round moves a branch on other people's behalf and
        // spends CI per open proposal, so it sits with whoever owns the
        // repository. A `write` holder can already move the branch, but
        // only by pushing their own work to it; asking the queue to
        // land everybody else's is a different thing to be trusted with.
        ("POST", "/api/queue/run") => {
            let repo = json()
                .as_ref()
                .and_then(|v| v.get("repo"))
                .and_then(serde_json::Value::as_str)
                .map(normalize_repo);
            match repo {
                Some(repo) => vec![(Scope::Repo(repo), Level::Own)],
                None => vec![(Scope::Node, Level::Own)],
            }
        }
        ("POST", "/api/submit") => match json() {
            Some(value) => submission_scopes(&value, &review_repo),
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
                    let mut scopes: Vec<(Scope, Level)> = Vec::new();
                    for entry in entries {
                        for required in submission_scopes(entry, &review_repo) {
                            if !scopes.contains(&required) {
                                scopes.push(required);
                            }
                        }
                    }
                    if scopes.is_empty() {
                        scopes.push((Scope::Node, Level::Write));
                    }
                    scopes
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
        // D72. Answering a request is issuing an invite or refusing to,
        // so it sits at exactly the authority issuing one does. Asking is
        // not here at all: `POST /api/access` is pre-auth and never
        // reaches this table.
        ("POST", "/api/accounts/request/grant" | "/api/accounts/request/decline") => {
            vec![(Scope::Node, Level::Write)]
        }
        ("GET", "/api/accounts") => vec![(Scope::Node, Level::Read)],
        // Nothing required, because this one narrows itself: the handler
        // filters the list to the repositories the caller can read, so a
        // grant requirement here would be a second, coarser answer to
        // the same question. A `@node read` requirement in particular
        // would be exactly wrong — it would hide the listing from every
        // reader who holds one repository, which is who it is for.
        ("GET", "/api/repos") => Vec::new(),
        // Enrolling and removing a passkey act on the caller's own
        // account and no one else's (D39): the handler never reads a
        // user from the body, so there is no scope here to check that
        // would not simply be "you are authenticated". A grant
        // requirement would be worse than none — `@node write` would
        // mean only operators could enrol, which is the opposite of the
        // point, and a repo scope would tie a fact about a person to a
        // repository they may not have.
        ("POST", "/api/accounts/passkey" | "/api/accounts/passkey/remove") => Vec::new(),
        // Preparing an op for a browser to sign grants nothing (D39):
        // the response is bytes the caller could have assembled
        // themselves, and what makes an op admissible is the signature
        // over them plus this same table applied at `/api/submit`. A
        // grant requirement here would gate a serializer, and would have
        // to be kept in agreement with the one that gates the write --
        // two places to answer one question, which is how they drift.
        ("POST", "/api/prepare") => Vec::new(),
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
pub const SECTIONS: [(&str, Disclosure); 29] = [
    ("log", Disclosure::Public),
    ("build", Disclosure::Public),
    // [`crate::bound`]'s marks. Public because of *when* they are
    // computed, not because a row count is harmless: bounding runs after
    // this filter, so each count describes the reader's own narrowed
    // slice. Were the order ever reversed, these would be the worst kind
    // of NodeWide — a measurement of the node handed to someone granted
    // one corner of it — and the row would be a lie rather than a leak.
    ("paging", Disclosure::Public),
    ("refs_omitted", Disclosure::Public),
    ("workspaces_omitted", Disclosure::Public),
    ("provenance_omitted", Disclosure::Public),
    ("reviews_omitted", Disclosure::Public),
    ("changes_omitted", Disclosure::Public),
    ("bindings_omitted", Disclosure::Public),
    ("vouches_omitted", Disclosure::Public),
    ("witnessed_omitted", Disclosure::Public),
    ("pending_omitted", Disclosure::Public),
    ("checks_omitted", Disclosure::Public),
    ("snapshot", Disclosure::NodeWide),
    ("bindings", Disclosure::NodeWide),
    // Node-wide for the same reason `bindings` is, and it needs saying
    // because a vouch reads like public reputation: the graph is a map
    // of who the node's operators are and who stands behind whom, which
    // is exactly the document a reader granted one repository was not
    // given. A profile derives from the narrowed view, so such a reader
    // is told there are no vouches to see rather than shown somebody
    // else's.
    ("vouches", Disclosure::NodeWide),
    ("witnessed", Disclosure::NodeWide),
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
    // Narrowed on the destination ref a report named, which is the only
    // repository a commit id can be attributed to. A check reported
    // without one stays node-wide, which is the fail-closed direction.
    ("checks", Disclosure::PerRepo),
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
pub fn filter_response(acl: &Effective, user: &str, path: &str, body: &str) -> String {
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
    retain_entries(object.get_mut("checks"), |_, check| {
        readable(
            check
                .get("target_ref")
                .and_then(serde_json::Value::as_str)
                .and_then(ref_repo),
        )
    });
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

    /// A fixed instant for every test that says nothing about deadlines,
    /// so those tests read exactly as they did before D66.
    const NOW: u64 = 1_700_000_000;

    /// Parses and dates in one step (D66). Tests that are about the
    /// grammar failing still call [`Acl::parse`] directly, because a file
    /// that does not parse never reaches a clock.
    fn parse(text: &str) -> Effective {
        Acl::parse(text).expect("parses").at(NOW)
    }

    /// The sections gated whole on a node-wide grant.
    fn node_wide_sections() -> impl Iterator<Item = &'static str> {
        SECTIONS
            .iter()
            .filter(|(_, rule)| *rule == Disclosure::NodeWide)
            .map(|(name, _)| *name)
    }

    /// D60. The level is only useful if it sits *between* read and
    /// write, and the enum's own note says a variant declared out of
    /// strength order silently changes every existing `>=` check. So the
    /// ordering is asserted directly rather than inferred from behaviour.
    #[test]
    fn propose_sits_between_read_and_write() {
        assert!(Level::Read < Level::Propose);
        assert!(Level::Propose < Level::Write);
        assert!(Level::Write < Level::Own);

        let acl = parse("carol  owner/p  propose\n");
        assert!(
            acl.allows_repo("carol", "owner/p", Level::Read),
            "propose must imply read"
        );
        assert!(acl.allows_repo("carol", "owner/p", Level::Propose));
        assert!(!acl.allows_repo("carol", "owner/p", Level::Write));
        assert!(!acl.allows_repo("carol", "owner/p", Level::Own));
    }

    /// `@node` is the log and the attestation, which hold no reviews, so
    /// the spelling is refused there rather than quietly granted over
    /// everything -- the same rule `own` follows.
    #[test]
    fn propose_is_not_a_node_wide_spelling() {
        let error = Acl::parse("carol  @node  propose\n").expect_err("`@node propose` must refuse");
        assert!(
            error.contains("repository grant"),
            "unhelpful refusal: {error}"
        );
    }

    /// The parity claim D66 rests on: three columns mean today exactly
    /// what they meant before the fourth existed, at any instant.
    #[test]
    fn a_grant_with_no_deadline_is_the_grant_it_always_was() {
        let text = "alice owner/p write\nbob @node auditor\n";
        for now in [0, NOW, u64::MAX] {
            let acl = Acl::parse(text).expect("parses").at(now);
            assert!(
                acl.allows_repo("alice", "owner/p", Level::Write),
                "a deadline-free grant lapsed at {now}"
            );
            assert!(acl.allows("bob", &Scope::Node, Level::Read));
        }
        assert_eq!(Acl::parse(text).expect("parses").expired(u64::MAX), 0);
    }

    /// The boundary, stated on both sides: `until=N` is in force through
    /// `N - 1` and gone at `N`, which is the comparison an invite makes.
    #[test]
    fn a_deadline_ends_the_grant_in_the_second_it_names() {
        let table = Acl::parse("alice owner/p write until=1000\n").expect("parses");
        assert!(table.at(999).allows_repo("alice", "owner/p", Level::Write));
        assert!(!table.at(1000).allows_repo("alice", "owner/p", Level::Write));
        assert!(!table.at(1001).allows_repo("alice", "owner/p", Level::Write));
        assert_eq!(table.expired(999), 0);
        assert_eq!(table.expired(1000), 1);
    }

    /// A lapse is a downgrade, not a lockout. This is what makes a
    /// time-locked privilege lendable: the account survives it.
    #[test]
    fn a_lapsed_write_falls_back_to_a_permanent_read() {
        let table =
            Acl::parse("alice owner/p read\nalice owner/p write until=1000\n").expect("parses");
        let after = table.at(2000);
        assert!(
            after.allows_repo("alice", "owner/p", Level::Read),
            "the permanent grant went with the expiring one"
        );
        assert!(!after.allows_repo("alice", "owner/p", Level::Write));
        // And the denial is the readable-repository one, not the
        // does-not-exist one: withholding a level is not withholding the
        // repository's existence from somebody who can still read it.
        let denial = after
            .check("alice", &Scope::Repo("owner/p".into()), Level::Write)
            .expect("denied");
        assert_eq!(denial.status, 403);
    }

    /// An expiring `own` returns the repository to the approval-weight
    /// rule (D42) rather than leaving it owned by nobody-in-particular.
    #[test]
    fn an_expired_owner_is_not_an_owner() {
        let table = Acl::parse("alice owner/p own until=1000\n").expect("parses");
        assert!(table.at(999).has_owner("owner/p"));
        assert!(!table.at(1000).has_owner("owner/p"));
    }

    /// The page cache is keyed on what the reader may see, so a lapse has
    /// to move the key or a cached page outlives the grant that filtered
    /// it.
    #[test]
    fn a_lapse_moves_the_cache_key() {
        let table =
            Acl::parse("alice owner/p read\nalice owner/q read until=1000\n").expect("parses");
        assert_ne!(
            table.at(999).cache_key("alice"),
            table.at(1000).cache_key("alice"),
            "a reader who lost a repository kept the key that cached it"
        );
        assert_eq!(
            table.at(1000).cache_key("alice"),
            Acl::parse("alice owner/p read\n")
                .expect("parses")
                .at(1000)
                .cache_key("alice"),
            "a lapsed grant left a trace in the key of a reader who no longer holds it"
        );
    }

    /// Both sources keep their deadlines through the merge (D36).
    #[test]
    fn merging_keeps_each_side_of_the_deadline() {
        let file = Acl::parse("alice owner/p read\n").expect("parses");
        let store = Acl::parse("alice owner/q write until=1000\n").expect("parses");
        let merged = file.merged(&store);
        assert!(merged.at(999).allows_repo("alice", "owner/q", Level::Write));
        assert!(!merged
            .at(1000)
            .allows_repo("alice", "owner/q", Level::Write));
        assert!(merged.at(1000).allows_repo("alice", "owner/p", Level::Read));
    }

    /// D36's rule is about what may be *written*, so an expired node
    /// grant is still a node grant and still refused.
    #[test]
    fn an_expired_node_grant_is_still_a_node_grant() {
        let table = Acl::parse("mallory @node auditor until=1000\n").expect("parses");
        assert!(
            table.grants_node("mallory"),
            "a deadline in the past made node scope look un-granted"
        );
        assert!(!table.at(2000).allows("mallory", &Scope::Node, Level::Read));
    }

    /// The fourth column is checked, not guessed at: a key that is not
    /// `until`, a value that is not a number, and a fifth column are all
    /// refusals naming the line.
    #[test]
    fn the_fourth_column_is_only_a_deadline() {
        for (bad, why) in [
            ("alice o/r read expires=1000\n", "a key that is not `until`"),
            ("alice o/r read 1000\n", "a bare number with no key"),
            ("alice o/r read until=soon\n", "a value that is not seconds"),
            ("alice o/r read until=-1\n", "a negative deadline"),
            ("alice o/r read until=1000 extra\n", "a fifth column"),
        ] {
            let error = Acl::parse(bad).expect_err(why);
            assert!(error.starts_with("line 1: "), "{why}: {error}");
        }
        // A deadline already in the past is not a parse error: one stale
        // line must not be a node-wide lockout.
        let stale = Acl::parse("alice o/r read until=1\n").expect("a past deadline parses");
        assert_eq!(stale.expired(NOW), 1);
        assert!(!stale.at(NOW).allows_repo("alice", "o/r", Level::Read));
    }

    #[test]
    fn the_grammar_accepts_the_documented_file_and_nothing_else() {
        // Counted on the file's own table rather than a dated one: this
        // test is about the grammar, and `len` is a fact about what was
        // written, not about what holds now.
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
            (
                "alice @node read",
                "`read` where the role is spelled `auditor`",
            ),
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
        let acl = parse("alice owner/project.git write");
        assert!(acl.allows_repo("alice", "owner/project", Level::Write));
        assert!(acl.allows_repo("alice", "owner/project.git", Level::Write));
    }

    #[test]
    fn write_implies_read_and_read_does_not_imply_write() {
        let acl = parse("alice o/r write\nbob o/r read");
        assert!(acl.allows_repo("alice", "o/r", Level::Read));
        assert!(acl.allows_repo("bob", "o/r", Level::Read));
        assert!(!acl.allows_repo("bob", "o/r", Level::Write));
    }

    /// The implication is the derived [`Ord`], which follows declaration
    /// order, so this pins the order rather than the spelling. Declaring
    /// `Own` before `Write` would compile, pass every existing test, and
    /// quietly turn every `allows(.., Write)` check in the daemon into a
    /// check for something weaker.
    #[test]
    fn own_implies_write_and_write_does_not_imply_own() {
        let acl = parse("alice o/r own\nbob o/r write");
        assert!(acl.allows_repo("alice", "o/r", Level::Read));
        assert!(acl.allows_repo("alice", "o/r", Level::Write));
        assert!(acl.allows_repo("alice", "o/r", Level::Own));
        assert!(acl.allows_repo("bob", "o/r", Level::Write));
        assert!(
            !acl.allows_repo("bob", "o/r", Level::Own),
            "write must not confer ownership: the landing gate asks for \
             Own precisely because push permission is not assent"
        );
        assert!(Level::Read < Level::Write && Level::Write < Level::Own);
    }

    /// `own` names an owner of a repository. `@node` is the op log and the
    /// attestation, which no repository owns; granting it there would be a
    /// node-wide authority nobody asked for.
    #[test]
    fn own_is_a_repository_grant_and_the_node_refuses_it() {
        assert!(Acl::parse("alice o/r own").is_ok());
        assert!(
            Acl::parse("alice * own").is_ok(),
            "owning every repo is sayable"
        );
        let error = Acl::parse("alice @node own").expect_err("@node cannot be owned");
        assert!(error.contains("repository grant"), "{error}");
    }

    /// `*` is a statement about repositories. If it also covered the node
    /// it would hand every repository-granted actor the op log and the
    /// key-management ops, which is the escalation this separation exists
    /// to prevent.
    #[test]
    fn the_wildcard_does_not_reach_the_node() {
        let acl = parse("bob * write");
        assert!(acl.allows_repo("bob", "anything/at-all", Level::Write));
        assert!(!acl.allows("bob", &Scope::Node, Level::Read));
    }

    #[test]
    fn an_unreadable_repository_is_not_found_and_a_readable_one_is_forbidden() {
        let acl = parse("bob o/r read");
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
            (
                "GET",
                "/o/r.git/info/refs?service=git-upload-pack",
                Some(Level::Read),
            ),
            (
                "GET",
                "/o/r.git/info/refs?service=git-receive-pack",
                Some(Level::Propose),
            ),
            ("GET", "/o/r.git/info/refs", Some(Level::Read)),
            ("GET", "/o/r.git/HEAD", Some(Level::Read)),
            ("GET", "/o/r.git/objects/info/packs", Some(Level::Read)),
            ("POST", "/o/r.git/git-upload-pack", Some(Level::Read)),
            // `propose`, not `write`, since D60: this boundary has no
            // refname to judge, so it admits the push and the hook
            // decides which refs the grant actually reaches.
            ("POST", "/o/r.git/git-receive-pack", Some(Level::Propose)),
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
    ///
    /// Both directions since D60, because the level moved down and a
    /// one-sided assertion would no longer notice it moving further: a
    /// `read` grant is still refused, and a `propose` grant is admitted.
    #[test]
    fn a_push_advertisement_needs_propose_not_read() {
        let (repo, level) =
            git_requirement("GET", "/o/r.git/info/refs?service=git-receive-pack").expect("maps");

        let reader = parse("bob o/r read");
        assert!(reader
            .check("bob", &Scope::Repo(repo.clone()), level)
            .is_some());

        let proposer = parse("carol o/r propose");
        assert!(
            proposer.check("carol", &Scope::Repo(repo), level).is_none(),
            "a propose grant must reach the advertisement, or the level is unusable"
        );
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
        assert_eq!(
            op_scopes(&op, none),
            vec![Scope::Repo("owner/project".into())]
        );
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
            "vouches": { "someone": { "another": { "at": 4, "note": "" } } },
            "witnessed": { "someone": { "snapshot": { "codec": 30, "digest": [] }, "at": 5 } },
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
        let acl = parse("alice owner/mine read");
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
        let repo_only = parse("alice owner/mine write");
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

        let auditor = parse("carol @node auditor");
        let whole = filter_response(&auditor, "carol", "/api/view", &sample_view());
        for section in node_wide_sections() {
            assert!(
                whole.contains(section),
                "{section} was withheld from an auditor"
            );
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
        let acl = parse("alice owner/mine read");
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
        let before = parse("alice owner/mine read\nbob owner/mine read");
        assert_ne!(before.cache_key("alice"), before.cache_key("bob"));
        let after = parse("alice owner/mine write\nbob owner/mine read");
        assert_ne!(
            before.cache_key("alice"),
            after.cache_key("alice"),
            "an ACL edit left the cache key unchanged"
        );
        // Order in the file is not identity: the same grants written the
        // other way round are the same key.
        let reordered = parse("alice owner/b read\nalice owner/a read");
        let forward = parse("alice owner/a read\nalice owner/b read");
        assert_eq!(reordered.cache_key("alice"), forward.cache_key("alice"));
    }

    /// A body the filter does not recognize is passed through rather than
    /// blanked: an empty response would read as "nothing here" and hide
    /// the mismatch that produced it.
    #[test]
    fn an_unrecognized_body_or_path_is_left_alone() {
        let acl = parse("alice owner/mine read");
        assert_eq!(
            filter_response(&acl, "alice", "/api/view", "not json"),
            "not json"
        );
        assert_eq!(
            filter_response(&acl, "alice", "/api/view", "[1,2]"),
            "[1,2]"
        );
        let other = r#"{"refs":{"owner/theirs.git:refs/heads/main":"git-2222"}}"#;
        assert_eq!(filter_response(&acl, "alice", "/api/submit", other), other);
    }
}
