//! The first five minutes: the invite link, and what this machine is.
//!
//! An invite is one link, because a link is what survives being pasted
//! into a chat window. The node mints it in exactly one place and it
//! looks like `https://node.example/join?i=<id>&k=<secret>`. Everything
//! `choir join` needs is inside it — which node, and the credential that
//! redeems one account there — so a person holding the link should never
//! have to take it apart by hand into an API base, an invite file and a
//! key path. [`Link::parse`] is that taking-apart, kept pure so the
//! whole of it is checkable without a node.
//!
//! The second half is [`Role`]: the one-line answer to "what is this
//! machine set up as", which `choir doctor` prints first and a bare
//! `choir` prints instead of the index. It is computed from paths that
//! are passed in rather than read here, so the caller's own resolution
//! is what gets reported and the logic is testable against a temporary
//! directory.
//!
//! # Examples
//!
//! ```
//! let link = choir_cli::join::Link::parse("https://node.example/join?i=abc&k=s3cret")
//!     .expect("an invite link");
//! assert_eq!(link.api, "https://node.example");
//! assert_eq!(link.id, "abc");
//! assert_eq!(link.secret, "s3cret");
//! ```

/// An invite link taken apart into the three things redeeming one needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// The node's API base: scheme and authority, no path, no trailing
    /// slash. The same string every other command takes as `<api>`.
    pub api: String,
    /// The invite's id, which is the user half of the credential that
    /// redeems it.
    pub id: String,
    /// The invite's secret, which is the password half.
    pub secret: String,
}

impl Link {
    /// Whether a string is worth handing to [`Link::parse`].
    ///
    /// Used by the argument matcher to tell `choir join <link>` from
    /// `choir join <api> <invite-file> <key-file>`, whose first argument
    /// is also a URL. Only the path decides, so a mistyped link still
    /// reaches `parse` and gets `parse`'s message rather than being
    /// silently read as the three-argument form with two arguments
    /// missing.
    #[must_use]
    pub fn looks_like(argument: &str) -> bool {
        let Some((scheme, rest)) = argument.split_once("://") else {
            return false;
        };
        (scheme == "http" || scheme == "https") && rest.contains("/join?")
    }

    /// Recovers the node and the invite from the link the operator sent.
    ///
    /// Hand-written rather than pulled from a URL crate: the grammar is
    /// one path and two query parameters, both of which the node mints
    /// itself, and this workspace adds dependencies reluctantly.
    ///
    /// # Errors
    ///
    /// Returns the sentence to print when the string is not an invite
    /// link: no scheme, a scheme with no API base behind it, no `/join`
    /// path, or either query parameter missing or empty. Every one of
    /// them ends with the `next:` line a reader can act on.
    pub fn parse(link: &str) -> Result<Link, String> {
        let bad = |why: &str| {
            format!(
                "that does not look like an invite link ({why}).\n\
                 An invite is one link, like https://node.example/join?i=…&k=…\n\
                 next: paste the whole link, quoted: choir join '<link>'"
            )
        };
        let (scheme, rest) = link.split_once("://").ok_or_else(|| bad("no scheme"))?;
        if scheme != "http" && scheme != "https" {
            return Err(bad("not an http(s) URL"));
        }
        let (authority, path) = rest.split_once('/').ok_or_else(|| bad("no /join path"))?;
        // Everything before the last `@` is userinfo. A link should
        // carry none, and one that does must not leave its tail in the
        // host.
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        if host.is_empty() {
            return Err(bad("no host"));
        }
        let (route, query) = path.split_once('?').ok_or_else(|| bad("no invite in it"))?;
        if route.trim_end_matches('/') != "join" {
            return Err(bad("its path is not /join"));
        }
        // A shell that ate the `&` leaves `?i=<id>` alone and drops the
        // rest, which is the single most likely way this arrives broken.
        // Naming the quoting is more use than naming the missing field.
        let mut id = None;
        let mut secret = None;
        for pair in query.split('&') {
            match pair.split_once('=') {
                Some(("i", value)) if !value.is_empty() => id = Some(value),
                Some(("k", value)) if !value.is_empty() => secret = Some(value),
                _ => {}
            }
        }
        let (Some(id), Some(secret)) = (id, secret) else {
            return Err(format!(
                "that invite link is missing half of itself.\n\
                 A shell eats the `&` in an unquoted URL, which leaves exactly this.\n\
                 next: quote it: choir join '{link}&k=…'"
            ));
        };
        Ok(Link {
            api: format!("{scheme}://{host}"),
            id: id.to_string(),
            secret: secret.to_string(),
        })
    }
}

/// The repositories an invite's grants name, each with the folder
/// `choir join` clones it into, in grant order.
///
/// A grant is the node's `<repo|*> <level> [until=…]` spelling. `*`
/// names no repository, so it clones nothing; the same repository under
/// both of the ACL's spellings (`owner/name` and `owner/name.git`) is one
/// clone, not two into the same folder. The folder is the last path
/// segment, which is where a plain `git clone` of the same URL would put
/// it, and a segment that is not a folder name is skipped rather than
/// handed to git as a path.
#[must_use]
pub fn repositories<'a>(grants: &[&'a str]) -> Vec<(&'a str, &'a str)> {
    let mut out: Vec<(&str, &str)> = Vec::new();
    for grant in grants {
        let Some(target) = grant.split_whitespace().next() else {
            continue;
        };
        let repo = target.strip_suffix(".git").unwrap_or(target);
        let folder = repo.rsplit('/').next().unwrap_or(repo);
        if repo == "*" || matches!(folder, "" | "." | "..") {
            continue;
        }
        if !out.iter().any(|(seen, _)| *seen == repo) {
            out.push((repo, folder));
        }
    }
    out
}

/// What one machine is set up as, in the vocabulary the documentation
/// uses.
///
/// Three personas, and a machine is at most one of them: a reviewer
/// never opens a terminal, so no local state can say "reviewer" and the
/// enum does not pretend otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Nothing has been set up here yet.
    Nothing,
    /// Holds a credential for somebody else's node.
    Contributor,
    /// Runs a node: `choir init` has written a repository root here.
    Operator,
}

impl Role {
    /// Reads the role off the two paths that distinguish it.
    ///
    /// `state` is `~/.choir`. The operator test is the repository root
    /// `choir init` creates, not the credential — an operator's node
    /// issues itself one too, so the credential alone cannot tell the
    /// two apart, and the root is the thing only an operator has.
    #[must_use]
    pub fn of(state: &std::path::Path, auth_file: Option<&str>) -> Role {
        if state.join("repos").is_dir() {
            return Role::Operator;
        }
        match auth_file {
            Some(path) if std::path::Path::new(path).exists() => Role::Contributor,
            _ => Role::Nothing,
        }
    }

    /// The line `choir doctor` prints above its checks.
    #[must_use]
    pub fn line(self) -> &'static str {
        match self {
            Role::Nothing => "nothing configured yet, run `choir join <link>` or `choir init`",
            Role::Contributor => "contributor: a credential for somebody else's node",
            Role::Operator => "operator: this machine runs a node",
        }
    }
}

/// The whole of `choir` for a machine that has nothing set up.
///
/// Six lines, because the index is forty-eight commands long and a
/// newcomer's question is not "what can this do". A reader who wants the
/// index asks for it by name, which is what the last line says.
///
/// The install step a contributor also needs is deliberately not here.
/// Anybody reading this already ran it — the binary printing the line is
/// the evidence — and naming a release host this workspace does not yet
/// have would be inventing one.
#[must_use]
pub fn orientation(style: crate::style::Style) -> String {
    let row = |who: &str, command: &str, what: &str| {
        format!(
            "  {}  {}  {}\n",
            style.dim(&format!("{who:11}")),
            style.cyan(&format!("{command:24}")),
            style.dim(what)
        )
    };
    let mut out = format!(
        "{}\n\n",
        style.bold("choir: one order over one repository, worked on by many agents at once.")
    );
    out.push_str(&row(
        "contributor",
        "choir join '<link>'",
        "redeem the invite you were sent",
    ));
    out.push_str(&row("operator", "choir init", "run a node on this machine"));
    out.push_str(&row(
        "reviewer",
        "(nothing to install)",
        "open the link in a browser",
    ));
    out.push_str(&format!(
        "  {}\n",
        style.dim("docs         choir --help              every command, and its arguments")
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_with_a_port_keeps_it() {
        let link = Link::parse("http://127.0.0.1:8417/join?i=a&k=b").expect("parses");
        assert_eq!(link.api, "http://127.0.0.1:8417");
    }

    #[test]
    fn the_three_argument_form_is_not_mistaken_for_a_link() {
        assert!(!Link::looks_like("https://node.example"));
        assert!(Link::looks_like("https://node.example/join?i=a&k=b"));
    }

    #[test]
    fn a_link_the_shell_truncated_says_so() {
        let error = Link::parse("https://node.example/join?i=a").expect_err("half a link");
        assert!(error.contains("missing half of itself"), "{error}");
        assert!(error.contains("next:"), "{error}");
    }

    #[test]
    fn every_refusal_names_a_next_step() {
        for bad in ["node.example/join?i=a&k=b", "ssh://x/join?i=a&k=b"] {
            let error = Link::parse(bad).expect_err("refused");
            assert!(error.contains("next:"), "{bad}: {error}");
        }
    }

    #[test]
    fn an_invite_clones_each_repository_it_names_once_and_never_the_wildcard() {
        let grants = [
            "agents/demo write",
            // The same repository spelled the ACL's other way is not a
            // second clone into the same folder.
            "agents/demo.git read",
            // `*` names no repository, so there is nothing to clone.
            "* write",
            "other/notes read until=1864000",
            // A last segment that is not a folder name is skipped rather
            // than handed to git as a path.
            "other/.. read",
        ];
        assert_eq!(
            repositories(&grants),
            [("agents/demo", "demo"), ("other/notes", "notes")]
        );
    }

    #[test]
    fn the_orientation_is_at_most_six_lines() {
        let text = orientation(crate::style::Style::plain());
        assert!(
            text.lines().count() <= 6,
            "orientation is {} lines:\n{text}",
            text.lines().count()
        );
    }
}
