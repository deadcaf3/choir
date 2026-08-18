//! Repository browsing (D30): the code itself, on the D28 surface.
//!
//! The D28 page shows what the node *knows* — refs, reviews, the
//! attestation, health. It shows nothing of what the repositories
//! *contain*, which is the thing a human opens a forge for. This module
//! adds read-only pages under `/r/`:
//!
//! ```text
//! /r/                                     repositories you may read
//! /r/<owner>/<repo>/tree/<rev>[/<path>]   a directory listing
//! /r/<owner>/<repo>/blob/<rev>/<path>     one file
//! /r/<owner>/<repo>/commits/<rev>         recent history
//! /r/<owner>/<repo>/commit/<oid>          one commit, with its diff
//! /r/<owner>/<repo>/reviews               reviews landing here (D34)
//! /r/<owner>/<repo>/review/<id>           one review, with its diff
//! ```
//!
//! The review pages (D34) are the pull-request-shaped surface: reviews
//! already carried assignment, verdicts, approval weight, per-operator
//! caps and retroactive slashing, and none of it was visible next to the
//! change it judged. They add no state — every field comes from the same
//! `review_json` the API serves — so there is nothing here that can
//! disagree with `/api/view`. The discussion D34 deferred is now on the
//! page too, under its own decision (D38): a comment is a signed
//! operation folded into the review, so this surface renders the thread
//! and accepts nothing. There is no form here and no POST route; the
//! only way a comment reaches the log is a signature over its payload.
//!
//! # Why this cannot collide with a git route
//!
//! Every smart-HTTP path carries a `.git` segment — [`repo_from_path`]
//! finds the repository by looking for exactly that, and refuses a path
//! without one. So a URL with no `.git` segment is not reachable as git,
//! whatever it is prefixed with, and an owner really named `r` keeps
//! working: their clone URL is `/r/project.git/info/refs`, which still
//! has the segment and is still routed to git. The routing order asserts
//! this rather than assuming it, and a test pins that exact case.
//!
//! [`repo_from_path`]: crate::repo_from_path
//!
//! # Why the oid, and not the view sequence
//!
//! D28 keys its page on `View::next_seq`, because everything on that
//! page is a projection of the op log. File content is not: it lives in
//! the bare repository, and the log's sequence is a statement about
//! *admitted ops*, not about what `git cat-file` will return. Keying a
//! file page on the sequence would make it stale in exactly the case
//! that matters — a ref that moved without an op, which is the
//! disagreement `/api/ref-agreement` exists to report. So every content
//! page resolves its ref to a commit oid first and revalidates on that.
//! The oid is the honest cache identity: same oid, same bytes, forever.
//!
//! # Robustness
//!
//! Everything from the URL is untrusted, and reaches a subprocess. Two
//! rules, both enforced in [`route`] before any git runs:
//!
//! 1. **No argument can look like an option.** A ref or path beginning
//!    with `-` is refused outright rather than escaped, and every git
//!    invocation puts user input after the subcommand's own separator
//!    where one exists.
//! 2. **No segment can leave the repository.** `..` and empty segments
//!    are refused in paths and refs alike, and the repository name goes
//!    through the same [`safe_segment`] rule provisioning uses.
//!
//! Rendered output goes through the D28 escaper without exception —
//! file *contents* most of all, since a repository can hold anything and
//! a file called `<img src=x onerror=…>` is a legal file.
//!
//! [`safe_segment`]: crate::provision::safe_segment

use std::path::{Path, PathBuf};

use crate::ui::esc;

/// Largest blob rendered inline. Past this the page describes the file
/// and links nothing: a browser is not a delivery mechanism for a
/// 40 MB asset, and the clone path already exists for that.
const MAX_BLOB_BYTES: u64 = 512 * 1024;

/// Largest diff rendered inline, in lines. A merge of a vendored tree
/// can run to hundreds of thousands and would pin a CPU formatting
/// something nobody reads to the end.
const MAX_DIFF_LINES: usize = 2_000;

/// How many commits one history page shows.
const COMMIT_PAGE: usize = 100;

/// How many search hits one page shows.
///
/// A cap rather than paging because a search that returns 200 rows is a
/// search that needs narrowing, and telling the reader that is more use
/// than handing them page 4 of 60.
const SEARCH_HITS: usize = 200;

/// How many reviews the landing page's pane shows before it defers to
/// the full list.
///
/// A cap on work as much as on height: every row costs a `git rev-list`
/// to learn how far behind it is, and a reader who opened a repository
/// came for the files. Ninety open reviews must not push the README
/// below the fold, nor spawn ninety subprocesses to get it there.
const PANE_ROWS: usize = 6;

/// What a search looks through.
///
/// Three scopes rather than one box that guesses, because they cost
/// different amounts and answer different questions: a name search reads
/// one tree listing, a content search reads every blob in the tree, and a
/// message search reads history and no tree at all. A reader who wants
/// one should not pay for the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// File and directory names in the tree at this revision.
    Files,
    /// File contents at this revision.
    Code,
    /// Commit messages reachable from this revision.
    Commits,
}

impl Scope {
    /// The `in=` value that names this scope in a URL.
    fn slug(self) -> &'static str {
        match self {
            Scope::Files => "files",
            Scope::Code => "code",
            Scope::Commits => "commits",
        }
    }

    /// What one match in this scope is called, for a sentence that
    /// counts them. `label` is the tab's plural heading and reads wrong
    /// after a number: "1 file names".
    fn singular(self) -> &'static str {
        match self {
            Scope::Files => "file name",
            Scope::Code => "line of code",
            Scope::Commits => "commit message",
        }
    }

    /// The plural of [`Scope::singular`], which is not always the tab's
    /// label: a tab says "file contents", a sentence says "3 lines of
    /// code".
    fn plural_noun(self) -> &'static str {
        match self {
            Scope::Files => "file names",
            Scope::Code => "lines of code",
            Scope::Commits => "commit messages",
        }
    }

    /// What the tab for this scope is labelled.
    fn label(self) -> &'static str {
        match self {
            Scope::Files => "file names",
            Scope::Code => "file contents",
            Scope::Commits => "commit messages",
        }
    }

    /// Parses an `in=` value. Anything unrecognised is a name search:
    /// the cheapest scope is the safe answer to a URL we cannot read.
    fn parse(raw: Option<&str>) -> Scope {
        match raw {
            Some("code") => Scope::Code,
            Some("commits") => Scope::Commits,
            _ => Scope::Files,
        }
    }

    /// Every scope, in the order the tabs present them.
    const ALL: [Scope; 3] = [Scope::Files, Scope::Code, Scope::Commits];
}

/// One browse request, after parsing and validation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Page {
    /// The repository index: everything the reader may see, narrowed to
    /// the rows matching `q` when it is not empty.
    Index { q: String },
    /// A directory listing at `rev`, rooted at `path` (empty for the
    /// repository root).
    Tree {
        repo: String,
        rev: String,
        path: String,
    },
    /// One file's contents at `rev`.
    Blob {
        repo: String,
        rev: String,
        path: String,
    },
    /// Recent history reachable from `rev`.
    Commits { repo: String, rev: String },
    /// One commit and its diff.
    Commit { repo: String, oid: String },
    /// Every review proposing to land on this repository.
    Reviews { repo: String },
    /// One review: what it proposes, who was asked, what they said, and
    /// the diff between the proposal and where it would land.
    Review { repo: String, id: String },
    /// A search of one repository at `rev`, in one [`Scope`].
    Search {
        repo: String,
        rev: String,
        q: String,
        scope: Scope,
    },
}

impl Page {
    /// The repository this page reads, in the D29 canonical spelling, or
    /// `None` for the index — which is not about one repository and is
    /// filtered per reader instead of gated.
    pub(crate) fn repo(&self) -> Option<&str> {
        match self {
            Page::Index { .. } => None,
            Page::Tree { repo, .. }
            | Page::Blob { repo, .. }
            | Page::Commits { repo, .. }
            | Page::Commit { repo, .. }
            | Page::Reviews { repo }
            | Page::Review { repo, .. }
            | Page::Search { repo, .. } => Some(repo),
        }
    }
}

/// Parses a browse URL, or `None` when it is not one — including when it
/// is *shaped* like one but carries something that must never reach git.
///
/// Refusing here rather than escaping later is deliberate: an argument
/// that cannot be constructed cannot be injected, and the alternative is
/// a quoting rule that has to be right at every call site forever.
pub(crate) fn route(url: &str) -> Option<Page> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    // A `.git` segment means git owns this URL, whatever it looks like
    // from here. Checked first so the answer never depends on which
    // branch of the router happened to run first.
    if crate::repo_from_path(path).is_some() {
        return None;
    }
    // The front door. A reader who types the bare host name is looking
    // for the repositories, not for the node's own telemetry, which now
    // lives at `/status`. `/r/` keeps working because links to it are
    // already in the wild.
    if path.is_empty() || path == "/" || path == "/index.html" {
        return Some(Page::Index {
            q: param(url, "q").unwrap_or_default(),
        });
    }
    let rest = path.strip_prefix("/r")?;
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    if rest.is_empty() {
        return Some(Page::Index {
            q: param(url, "q").unwrap_or_default(),
        });
    }

    let mut segments = rest.split('/');
    let owner = decode(segments.next()?)?;
    let name = decode(segments.next()?)?;
    if !crate::provision::safe_segment(&owner) || !crate::provision::safe_segment(&name) {
        return None;
    }
    let repo = format!("{owner}/{name}");

    // Reviews are keyed by an operator-chosen id rather than by a
    // revision, so they take the segment where a rev would otherwise go.
    if let Some(rest) = rest.strip_prefix(&format!("{owner}/{name}/")) {
        if rest == "reviews" {
            return Some(Page::Reviews { repo });
        }
        if let Some(id) = rest.strip_prefix("review/") {
            let id = decode(id)?;
            // The same rule repository and workspace names follow: an id
            // reaches no subprocess, but it does reach a URL and a page,
            // and one grammar for names is easier to keep right than two.
            if !crate::provision::safe_segment(&id) {
                return None;
            }
            return Some(Page::Review { repo, id });
        }
    }

    let kind = match segments.next() {
        // `/r/owner/repo` is the repository's front door: its default
        // branch at the root, so a reader who followed a link from the
        // index does not have to know a ref name to see anything.
        None | Some("") => {
            return Some(Page::Tree {
                repo,
                rev: "HEAD".into(),
                path: String::new(),
            })
        }
        Some(kind) => kind,
    };
    let rev_or_oid = decode(segments.next()?)?;
    let rest: Vec<String> = segments.map(decode).collect::<Option<_>>()?;
    let path = rest.join("/");

    match kind {
        "tree" => {
            let rev = safe_rev(&rev_or_oid)?;
            safe_path(&path)?;
            Some(Page::Tree { repo, rev, path })
        }
        "blob" => {
            let rev = safe_rev(&rev_or_oid)?;
            // A blob with no path is a directory request wearing the
            // wrong verb; refuse it rather than guess which was meant.
            if path.is_empty() {
                return None;
            }
            safe_path(&path)?;
            Some(Page::Blob { repo, rev, path })
        }
        "commits" if path.is_empty() => Some(Page::Commits {
            repo,
            rev: safe_rev(&rev_or_oid)?,
        }),
        "commit" if path.is_empty() => Some(Page::Commit {
            repo,
            oid: safe_oid(&rev_or_oid)?,
        }),
        // The terms live in the query string, not the path: a search is
        // not a location, and a term containing `/` would otherwise have
        // to be smuggled through a path segment that refuses it.
        "search" if path.is_empty() => Some(Page::Search {
            repo,
            rev: safe_rev(&rev_or_oid)?,
            q: param(url, "q").unwrap_or_default(),
            scope: Scope::parse(param(url, "in").as_deref()),
        }),
        _ => None,
    }
}

/// One query-string parameter, percent-decoded, or `None` when absent.
///
/// Hand-rolled for the same reason [`decode`] is: the grammar is two
/// rules. `+` is a space here and *only* here — it is a literal plus in a
/// path, and conflating the two is how a search for `a+b` becomes a
/// search for `a b`.
///
/// A parameter that decodes to nothing usable is `None` rather than an
/// error, because a malformed query is a reader who edited a URL, and the
/// honest answer is the unfiltered page rather than a refusal.
fn param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    let query = query.split('#').next().unwrap_or(query);
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        if name != key {
            return None;
        }
        // `decode` refuses control characters and invalid UTF-8, which is
        // exactly the rule wanted here: these terms reach a subprocess
        // argument and a rendered page.
        let value = decode(&value.replace('+', "%20"))?;
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

/// Percent-decodes one path segment, refusing anything that decodes to a
/// control character or invalid UTF-8.
///
/// Hand-rolled rather than pulled in: the whole grammar is two hex
/// digits, and this crate adds dependencies reluctantly.
fn decode(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = segment.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let text = String::from_utf8(out).ok()?;
    // A decoded `/` would invent a segment boundary the parser already
    // passed, and a control byte has no business in a ref or a path.
    if text.contains('/') || text.chars().any(char::is_control) {
        return None;
    }
    Some(text)
}

/// Validates a ref name, returning it unchanged.
///
/// Deliberately narrower than git's own rules: this is what may become a
/// subprocess argument, so the question is not "would git accept it" but
/// "can this be anything other than a ref".
fn safe_rev(rev: &str) -> Option<String> {
    if rev.is_empty() || rev.starts_with('-') || rev.len() > 255 {
        return None;
    }
    if rev.contains("..") || rev.ends_with('.') || rev.ends_with(".lock") {
        return None;
    }
    let ok = rev
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'));
    // `^`, `~`, `:` and `@{…}` are legal revision syntax and deliberately
    // excluded: a browse URL names a ref or an oid, and revision
    // arithmetic in a URL is a way to ask for something the route was
    // never authorized against.
    ok.then(|| rev.to_string())
}

/// Percent-encodes a repository-relative path for use in a URL — the
/// inverse of [`decode`], and the reason a link to an awkward filename
/// resolves to the file it names.
///
/// A path is the one thing on this surface with no grammar: a repository
/// may hold `notes?.txt` or `plan #2.md`, and those are ordinary files.
/// HTML-escaping alone is not enough for them, because the characters
/// that break a URL are not the characters that break markup — a browser
/// asked for `…/blob/main/notes?.txt` sends the path `…/blob/main/notes`
/// with `.txt` as a query string, and the page had just rendered a dead
/// link to a file it could see. `/` is deliberately left literal: it is
/// the segment separator both this and [`route`] agree on, and encoding
/// it would invent a segment boundary the parser refuses.
///
/// Unreserved set per RFC 3986, which is the conservative choice: over-
/// encoding costs a few bytes and always decodes back, while guessing at
/// what a browser leaves alone does not.
fn url_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(char::from(byte));
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Validates a repository-relative path.
fn safe_path(path: &str) -> Option<()> {
    if path.is_empty() {
        return Some(());
    }
    if path.len() > 4096 || path.starts_with('-') {
        return None;
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == ".." || segment == "." {
            return None;
        }
    }
    Some(())
}

/// Validates a full-length object id.
fn safe_oid(oid: &str) -> Option<String> {
    let ok = matches!(oid.len(), 40 | 64) && oid.chars().all(|c| c.is_ascii_hexdigit());
    ok.then(|| oid.to_ascii_lowercase())
}

/// Runs git against one bare repository, capturing stdout as bytes.
///
/// Bytes rather than a `String` because a blob is arbitrary content:
/// deciding it is text is the caller's job, after looking at it.
fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = std::process::Command::new("git")
        // `core.quotePath` defaults to *on*, which makes every path
        // outside ASCII come back as a C-quoted octal string:
        // `héllo.txt` arrives as `"h\303\251llo.txt"`, quotes included.
        // That is not a display blemish — the listing builds its links
        // out of the name it was given, so a repository containing one
        // non-ASCII filename gets a page of dead links to files whose
        // real names it never showed. Measured against a real repository
        // rather than assumed: `ls-tree`, the diffstat and the
        // `diff --git` headers all quote, and `cat-file` accepts the raw
        // UTF-8 path, so turning it off is what makes the two agree.
        .arg("-c")
        .arg("core.quotePath=false")
        .arg("--git-dir")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        // A browse request must never be answered with somebody's
        // pager, editor or credential prompt.
        .env("GIT_PAGER", "cat")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        // git names the `--git-dir` it was handed in several of its
        // failures — `fatal: not a git repository: '/srv/choir/repos/…'`
        // — and this string is shown to a reader on the `404`. That is
        // the node's filesystem layout, its account name and its install
        // root, disclosed to anyone who mistypes an address. The reason
        // for the message is the *distinction* it carries ("no such
        // revision" and "not a tree object" send a reader to different
        // fixes), and the path carries none of that, so it goes.
        let path = dir.to_string_lossy();
        Err(String::from_utf8_lossy(&out.stderr)
            .trim()
            .replace(path.as_ref(), "<repository>"))
    }
}

/// Same, decoded as UTF-8 with replacement — for git's own output,
/// which is metadata rather than file content.
fn git_text(dir: &Path, args: &[&str]) -> Result<String, String> {
    git(dir, args).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// The commit oid a page is really about, which is its cache identity.
///
/// `HEAD` gets one retry that no other revision gets. `Node::create_repo`
/// runs `git init --bare`, which points `HEAD` at whatever the *host's*
/// `init.defaultBranch` says, while a push lands on the branch the client
/// chose. When those disagree — a host defaulting to `master` and a client
/// pushing `main` is the common case — `HEAD` is a symref to a branch that
/// was never created, and `/r/owner/repo`, the front door of a repository
/// full of commits, renders as empty. So a dangling `HEAD` resolves
/// through a real branch instead of reporting nothing.
fn resolve(dir: &Path, rev: &str) -> Result<String, String> {
    match rev_parse(dir, rev) {
        Ok(oid) => Ok(oid),
        Err(why) if rev == "HEAD" => match default_branch(dir) {
            Some(branch) => rev_parse(dir, &branch),
            None => Err(why),
        },
        Err(why) => Err(why),
    }
}

/// One `git rev-parse`, with an empty answer treated as a failure.
fn rev_parse(dir: &Path, rev: &str) -> Result<String, String> {
    let out = git_text(
        dir,
        &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
    )?;
    let oid = out.trim().to_string();
    if oid.is_empty() {
        return Err("no such revision".to_string());
    }
    Ok(oid)
}

/// The branch a repository opens at when `HEAD` does not resolve.
///
/// `main` and `master` are preferred in that order because they are what
/// a dangling `HEAD` is usually pointing *at*; anything else falls back to
/// the first branch by name, so a repository with only `trunk` still opens.
/// `None` means the repository genuinely has no branches, which is the one
/// case where "empty" is the true answer.
fn default_branch(dir: &Path) -> Option<String> {
    let text = git_text(
        dir,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
    )
    .ok()?;
    let names: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    for preferred in ["main", "master"] {
        if names.contains(&preferred) {
            return Some(preferred.to_string());
        }
    }
    names.first().map(|name| (*name).to_string())
}

/// A rendered page: its HTML, and the identity a client revalidates on.
pub(crate) struct Rendered {
    /// HTTP status. Anything other than 200 is a page that says why.
    pub(crate) status: u16,
    /// The `ETag` value, or `None` for a page not worth revalidating.
    pub(crate) etag: Option<String>,
    /// The page itself.
    pub(crate) html: String,
}

/// Renders one browse page from the repositories under `root`.
///
/// `readable` decides which repositories the index lists; content pages
/// are gated by the caller before this runs, because a denial has to be
/// a `404` from the router rather than a rendered apology.
/// Narrows a page to the one repository this node presents as its site.
///
/// A node serving `git.example.com` for one project has no use for an
/// index: the front door *is* the repository, and a list naming every
/// other repository the host happens to hold is both noise and a
/// disclosure. So `/` becomes that repository's tree, and any page about
/// a different repository is `None` — which the caller answers with the
/// same refusal a reader without a grant gets, because "this node does
/// not present that" and "you may not read that" must stay
/// indistinguishable from outside.
///
/// This is presentation only. It narrows no grant and widens none: git
/// access is the ACL's answer, and a repository hidden from the browser
/// is still clonable by whoever could clone it before.
pub(crate) fn scope(page: Page, site: &str) -> Option<Page> {
    match page.repo() {
        None => Some(Page::Tree {
            repo: site.to_string(),
            rev: "HEAD".into(),
            path: String::new(),
        }),
        Some(repo) if repo == site => Some(page),
        Some(_) => None,
    }
}

pub(crate) fn render(
    root: &Path,
    page: &Page,
    readable: &dyn Fn(&str) -> bool,
    platform: Option<&crate::platform::Platform>,
    user: &str,
    browser_writes: bool,
    site: Option<&str>,
) -> Rendered {
    // A page that reads the repository off disk must not start describing
    // one that is not there. Without this, `resolve` fails and the reader
    // is handed git's own words — which name the absolute path git was
    // given — under a headline claiming they may read the repository.
    // Answering the same refusal a denied reader gets is both the honest
    // page and the one that keeps the two states indistinguishable.
    //
    // `Reviews` is exempt because it is served from the view rather than
    // from disk: a review can name a destination this node does not host,
    // and 404-ing the list would hide a review that genuinely exists.
    if let Some(repo) = page.repo() {
        if !matches!(page, Page::Reviews { .. }) && !bare(root, repo).is_dir() {
            return no_such_repository();
        }
    }
    match page {
        Page::Index { q } => index(root, readable, platform, q),
        Page::Search {
            repo,
            rev,
            q,
            scope,
        } => search(&bare(root, repo), repo, rev, q, *scope, site),
        Page::Tree { repo, rev, path } => tree(&bare(root, repo), repo, rev, path, platform, site),
        Page::Blob { repo, rev, path } => blob(&bare(root, repo), repo, rev, path, site),
        Page::Commits { repo, rev } => commits(&bare(root, repo), repo, rev, site),
        Page::Commit { repo, oid } => commit(&bare(root, repo), repo, oid, site),
        Page::Reviews { repo } => reviews(repo, platform, site),
        Page::Review { repo, id } => review(
            &bare(root, repo),
            repo,
            id,
            platform,
            user,
            browser_writes,
            site,
        ),
    }
}

/// On-disk location of a repository's bare directory.
fn bare(root: &Path, repo: &str) -> PathBuf {
    root.join(format!("{repo}.git"))
}

/// The repository index, filtered to what the reader may see.
///
/// Each row carries its open-review count as an inline chip: the status
/// lives on the list, because a page a reader has to remember to visit
/// is a page nobody visits. The count comes from the same platform view
/// the review pages render, so the two cannot disagree.
fn index(
    root: &Path,
    readable: &dyn Fn(&str) -> bool,
    platform: Option<&crate::platform::Platform>,
    q: &str,
) -> Rendered {
    let mut repos: Vec<String> = Vec::new();
    if let Ok(owners) = std::fs::read_dir(root) {
        for owner in owners.flatten() {
            let owner_name = owner.file_name().to_string_lossy().into_owned();
            // `.choir` holds the node's own state, not a repository.
            if owner_name.starts_with('.') || !owner.path().is_dir() {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(owner.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(stem) = name.strip_suffix(".git") else {
                    continue;
                };
                let repo = format!("{owner_name}/{stem}");
                if readable(&repo) {
                    repos.push(repo);
                }
            }
        }
    }
    repos.sort();
    // The filter runs after the grant check, never before it: a reader
    // must never be able to learn that a repository exists by searching
    // for it. `shown` is a subset of what this reader could already list.
    let needle = q.to_lowercase();
    let shown: Vec<&String> = repos
        .iter()
        .filter(|repo| needle.is_empty() || repo.to_lowercase().contains(&needle))
        .collect();

    let mut h = shell("repositories", Bar::filtered(q));
    h.push_str("<header class=\"top\"><h1>repositories</h1><div class=\"sub\">");
    h.push_str("<span class=\"pill\">");
    h.push_str(&repos.len().to_string());
    h.push_str(" readable</span>");
    if !needle.is_empty() {
        h.push_str("<span class=\"pill\">");
        h.push_str(&shown.len().to_string());
        h.push_str(" matching</span>");
    }
    h.push_str("<span class=\"pill\"><a href=\"/status\">node state</a></span>");
    h.push_str("</div>");
    h.push_str("</header><main id=\"main\"><section>");
    if !repos.is_empty() && shown.is_empty() {
        h.push_str("<p class=\"empty\">No repository you can read matches <b>");
        h.push_str(&esc(q));
        h.push_str("</b>. <a href=\"/r/\">Clear the filter</a> to see all ");
        h.push_str(&repos.len().to_string());
        h.push_str(".</p>");
    } else if repos.is_empty() {
        // Deliberately not "this node holds N repositories, you may read
        // none": the count is node-wide state, and a reader with no grant
        // is exactly who must not learn it. So the page names both causes
        // and both fixes instead, and says that it cannot tell them apart
        // on purpose — otherwise a new reader reads D29 working correctly
        // as the node being broken.
        h.push_str(
            "<p class=\"lede\">No repositories you can read. Either this node holds none yet, \
             or this credential was not granted read on the ones it holds — from here those \
             look the same, which is deliberate: a credential is shown what it was granted \
             and never told what else exists.</p>",
        );
        crate::ui::next_action(
            &mut h,
            "If you run this node, create one: <code>choir-node &lt;root&gt; &lt;port&gt; \
             --create &lt;owner&gt;/&lt;name&gt;.git</code>. If you do not, ask whoever does \
             for a <code>read</code> grant on the repository you were pointed at.",
        );
    } else {
        h.push_str("<table><tbody>");
        for repo in &shown {
            h.push_str("<tr><td><a href=\"/r/");
            h.push_str(&esc(repo));
            h.push_str("\">");
            h.push_str(&esc(repo));
            h.push_str("</a></td><td class=\"num\">");
            let open = platform
                .map(|p| {
                    p.reviews_for_repo(repo)
                        .iter()
                        .filter(|(_, r)| {
                            !r["complete"].as_bool().unwrap_or(false)
                                && !r["archived"].as_bool().unwrap_or(false)
                        })
                        .count()
                })
                .unwrap_or(0);
            // Zero open reviews is silence, not a zero: the chip exists
            // to pull a reader toward waiting work, and a row of grey
            // zeros would train the eye to skip the column.
            if open > 0 {
                h.push_str("<a href=\"/r/");
                h.push_str(&esc(repo));
                h.push_str("/reviews\"><b class=\"tag pending\">");
                h.push_str(&open.to_string());
                h.push_str(" open</b></a>");
            }
            h.push_str("</td></tr>");
        }
        h.push_str("</tbody></table>");
    }
    h.push_str("</section>");
    Rendered {
        status: 200,
        etag: None,
        html: close(h),
    }
}

/// Percent-encodes a string for use inside a query-string value.
///
/// Stricter than [`url_path`] on purpose: everything outside an
/// unreserved set is encoded, so no term can end a value early or start
/// a parameter of its own however it is spelled.
fn url_query(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// One repository, searched.
///
/// Every scope runs against a resolved commit oid rather than the ref
/// name it came from, so the three tabs of one search always describe the
/// same tree even if a push lands between two clicks.
fn search(
    dir: &Path,
    repo: &str,
    rev: &str,
    q: &str,
    scope: Scope,
    site: Option<&str>,
) -> Rendered {
    let oid = match resolve(dir, rev) {
        Ok(oid) => oid,
        Err(why) => return missing(repo, rev, &why),
    };
    let named = if rev == "HEAD" {
        default_branch(dir).unwrap_or_else(|| rev.to_string())
    } else {
        rev.to_string()
    };
    let rev = named.as_str();

    let mut h = shell(
        &format!("{repo}: search"),
        Bar::searched(repo, rev, q, site),
    );
    repo_header(&mut h, repo, rev, &oid, "", "search");
    h.push_str("<section>");
    // All three scopes run, not just the one asked for, because the tabs
    // carry counts. Without them a reader who lands on the scope with no
    // hits sees "no match" and concludes the search is broken — which is
    // exactly what happened on the first real query typed into this box:
    // `latency` matched no *file name* and 178 lines of code, and the
    // page said only the first half.
    //
    // It costs two extra git invocations per search. That is the price of
    // the page never being a dead end, and it is paid on a page a reader
    // reaches deliberately.
    let hits = if q.is_empty() {
        Hits::default()
    } else {
        Hits::find(dir, &oid, q)
    };

    h.push_str("<nav class=\"tabs\">");
    for one in Scope::ALL {
        let count = hits.count(one);
        if one == scope {
            h.push_str("<b class=\"tab here\">");
            h.push_str(one.label());
            tab_count(&mut h, count, q);
            h.push_str("</b>");
        } else {
            h.push_str("<a class=\"tab\" href=\"/r/");
            h.push_str(&esc(repo));
            h.push_str("/search/");
            h.push_str(&esc(&url_path(rev)));
            h.push_str("?q=");
            h.push_str(&url_query(q));
            h.push_str("&amp;in=");
            h.push_str(one.slug());
            h.push_str("\">");
            h.push_str(one.label());
            tab_count(&mut h, count, q);
            h.push_str("</a>");
        }
    }
    h.push_str("</nav>");

    if q.is_empty() {
        h.push_str(
            "<p class=\"lede\">Type a term above. <b>File names</b> matches anywhere in a \
             path, <b>file contents</b> searches every file in this revision, and <b>commit \
             messages</b> searches the history reachable from it. All three are literal \
             substrings, never patterns — a term containing <code>*</code> or <code>.</code> \
             matches those characters.</p>",
        );
        h.push_str("</section>");
        return Rendered {
            status: 200,
            etag: None,
            html: close(h),
        };
    }

    match scope {
        Scope::Files => search_files(&mut h, repo, rev, q, &hits),
        Scope::Code => search_code(&mut h, repo, rev, q, &hits),
        Scope::Commits => search_commits(&mut h, repo, q, &hits),
    }
    // The dead end, closed. A scope with nothing in it points at the
    // scopes that do rather than leaving the reader to guess that the
    // other tabs are worth a click.
    if hits.count(scope) == 0 {
        elsewhere(&mut h, repo, rev, q, scope, &hits);
    }
    h.push_str("</section>");
    Rendered {
        status: 200,
        etag: None,
        html: close(h),
    }
}

/// Every scope's matches for one term, at one revision.
///
/// All three are found together so the tabs can carry counts. Owned
/// rather than borrowed from git's output because the three outputs have
/// different lifetimes and the page outlives all of them.
#[derive(Default)]
struct Hits {
    /// Matching paths.
    files: Vec<String>,
    /// Matching lines, as `(path, line number, line)`.
    code: Vec<(String, String, String)>,
    /// Matching commits, as `(oid, author, unix time, subject, body)`.
    commits: Vec<(String, String, String, String, String)>,
}

impl Hits {
    /// Runs all three searches against one resolved commit.
    fn find(dir: &Path, oid: &str, q: &str) -> Hits {
        Hits {
            files: find_files(dir, oid, q),
            code: find_code(dir, oid, q),
            commits: find_commits(dir, oid, q),
        }
    }

    /// How many matches one scope holds.
    fn count(&self, scope: Scope) -> usize {
        match scope {
            Scope::Files => self.files.len(),
            Scope::Code => self.code.len(),
            Scope::Commits => self.commits.len(),
        }
    }
}

/// File-name search: one tree listing, filtered here.
///
/// Filtered in this process rather than by `git ls-files --glob` because
/// a reader typing `config` means a substring, and turning that into a
/// glob would either miss `src/config.rs` or require them to know to type
/// `*config*`.
fn find_files(dir: &Path, oid: &str, q: &str) -> Vec<String> {
    let Ok(listing) = git_text(dir, &["ls-tree", "-r", "--name-only", oid]) else {
        return Vec::new();
    };
    let needle = q.to_lowercase();
    listing
        .lines()
        .filter(|path| path.to_lowercase().contains(&needle))
        .map(str::to_string)
        .collect()
}

/// Content search, delegated to `git grep` against one tree.
fn find_code(dir: &Path, oid: &str, q: &str) -> Vec<(String, String, String)> {
    // `-e` is what makes a term beginning with `-` a term rather than an
    // option, `-F` makes it a literal rather than a pattern, and `-I`
    // keeps binary files from being reported as one-line matches.
    //
    // `git grep` exits 1 for "no matches", which `git` reports as a
    // failure. That is a result, not an error, and treating it as empty
    // output keeps a clean miss from rendering as a broken page.
    let text = git_text(
        dir,
        &[
            "grep",
            "--no-color",
            "-n",
            "-I",
            "-i",
            "-F",
            "--max-count=10",
            "-e",
            q,
            oid,
        ],
    )
    .unwrap_or_default();
    text.lines()
        .filter_map(|line| {
            // `<oid>:<path>:<line>:<text>` — the oid prefix is ours, so
            // it is stripped by length rather than by splitting, which a
            // path containing a colon would break.
            let rest = line.strip_prefix(oid)?.strip_prefix(':')?;
            let (path, rest) = rest.split_once(':')?;
            let (number, body) = rest.split_once(':')?;
            Some((path.to_string(), number.to_string(), body.to_string()))
        })
        .collect()
}

/// Commit-message search, delegated to `git log --grep`.
fn find_commits(dir: &Path, oid: &str, q: &str) -> Vec<(String, String, String, String, String)> {
    // The term is embedded after `--grep=`, so it cannot be read as an
    // option however it is spelled; `-F` keeps it a literal.
    let grep = format!("--grep={q}");
    let limit = format!("--max-count={}", SEARCH_HITS + 1);
    // NUL-terminated records, because the body is part of the match and
    // a body spans lines. Splitting the output on newlines would turn
    // one commit into several malformed rows.
    let text = git_text(
        dir,
        &[
            "log",
            "-F",
            "-i",
            &grep,
            &limit,
            "--format=%H%x09%an%x09%at%x09%s%x09%b%x00",
            oid,
        ],
    )
    .unwrap_or_default();
    text.split('\0')
        .map(str::trim_start)
        .filter(|record| !record.is_empty())
        .filter_map(|record| {
            let mut f = record.splitn(5, '\t');
            Some((
                f.next()?.to_string(),
                f.next()?.to_string(),
                f.next()?.to_string(),
                f.next()?.to_string(),
                f.next()?.to_string(),
            ))
        })
        .collect()
}

/// The count beside a tab's label.
///
/// Nothing at all before a term is typed: a row of zeros on an empty
/// search says only that nothing has been searched yet.
fn tab_count(h: &mut String, count: usize, q: &str) {
    if q.is_empty() {
        return;
    }
    h.push_str(" <span class=\"count\">");
    h.push_str(&thousands(count as u64));
    h.push_str("</span>");
}

/// Says how many hits are shown when the cap cut the list short.
fn capped(h: &mut String, more: bool, what: &str) {
    // `what` is already `"9 files"` — `plural` carries the count, and
    // writing it again here is how the page said "9 9 files".
    h.push_str("<p class=\"note\">");
    h.push_str(what);
    if more {
        h.push_str(", and the list stopped there. Narrow the term to see the rest");
    }
    h.push_str(".</p>");
}

/// Nothing matched, said once for every scope.
fn nothing(h: &mut String, q: &str, what: &str) {
    h.push_str("<p class=\"empty\">No ");
    h.push_str(what);
    h.push_str(" matches <b>");
    h.push_str(&esc(q));
    h.push_str("</b> at this revision.</p>");
}

/// Where the matches are, when they are not here.
///
/// The counts are already on the tabs; this is the sentence that makes a
/// reader look at them. A search that found nothing anywhere says so
/// once, rather than listing two more places that are also empty.
fn elsewhere(h: &mut String, repo: &str, rev: &str, q: &str, scope: Scope, hits: &Hits) {
    let others: Vec<Scope> = Scope::ALL
        .into_iter()
        .filter(|one| *one != scope && hits.count(*one) > 0)
        .collect();
    if others.is_empty() {
        return;
    }
    h.push_str("<p class=\"note\">Found in ");
    for (n, one) in others.iter().enumerate() {
        if n > 0 {
            h.push_str(" and ");
        }
        h.push_str("<a href=\"/r/");
        h.push_str(&esc(repo));
        h.push_str("/search/");
        h.push_str(&esc(&url_path(rev)));
        h.push_str("?q=");
        h.push_str(&url_query(q));
        h.push_str("&amp;in=");
        h.push_str(one.slug());
        h.push_str("\">");
        h.push_str(&plural(hits.count(*one), one.singular(), one.plural_noun()));
        h.push_str("</a>");
    }
    h.push_str(" instead.</p>");
}

/// Renders the file-name hits.
fn search_files(h: &mut String, repo: &str, rev: &str, q: &str, hits: &Hits) {
    if hits.files.is_empty() {
        nothing(h, q, "file name");
        return;
    }
    let more = hits.files.len() > SEARCH_HITS;
    let shown = &hits.files[..hits.files.len().min(SEARCH_HITS)];
    capped(h, more, &plural(shown.len(), "file", "files"));
    h.push_str("<table class=\"listing\"><tbody>");
    for path in shown {
        h.push_str("<tr><td><a class=\"mono\" href=\"/r/");
        h.push_str(&esc(repo));
        h.push_str("/blob/");
        h.push_str(&esc(&url_path(rev)));
        h.push('/');
        h.push_str(&esc(&url_path(path)));
        h.push_str("\">");
        // The matched span is marked so the eye lands on why the row is
        // here, which on a path like `src/config/mod.rs` is not obvious.
        highlight(h, path, q);
        h.push_str("</a></td></tr>");
    }
    h.push_str("</tbody></table>");
}

/// Renders the content hits.
fn search_code(h: &mut String, repo: &str, rev: &str, q: &str, hits: &Hits) {
    if hits.code.is_empty() {
        nothing(h, q, "file content");
        return;
    }
    let more = hits.code.len() > SEARCH_HITS;
    let shown = &hits.code[..hits.code.len().min(SEARCH_HITS)];
    capped(h, more, &plural(shown.len(), "line", "lines"));
    h.push_str("<table class=\"listing hits\"><tbody>");
    for (path, number, body) in shown {
        h.push_str("<tr><td class=\"mono muted\"><a href=\"/r/");
        h.push_str(&esc(repo));
        h.push_str("/blob/");
        h.push_str(&esc(&url_path(rev)));
        h.push('/');
        h.push_str(&esc(&url_path(path)));
        h.push_str("\">");
        h.push_str(&esc(path));
        h.push_str("</a>:");
        h.push_str(&esc(number));
        h.push_str("</td><td><code>");
        // Long minified lines would otherwise push the table off screen.
        let body = body.trim_end();
        let clipped: String = body.chars().take(200).collect();
        highlight(h, &clipped, q);
        if clipped.len() < body.len() {
            h.push('…');
        }
        h.push_str("</code></td></tr>");
    }
    h.push_str("</tbody></table>");
}

/// Renders the commit-message hits.
fn search_commits(h: &mut String, repo: &str, q: &str, hits: &Hits) {
    if hits.commits.is_empty() {
        nothing(h, q, "commit message");
        return;
    }
    let more = hits.commits.len() > SEARCH_HITS;
    let shown = &hits.commits[..hits.commits.len().min(SEARCH_HITS)];
    capped(h, more, &plural(shown.len(), "commit", "commits"));
    let now = now_secs();
    let needle = q.to_lowercase();
    h.push_str("<table class=\"listing\"><tbody>");
    for (commit, author, when, subject, body) in shown {
        h.push_str("<tr><td class=\"subject\"><a href=\"/r/");
        h.push_str(&esc(repo));
        h.push_str("/commit/");
        h.push_str(&esc(commit));
        h.push_str("\">");
        highlight(h, subject, q);
        // git matched the whole message, so a row whose subject holds no
        // mark looks like a false positive until the line that did match
        // is shown. Without this the list reads as broken on any project
        // that writes commit bodies.
        if !subject.to_lowercase().contains(&needle) {
            if let Some(line) = body
                .lines()
                .find(|line| line.to_lowercase().contains(&needle))
            {
                h.push_str("<span class=\"why\">");
                let clipped: String = line.trim().chars().take(160).collect();
                highlight(h, &clipped, q);
                h.push_str("</span>");
            }
        }
        h.push_str("</a></td><td class=\"mono muted\">");
        h.push_str(&esc(&short_oid(commit)));
        h.push_str("</td><td class=\"muted\">");
        h.push_str(&esc(author));
        h.push_str("</td><td class=\"when muted\">");
        h.push_str(&esc(&ago(now, when.parse().unwrap_or(now))));
        h.push_str("</td></tr>");
    }
    h.push_str("</tbody></table>");
}

/// Writes `text`, escaped, with every case-insensitive occurrence of
/// `needle` wrapped in `<mark>`.
///
/// The escaping happens per fragment rather than once over the whole
/// string, because inserting tags into already-escaped text means
/// computing offsets in the escaped string — and getting that wrong is
/// how a highlighter becomes an injection. Here nothing unescaped is
/// ever written.
fn highlight(h: &mut String, text: &str, needle: &str) {
    if needle.is_empty() {
        h.push_str(&esc(text));
        return;
    }
    let hay = text.to_lowercase();
    let pin = needle.to_lowercase();
    // Lowercasing can change a string's length (`İ` is one char and two
    // lowercased), which would make offsets from `hay` wrong for `text`.
    // When that happens the marks are dropped rather than misplaced.
    if hay.len() != text.len() {
        h.push_str(&esc(text));
        return;
    }
    let mut at = 0;
    while let Some(found) = hay[at..].find(&pin) {
        let start = at + found;
        let end = start + pin.len();
        if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            break;
        }
        h.push_str(&esc(&text[at..start]));
        h.push_str("<mark>");
        h.push_str(&esc(&text[start..end]));
        h.push_str("</mark>");
        at = end;
    }
    h.push_str(&esc(&text[at..]));
}

/// Every branch and every tag, short names, in git's own order.
///
/// Two lists rather than one because they answer different questions: a
/// branch is where work is happening, a tag is a release. Presenting them
/// in one flat list is how a reader ends up browsing `v0.1.0` believing
/// it is current.
fn branches_and_tags(dir: &Path) -> (Vec<String>, Vec<String>) {
    let read = |pattern: &str| -> Vec<String> {
        git_text(dir, &["for-each-ref", "--format=%(refname:short)", pattern])
            .map(|text| {
                text.lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    (read("refs/heads/"), read("refs/tags/"))
}

/// The commit a path was last changed by: its subject and its timestamp.
///
/// One `git log -1` per listed entry. That is a subprocess per row, which
/// is the cost of being right about renames and merges without
/// reimplementing git's history simplification here. A directory listing
/// is tens of rows, not thousands, and the alternative — one walk with a
/// commit budget — silently leaves blank cells on exactly the old, stable
/// files a reader is most likely to be looking for.
fn last_touch(dir: &Path, oid: &str, path: &str) -> Option<(String, i64)> {
    // `--diff-merges=first-parent` is what makes a file whose only recent
    // change arrived through a merge show that merge instead of nothing.
    // It needs git 2.31; an older git rejects the flag outright, and a
    // blank column is a worse answer than a slightly less accurate one,
    // so the plain walk is the fallback rather than the failure.
    let text = git_text(
        dir,
        &[
            "log",
            "-1",
            "--format=%at%x00%s",
            "--diff-merges=first-parent",
            oid,
            "--",
            path,
        ],
    )
    .or_else(|_| git_text(dir, &["log", "-1", "--format=%at%x00%s", oid, "--", path]))
    .ok()?;
    let line = text.lines().next()?;
    let (at, subject) = line.split_once('\0')?;
    Some((subject.to_string(), at.trim().parse().ok()?))
}

/// Seconds since the epoch, or `0` if the clock is before it.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A timestamp as a reader thinks about it: "yesterday", not an epoch.
///
/// Coarse on purpose. The exact instant is on the commit page, and a
/// listing that says `2 days ago` for everything older than a week is
/// less readable than one that says `last month`.
fn ago(now: i64, then: i64) -> String {
    let seconds = now.saturating_sub(then);
    if seconds < 0 {
        // A commit dated in the future is a clock problem somewhere, not
        // something to render as "in -3 days".
        return "just now".to_string();
    }
    let minutes = seconds / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    match (minutes, hours, days) {
        (0..=1, _, _) => "just now".to_string(),
        (m, 0, _) => format!("{m} minutes ago"),
        (_, 1, _) => "an hour ago".to_string(),
        (_, h, 0) => format!("{h} hours ago"),
        (_, _, 1) => "yesterday".to_string(),
        (_, _, d) if d < 30 => format!("{d} days ago"),
        // `1 months ago` and `1 years ago` are what a bare `n / 30`
        // prints for five weeks and thirteen months, and both read as a
        // bug in the page rather than an age.
        (_, _, d) if d < 60 => "last month".to_string(),
        (_, _, d) if d < 365 => format!("{} months ago", d / 30),
        (_, _, d) if d < 730 => "last year".to_string(),
        (_, _, d) => format!("{} years ago", d / 365),
    }
}

/// The revision picker: every branch and tag, one click each.
///
/// A `<details>` rather than a `<select>` because this surface runs no
/// script, and a `<select>` without one is a control that changes nothing
/// when a reader uses it.
fn ref_picker(h: &mut String, repo: &str, rev: &str, branches: &[String], tags: &[String]) {
    h.push_str("<details class=\"picker\"><summary>");
    h.push_str(&esc(rev));
    h.push_str("</summary>");
    for (label, names) in [("Branches", branches), ("Tags", tags)] {
        if names.is_empty() {
            continue;
        }
        h.push_str("<h3>");
        h.push_str(label);
        h.push_str("</h3><ul>");
        for name in names {
            h.push_str("<li><a href=\"/r/");
            h.push_str(&esc(repo));
            h.push_str("/tree/");
            h.push_str(&esc(&url_path(name)));
            h.push_str("\">");
            h.push_str(&esc(name));
            h.push_str("</a></li>");
        }
        h.push_str("</ul>");
    }
    h.push_str("</details>");
}

/// The repository's README at this revision: its filename and its text.
///
/// The name is taken from the listing that was already fetched rather
/// than probed for, so a repository without one costs no extra `git`
/// call, and the file that renders is provably one of the files the
/// reader can see listed above it.
fn readme_of(dir: &Path, oid: &str, listing: &str) -> Option<(String, String)> {
    // `ls-tree` prints paths from the repository root, so inside a
    // directory the candidates are `src/README.md`, not `README.md`. The
    // match is on the leaf; the path is what `cat-file` needs.
    let present: Vec<(&str, &str)> = listing
        .lines()
        .filter_map(|line| line.split_once('\t').map(|(_, path)| path))
        .map(|path| (path.rsplit('/').next().unwrap_or(path), path))
        .collect();
    let (_, path) = crate::readme::NAMES.iter().find_map(|candidate| {
        present
            .iter()
            .find(|(leaf, _)| leaf == candidate)
            .map(|(leaf, path)| (*leaf, *path))
    })?;
    let name = path.rsplit('/').next().unwrap_or(path);
    let spec = format!("{oid}:{path}");
    let size: u64 = git_text(dir, &["cat-file", "-s", &spec])
        .ok()?
        .trim()
        .parse()
        .ok()?;
    // A README past the blob ceiling is not rendered at all: it is
    // reachable through the listing like any other file, and a truncated
    // one would be a document that silently stops mid-sentence.
    if size > MAX_BLOB_BYTES {
        return None;
    }
    let body = git_text(dir, &["cat-file", "blob", &spec]).ok()?;
    Some((name.to_string(), body))
}

/// A directory listing.
fn tree(
    dir: &Path,
    repo: &str,
    rev: &str,
    path: &str,
    platform: Option<&crate::platform::Platform>,
    site: Option<&str>,
) -> Rendered {
    let oid = match resolve(dir, rev) {
        Ok(oid) => oid,
        // A repository that exists but resolves nothing is empty, not
        // missing — and it is the first thing an operator browses after
        // creating one, so answering `404` there says the create failed
        // when it did not.
        Err(_) if dir.is_dir() && rev == "HEAD" => return empty(repo),
        Err(why) => return missing(repo, rev, &why),
    };
    // `HEAD` is how the front door is *addressed*, not what a reader
    // wants to be told they are looking at, and it is not a link anyone
    // can usefully share. Every link this page emits therefore names the
    // branch, so a reader who copies one gets a URL that keeps meaning
    // the same thing.
    let named = if rev == "HEAD" {
        default_branch(dir).unwrap_or_else(|| rev.to_string())
    } else {
        rev.to_string()
    };
    let rev = named.as_str();
    // The trailing slash is what makes `ls-tree` list a directory's
    // children rather than the directory entry itself.
    let spec = if path.is_empty() {
        String::new()
    } else {
        format!("{path}/")
    };
    let listing = if spec.is_empty() {
        git_text(dir, &["ls-tree", "--long", &oid])
    } else {
        git_text(dir, &["ls-tree", "--long", &oid, "--", &spec])
    };
    let listing = match listing {
        Ok(text) => text,
        Err(why) => return missing(repo, rev, &why),
    };

    let mut rows: Vec<(bool, String, String)> = Vec::new();
    for line in listing.lines() {
        // `<mode> SP <type> SP <oid> SP* <size> TAB <name>`
        let Some((meta, name)) = line.split_once('\t') else {
            continue;
        };
        let mut fields = meta.split_whitespace();
        let (_mode, kind, _oid, size) =
            match (fields.next(), fields.next(), fields.next(), fields.next()) {
                (Some(m), Some(k), Some(o), Some(s)) => (m, k, o, s),
                _ => continue,
            };
        let is_dir = kind == "tree";
        let size = if is_dir { String::new() } else { human(size) };
        rows.push((is_dir, name.to_string(), size));
    }
    // Directories first, then files, each alphabetically — the order
    // every file browser has used for thirty years.
    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

    let mut h = shell(
        &format!("{repo}: {}", if path.is_empty() { "/" } else { path }),
        Bar::repo(repo, rev, site),
    );
    repo_header(&mut h, repo, rev, &oid, path, "tree");
    // The bar a reader uses to orient: which revision they are on, how
    // much history is under it, and what else they could switch to. Only
    // at the repository root — inside a directory it is the breadcrumb
    // that answers "where am I", and repeating the picker there just
    // pushes the listing further down the page.
    if path.is_empty() {
        let (branches, tags) = branches_and_tags(dir);
        let commits = git_text(dir, &["rev-list", "--count", &oid])
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok());
        h.push_str("<div class=\"repobar\">");
        ref_picker(&mut h, repo, rev, &branches, &tags);
        h.push_str("<span class=\"counts\">");
        if let Some(count) = commits {
            h.push_str("<a href=\"/r/");
            h.push_str(&esc(repo));
            h.push_str("/commits/");
            h.push_str(&esc(rev));
            h.push_str("\">");
            h.push_str(&thousands(count));
            h.push_str(if count == 1 { " commit" } else { " commits" });
            h.push_str("</a>");
        }
        h.push_str("<span class=\"muted\">");
        h.push_str(&plural(branches.len(), "branch", "branches"));
        h.push_str("</span><span class=\"muted\">");
        h.push_str(&plural(tags.len(), "tag", "tags"));
        h.push_str("</span></span></div>");
    }
    // The repository root is three panes side by side: what work is in
    // flight, what files exist, and what the README says. A repository
    // many agents are writing at once is one where "what is happening"
    // outranks "what is here", so the reviews pane comes first and the
    // reader never has to go looking for it. Inside a directory there is
    // no such question to answer, so those pages stay a single column.
    let file_count = rows.len();
    if path.is_empty() {
        h.push_str("<div class=\"panes\">");
        reviews_pane(&mut h, dir, repo, platform);
        h.push_str("<section class=\"pane pane-files\"><h2>");
        h.push_str(&plural(file_count, "entry", "entries"));
        h.push_str("<span class=\"mono muted\">");
        h.push_str(&esc(&oid[..oid.len().min(12)]));
        h.push_str("</span></h2>");
    } else {
        h.push_str("<section>");
    }
    if rows.is_empty() {
        // Reached two ways that need different fixes: a path that names
        // nothing at this revision, and a genuinely empty directory at
        // the root. `ls-tree` answers both with silence, so the page says
        // which one it is by looking at whether a path was asked for.
        if path.is_empty() {
            h.push_str(
                "<p class=\"lede\">This revision has no files in it. The commit exists; its \
                 tree is empty.</p>",
            );
            crate::ui::next_action(
                &mut h,
                "Try another branch from the history above, or push a commit that adds \
                 something.",
            );
        } else {
            h.push_str(
                "<p class=\"lede\">Nothing here at this revision. The path is spelled \
                 correctly enough to be a path, but this commit's tree does not contain it.</p>",
            );
            crate::ui::next_action(
                &mut h,
                "Walk down from the repository root using the breadcrumb above — that path \
                 exists by construction. A file that was deleted is still in the history.",
            );
        }
    } else {
        let now = now_secs();
        h.push_str("<table class=\"listing\"><tbody>");
        for (is_dir, name, size) in rows {
            let touched = last_touch(dir, &oid, &name);
            // `ls-tree` prints the full path from the root; the link
            // needs that, the listing wants only the last segment.
            let leaf = name.rsplit('/').next().unwrap_or(&name);
            let verb = if is_dir { "tree" } else { "blob" };
            h.push_str("<tr><td><a href=\"/r/");
            h.push_str(&esc(repo));
            h.push('/');
            h.push_str(verb);
            h.push('/');
            h.push_str(&esc(rev));
            h.push('/');
            // Encoded for the URL, then escaped for the attribute. The
            // label below is the other way round on purpose: a reader
            // must see the name the repository has, not its encoding.
            h.push_str(&esc(&url_path(&name)));
            h.push_str("\">");
            if is_dir {
                h.push_str("<span class=\"muted\">/</span>");
            }
            h.push_str(&esc(leaf));
            h.push_str("</a>");
            // Subject and age, the two columns that turn a file list into
            // a description of what is happening in the repository. A
            // path git cannot date is left blank rather than filled with
            // a guess.
            //
            // The root's files pane is one narrow column of three, and
            // four columns in it clip to nothing — the subject rendered
            // as `re`, and the size fell off the edge entirely. Dropping
            // the subject would have been the easy fix and the wrong
            // one: a listing of bare names is a directory, and saying
            // what each path is *for* is why this column exists. So in
            // the pane it stacks under the name instead of beside it,
            // where it has the width to be read. The directory pages
            // have the whole page, and keep all four columns.
            if path.is_empty() {
                if let Some((subject, _)) = touched.as_ref() {
                    h.push_str("<span class=\"why muted\">");
                    h.push_str(&esc(subject));
                    h.push_str("</span>");
                }
                h.push_str("</td>");
            } else {
                h.push_str("</td><td class=\"subject muted\">");
                if let Some((subject, _)) = touched.as_ref() {
                    h.push_str(&esc(subject));
                }
                h.push_str("</td>");
            }
            h.push_str("<td class=\"when muted\">");
            if let Some((_, at)) = touched.as_ref() {
                h.push_str(&esc(&ago(now, *at)));
            }
            h.push_str("</td>");
            if !path.is_empty() {
                h.push_str("<td class=\"num muted\">");
                h.push_str(&esc(&size));
                h.push_str("</td>");
            }
            h.push_str("</tr>");
        }
        h.push_str("</tbody></table>");
    }
    h.push_str("</section>");
    // The README, under the listing, the way every code host has put it
    // since the convention started. At every level, not just the root: a
    // README beside a directory's files is documentation for exactly the
    // files a reader is looking at, and making them click it is making
    // them click the one file that was written to save them the trip.
    //
    // At the root it is the third pane rather than a section under the
    // listing, so it sits beside the files instead of below them.
    {
        if path.is_empty() {
            h.push_str("<section class=\"pane pane-content\">");
        }
        if let Some((name, body)) = readme_of(dir, &oid, &listing) {
            h.push_str("<section class=\"readme\"><h2>");
            h.push_str(&esc(&name));
            h.push_str("</h2>");
            h.push_str(&crate::readme::render(&body));
            h.push_str("</section>");
        } else if path.is_empty() {
            // An empty third pane reads as a broken layout. Saying what
            // is missing, and what would fill it, does not.
            h.push_str(
                "<p class=\"lede\">No README at this revision. A <code>README.md</code> in \
                 the repository root renders here.</p>",
            );
        }
        if path.is_empty() {
            h.push_str("</section></div>");
        }
    }
    Rendered {
        status: 200,
        // The listing now renders ages, which move while the commit does
        // not. Folding the current hour into the validator keeps a
        // cached page from insisting it is still "2 hours ago" tomorrow,
        // at the cost of one revalidation an hour.
        // The root page also carries the reviews pane, and a review
        // arrives with no push behind it: the tree oid does not move, so
        // without the sequence here a browser holds "nothing proposes to
        // land here" for up to an hour after something did. Only the
        // root, because folding it into every subdirectory would
        // invalidate the whole tree on any unrelated op.
        etag: Some(tag(
            &oid,
            &format!(
                "{path}@{}{}",
                now_secs() / 3600,
                match platform.filter(|_| path.is_empty()) {
                    Some(platform) => format!("+{}", platform.view_seq()),
                    None => String::new(),
                }
            ),
        )),
        html: close(h),
    }
}

/// One file.
fn blob(dir: &Path, repo: &str, rev: &str, path: &str, site: Option<&str>) -> Rendered {
    let oid = match resolve(dir, rev) {
        Ok(oid) => oid,
        Err(why) => return missing(repo, rev, &why),
    };
    let spec = format!("{oid}:{path}");
    let size: u64 = match git_text(dir, &["cat-file", "-s", &spec]) {
        Ok(text) => text.trim().parse().unwrap_or(0),
        Err(why) => return missing(repo, rev, &why),
    };

    let mut h = shell(&format!("{repo}: {path}"), Bar::repo(repo, rev, site));
    repo_header(&mut h, repo, rev, &oid, path, "blob");
    h.push_str("<section>");
    if size > MAX_BLOB_BYTES {
        h.push_str("<p class=\"note\">");
        h.push_str(&esc(&human(&size.to_string())));
        h.push_str(" — too large to show. Clone the repository to read it.</p>");
    } else {
        match git(dir, &["cat-file", "blob", &spec]) {
            Err(why) => {
                // The commit resolved and the size read, so this is a
                // path that is not a blob — most often a directory asked
                // for with the wrong verb, which is a link away from
                // working rather than a fault.
                h.push_str(
                    "<p class=\"lede\">This revision holds nothing readable at that \
                            path. A directory asked for as a file lands here.</p>",
                );
                h.push_str("<p class=\"note\">git says: ");
                h.push_str(&esc(&why));
                h.push_str("</p>");
                crate::ui::next_action(
                    &mut h,
                    "Use the breadcrumb above to walk to it as a directory, or check the \
                     spelling against the listing it came from.",
                );
            }
            // A NUL in the first block is the same heuristic git itself
            // uses to call a file binary, and it is right often enough
            // that a browser has never needed a better one.
            Ok(bytes) if bytes.iter().take(8000).any(|b| *b == 0) => {
                h.push_str("<p class=\"note\">Binary file, ");
                h.push_str(&esc(&human(&size.to_string())));
                h.push_str(".</p>");
            }
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                h.push_str("<pre class=\"code\">");
                for (n, line) in text.lines().enumerate() {
                    h.push_str("<span class=\"ln\">");
                    h.push_str(&(n + 1).to_string());
                    h.push_str("</span>");
                    h.push_str(&esc(line));
                    h.push('\n');
                }
                h.push_str("</pre>");
            }
        }
    }
    h.push_str("</section>");
    Rendered {
        status: 200,
        etag: Some(tag(&oid, path)),
        html: close(h),
    }
}

/// Recent history.
fn commits(dir: &Path, repo: &str, rev: &str, site: Option<&str>) -> Rendered {
    let oid = match resolve(dir, rev) {
        Ok(oid) => oid,
        Err(why) => return missing(repo, rev, &why),
    };
    // Unit separators rather than spaces: a subject can contain
    // anything, including whatever delimiter looked safe.
    let format = "--format=%H%x1f%an%x1f%aI%x1f%s";
    // One more than the page shows: the extra row is how the page knows
    // the history continues without walking all of it, so the cut can
    // say so instead of ending mid-sentence.
    let log = match git_text(
        dir,
        &[
            "log",
            &format!("--max-count={}", COMMIT_PAGE + 1),
            format,
            &oid,
        ],
    ) {
        Ok(text) => text,
        Err(why) => return missing(repo, rev, &why),
    };
    let rows: Vec<&str> = log.lines().collect();
    let truncated = rows.len() > COMMIT_PAGE;

    let mut h = shell(&format!("{repo}: commits"), Bar::repo(repo, rev, site));
    repo_header(&mut h, repo, rev, &oid, "", "commits");
    h.push_str("<section><table><thead><tr><th>commit</th><th>subject</th>");
    h.push_str("<th>author</th><th>when</th></tr></thead><tbody>");
    for line in rows.iter().take(COMMIT_PAGE) {
        let mut fields = line.split('\u{1f}');
        let (Some(id), Some(author), Some(when), Some(subject)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        h.push_str("<tr><td class=\"mono\"><a href=\"/r/");
        h.push_str(&esc(repo));
        h.push_str("/commit/");
        h.push_str(&esc(id));
        h.push_str("\">");
        h.push_str(&esc(&id[..id.len().min(12)]));
        h.push_str("</a></td><td>");
        h.push_str(&esc(subject));
        h.push_str("</td><td class=\"muted\">");
        h.push_str(&esc(author));
        h.push_str("</td><td class=\"muted mono\">");
        h.push_str(&esc(when.split('T').next().unwrap_or(when)));
        h.push_str("</td></tr>");
    }
    h.push_str("</tbody></table>");
    if truncated {
        h.push_str("<p class=\"muted\">… only the latest ");
        h.push_str(&COMMIT_PAGE.to_string());
        h.push_str(" commits are shown. Clone /");
        h.push_str(&esc(repo));
        h.push_str(".git for the full history.</p>");
    }
    h.push_str("</section>");
    Rendered {
        status: 200,
        etag: Some(tag(&oid, "commits")),
        html: close(h),
    }
}

/// One commit, with its diff.
fn commit(dir: &Path, repo: &str, oid: &str, site: Option<&str>) -> Rendered {
    let format = "--format=%H%x1f%an%x1f%aI%x1f%s%x1f%b";
    let header = match git_text(dir, &["show", "--no-patch", format, oid]) {
        Ok(text) => text,
        Err(why) => return missing(repo, oid, &why),
    };
    let mut fields = header.split('\u{1f}');
    let id = fields.next().unwrap_or(oid).trim().to_string();
    let author = fields.next().unwrap_or("").to_string();
    let when = fields.next().unwrap_or("").to_string();
    let subject = fields.next().unwrap_or("").to_string();

    let mut h = shell(
        &format!("{repo}: {}", &id[..id.len().min(12)]),
        Bar::repo(repo, oid, site),
    );
    repo_header(&mut h, repo, &id, &id, "", "commit");
    h.push_str("<section><h2>");
    h.push_str(&esc(&subject));
    h.push_str("</h2><p class=\"muted\">");
    h.push_str(&esc(&author));
    h.push_str(" · ");
    h.push_str(&esc(&when));
    h.push_str("</p>");

    match git_text(dir, &["show", "--numstat", "--patch", "--format=", oid]) {
        Ok(diff) => patch(
            &mut h,
            &diff,
            &format!("Clone /{repo}.git and run git show {id} to read it all."),
        ),
        Err(why) => {
            h.push_str("<p class=\"empty\">");
            h.push_str(&esc(&why));
            h.push_str("</p>");
        }
    }
    h.push_str("</section>");
    Rendered {
        status: 200,
        etag: Some(tag(&id, "commit")),
        html: close(h),
    }
}

/// What one patch line is, decided exactly as the old renderer did:
/// `---`/`+++` file markers stay context so they never wash as changes.
enum LineKind {
    Add,
    Del,
    Hunk,
    File,
    Context,
}

fn line_kind(line: &str) -> LineKind {
    match line.as_bytes().first() {
        Some(b'+') if !line.starts_with("+++") => LineKind::Add,
        Some(b'-') if !line.starts_with("---") => LineKind::Del,
        Some(b'@') => LineKind::Hunk,
        _ if line.starts_with("diff --git") => LineKind::File,
        _ => LineKind::Context,
    }
}

/// One coloured patch line, escaped, class-wrapped, newline-terminated.
fn span(h: &mut String, class: &str, line: &str) {
    h.push_str("<span class=\"");
    h.push_str(class);
    h.push_str("\">");
    h.push_str(&esc(line));
    h.push_str("</span>\n");
}

/// Byte lengths of the common prefix and suffix of two strings, cut on
/// character boundaries, with the suffix measured on what the prefix
/// left — so the two can never overlap and slicing between them is safe.
fn common_affixes(a: &str, b: &str) -> (usize, usize) {
    let prefix = a
        .char_indices()
        .zip(b.char_indices())
        .take_while(|((_, ca), (_, cb))| ca == cb)
        .map(|((i, c), _)| i + c.len_utf8())
        .last()
        .unwrap_or(0);
    let suffix = a[prefix..]
        .chars()
        .rev()
        .zip(b[prefix..].chars().rev())
        .take_while(|(ca, cb)| ca == cb)
        .map(|(c, _)| c.len_utf8())
        .sum();
    (prefix, suffix)
}

/// One side of a modified line pair, with the span that differs from the
/// other side wrapped in `<mark>`.
///
/// The line's wash says "this line moved"; the mark says which words.
/// A pair sharing next to nothing gets no mark — marking a whole
/// rewritten line says nothing the wash does not, and a rewrite that
/// happens to end in the same letter is still a rewrite, so the shared
/// affixes must make up at least a quarter of the longer body before
/// they count as "the same line, edited". The comparison runs on the
/// bodies, past the `-`/`+` sigil, which always differs by design.
fn marked(h: &mut String, class: &str, this: &str, other: &str) {
    let (sigil, body) = this.split_at(1);
    let (_, other_body) = other.split_at(1);
    let (prefix, suffix) = common_affixes(body, other_body);
    let mid = &body[prefix..body.len() - suffix];
    if mid.is_empty() || (prefix + suffix) * 4 < body.len().max(other_body.len()) {
        span(h, class, this);
        return;
    }
    h.push_str("<span class=\"");
    h.push_str(class);
    h.push_str("\">");
    h.push_str(sigil);
    h.push_str(&esc(&body[..prefix]));
    h.push_str("<mark>");
    h.push_str(&esc(mid));
    h.push_str("</mark>");
    h.push_str(&esc(&body[body.len() - suffix..]));
    h.push_str("</span>\n");
}

/// The per-file counts above a diff, each linking to its file header in
/// the patch below. `-` in either column is git's spelling for binary.
fn stat_row(h: &mut String, files: &[(&str, &str, &str)]) {
    if files.is_empty() {
        return;
    }
    let mut added: u64 = 0;
    let mut removed: u64 = 0;
    for (a, d, _) in files {
        added += a.parse::<u64>().unwrap_or(0);
        removed += d.parse::<u64>().unwrap_or(0);
    }
    h.push_str("<p class=\"muted\">");
    h.push_str(&files.len().to_string());
    h.push_str(if files.len() == 1 {
        " file changed, "
    } else {
        " files changed, "
    });
    h.push_str("<span class=\"plus\">+");
    h.push_str(&added.to_string());
    h.push_str("</span> <span class=\"minus\">−");
    h.push_str(&removed.to_string());
    h.push_str("</span></p><table class=\"stat\"><tbody>");
    for (n, (a, d, path)) in files.iter().enumerate() {
        h.push_str("<tr><td class=\"mono\"><a href=\"#f");
        h.push_str(&n.to_string());
        h.push_str("\">");
        h.push_str(&esc(path));
        h.push_str("</a></td>");
        if *a == "-" || *d == "-" {
            h.push_str("<td class=\"num muted\" colspan=\"2\">binary</td>");
        } else {
            h.push_str("<td class=\"num plus\">+");
            h.push_str(a);
            h.push_str("</td><td class=\"num minus\">−");
            h.push_str(d);
            h.push_str("</td>");
        }
        h.push_str("</tr>");
    }
    h.push_str("</tbody></table>");
}

/// Renders `--numstat --patch` output: the stat block becomes the table
/// above the diff, the diff is coloured by line kind, runs of removed
/// lines followed by an equal run of added lines get word-level marks,
/// and the length bound ends in a line that says what it cut.
///
/// Shared by the commit page and the review page so a diff cannot come
/// to mean two different things depending on which one a reader opened.
/// `where_else` names where the rest lives when the bound cuts; it
/// carries a repository name, so it is data and is escaped here.
fn patch(h: &mut String, output: &str, where_else: &str) {
    let lines: Vec<&str> = output.lines().collect();
    // The numstat block: `<added> TAB <removed> TAB <path>` per file.
    // The first line that is not shaped like that starts the patch.
    let mut files: Vec<(&str, &str, &str)> = Vec::new();
    let mut body = 0;
    while body < lines.len() {
        let mut fields = lines[body].splitn(3, '\t');
        match (fields.next(), fields.next(), fields.next()) {
            (Some(a), Some(d), Some(path))
                if !path.is_empty()
                    && [a, d].iter().all(|n| {
                        *n == "-" || (!n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
                    }) =>
            {
                files.push((a, d, path));
                body += 1;
            }
            _ => break,
        }
    }
    while body < lines.len() && lines[body].is_empty() {
        body += 1;
    }
    stat_row(h, &files);

    let patch_lines = &lines[body..];
    let total = patch_lines.len();
    let shown = total.min(MAX_DIFF_LINES);
    h.push_str("<pre class=\"code diff\">");
    let mut file_no = 0usize;
    let mut i = 0;
    while i < shown {
        let line = patch_lines[i];
        match line_kind(line) {
            LineKind::Del => {
                // The run of deletions, and the run of additions
                // directly under it. Equal runs are a modification and
                // pair line-for-line; anything else renders plain.
                let mut j = i;
                while j < shown && matches!(line_kind(patch_lines[j]), LineKind::Del) {
                    j += 1;
                }
                let mut k = j;
                while k < shown && matches!(line_kind(patch_lines[k]), LineKind::Add) {
                    k += 1;
                }
                if k - j == j - i {
                    for p in 0..(j - i) {
                        marked(h, "del", patch_lines[i + p], patch_lines[j + p]);
                    }
                    for p in 0..(j - i) {
                        marked(h, "add", patch_lines[j + p], patch_lines[i + p]);
                    }
                } else {
                    for line in &patch_lines[i..j] {
                        span(h, "del", line);
                    }
                    for line in &patch_lines[j..k] {
                        span(h, "add", line);
                    }
                }
                i = k;
                continue;
            }
            LineKind::Add => span(h, "add", line),
            LineKind::Hunk => span(h, "hunk", line),
            LineKind::File => {
                // The anchor the stat row links to. Counted in patch
                // order, which is the order numstat listed the files in.
                h.push_str("<span class=\"file\" id=\"f");
                h.push_str(&file_no.to_string());
                h.push_str("\">");
                h.push_str(&esc(line));
                h.push_str("</span>\n");
                file_no += 1;
            }
            LineKind::Context => {
                h.push_str(&esc(line));
                h.push('\n');
            }
        }
        i += 1;
    }
    if total > shown {
        h.push_str("\n<span class=\"muted\">… ");
        h.push_str(&(total - shown).to_string());
        h.push_str(" more lines not shown (");
        h.push_str(&total.to_string());
        h.push_str(" in full). ");
        h.push_str(&esc(where_else));
        h.push_str("</span>");
    }
    h.push_str("</pre>");
}

/// The reviews pane: what is in flight on this repository, how close
/// each one is to landing, and how far the world moved under it.
fn reviews_pane(
    h: &mut String,
    dir: &Path,
    repo: &str,
    platform: Option<&crate::platform::Platform>,
) {
    h.push_str("<aside class=\"pane pane-reviews\"><h2>Reviews");
    let Some(platform) = platform else {
        // Not an error, and not this reader's to fix: a node started
        // without `--keys-file` has no platform at all, and the browse
        // surface still works. Saying so beats an empty pane.
        h.push_str(
            "</h2><p class=\"muted\">This node runs without the platform, so it holds no \
             reviews.</p></aside>",
        );
        return;
    };
    let all = platform.reviews_for_repo(repo);
    // Open and settled are different questions, and one total answers
    // neither: a repository with forty archived reviews and nothing in
    // flight is quiet, and a bare "40" says the opposite.
    let (archived, open): (Vec<_>, Vec<_>) = all
        .iter()
        .partition(|(_, r)| r["archived"].as_bool().unwrap_or(false));
    h.push_str("<span class=\"count\">");
    h.push_str(&open.len().to_string());
    h.push_str("</span>");
    if !archived.is_empty() {
        h.push_str("<span class=\"muted\">+ ");
        h.push_str(&archived.len().to_string());
        h.push_str(" archived</span>");
    }
    h.push_str("</h2>");
    if open.is_empty() {
        h.push_str(
            "<p class=\"muted\">Nothing proposes to land here yet. A review names the ref \
                    it targets, and appears in this pane from the moment it is requested.</p>",
        );
        h.push_str("</aside>");
        return;
    }
    h.push_str("<table class=\"flight\"><tbody>");
    for (id, review) in open.iter().take(PANE_ROWS) {
        // Three stacked lines and one number, not four columns. A pane
        // this narrow cannot hold four: measured in a browser, the
        // chips alone claimed the first column and left the identifier
        // wrapping mid-word ("r-" / "notes") with the refname broken
        // over three lines under it.
        h.push_str("<tr><td><a href=\"/r/");
        h.push_str(&esc(repo));
        h.push_str("/review/");
        h.push_str(&esc(id));
        h.push_str("\">");
        h.push_str(&esc(id));
        h.push_str("</a><span class=\"why\">");
        state_tag(h, review);
        // How far the destination has moved since this commit was
        // proposed. Zero renders nothing: the absence of the chip is
        // the good news, and a "0 behind" on every row would train a
        // reader to stop reading the column that matters.
        if let Some(n) = behind(dir, review).filter(|n| *n > 0) {
            h.push_str("<b class=\"tag warn\">\u{2193} ");
            h.push_str(&n.to_string());
            h.push_str(" behind</b>");
        }
        h.push_str("</span><span class=\"why mono muted\">");
        // The destination without the repository it is in. Every row in
        // this pane targets the repository whose page this is, so the
        // prefix is the same eleven characters on every line, in the
        // narrowest column on the page. The full `<repo>:<refname>` is
        // still what the reviews list and the review page show.
        h.push_str(&esc(onto_ref(review)));
        h.push_str("</span></td><td class=\"num muted\">");
        // Answered out of assigned, the one number that says how close
        // this is to landing. An unassigned review says so instead of
        // rendering "0 of 0", which reads as stalled rather than new.
        let assigned = review["reviewers"].as_array().cloned().unwrap_or_default();
        if assigned.is_empty() {
            h.push_str("unassigned");
        } else {
            let answered = assigned
                .iter()
                .filter_map(serde_json::Value::as_str)
                .filter(|who| review["verdicts"][who]["verdict"].as_str().is_some())
                .count();
            h.push_str(&answered.to_string());
            h.push_str(" of ");
            h.push_str(&assigned.len().to_string());
        }
        h.push_str("</td></tr>");
    }
    h.push_str("</tbody></table><p class=\"more\"><a href=\"/r/");
    h.push_str(&esc(repo));
    h.push_str("/reviews\">");
    // The link names what it holds when the pane is not showing all of
    // it, because "All reviews" beside six rows reads as "these six".
    if open.len() > PANE_ROWS {
        h.push_str("All ");
        h.push_str(&all.len().to_string());
        h.push_str(" reviews");
    } else {
        h.push_str("All reviews");
    }
    h.push_str("</a></p></aside>");
}

/// A review's destination refname, with the repository it names
/// stripped — for surfaces that are already inside that repository.
///
/// Falls back to the whole string rather than to nothing: a `target_ref`
/// that does not split is malformed, and showing it is how someone finds
/// out.
fn onto_ref(review: &serde_json::Value) -> &str {
    let onto = ref_name(review);
    onto.split_once(':').map_or(onto, |(_, refname)| refname)
}

/// How many commits the destination ref holds that the proposed commit
/// does not — the "behind" count, asked of git rather than guessed.
///
/// `None` when the question does not apply or cannot be answered: a
/// review naming no commit, no destination ref, or a ref that no longer
/// resolves. Those are different situations, and none of them is
/// "0 behind"; returning a number for any of them would state a
/// relationship this node never checked.
fn behind(dir: &Path, review: &serde_json::Value) -> Option<u64> {
    let commit = git_oid_of(review["target"].as_str()?)?;
    let (_, refname) = ref_name(review).split_once(':')?;
    // A refname arrives inside a signed op, so it is not attacker-chosen
    // in the usual sense — but it reaches a subprocess argument list, and
    // one beginning with `-` is a git option rather than a revision.
    if refname.is_empty() || refname.starts_with('-') {
        return None;
    }
    let range = format!("{commit}..{refname}");
    git_text(dir, &["rev-list", "--count", &range])
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Every review proposing to land on this repository.
fn reviews(
    repo: &str,
    platform: Option<&crate::platform::Platform>,
    site: Option<&str>,
) -> Rendered {
    let Some(platform) = platform else {
        return unavailable(repo);
    };
    let rows = platform.reviews_for_repo(repo);
    // Reviewer seats are channels, and a channel is an opaque handle
    // after D46, so the list resolves them the same way the D28 page
    // does. This page carries no `ETag`, so unlike that one it has no
    // cache identity to fold the store generation into.
    let (_, roster) = platform.roster();

    let mut h = shell(&format!("{repo}: reviews"), Bar::repo(repo, "HEAD", site));
    h.push_str("<header class=\"top\"><h1><a href=\"/r/");
    h.push_str(&esc(repo));
    h.push_str("\">");
    h.push_str(&esc(repo));
    h.push_str("</a></h1><div class=\"sub\"><span class=\"pill\">");
    h.push_str(&rows.len().to_string());
    h.push_str(" reviews</span><span class=\"pill\"><a href=\"/r/\">all repositories</a></span>");
    h.push_str("</div></header><main id=\"main\"><section>");
    if rows.is_empty() {
        h.push_str(
            "<p class=\"lede\">No review proposes to land on this repository. Reviews are \
             listed here by the ref they target, so one that named no destination ref will \
             not appear even though it exists.</p>",
        );
        crate::ui::next_action(
            &mut h,
            "Request one with <code>choir review &lt;api&gt; &lt;key-file&gt; &lt;channel&gt; \
             &lt;id&gt; &lt;git-oid&gt; --ref &lt;repo&gt;:&lt;refname&gt;</code>. Name no \
             reviewers and this node draws them for you.",
        );
    } else {
        h.push_str("<table><thead><tr><th>review</th><th>onto</th><th>state</th>");
        h.push_str("<th class=\"num\">weight</th><th>verdicts</th></tr></thead><tbody>");
        for (id, review) in &rows {
            h.push_str("<tr><td><a href=\"/r/");
            h.push_str(&esc(repo));
            h.push_str("/review/");
            h.push_str(&esc(id));
            h.push_str("\">");
            h.push_str(&esc(id));
            h.push_str("</a></td><td class=\"mono muted\">");
            h.push_str(&esc(ref_name(review)));
            h.push_str("</td><td>");
            state_tag(&mut h, review);
            h.push_str("</td><td class=\"num\">");
            h.push_str(&esc(&review["approval_weight"].to_string()));
            h.push_str("</td><td>");
            // Who was asked and what they said, one click away without
            // leaving the list: a glance is not worth a page navigation.
            // The summary is the answered/assigned count, so the closed
            // state already says how far along the review is.
            let assigned = review["reviewers"].as_array().cloned().unwrap_or_default();
            if assigned.is_empty() {
                h.push_str("<span class=\"muted\">unassigned</span>");
            } else {
                let answered = assigned
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .filter(|who| review["verdicts"][who]["verdict"].as_str().is_some())
                    .count();
                h.push_str("<details><summary>");
                h.push_str(&answered.to_string());
                h.push_str(" of ");
                h.push_str(&assigned.len().to_string());
                h.push_str("</summary>");
                crate::ui::verdicts(&mut h, review, &roster);
                h.push_str("</details>");
            }
            h.push_str("</td></tr>");
        }
        h.push_str("</tbody></table>");
    }
    h.push_str("</section>");
    Rendered {
        status: 200,
        etag: None,
        html: close(h),
    }
}

/// One review: the proposal, the people, and the diff between what is
/// proposed and where it would land.
///
/// The diff is three-dot on purpose — what the proposal adds *since it
/// diverged*, not every difference between two branches. A two-dot diff
/// on a target ref that has moved shows the reviewer other people's
/// landed work as though the author wrote it, which is the single most
/// misleading thing a review surface can do.
fn review(
    dir: &Path,
    repo: &str,
    id: &str,
    platform: Option<&crate::platform::Platform>,
    user: &str,
    browser_writes: bool,
    site: Option<&str>,
) -> Rendered {
    let Some(platform) = platform else {
        return unavailable(repo);
    };
    let Some(state) = platform.review_json(id) else {
        return no_such_review(repo, id);
    };
    let commit_oid = git_oid_of(state["target"].as_str().unwrap_or("")).unwrap_or_default();
    // Reviewer seats and comment authors are channels, and a channel is
    // an opaque handle after D46. This page is where a person reads who
    // said what, so it resolves them the same way the D28 page does.
    let (_, roster) = platform.roster();

    let mut h = shell(
        &format!("{repo}: review {id}"),
        Bar::repo(repo, "HEAD", site),
    );
    h.push_str("<header class=\"top\"><h1><a href=\"/r/");
    h.push_str(&esc(repo));
    h.push_str("\">");
    h.push_str(&esc(repo));
    h.push_str("</a> · review ");
    h.push_str(&esc(id));
    h.push_str("</h1><div class=\"sub\">");
    state_tag(&mut h, &state);
    h.push_str("<span class=\"pill\">weight ");
    h.push_str(&esc(&state["approval_weight"].to_string()));
    h.push_str("</span><span class=\"pill\"><a href=\"/r/");
    h.push_str(&esc(repo));
    h.push_str("/reviews\">all reviews</a></span>");
    h.push_str("</div></header><main id=\"main\">");

    // The relationship: what lands where, which is the thing reviews
    // have always carried and never shown in one place.
    h.push_str("<section><h2>Proposal</h2><table><tbody>");
    h.push_str("<tr><td>commit</td><td class=\"mono\"><a href=\"/r/");
    h.push_str(&esc(repo));
    h.push_str("/commit/");
    h.push_str(&esc(&commit_oid));
    h.push_str("\">");
    h.push_str(&esc(&short_oid(&commit_oid)));
    h.push_str("</a></td></tr><tr><td>onto</td><td class=\"mono\">");
    h.push_str(&esc(ref_name(&state)));
    h.push_str("</td></tr>");
    if state["re_review_required"].as_bool().unwrap_or(false) {
        h.push_str("<tr><td>re-review</td><td><b class=\"tag warn\">required</b></td></tr>");
    }
    h.push_str("</tbody></table></section>");

    // Who was asked, and what they said. Verdict notes are the closest
    // thing the log has to discussion today.
    h.push_str("<section><h2>Reviewers</h2>");
    let reviewers = state["reviewers"].as_array().cloned().unwrap_or_default();
    if reviewers.is_empty() {
        // Not a cosmetic gap. An unassigned review can never read as
        // approved, so a reader waiting on this one is waiting on
        // nothing, and the page has to say so rather than look pending.
        h.push_str(
            "<p class=\"lede\">Nobody is assigned. This review cannot be approved in this \
             state — an unassigned review never counts as approved, however many verdicts \
             arrive.</p>",
        );
        crate::ui::next_action(
            &mut h,
            "Ask for the draw: a <code>RequestReview</code> naming no reviewers is answered \
             by this node with an assignment. On a node started <code>--require-assignment</code> \
             that is the only way a reviewer list is ever set.",
        );
    } else {
        h.push_str("<table><thead><tr><th>reviewer</th><th>verdict</th><th>note</th>");
        h.push_str("</tr></thead><tbody>");
        for who in reviewers.iter().filter_map(serde_json::Value::as_str) {
            let verdict = &state["verdicts"][who];
            let slashed = state["slashes"]
                .get(who)
                .and_then(serde_json::Value::as_str);
            h.push_str("<tr><td class=\"mono\">");
            h.push_str(&esc(&crate::ui::person(who, &roster)));
            h.push_str("</td><td>");
            match verdict["verdict"].as_str() {
                Some("Approve") => h.push_str("<b class=\"tag ok\">approve</b>"),
                Some("RequestChanges") => h.push_str("<b class=\"tag danger\">changes</b>"),
                Some(other) => h.push_str(&esc(other)),
                None => h.push_str("<b class=\"tag pending\">waiting</b>"),
            }
            if let Some(reason) = slashed {
                h.push_str(" <b class=\"tag danger\">slashed</b> <span class=\"muted\">");
                h.push_str(&esc(reason));
                h.push_str("</span>");
            }
            h.push_str("</td><td>");
            h.push_str(&esc(verdict["note"].as_str().unwrap_or("")));
            h.push_str("</td></tr>");
        }
        h.push_str("</tbody></table>");
    }
    h.push_str("</section>");

    // The discussion (D38). Rendered, never accepted: a comment is a
    // signed operation, so the only way one reaches the log is the
    // submit endpoint. This surface stays read-only, exactly as D34
    // built it.
    h.push_str("<section><h2>Discussion</h2>");
    let comments = state["comments"].as_array().cloned().unwrap_or_default();
    let archived = state["archived"].as_bool().unwrap_or(false);
    if comments.is_empty() {
        if archived {
            // Saying "no comments" here would be a claim the node cannot
            // support: archiving drops the thread with the verdicts, so
            // an emptied review and a review nobody discussed look
            // identical from the view. Report the one that is known.
            h.push_str("<p class=\"empty\">This review was archived; its discussion was ");
            h.push_str("dropped with its verdicts. The log still holds every comment.</p>");
        } else {
            h.push_str("<p class=\"empty\">Nothing said yet.</p>");
        }
    } else {
        h.push_str("<table><thead><tr><th>op</th><th>author</th><th>comment</th>");
        h.push_str("</tr></thead><tbody>");
        for comment in &comments {
            h.push_str("<tr><td class=\"num mono muted\">");
            h.push_str(&esc(&comment["at"].to_string()));
            h.push_str("</td><td class=\"mono\">");
            h.push_str(&esc(&crate::ui::person(
                comment["author"].as_str().unwrap_or(""),
                &roster,
            )));
            h.push_str("</td><td>");
            h.push_str(&esc(comment["body"].as_str().unwrap_or("")));
            h.push_str("</td></tr>");
        }
        h.push_str("</tbody></table>");
    }
    h.push_str("</section>");

    // The diff, against wherever the target ref points right now.
    h.push_str("<section><h2>Changes</h2>");
    let onto = ref_name(&state);
    let base = onto
        .split_once(':')
        .map(|(_, refname)| refname.to_string())
        .filter(|refname| !refname.is_empty());
    match (commit_oid.is_empty(), base) {
        (true, _) => h.push_str("<p class=\"empty\">This review names no commit.</p>"),
        (false, None) => {
            h.push_str("<p class=\"note\">This review names no destination ref, so there is ");
            h.push_str("nothing to diff it against. The commit itself is linked above.</p>");
        }
        (false, Some(refname)) => {
            let range = format!("{refname}...{commit_oid}");
            match git_text(dir, &["diff", "--numstat", "--patch", &range]) {
                Ok(diff) if diff.trim().is_empty() => {
                    h.push_str("<p class=\"empty\">Nothing to land: the destination already ");
                    h.push_str("contains this commit.</p>");
                }
                Ok(diff) => patch(
                    &mut h,
                    &diff,
                    &format!("Clone /{repo}.git and run git diff {range} to read it all."),
                ),
                Err(why) => {
                    h.push_str("<p class=\"note\">");
                    h.push_str(&esc(&why));
                    h.push_str("</p>");
                }
            }
        }
    }
    h.push_str("</section>");

    if browser_writes {
        write_sections(&mut h, id, &state, user);
    } else {
        h.push_str("<section><h2>Writes</h2><p class=\"note\">This beta keeps browser access read-only. Use the signed CLI for verdicts and comments.</p></section>");
    }
    Rendered {
        status: 200,
        etag: None,
        html: close(h),
    }
}

/// Both write affordances, and the one `<script>` element that serves
/// them — emitted only if one of them rendered.
///
/// A reader with no verdict to cast and no right to comment gets the
/// page they always got, script in neither sense: no element and no
/// request. That is the read surface's guarantee, and it is a function
/// rather than four lines inside `review` so a test can hold it without
/// standing up a platform.
fn write_sections(h: &mut String, id: &str, state: &serde_json::Value, user: &str) {
    let before = h.len();
    verdict_buttons(h, id, state, user);
    comment_box(h, id, state, user);
    if h.len() > before {
        h.push_str(crate::ui::CEREMONY_SCRIPT);
    }
}

/// The passkey write affordance (D39), and the one place this repository
/// runs script in a page.
///
/// **Everything above this call renders identically without it.** D28's
/// rule was "no JavaScript"; D39 reverses that in one narrow place
/// because a browser write the node cannot forge requires the browser to
/// *produce a signature*, and an HTML form submits values rather than
/// computing them. The reversal buys exactly one thing and must keep
/// buying only that: the read surface is unchanged, the script is
/// [`crate::ui::WEBAUTHN_JS`] and nothing else, nothing is fetched off
/// this origin, and with scripting off the section below is a sentence
/// naming the CLI rather than a broken control.
///
/// The page does not know the op format. The node renders the two
/// payloads and their challenges; the script's whole job is to hand a
/// challenge to the authenticator and post what comes back. That is what
/// keeps this forty lines instead of a second implementation of a hashed
/// format in a language with no tests here.
fn verdict_buttons(h: &mut String, id: &str, state: &serde_json::Value, user: &str) {
    // Only a reviewer on this review has a verdict to cast. Showing the
    // buttons to anyone else would be offering an action the node will
    // refuse, which is worse than not offering it.
    let is_reviewer = state["reviewers"]
        .as_array()
        .is_some_and(|list| list.iter().any(|r| r.as_str() == Some(user)));
    if !is_reviewer || state["archived"].as_bool().unwrap_or(false) {
        return;
    }
    // Already answered: the log keeps the first verdict, so a second
    // button press would be refused. Say so instead of offering it.
    if state["verdicts"][user]["verdict"].as_str().is_some() {
        return;
    }

    h.push_str("<section><h2>Your verdict</h2>");
    h.push_str(
        "<noscript><p class=\"note\">Casting a verdict needs a passkey, which the \
                browser can only produce with scripting enabled. With it off, use \
                <code>choir verdict</code> — the CLI is the write path this page is an \
                alternative to, never a replacement for.</p></noscript>",
    );
    // The channel travels on the element rather than in the script, so
    // the script is one file with nothing interpolated into it — the one
    // property that makes "is this page's script safe" a question you
    // answer once instead of per render, and the reason it can be a
    // shared resource at all.
    h.push_str("<div id=\"verdict\" hidden data-user=\"");
    h.push_str(&esc(user));
    h.push_str(
        "\"><p class=\"note\">Signed by your passkey on this device. Nothing is sent \
                until you approve the prompt.</p>",
    );
    for (verdict, label, class) in [
        ("Approve", "Approve", "ok"),
        ("RequestChanges", "Request changes", "danger"),
    ] {
        let Some(prepared) = crate::prepare::verdict(id, user, verdict) else {
            continue;
        };
        h.push_str("<button class=\"verdict ");
        h.push_str(class);
        h.push_str("\" data-payload=\"");
        h.push_str(&esc(&prepared.payload_hex));
        h.push_str("\" data-challenge=\"");
        h.push_str(&esc(&prepared.challenge));
        h.push_str("\">");
        h.push_str(label);
        h.push_str("</button> ");
    }
    h.push_str("<p id=\"verdict-said\" class=\"note\" hidden></p></div>");
    h.push_str("</section>");
}

/// The comment box (D39 × D38), offered to anyone who may write here
/// rather than to reviewers only.
///
/// Discussion is deliberately wider than judgement: a verdict is an
/// authorization and belongs to the people asked for one, while a comment
/// is a statement and belongs to anyone the ACL already lets write to
/// this repository. The node makes that call at `/api/submit`; the page
/// offering the box to a reader who is then refused would be worse than
/// either, so it is shown only to a caller with a name — never to `anon`.
///
/// Unlike a verdict, the payload cannot be rendered ahead of time: it
/// carries text nobody has typed yet. So this posts to `/api/prepare`
/// first and signs what comes back, which keeps the op format in one
/// implementation exactly as the verdict path does.
fn comment_box(h: &mut String, id: &str, state: &serde_json::Value, user: &str) {
    if user.is_empty() || user == "anon" || state["archived"].as_bool().unwrap_or(false) {
        return;
    }
    h.push_str("<section><h2>Say something</h2>");
    h.push_str(
        "<noscript><p class=\"note\">Commenting signs an operation with your passkey, \
                which needs scripting. With it off, use <code>choir comment</code>.</p></noscript>",
    );
    h.push_str("<div id=\"comment\" hidden data-review=\"");
    h.push_str(&esc(id));
    h.push_str("\" data-user=\"");
    h.push_str(&esc(user));
    h.push_str(
        "\"><textarea id=\"comment-body\" rows=\"3\" maxlength=\"4096\" \
                placeholder=\"What do you make of it?\"></textarea>",
    );
    h.push_str("<p><button id=\"comment-go\">Sign and post</button></p>");
    h.push_str("<p id=\"comment-said\" class=\"note\" hidden></p></div>");
    h.push_str("</section>");
}

/// The git object id inside a view hash, or `None` when the hash is not
/// a git object at all.
///
/// Hashes in the log are self-describing (invariant 2), and a review may
/// legitimately name a BLAKE3 target that git has never heard of. Reading
/// the codec rather than stripping the prefix is what keeps such a target
/// off a subprocess command line, and lets the page say so instead of
/// showing git's confusion.
fn git_oid_of(target: &str) -> Option<String> {
    let (codec, digest) = target.split_once('-')?;
    // 0x11 is git SHA-1 and 0x12 git SHA-256, per the multicodec table
    // `ContentHash::from_git_oid` writes.
    let git = matches!(codec, "11" | "12");
    (git && matches!(digest.len(), 40 | 64) && digest.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| digest.to_ascii_lowercase())
}

/// A review's destination ref, or the empty string.
fn ref_name(review: &serde_json::Value) -> &str {
    review["target_ref"].as_str().unwrap_or("")
}

/// Shortens an oid for display without pretending it is a codec-tagged
/// hash, which [`short`] would.
///
/// [`short`]: crate::ui
fn short_oid(oid: &str) -> String {
    oid.chars().take(12).collect()
}

/// The one-word state of a review, as a tag.
fn state_tag(h: &mut String, review: &serde_json::Value) {
    let approved = review["approved"].as_bool().unwrap_or(false);
    let complete = review["complete"].as_bool().unwrap_or(false);
    let archived = review["archived"].as_bool().unwrap_or(false);
    let (class, word) = match (archived, approved, complete) {
        (true, true, _) => ("ok", "archived approved"),
        (true, false, _) => ("muted", "archived"),
        (false, true, _) => ("ok", "approved"),
        (false, false, true) => ("danger", "changes requested"),
        (false, false, false) => ("pending", "open"),
    };
    h.push_str("<b class=\"tag ");
    h.push_str(class);
    h.push_str("\">");
    h.push_str(word);
    h.push_str("</b>");
}

/// The page for a review request on a node with no platform enabled.
fn unavailable(repo: &str) -> Rendered {
    Rendered {
        status: 503,
        etag: None,
        html: crate::ui::refusal(
            "Reviews are not enabled here",
            503,
            &crate::ui::Refusal {
                code: "platform_disabled",
                error: "This node serves git, but its platform API is switched off, so it holds \
                        no reviews to show you. Nothing is broken and nothing was lost.",
                expected: Some("a node started with the platform API enabled"),
                actual: Some("a git-only node"),
                next: "Browse the code instead — the link above works. Reviews appear here only \
                       once the operator restarts this node with the platform API on.",
            },
            &[
                (&format!("/r/{repo}"), "this repository"),
                ("/r/", "all repositories"),
            ],
        ),
    }
}

/// The page for a repository this credential cannot be shown.
///
/// One body for two situations, which is the whole point: a repository
/// that does not exist and one the reader holds no grant on must be
/// indistinguishable, so there is a single function rather than two that
/// happen to agree today. Every word has to be true in both worlds, so
/// nothing echoes the name that was asked for and the next action is one
/// that works either way.
///
/// The alternative — letting a missing repository fall through to the
/// pages that read it — is how git's own "not a git repository:
/// '/srv/…'" ended up on a `404` a stranger could ask for.
pub(crate) fn no_such_repository() -> Rendered {
    Rendered {
        status: 404,
        etag: None,
        html: crate::ui::refusal(
            "No repository here",
            404,
            &crate::ui::Refusal {
                code: "no_such_repository",
                error: "Nothing readable by this credential is at that address. A repository \
                        that does not exist and one you were not granted look identical from \
                        here, on purpose — a credential is never told what it cannot read.",
                expected: Some("a repository this credential holds a read grant on"),
                actual: None,
                next: "Open the repository list — it names every repository this credential \
                       can read, and following a link from it always works. If what you \
                       wanted is missing, ask the operator for a read grant by name.",
            },
            &[
                ("/", "repositories you can read"),
                ("/status", "node state"),
            ],
        ),
    }
}

/// The page for a repository with no commits yet.
///
/// Deliberately a `200`: the repository is exactly what the operator
/// asked for, and the only thing missing is a push. Saying so beats
/// reporting git's "Needed a single revision", which reads like a fault.
fn empty(repo: &str) -> Rendered {
    // A repository with no commits has nothing to search, so the box
    // falls back to the list rather than offering an empty tree.
    let mut h = shell(&format!("{repo}: empty"), Bar::index());
    h.push_str("<header class=\"top\"><h1>");
    h.push_str(&esc(repo));
    h.push_str("</h1><div class=\"sub\"><span class=\"pill\">empty</span>");
    h.push_str("<span class=\"pill\"><a href=\"/r/\">all repositories</a></span>");
    h.push_str("</div></header><main id=\"main\"><section>");
    h.push_str(
        "<p class=\"lede\">No commits yet. This repository exists and you may read it; \
                nobody has pushed to it.</p>",
    );
    crate::ui::next_action(
        &mut h,
        "Clone it, commit, and push — <code>git push origin HEAD:main</code>. The tree, the \
         history and the diffs all appear here on the first push.",
    );
    h.push_str("</section>");
    Rendered {
        status: 200,
        etag: None,
        html: close(h),
    }
}

/// The page for a revision that does not resolve.
///
/// A `404` carrying git's own words in `found`, because "no such
/// revision" and "not a tree object" send a reader to different fixes,
/// and flattening both into "not found" costs them the difference.
///
/// The reader is told plainly that the *repository* was readable, which
/// is the one thing this page can say without breaking D29: they already
/// hold the grant, so confirming it leaks nothing, and it separates "you
/// typed the branch wrong" from "you are not allowed here" — two states
/// that otherwise look identical and have opposite fixes.
fn missing(repo: &str, rev: &str, why: &str) -> Rendered {
    Rendered {
        status: 404,
        etag: None,
        html: crate::ui::refusal(
            &format!("{repo}: no such revision"),
            404,
            &crate::ui::Refusal {
                code: "no_such_revision",
                error: "You may read this repository, but nothing in it resolves to what the \
                        address asked for.",
                expected: Some("a branch, a tag, or a full object id this repository holds"),
                actual: Some(&format!("{rev} — git says: {why}")),
                next: "Open the repository above and read the branch names off its front page. \
                       A shortened object id will not work here; browsing wants the whole one.",
            },
            &[
                (&format!("/r/{repo}"), "this repository"),
                ("/r/", "all repositories"),
            ],
        ),
    }
}

/// The page for a review id the view does not carry.
///
/// Separate from [`missing`] because a review id is not a revision: the
/// fix is a different command, and telling somebody to check their branch
/// names when they mistyped a review id wastes the trip.
fn no_such_review(repo: &str, id: &str) -> Rendered {
    Rendered {
        status: 404,
        etag: None,
        html: crate::ui::refusal(
            &format!("{repo}: no such review"),
            404,
            &crate::ui::Refusal {
                code: "no_such_review",
                error: "No review by that id is in this node's view. An id that was never \
                        requested and one whose review was archived away both land here.",
                expected: Some("a review id this node has sequenced"),
                actual: Some(id),
                next: "Open the review list above — it names every review proposing to land on \
                       this repository, archived ones included.",
            },
            &[
                (&format!("/r/{repo}/reviews"), "this repository's reviews"),
                (&format!("/r/{repo}"), "this repository"),
            ],
        ),
    }
}

/// Document head and opening tags, shared with the D28 page so the two
/// surfaces cannot drift into looking like different products.
fn shell(title: &str, bar: Bar<'_>) -> String {
    let mut h = String::with_capacity(8 * 1024);
    h.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    h.push_str("<title>");
    h.push_str(&esc(title));
    h.push_str("</title>");
    h.push_str(crate::ui::STYLE);
    h.push_str("</head><body>");
    h.push_str("<a class=\"skip\" href=\"#main\">Skip to content</a>");
    chrome(&mut h, bar);
    h
}

/// What the one fixed search box searches, on the page it is drawn on.
///
/// Carried into [`shell`] rather than drawn per page, because "the same
/// box in the same place on every page" is the whole property: a box
/// that four pages remember to render is a box the fifth page forgets.
#[derive(Clone, Copy)]
pub(crate) struct Bar<'a> {
    /// The repository the box searches and the revision it searches at,
    /// or `None` on a page that is not about one repository — where it
    /// filters the repository list instead.
    pub(crate) scope: Option<(&'a str, &'a str)>,
    /// The term already searched, so the one box shows it rather than a
    /// second box below the results doing that job. Empty everywhere
    /// except the results pages themselves.
    pub(crate) q: &'a str,
    /// Whether this node presents a single repository as its whole site.
    /// Such a node has no repository list, so the box never offers to
    /// search one and the brand goes to the repository instead.
    pub(crate) site: bool,
}

impl<'a> Bar<'a> {
    /// The bar for a page about no particular repository.
    pub(crate) fn index() -> Bar<'a> {
        Bar {
            scope: None,
            q: "",
            site: false,
        }
    }

    /// The bar on the repository index, showing the filter in force.
    fn filtered(q: &'a str) -> Bar<'a> {
        Bar {
            scope: None,
            q,
            site: false,
        }
    }

    /// The bar on a repository search, showing the term in force.
    fn searched(repo: &'a str, rev: &'a str, q: &'a str, site: Option<&str>) -> Bar<'a> {
        Bar {
            scope: Some((repo, rev)),
            q,
            site: site.is_some(),
        }
    }

    /// The bar for a page about one repository at one revision.
    fn repo(repo: &'a str, rev: &'a str, site: Option<&str>) -> Bar<'a> {
        Bar {
            scope: Some((repo, rev)),
            q: "",
            site: site.is_some(),
        }
    }
}

/// The fixed bar every page carries: where you are, and one box.
///
/// It is `position:sticky` rather than a per-page block because the
/// request was for a box that is always in the same place — which is a
/// claim about the *viewport*, not about the document. Everything in it
/// is a link or a form control, so it costs no script.
pub(crate) fn chrome(h: &mut String, bar: Bar<'_>) {
    h.push_str("<div class=\"chrome\"><div class=\"chrome-in\">");

    // Home. On a single-repository node that is the repository itself,
    // because there is no list to go back to.
    h.push_str("<a class=\"brand\" href=\"");
    h.push_str(if bar.site { "/" } else { "/r/" });
    h.push_str("\">");
    match (bar.site, bar.scope) {
        (true, Some((repo, _))) => h.push_str(&esc(repo)),
        _ => h.push_str("choir"),
    }
    h.push_str("</a>");

    // The box, scoped to whatever this page is about. The scope is shown
    // *inside* the box rather than implied by the page around it: a
    // reader typing into a box that is sometimes global and sometimes
    // not has to be told which one this is, every time.
    let (action, scope_label, placeholder) = match bar.scope {
        Some((repo, rev)) => (
            format!("/r/{repo}/search/{}", url_path(rev)),
            repo.to_string(),
            "files, code and commits",
        ),
        None => ("/r/".to_string(), "all repositories".into(), "repositories"),
    };
    h.push_str("<form class=\"omni\" method=\"get\" action=\"");
    h.push_str(&esc(&action));
    h.push_str("\" role=\"search\"><span class=\"scope\">");
    h.push_str(&esc(&scope_label));
    h.push_str(
        "</span><input type=\"search\" name=\"q\" autocomplete=\"off\" \
                 aria-label=\"Search ",
    );
    h.push_str(&esc(&scope_label));
    h.push_str("\" placeholder=\"Search ");
    h.push_str(placeholder);
    h.push_str("\" value=\"");
    h.push_str(&esc(bar.q));
    h.push_str("\">");
    // The tree search needs a scope; the repository filter does not, and
    // a stray `in=` on that URL would be noise a reader has to read.
    if bar.scope.is_some() {
        h.push_str("<input type=\"hidden\" name=\"in\" value=\"files\">");
    }
    h.push_str("</form>");

    // The way out of a repository, which is the one navigation a reader
    // cannot perform from the page body once they are deep in a tree.
    h.push_str("<nav class=\"chrome-nav\">");
    if bar.scope.is_some() && !bar.site {
        h.push_str("<a href=\"/r/\">repositories</a>");
    }
    h.push_str("<a href=\"/status\">node</a>");
    h.push_str("</nav></div></div>");
}

/// Repository name, revision, and the breadcrumb back up the tree.
fn repo_header(h: &mut String, repo: &str, rev: &str, oid: &str, path: &str, here: &str) {
    h.push_str("<header class=\"top\"><h1><a href=\"/r/");
    h.push_str(&esc(repo));
    h.push_str("\">");
    h.push_str(&esc(repo));
    h.push_str("</a></h1><div class=\"sub\">");
    h.push_str("<span class=\"pill\">");
    h.push_str(&esc(rev));
    h.push_str("</span><span class=\"pill mono\">");
    h.push_str(&esc(&oid[..oid.len().min(12)]));
    h.push_str("</span>");
    if here != "commits" {
        h.push_str("<span class=\"pill\"><a href=\"/r/");
        h.push_str(&esc(repo));
        h.push_str("/commits/");
        h.push_str(&esc(rev));
        h.push_str("\">history</a></span>");
    }
    h.push_str("<span class=\"pill\"><a href=\"/r/");
    h.push_str(&esc(repo));
    h.push_str("/reviews\">reviews</a></span>");
    // No "all repositories" pill: the fixed bar carries that link on
    // every page, and two links to one list in one header is noise.
    // The path a reader clones, which nothing on this surface showed. It
    // is also the other half of the node's two names for one repository:
    // this page is `/r/<repo>` and the clone is `/<repo>.git`, and a
    // reader who only ever saw one of them had to guess the other.
    //
    // Relative, with no scheme or host: whatever origin the reader is
    // already on is the right one, and it is the only one this process
    // can state without being told what proxy sits in front of it.
    h.push_str("<span class=\"pill mono\">clone /");
    h.push_str(&esc(repo));
    h.push_str(".git</span>");
    h.push_str("</div>");
    if !path.is_empty() {
        h.push_str("<nav class=\"crumbs\"><a href=\"/r/");
        h.push_str(&esc(repo));
        h.push_str("/tree/");
        h.push_str(&esc(rev));
        h.push_str("\">root</a>");
        let mut walked = String::new();
        let count = path.split('/').count();
        for (n, segment) in path.split('/').enumerate() {
            if !walked.is_empty() {
                walked.push('/');
            }
            walked.push_str(segment);
            h.push_str(" / ");
            // The last crumb is where the reader already is.
            if n + 1 == count {
                h.push_str(&esc(segment));
            } else {
                h.push_str("<a href=\"/r/");
                h.push_str(&esc(repo));
                h.push_str("/tree/");
                h.push_str(&esc(rev));
                h.push('/');
                h.push_str(&esc(&url_path(&walked)));
                h.push_str("\">");
                h.push_str(&esc(segment));
                h.push_str("</a>");
            }
        }
        h.push_str("</nav>");
    }
    h.push_str("</header><main id=\"main\">");
}

/// Closing tags and the same footer promise the D28 page makes.
fn close(mut h: String) -> String {
    // Not "read-only" any more, and the merge is where that became
    // false: the review page now carries D39's verdict and comment
    // controls, so a footer under them claiming the surface takes no
    // writes contradicts the buttons directly above it. The half that is
    // still true is the half worth keeping — those buttons do not bypass
    // the signed-op API, they use it. The D28 node page keeps the full
    // sentence, because that page really does offer nothing.
    h.push_str("</main><footer>Every write here is a signed operation.</footer>");
    h.push_str("</body></html>");
    h
}

/// The `ETag` for content at one commit oid.
///
/// Weak, and scoped by what the page is about as well as the commit, so
/// two pages of the same tree never share a tag.
fn tag(oid: &str, what: &str) -> String {
    tag_for(crate::BUILD_COMMIT, oid, what)
}

/// The same, with the build stamp passed in.
///
/// Split out only so a test can vary the stamp. It cannot be varied any
/// other way — [`crate::BUILD_COMMIT`] is a compile-time constant, so an
/// end-to-end test would need two builds of this binary to observe the
/// property, and would silently pass on one.
fn tag_for(build: &str, oid: &str, what: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    // The build is part of the cache identity, not just the content. An
    // `ETag` answers "is the page I hold still current", and the page is
    // this node's *rendering* of the commit — so a node that renders it
    // differently is serving a different page even though the oid has not
    // moved. Without this, upgrading the daemon leaves every reader who
    // has already visited a blob page on the old HTML for as long as
    // their cache keeps it, because a blob's tag is otherwise the oid and
    // path alone and both are still correct. Not hypothetical: it is how
    // a UI change ships to nobody and reads as the change never landing.
    //
    // The separator matters. Without it the stamp and the path are one
    // byte string, so build `ab` + path `c` and build `a` + path `bc`
    // hash alike — and a rebuild could land on a colliding tag.
    for byte in build
        .as_bytes()
        .iter()
        .chain(b"\x1f")
        .chain(what.as_bytes())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("W/\"{oid}-{hash:x}\"")
}

/// A byte count a person can read at a glance.
fn human(bytes: &str) -> String {
    let Ok(n) = bytes.trim().parse::<u64>() else {
        return bytes.trim().to_string();
    };
    if n < 1024 {
        return format!("{n} B");
    }
    if n < 1024 * 1024 {
        return format!("{:.1} KiB", n as f64 / 1024.0);
    }
    format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
}

/// A count with thousands separators, so `1298` reads as `1,298`.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `1 branch`, `8 branches` — the count and the right noun for it.
fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// git's failures are shown to a reader, and several of them quote
    /// the `--git-dir` they were handed. That path is the node's install
    /// root and the account it runs as; the distinction the message
    /// carries is worth showing and the path is not.
    ///
    /// Asserted on `git` itself rather than on a page, because the page
    /// that used to reach it is now refused earlier — so this is the only
    /// place the scrub can still be made to fail, and every future caller
    /// inherits it.
    /// Two builds rendering one commit must not share a cache entry.
    ///
    /// The end-to-end test beside this one cannot see this: the stamp is
    /// a compile-time constant, so observing it vary would take two
    /// builds of this binary. A mutation removing the stamp from the tag
    /// entirely passed that test and fails this one.
    #[test]
    fn the_build_is_part_of_a_page_tag() {
        let (oid, path) = ("deadbeef", "src/lib.rs");
        assert_ne!(
            tag_for("build-one", oid, path),
            tag_for("build-two", oid, path),
            "two builds of this node share a cache entry for one file, so a reader \
             who visited before an upgrade keeps the old page until their cache expires"
        );
        // Content still has to move the tag, or the line above could be
        // satisfied by a tag that is the build stamp and nothing else.
        assert_ne!(
            tag_for("build-one", oid, "src/lib.rs"),
            tag_for("build-one", oid, "src/main.rs"),
            "two files share a cache entry"
        );
        assert_ne!(
            tag_for("build-one", "aaa", path),
            tag_for("build-one", "bbb", path),
            "two commits share a cache entry"
        );
        // The separator, stated as the collision it prevents: without
        // one, `ab` + `c` and `a` + `bc` are the same byte string.
        assert_ne!(
            tag_for("ab", oid, "c"),
            tag_for("a", oid, "bc"),
            "the stamp and the path run together, so a rebuild can collide"
        );
    }

    #[test]
    fn a_git_failure_never_quotes_the_directory_it_was_handed() {
        let dir = std::env::temp_dir().join("choir-browse-absent-on-purpose/nowhere.git");
        let why = git(&dir, &["rev-parse", "--verify", "HEAD"])
            .expect_err("git cannot resolve a revision in a directory that is not there");
        assert!(
            !why.contains(&*dir.to_string_lossy()),
            "a reader is shown where this node keeps its repositories: {why}"
        );
        assert!(
            why.contains("<repository>"),
            "the path was dropped rather than replaced, so the message lost its subject: {why}"
        );
    }

    /// The routing claim the module doc makes, as an assertion: a git
    /// URL is never a browse URL, including for an owner whose name is
    /// the browse prefix. Getting this wrong takes somebody's clone URL
    /// away, which is worse than the feature is good.
    #[test]
    fn a_git_url_is_never_a_browse_url() {
        for url in [
            "/r/project.git/info/refs?service=git-upload-pack",
            "/r/project.git/git-upload-pack",
            "/owner/repo.git/HEAD",
            "/r/r/deep.git/objects/info/packs",
        ] {
            assert_eq!(route(url), None, "browse claimed a git URL: {url}");
        }
        // ...and an owner named `r` can still be browsed, which is the
        // other half of the same claim.
        assert_eq!(
            route("/r/r/project"),
            Some(Page::Tree {
                repo: "r/project".into(),
                rev: "HEAD".into(),
                path: String::new()
            })
        );
    }

    #[test]
    fn the_documented_shapes_parse() {
        assert_eq!(route("/r"), Some(Page::Index { q: String::new() }));
        assert_eq!(route("/r/"), Some(Page::Index { q: String::new() }));
        assert_eq!(
            route("/r/o/p/tree/main/src/lib"),
            Some(Page::Tree {
                repo: "o/p".into(),
                rev: "main".into(),
                path: "src/lib".into()
            })
        );
        assert_eq!(
            route("/r/o/p/blob/main/src/lib.rs"),
            Some(Page::Blob {
                repo: "o/p".into(),
                rev: "main".into(),
                path: "src/lib.rs".into()
            })
        );
        assert_eq!(
            route("/r/o/p/commits/main"),
            Some(Page::Commits {
                repo: "o/p".into(),
                rev: "main".into()
            })
        );
        let oid = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            route(&format!("/r/o/p/commit/{oid}")),
            Some(Page::Commit {
                repo: "o/p".into(),
                oid: oid.into()
            })
        );
        // A query string is not part of the route.
        assert_eq!(
            route("/r/o/p/tree/main?x=1"),
            Some(Page::Tree {
                repo: "o/p".into(),
                rev: "main".into(),
                path: String::new()
            })
        );
    }

    /// Nothing that could become an option, a traversal or a revision
    /// expression may survive parsing. These are the arguments that
    /// reach a subprocess, so the test is the security boundary.
    #[test]
    fn nothing_dangerous_survives_parsing() {
        for url in [
            // Option injection.
            "/r/o/p/tree/--upload-pack=id",
            "/r/o/p/blob/main/-rf",
            // Traversal, encoded and plain.
            "/r/o/p/blob/main/../../../etc/passwd",
            "/r/o/p/blob/main/%2e%2e/%2e%2e/etc/passwd",
            "/r/../../etc/blob/main/x",
            // A decoded slash would invent a segment the parser passed.
            "/r/o/p/blob/main/a%2fb%2f..%2f..",
            // Revision arithmetic: not a ref, and not authorized as one.
            "/r/o/p/tree/main~3",
            "/r/o/p/tree/main^{tree}",
            "/r/o/p/tree/HEAD@{1}",
            "/r/o/p/tree/a..b",
            // A control character, however it arrives.
            "/r/o/p/tree/ma%00in",
            // Not a full oid.
            "/r/o/p/commit/abc",
            "/r/o/p/commit/0123456789abcdef0123456789abcdef0123456z",
            // Repository names the provisioning rule already refuses.
            "/r/.hidden/p",
            "/r/o/.git",
            // Verbs that do not exist.
            "/r/o/p/raw/main/x",
            "/r/o/p/blob/main",
        ] {
            assert_eq!(route(url), None, "a dangerous URL parsed: {url}");
        }
    }

    /// Encoding and parsing are inverses, checked as a pair rather than
    /// separately: a link is only correct if the router gives back the
    /// path the listing started from, and two functions that each look
    /// right on their own are exactly how that stops being true.
    #[test]
    fn an_encoded_path_parses_back_to_the_path_it_came_from() {
        for path in [
            "src/lib.rs",
            "src/a file.txt",
            "src/query?.txt",
            "src/hash#one.txt",
            "src/ünïcode-café.rs",
            "src/日本語.txt",
            "src/<img src=x onerror=alert(1)>.txt",
            "src/100% done.md",
            "deep/very/long/nested/path/leaf.txt",
        ] {
            let url = format!("/r/o/p/blob/main/{}", url_path(path));
            assert_eq!(
                route(&url),
                Some(Page::Blob {
                    repo: "o/p".into(),
                    rev: "main".into(),
                    path: path.to_string(),
                }),
                "the link this page renders for {path} does not route back to it"
            );
        }
        // The separator stays a separator, and nothing else does.
        assert_eq!(url_path("a/b"), "a/b");
        assert_eq!(url_path("a?b"), "a%3Fb");
        assert_eq!(url_path("a b"), "a%20b");
        assert_eq!(url_path("a%b"), "a%25b");
    }

    #[test]
    fn percent_decoding_handles_the_ordinary_case() {
        assert_eq!(
            route("/r/o/p/blob/main/a%20file.txt"),
            Some(Page::Blob {
                repo: "o/p".into(),
                rev: "main".into(),
                path: "a file.txt".into()
            })
        );
    }

    /// A modified line pair gets word-level marks: the line wash says
    /// the line moved, the mark says which words.
    #[test]
    fn a_modified_line_pair_marks_the_words_that_changed() {
        let out = "1\t1\tsrc/lib.rs\n\ndiff --git a/src/lib.rs b/src/lib.rs\n\
                   @@ -1 +1 @@\n-let count = 4;\n+let count = 5;\n";
        let mut h = String::new();
        patch(&mut h, out, "clone hint");
        assert!(
            h.contains("<span class=\"del\">-let count = <mark>4</mark>;</span>"),
            "the removed word is not marked: {h}"
        );
        assert!(
            h.contains("<span class=\"add\">+let count = <mark>5</mark>;</span>"),
            "the added word is not marked: {h}"
        );
    }

    /// The changed span is file content and escapes like everything
    /// else: a repository whose diff contains markup is a legal
    /// repository, and the mark must not become an injection point.
    #[test]
    fn word_marks_escape_markup_in_the_changed_span() {
        let out = "1\t1\tf\n\ndiff --git a/f b/f\n@@ -1 +1 @@\n\
                   -say <script>one</script>\n+say <script>two</script>\n";
        let mut h = String::new();
        patch(&mut h, out, "clone hint");
        assert!(!h.contains("<script"), "raw markup reached the page: {h}");
        assert!(
            h.contains("<mark>one</mark>"),
            "the changed word vanished: {h}"
        );
        assert!(
            h.contains("<mark>two</mark>"),
            "the changed word vanished: {h}"
        );
        assert!(
            h.contains("&lt;script&gt;"),
            "the shared markup was dropped, not escaped: {h}"
        );
    }

    /// Unequal runs are not a modification, and a pair sharing nothing
    /// is a rewrite: both render with the wash alone. A mark that fires
    /// on everything marks nothing.
    #[test]
    fn only_a_paired_modification_gets_marks() {
        let unequal = "1\t2\tf\n\ndiff --git a/f b/f\n@@ -1,2 +1 @@\n-a\n-b\n+c\n";
        let mut h = String::new();
        patch(&mut h, unequal, "clone hint");
        assert!(!h.contains("<mark>"), "an unpaired run was marked: {h}");

        let rewrite = "1\t1\tf\n\ndiff --git a/f b/f\n@@ -1 +1 @@\n-abc\n+xyz\n";
        let mut h = String::new();
        patch(&mut h, rewrite, "clone hint");
        assert!(!h.contains("<mark>"), "a total rewrite was marked: {h}");

        // A rewrite that happens to end in the same letter is still a
        // rewrite. Found by a real emission, not invented: `-base` /
        // `+proposed change` share a trailing `e`, and the first
        // version of this renderer marked fourteen of fifteen
        // characters — noise wearing precision's class attribute.
        let coincidence = "1\t1\tf\n\ndiff --git a/f b/f\n@@ -1 +1 @@\n-base\n+proposed change\n";
        let mut h = String::new();
        patch(&mut h, coincidence, "clone hint");
        assert!(
            !h.contains("<mark>"),
            "a coincidental shared letter promoted a rewrite to an edit: {h}"
        );
    }

    /// The stat row counts every file, says binary where git did, and
    /// links each row to the file's header in the patch below.
    #[test]
    fn the_stat_row_counts_every_file_and_anchors_the_headers() {
        let out = "10\t2\tsrc/a.rs\n-\t-\timg.png\n\n\
                   diff --git a/src/a.rs b/src/a.rs\n@@ -1 +1 @@\n-x\n+y\n\
                   diff --git a/img.png b/img.png\nBinary files differ\n";
        let mut h = String::new();
        patch(&mut h, out, "clone hint");
        assert!(h.contains("2 files changed"), "no summary line: {h}");
        assert!(
            h.contains("+10") && h.contains("−2"),
            "per-file counts missing: {h}"
        );
        assert!(h.contains("binary"), "the binary file lost its label: {h}");
        assert!(
            h.contains("href=\"#f0\""),
            "the stat row links nowhere: {h}"
        );
        assert!(
            h.contains("<span class=\"file\" id=\"f0\">") && h.contains("id=\"f1\""),
            "the file headers carry no anchors: {h}"
        );
    }

    /// The bound ends in a line that says what it cut and where the
    /// rest lives — a truncation that ends mid-sentence reads as a bug,
    /// and one that names no destination strands the reader.
    #[test]
    fn the_truncation_line_says_what_was_cut_and_where_the_rest_is() {
        let mut out = String::from("1\t0\tf\n\n");
        for n in 0..MAX_DIFF_LINES + 7 {
            out.push_str(&format!("ctx{n}\n"));
        }
        let mut h = String::new();
        patch(&mut h, &out, "Clone /o/r.git to read it all.");
        assert!(
            h.contains("… 7 more lines not shown"),
            "the cut does not say how much it cut: {h}"
        );
        assert!(
            h.contains(&format!("({} in full)", MAX_DIFF_LINES + 7)),
            "the cut does not say the whole size"
        );
        assert!(
            h.contains("Clone /o/r.git to read it all."),
            "the cut names nowhere to get the rest"
        );
        let last_shown = format!("ctx{}", MAX_DIFF_LINES - 1);
        let first_cut = format!("ctx{}", MAX_DIFF_LINES);
        assert!(h.contains(&last_shown), "the bound cut too early");
        assert!(!h.contains(&first_cut), "the bound did not cut");
    }

    #[test]
    fn byte_counts_read_like_sizes() {
        assert_eq!(human("0"), "0 B");
        assert_eq!(human("1023"), "1023 B");
        assert_eq!(human("1024"), "1.0 KiB");
        assert_eq!(human("1048576"), "1.0 MiB");
        // `ls-tree --long` prints `-` for a tree's size column.
        assert_eq!(human("-"), "-");
    }

    /// D39's reversal, held to the scope the row approved: the two write
    /// sections carry markup and data, never code.
    ///
    /// The page emits one `<script src>` for both of them, once, in
    /// `review`. A section that grew its own element would be an inline
    /// script under a `script-src 'self'` header — blocked by the
    /// browser, so the control would simply stop working, and this says
    /// so at the section that did it rather than in a served page
    /// somebody has to fetch.
    ///
    /// `ui.rs` holds the rule for the script itself, the served pages are
    /// swept for inline script and handlers by
    /// `passkeys::the_ceremony_pages_carry_no_code_and_fetch_one_file`,
    /// and the read surface keeps its own rule: no script at all.
    #[test]
    fn the_write_sections_carry_no_code_of_their_own() {
        let state = serde_json::json!({
            "reviewers": ["carol"], "verdicts": {}, "archived": false,
        });
        let mut verdict = String::new();
        super::verdict_buttons(&mut verdict, "review-1", &state, "carol");
        let mut comment = String::new();
        super::comment_box(&mut comment, "review-1", &state, "carol");
        for (what, html) in [("verdict", &verdict), ("comment", &comment)] {
            assert!(
                !html.is_empty(),
                "{what}: nothing rendered, so nothing was checked"
            );
            assert!(
                !html.contains("<script"),
                "{what} grew a script element: {html}"
            );
        }
        // And the ids each section hands the shared script, named on
        // both sides so a rename fails a test rather than a person: the
        // control renders, nothing binds to it, and the page looks
        // exactly as it should.
        for id in [
            "verdict",
            "verdict-said",
            "comment",
            "comment-go",
            "comment-body",
        ] {
            assert!(
                crate::ui::WEBAUTHN_JS.contains(&format!("'{id}'")),
                "the shared script never looks for {id}"
            );
        }
    }

    /// The page asks for the script only when it has something for the
    /// script to do.
    ///
    /// The case that matters is a review nobody can act on any more,
    /// because that is the one where both sections render nothing and
    /// the page is a read page again. Every authenticated reader gets a
    /// comment box on a live review, so a served page cannot show this:
    /// archiving is admission policy the daemon spends its own key on,
    /// and no curl-driven test can reach it. Hence here, on the four
    /// lines that decide.
    #[test]
    fn a_review_nobody_can_act_on_asks_for_no_script() {
        let settled = serde_json::json!({
            "reviewers": ["carol"], "verdicts": {}, "archived": true,
        });
        let mut h = String::new();
        super::write_sections(&mut h, "review-1", &settled, "carol");
        assert!(h.is_empty(), "an archived review fetched the ceremony: {h}");

        // And the live case still does, exactly once, however many
        // sections rendered.
        let live = serde_json::json!({
            "reviewers": ["carol"], "verdicts": {}, "archived": false,
        });
        let mut h = String::new();
        super::write_sections(&mut h, "review-1", &live, "carol");
        assert_eq!(h.matches("<script").count(), 1, "{h}");
        assert!(
            h.contains("Your verdict") && h.contains("comment-go"),
            "{h}"
        );
    }

    /// Discussion is wider than judgement, and narrower than the page.
    /// A verdict belongs to the people asked for one; a comment belongs
    /// to anyone with a name; neither is offered to an anonymous reader
    /// or on a settled review.
    #[test]
    fn the_comment_box_is_offered_more_widely_than_a_verdict_and_never_to_anon() {
        let live = serde_json::json!({
            "reviewers": ["carol"], "verdicts": {}, "archived": false,
        });
        let rendered = |user: &str| {
            let mut h = String::new();
            super::comment_box(&mut h, "review-1", &live, user);
            h
        };
        assert!(rendered("carol").contains("comment-go"), "a reviewer");
        assert!(
            rendered("mallory").contains("comment-go"),
            "someone who is not a reviewer may still discuss"
        );
        assert!(
            rendered("anon").is_empty(),
            "an anonymous reader was offered a box"
        );
        assert!(
            rendered("").is_empty(),
            "an unnamed caller was offered a box"
        );

        let archived = serde_json::json!({
            "reviewers": ["carol"], "verdicts": {}, "archived": true,
        });
        let mut h = String::new();
        super::comment_box(&mut h, "review-1", &archived, "carol");
        assert!(h.is_empty(), "an archived review still took comments");
    }

    /// The write affordance is offered only to someone who can actually
    /// use it, and never to a reader.
    #[test]
    fn verdict_buttons_appear_only_for_a_reviewer_who_has_not_voted() {
        let state = serde_json::json!({
            "reviewers": ["carol", "dave"],
            "verdicts": { "dave": { "verdict": "Approve", "note": "" } },
            "archived": false,
        });
        let rendered = |user: &str| {
            let mut h = String::new();
            super::verdict_buttons(&mut h, "review-1", &state, user);
            h
        };
        assert!(
            rendered("carol").contains("button class=\"verdict"),
            "an asked reviewer"
        );
        assert!(rendered("dave").is_empty(), "dave already answered");
        assert!(
            rendered("mallory").is_empty(),
            "not a reviewer on this review"
        );

        let archived = serde_json::json!({
            "reviewers": ["carol"], "verdicts": {}, "archived": true,
        });
        let mut h = String::new();
        super::verdict_buttons(&mut h, "review-1", &archived, "carol");
        assert!(h.is_empty(), "an archived review takes no more verdicts");
    }

    /// With scripting off the section is a sentence, not a dead control.
    /// D39 carries this as a tripwire: the enhancement must never become
    /// a dependency.
    #[test]
    fn the_verdict_section_says_what_to_do_without_script() {
        let state = serde_json::json!({
            "reviewers": ["carol"], "verdicts": {}, "archived": false,
        });
        let mut h = String::new();
        super::verdict_buttons(&mut h, "review-1", &state, "carol");
        assert!(h.contains("<noscript>"), "no fallback at all");
        assert!(
            h.contains("choir verdict"),
            "the fallback must name the CLI"
        );
        // The controls start hidden and are revealed by the script, so a
        // reader with scripting off is never shown a button that cannot
        // work.
        assert!(
            h.contains("id=\"verdict\" hidden"),
            "the controls are not hidden by default"
        );
    }
}
