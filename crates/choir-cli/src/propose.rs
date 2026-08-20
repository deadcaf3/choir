//! One-command proposal: the inference that turns a git checkout into a
//! change, a pushed ref and a review request.
//!
//! The five steps a contributor runs by hand today — provision a
//! workspace with an owner-signed base, commit, push, checkpoint,
//! request review — are all already endpoints. What made them five was
//! that each one needed an identifier the previous one produced, and
//! nothing derived those identifiers from what the contributor already
//! had. Everything here is that derivation, kept pure so it can be
//! tested without a node: the remote URL carries the API base and the
//! repository, and the branch name carries the change identity.
//!
//! # Why the branch name is the change identity (D52)
//!
//! A proposal has to survive being amended and rebased, or every push
//! forks a second change for one unit of work. Gerrit solves this with a
//! `Change-Id` trailer in the commit message and jj with a change id
//! kept beside the commit; both need the client to write something
//! durable. The branch name is the one durable label a git contributor
//! already maintains across a rebase, and it is what GitHub keys a pull
//! request on. So it is the external identity fingerprinted by
//! [`crate::runner::Identity::from_external`], and re-proposing from the
//! same branch reaches the same change.
//!
//! The cost is stated rather than hidden: rename the branch and you have
//! a second proposal. `--change` exists for that case.
//!
//! # Examples
//!
//! ```
//! let remote = choir_cli::propose::Remote::parse("https://ci:tok@node.example/agents/demo.git")
//!     .expect("a choir remote");
//! assert_eq!(remote.api, "https://node.example");
//! assert_eq!(remote.repo, "agents/demo");
//! ```

use crate::runner::{safe_segment, split_repo, Failure, Identity};

/// The namespace every `choir propose` binding is derived under.
///
/// Separates proposals from an orchestrator's changes in the same
/// repository, so the two can never converge onto one another's change
/// id by deriving the same fingerprint.
pub const NAMESPACE: &str = "propose";

/// The generation component of a proposal's fingerprint.
///
/// Fixed at one because a proposal's retries are meant to *converge*:
/// pushing again after an amend must reach the change that already
/// exists, which is exactly what holding the generation constant does.
const GENERATION: &str = "1";

/// The API base and repository recovered from a git remote URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    /// Daemon base URL, with any credentials removed.
    pub api: String,
    /// `owner/repo`, without the `.git` suffix.
    pub repo: String,
}

impl Remote {
    /// Recovers the API base and repository from a clone URL.
    ///
    /// Credentials in the authority are dropped rather than carried:
    /// this value is printed in progress output and passed to `curl` on
    /// an argv, and a password on an argv is readable by every process
    /// on the host through `ps`. The token the request actually needs
    /// comes from `--auth-file`.
    ///
    /// # Errors
    ///
    /// Returns a [`Failure`] when the URL carries no scheme, names no
    /// `owner/repo` path, or uses a scheme with no HTTP API base to
    /// derive — `ssh://` being the one that reaches a choir node but
    /// says nothing about where its API listens.
    pub fn parse(url: &str) -> Result<Self, Failure> {
        // The URL is quoted back so the contributor can see what was
        // wrong with it -- but with any userinfo removed first. A remote
        // of the old `https://user:token@host/...` shape is exactly the
        // one most likely to fail this parse, and printing it verbatim
        // would put the token on a terminal, in a scrollback, and in
        // whatever the contributor pastes into a bug report.
        let shown = redact(url);
        let invalid = move |message: &str| {
            Failure::terminal(
                "invalid_config",
                format!(
                    "{message}: {shown}. \
                     Name the node explicitly with --api <url> --repo <owner/repo>."
                ),
            )
        };
        let Some((scheme, rest)) = url.split_once("://") else {
            return Err(invalid("remote is not an http(s) choir URL"));
        };
        if scheme != "http" && scheme != "https" {
            return Err(invalid("remote scheme carries no API base"));
        }
        let (authority, path) = rest
            .split_once('/')
            .ok_or_else(|| invalid("remote names no repository"))?;
        // Everything before the last `@` is userinfo. Splitting on the
        // last one and not the first is what keeps a password containing
        // `@` from leaving its tail in the host.
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        if host.is_empty() {
            return Err(invalid("remote names no host"));
        }
        let repo = path.strip_suffix('/').unwrap_or(path);
        let repo = repo.strip_suffix(".git").unwrap_or(repo);
        // Wrapped rather than propagated: `split_repo`'s own message is
        // about a config field, and the reader here is looking at a git
        // remote. Routing it through `invalid` also means every refusal
        // from this function is redacted by one piece of code.
        split_repo(repo).map_err(|_| invalid("remote path is not owner/repo"))?;
        Ok(Self {
            api: format!("{scheme}://{host}"),
            repo: repo.to_string(),
        })
    }
}

/// Everything one proposal is bound to, derived from the checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    /// The change, workspace and idempotency key, derived together.
    pub identity: Identity,
    /// Branch the proposal asks to land on, as a full refname.
    pub target_ref: String,
}

impl Proposal {
    /// Derives a proposal's identifiers from the checkout's branch and
    /// the branch it wants to land on.
    ///
    /// # Errors
    ///
    /// Returns a [`Failure`] when the repository, branch or target is
    /// not a usable identifier.
    pub fn derive(repo: &str, branch: &str, onto: &str) -> Result<Self, Failure> {
        if branch.is_empty() {
            return Err(Failure::terminal(
                "invalid_request",
                "HEAD is detached, so there is no branch name to identify this change by. \
                 Check out a branch, or name the change with --change.",
            ));
        }
        if !safe_segment(onto) {
            return Err(Failure::terminal(
                "invalid_request",
                "--onto must name a branch, not a full refname or a path",
            ));
        }
        // The branch is the external identity; a sanitized copy is only
        // the readable prefix of the directory name, so two branches
        // that sanitize alike still fingerprint apart.
        let key = workspace_key(branch);
        let identity = Identity::from_external(NAMESPACE, repo, &key, branch, GENERATION)?;
        Ok(Self {
            target_ref: format!("refs/heads/{onto}"),
            identity,
        })
    }

    /// The refname one revision of this proposal is pushed to.
    ///
    /// Named by the commit, under the proposal's own namespace, and so
    /// **append-only**: re-proposing after an amend or a rebase adds a
    /// ref rather than moving one. Two properties follow, and both are
    /// why this is not simply a branch.
    ///
    /// First, no force-push. A branch would refuse an amended commit as
    /// a non-fast-forward, and the repair for that is the force-push
    /// this project refuses everywhere else.
    ///
    /// Second, and the reason worth the odd-looking refname: every
    /// revision the op log records stays fetchable. A checkpoint puts a
    /// revision hash in the log permanently; if the ref that made its
    /// objects reachable were overwritten, the log would go on naming a
    /// revision the repository could no longer produce. Gerrit keeps
    /// each patchset under `refs/changes/NN/NNNN/P` for the same reason.
    ///
    /// The namespace is deliberately outside `refs/heads/`: these are
    /// object anchors, not branches, and a clone should not grow one
    /// local branch per revision of every open proposal. The current
    /// revision of a change is `revision_id` in the view, never the
    /// newest ref.
    #[must_use]
    pub fn revision_ref(&self, commit: &str) -> String {
        format!("refs/proposals/{}/{commit}", self.identity.workspace_name)
    }

    /// The `repo:ref` spelling `RequestReview` records as the
    /// destination, which is what per-ref review policy reads.
    #[must_use]
    pub fn review_target(&self, repo: &str) -> String {
        format!("{repo}:{}", self.target_ref)
    }
}

/// Replaces any userinfo in a URL with `<redacted>`.
///
/// Applied before a URL is quoted into a message, never after: there is
/// no second place that scrubs these, so a message assembled without
/// this one is a message that leaks.
fn redact(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    match authority.rsplit_once('@') {
        None => url.to_string(),
        Some((_, host)) => format!("{scheme}://<redacted>@{host}/{path}"),
    }
}

/// Reduces a branch name to a readable path segment.
///
/// Branch names carry `/`, and a workspace name is one path component.
/// This is deliberately lossy and deliberately not the identity: it
/// names the directory, while the fingerprint over the *original*
/// branch name keeps `feat/x` and `feat-x` apart.
fn workspace_key(branch: &str) -> String {
    let mapped: String = branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = mapped.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    if trimmed.is_empty() {
        "branch".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Renders a change id for a progress line.
///
/// A change id ends in a 64-character fingerprint, which is a record
/// rather than a sentence: at the width of a terminal it pushes the part
/// a reader recognises -- the namespace and the repository -- off the
/// line. The JSON summary still carries the id whole, so nothing anyone
/// has to copy is shortened here.
#[must_use]
pub fn short_change_id(id: &str) -> String {
    match id.rsplit_once(':') {
        Some((head, digest)) if digest.chars().count() > SHORT_FINGERPRINT => {
            let short: String = digest.chars().take(SHORT_FINGERPRINT).collect();
            format!("{head}:{short}\u{2026}")
        }
        _ => id.to_string(),
    }
}

/// Fingerprint characters kept by [`short_change_id`].
const SHORT_FINGERPRINT: usize = 12;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_plain_remote() {
        let remote = Remote::parse("http://127.0.0.1:8080/agents/demo.git").unwrap();
        assert_eq!(remote.api, "http://127.0.0.1:8080");
        assert_eq!(remote.repo, "agents/demo");
    }

    #[test]
    fn drops_credentials_from_the_api_base() {
        // The whole reason this function exists rather than a split on
        // '/': a token in the remote must not reach an argv.
        let remote = Remote::parse("https://ana:s3cr@t@node.example/agents/demo.git").unwrap();
        assert_eq!(remote.api, "https://node.example");
        assert!(!remote.api.contains("s3cr"));
    }

    #[test]
    fn accepts_a_remote_without_the_git_suffix() {
        let remote = Remote::parse("https://node.example/agents/demo").unwrap();
        assert_eq!(remote.repo, "agents/demo");
    }

    #[test]
    fn a_refusal_never_quotes_the_credential_back() {
        // The remote most likely to fail this parse is the one carrying
        // a token, so the refusal is where a leak would happen.
        let failure = Remote::parse("https://ana:s3cr3t@node.example/a/b/c.git").unwrap_err();
        assert!(
            !failure.message.contains("s3cr3t"),
            "the refusal quoted the token: {}",
            failure.message
        );
        assert!(
            failure.message.contains("<redacted>"),
            "{}",
            failure.message
        );
        assert!(
            failure.message.contains("node.example"),
            "{}",
            failure.message
        );
    }

    #[test]
    fn refuses_a_scheme_with_no_api_base() {
        let failure = Remote::parse("ssh://git@node.example/agents/demo.git").unwrap_err();
        assert_eq!(failure.code, "invalid_config");
        assert!(failure.message.contains("--api"), "{}", failure.message);
    }

    #[test]
    fn refuses_a_path_that_is_not_owner_repo() {
        assert!(Remote::parse("https://node.example/demo.git").is_err());
        assert!(Remote::parse("https://node.example/a/b/c.git").is_err());
    }

    #[test]
    fn the_same_branch_derives_the_same_change() {
        let first = Proposal::derive("agents/demo", "fix-parser", "main").unwrap();
        let second = Proposal::derive("agents/demo", "fix-parser", "main").unwrap();
        assert_eq!(first.identity.change_id, second.identity.change_id);
        assert_eq!(first.revision_ref("abc"), second.revision_ref("abc"));
    }

    #[test]
    fn a_different_branch_derives_a_different_change() {
        let first = Proposal::derive("agents/demo", "fix-parser", "main").unwrap();
        let other = Proposal::derive("agents/demo", "fix-lexer", "main").unwrap();
        assert_ne!(first.identity.change_id, other.identity.change_id);
    }

    #[test]
    fn branches_that_sanitize_alike_stay_distinct() {
        // `feat/x` and `feat-x` share a workspace key prefix; the
        // fingerprint is over the branch name, so the changes differ.
        let slashed = Proposal::derive("agents/demo", "feat/x", "main").unwrap();
        let dashed = Proposal::derive("agents/demo", "feat-x", "main").unwrap();
        assert_ne!(slashed.identity.change_id, dashed.identity.change_id);
        assert_ne!(slashed.revision_ref("abc"), dashed.revision_ref("abc"));
    }

    #[test]
    fn the_target_branch_does_not_change_the_identity() {
        // Retargeting a proposal must not fork it into a second change.
        let main = Proposal::derive("agents/demo", "fix-parser", "main").unwrap();
        let release = Proposal::derive("agents/demo", "fix-parser", "release").unwrap();
        assert_eq!(main.identity.change_id, release.identity.change_id);
        assert_eq!(release.target_ref, "refs/heads/release");
    }

    #[test]
    fn a_slashed_branch_becomes_one_path_segment() {
        let proposal = Proposal::derive("agents/demo", "feat/deep/name", "main").unwrap();
        assert!(safe_segment(&proposal.identity.workspace_name));
        assert!(!proposal.identity.workspace_name.contains('/'));
    }

    #[test]
    fn each_revision_gets_its_own_ref_outside_refs_heads() {
        let proposal = Proposal::derive("agents/demo", "fix-parser", "main").unwrap();
        let first = proposal.revision_ref("1111111111111111111111111111111111111111");
        let amended = proposal.revision_ref("2222222222222222222222222222222222222222");
        assert_ne!(first, amended, "an amend would have overwritten a revision");
        assert!(first.starts_with("refs/proposals/"));
        assert!(!first.starts_with("refs/heads/"));
    }

    #[test]
    fn a_detached_head_is_refused_with_the_repair() {
        let failure = Proposal::derive("agents/demo", "", "main").unwrap_err();
        assert!(failure.message.contains("--change"), "{}", failure.message);
    }

    #[test]
    fn review_target_names_the_destination_not_the_proposal() {
        let proposal = Proposal::derive("agents/demo", "fix-parser", "main").unwrap();
        assert_eq!(
            proposal.review_target("agents/demo"),
            "agents/demo:refs/heads/main"
        );
        assert_ne!(
            proposal.review_target("agents/demo"),
            proposal.revision_ref("abc")
        );
    }

    #[test]
    fn a_shortened_change_id_keeps_the_part_a_reader_recognises() {
        let long = "propose:agents/demo:".to_string() + &"a1".repeat(32);
        let short = short_change_id(&long);
        assert!(short.starts_with("propose:agents/demo:a1a1a1a1a1a1"));
        assert!(short.ends_with('\u{2026}'));
        assert!(short.chars().count() < long.chars().count());
    }

    #[test]
    fn an_id_with_no_room_to_shorten_is_returned_whole() {
        // Truncating here would produce an id shorter than the one it
        // stands for while still claiming to elide something.
        for id in ["propose:agents/demo:abc", "no-colons-at-all"] {
            assert_eq!(short_change_id(id), id);
        }
    }
}
