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

    /// Parses an `in=` value strictly, for a caller that can be told it
    /// was wrong.
    ///
    /// [`Scope::parse`] guesses because a URL a reader typed has no way
    /// to receive an explanation; an API request does, and answering a
    /// misspelled `in=cod` with file names would be a wrong answer
    /// wearing the shape of a right one.
    fn from_slug(raw: &str) -> Option<Scope> {
        Scope::ALL.into_iter().find(|one| one.slug() == raw)
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
    /// How to propose a change to this repository, in three commands.
    Contribute { repo: String },
    /// Every review proposing to land on this repository.
    Reviews { repo: String },
    /// Every review this reader was drawn for and has not answered,
    /// across every repository they may read.
    ///
    /// The one page on this surface that is about the *reader* rather
    /// than about a repository, which is why it takes no `repo` and is
    /// filtered per reader the way [`Page::Index`] is.
    Owed,
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
            Page::Index { .. } | Page::Owed => None,
            Page::Tree { repo, .. }
            | Page::Blob { repo, .. }
            | Page::Commits { repo, .. }
            | Page::Commit { repo, .. }
            | Page::Contribute { repo }
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
    // The reader's own queue. Above the `/r/` prefix check because it is
    // not about one repository, and matched exactly so that `/reviewers`
    // or `/reviews/anything` falls through rather than rendering this
    // page under an address that promises something else.
    if path == "/reviews" {
        return Some(Page::Owed);
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
        if rest == "contribute" {
            return Some(Page::Contribute { repo });
        }
        if let Some(id) = rest.strip_prefix("review/") {
            let id = decode_review_id(id)?;
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
pub(crate) fn param(url: &str, key: &str) -> Option<String> {
    // `decode` refuses control characters and invalid UTF-8, which is
    // exactly the rule wanted here: these terms reach a subprocess
    // argument and a rendered page.
    let value = decode(&raw_param(url, key)?.replace('+', "%20"))?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// One query-string parameter, still encoded.
///
/// Split out of [`param`] because a repository name is the one parameter
/// whose value legitimately carries a `/`, which `decode` refuses by
/// design: a decoded slash invents a path segment the parser already
/// walked past. Extracting the pair and decoding it are therefore two
/// steps, and this is the first of them.
pub(crate) fn raw_param<'u>(url: &'u str, key: &str) -> Option<&'u str> {
    let query = url.split_once('?')?.1;
    let query = query.split('#').next().unwrap_or(query);
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then_some(value)
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

/// Percent-decodes a review id, which is not a path segment.
///
/// A D52 change id is `propose:<owner>/<repo>:<fingerprint>`, so it
/// carries both a colon and a slash. Held to [`crate::provision::safe_segment`]
/// — the grammar for names that become directories — every id `choir
/// propose` mints was refused, and the review page that both the pane
/// and the reviews table link to answered 404 for the whole D51-D53
/// onboarding path.
///
/// The id reaches a map lookup and a page, never a subprocess and never
/// the filesystem, so the question here is not "could this be a name"
/// but "could this be a path or a control byte". A traversal segment is
/// refused anyway: an id is matched against the platform's own keys, and
/// one that looks like a path only invites a future reader to join it to
/// one.
fn decode_review_id(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = raw.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let id = String::from_utf8(out).ok()?;
    if id.is_empty() || id.len() > MAX_REVIEW_ID || id.chars().any(char::is_control) {
        return None;
    }
    if id.contains('\\') || id.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
        return None;
    }
    Some(id)
}

/// Longest review id this router will decode.
///
/// A `propose` id is 87 characters plus the repository name; a hand-named
/// one is a word. The cap is what stops a URL from carrying a page-sized
/// string into a lookup that will miss.
const MAX_REVIEW_ID: usize = 512;

/// Renders a change id at the width of a sentence.
///
/// Mirrors `choir_cli::propose::short_change_id` deliberately rather than
/// sharing it: the node does not depend on the client. A change id ends
/// in a 64-character fingerprint, which is a record and not something
/// anyone reads — rendered whole in the reviews pane it overran the pane
/// and drew across the file listing beside it. The `href` and the
/// `title` still carry the id whole, because that is what a reader
/// copies into `choir verdict`.
fn short_change_id(id: &str) -> String {
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
pub(crate) fn url_path(path: &str) -> String {
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
/// How many times this process has shelled out to git.
///
/// A page's cost is dominated by process spawns — around 10 ms each on
/// the development machine — so the spawn count is what drives the read
/// latency Phase 1 puts a ceiling on. It is counted rather than timed
/// for the reason `tests/alloc_budget.rs` counts allocations: wall-clock
/// on a shared laptop is too noisy to gate on, and the same page
/// measured p99 94 ms and p99 246 ms an hour apart with identical code.
/// The count does not move with the weather.
///
/// Always on, not `#[cfg(test)]`. A counter compiled out of the binary
/// being shipped is a counter measuring a different binary, and one
/// relaxed add on a path that is about to spawn a process is not a cost
/// worth reasoning about.
pub(crate) static GIT_INVOCATIONS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    GIT_INVOCATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
    default_from(&branch_names(dir))
}

/// The same choice, made from a branch list already in hand.
///
/// Split out from [`default_branch`] because a repository front page
/// wants both this and the ref picker, and asking git for `refs/heads/`
/// once per question spawned the same `for-each-ref` twice per render.
fn default_from(names: &[String]) -> Option<String> {
    for preferred in ["main", "master"] {
        if names.iter().any(|name| name == preferred) {
            return Some(preferred.to_string());
        }
    }
    names.first().cloned()
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
        // The reader's own queue survives narrowing. It is about a
        // person rather than a repository, so a node that presents one
        // repository has not made it redundant — it has made it short.
        None if matches!(page, Page::Owed) => Some(page),
        None => Some(Page::Tree {
            repo: site.to_string(),
            rev: "HEAD".into(),
            path: String::new(),
        }),
        Some(repo) if repo == site => Some(page),
        Some(_) => None,
    }
}

/// The four things a page needs to know about the *request* rather than
/// about the repository.
///
/// Grouped rather than passed alongside `root`, `page` and `platform`
/// because they travel together and always have: each new one -- browser
/// writes, then the single-repository site, then the reader's origin --
/// arrived as one more parameter on a signature that already carried
/// the previous ones to every page whether it used them or not.
#[derive(Clone, Copy)]
pub(crate) struct Viewer<'a> {
    /// The authenticated reader, whose grants decide what renders.
    pub(crate) user: &'a str,
    /// Whether the browser may offer mutation controls.
    pub(crate) browser_writes: bool,
    /// The one repository this node presents, if it presents one.
    pub(crate) site: Option<&'a str>,
    /// The scheme-and-host this reader reached the node on, so a page
    /// that prints a command can print one they can paste.
    pub(crate) origin: Option<&'a str>,
    /// The palette this reader chose, or `None` for the system's.
    pub(crate) theme: Option<&'a str>,
    /// The address this reader is on, so a palette link returns them to
    /// it rather than to the front door.
    pub(crate) here: &'a str,
    /// Whether this node issues invites at all.
    pub(crate) self_service: bool,
    /// Whether `/account` renders on this node. See [`Chrome::account`].
    pub(crate) account: bool,
    /// Whether this reader holds `@node write`. See [`Chrome::console`].
    pub(crate) console: bool,
    /// Where this node's book is, if its operator has said. See
    /// [`Chrome::docs`].
    pub(crate) docs: Option<&'a str>,
    /// Whether this request carries a credential. See
    /// [`Chrome::signed_in`].
    pub(crate) signed_in: Option<bool>,
    /// Who holds `own` over a repository, for the sentence the review
    /// page states about what would authorize a landing.
    ///
    /// A closure rather than the [`crate::acl::Acl`] itself, so this
    /// module keeps knowing nothing about how authorization is spelled:
    /// it asks a question and renders the answer. A node with no ACL
    /// hands back an empty list, which is the right input — nobody owns
    /// anything, so the approval-weight rule is the one in force.
    pub(crate) owners: &'a dyn Fn(&str) -> Vec<String>,
}

pub(crate) fn render(
    root: &Path,
    page: &Page,
    readable: &dyn Fn(&str) -> bool,
    platform: Option<&crate::platform::Platform>,
    viewer: Viewer<'_>,
) -> Rendered {
    let Viewer {
        user,
        browser_writes,
        site,
        origin,
        self_service,
        theme,
        here,
        account,
        console,
        docs,
        signed_in,
        owners,
    } = viewer;
    let chrome = Chrome {
        site,
        theme,
        here,
        account,
        console,
        docs,
        signed_in,
        origin,
    };
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
            return no_such_repository(chrome);
        }
    }
    match page {
        Page::Index { q } => index(root, readable, platform, q, chrome),
        Page::Search {
            repo,
            rev,
            q,
            scope,
        } => search(&bare(root, repo), repo, rev, q, *scope, chrome, origin),
        Page::Tree { repo, rev, path } => {
            tree(&bare(root, repo), repo, rev, path, platform, chrome, origin)
        }
        Page::Blob { repo, rev, path } => blob(&bare(root, repo), repo, rev, path, chrome, origin),
        Page::Commits { repo, rev } => commits(&bare(root, repo), repo, rev, chrome, origin),
        Page::Commit { repo, oid } => commit(&bare(root, repo), repo, oid, chrome, origin),
        Page::Contribute { repo } => contribute(root, repo, chrome, origin, self_service),
        Page::Reviews { repo } => reviews(repo, platform, chrome),
        Page::Owed => owed(readable, platform, user, chrome),
        Page::Review { repo, id } => review(
            &bare(root, repo),
            repo,
            id,
            platform,
            Reader {
                user,
                browser_writes,
                owners,
            },
            chrome,
        ),
    }
}

/// On-disk location of a repository's bare directory.
fn bare(root: &Path, repo: &str) -> PathBuf {
    root.join(format!("{repo}.git"))
}

/// Every repository on the node this reader may read, sorted.
///
/// Extracted from the index page because the search API asks the same
/// question, and two walks that agree today are a coincidence with a
/// test on it. The grant check happens here rather than at either
/// caller, so no caller can enumerate what it may not read by
/// forgetting to filter.
pub(crate) fn repositories(root: &Path, readable: &dyn Fn(&str) -> bool) -> Vec<String> {
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
    repos
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
    chrome: Chrome<'_>,
) -> Rendered {
    let repos = repositories(root, readable);
    // The filter runs after the grant check, never before it: a reader
    // must never be able to learn that a repository exists by searching
    // for it. `shown` is a subset of what this reader could already list.
    let needle = q.to_lowercase();
    let shown: Vec<&String> = repos
        .iter()
        .filter(|repo| needle.is_empty() || repo.to_lowercase().contains(&needle))
        .collect();

    let mut h = shell("repositories", Bar::filtered(q, chrome));
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
        h.push_str("<table class=\"repos\"><tbody>");
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
        html: close(h, chrome.signed_in),
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

/// A `repo=` value, decoded and checked, or `None` when it is not a
/// repository name.
///
/// The two halves are decoded separately, so `%2F` cannot smuggle in a
/// third segment, and each is held to the same `provision::safe_segment`
/// the browse routes apply -- because this is where the name becomes a
/// filesystem path, and a rule enforced at one of two entrances is not a
/// rule.
///
/// It returns `None` rather than the caller's parameter parsing doing
/// so, which is the whole point: a `repo=` that fails to parse must stay
/// *present*. Parsed at the query-string layer, `repo=../../etc` came
/// back as "no repo= given" and the request silently widened into a
/// node-wide search -- an answer about everything to a question about
/// one thing.
fn repo_name(raw: &str) -> Option<String> {
    let (owner, name) = raw.split_once('/')?;
    let (owner, name) = (decode(owner)?, decode(name)?);
    (crate::provision::safe_segment(&owner) && crate::provision::safe_segment(&name))
        .then(|| format!("{owner}/{name}"))
}

/// The same search, as JSON, over every repository the caller may read
/// (D62).
///
/// This exists because the search that shipped was a page and only a
/// page, on a platform whose primary reader is an agent: the one caller
/// most likely to want "where is this symbol" had no way to ask. It runs
/// the same three functions the page runs, against the same resolved
/// oid, so the two can never disagree about what a match is.
///
/// Two deliberate differences from the page:
///
/// - **One scope, not three.** The page runs all three because its tabs
///   carry counts and a reader who lands on an empty tab concludes the
///   search is broken. An API caller named the scope it wanted, and
///   node-wide that difference is `3n` git invocations rather than `n`.
/// - **A bad `in=` is a refusal**, where the page falls back to the
///   cheapest scope. See [`Scope::from_slug`].
///
/// The cost is one `git grep` per readable repository, unindexed and
/// unbounded in the number of repositories. That is the honest shape of
/// the thing today; an index is what answers a measurement that says it
/// is too slow, and no such measurement exists yet.
///
/// Returns `(status, body)`. A repository named in `repo` that the
/// caller may not read is answered exactly as one that does not exist,
/// so a caller cannot learn that a repository exists by searching for
/// it.
pub(crate) fn api_search(
    root: &Path,
    readable: &dyn Fn(&str) -> bool,
    repo: Option<&str>,
    rev: Option<&str>,
    q: &str,
    scope: Option<&str>,
    limit: Option<&str>,
) -> (u16, String) {
    let refusal = |why: &str| (400, serde_json::json!({ "error": why }).to_string() + "\n");
    if q.is_empty() {
        return refusal("q is required and cannot be empty");
    }
    let scope = match scope {
        None => Scope::Code,
        Some(raw) => match Scope::from_slug(raw) {
            Some(one) => one,
            None => return refusal("in must be one of files, code, commits"),
        },
    };
    let limit = match limit {
        None => SEARCH_HITS,
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) if (1..=SEARCH_HITS).contains(&n) => n,
            _ => return refusal("limit must be a whole number from 1 to 200"),
        },
    };

    // Named repository or every readable one. `repositories` applies the
    // grant, so the node-wide arm cannot over-report; the named arm
    // checks the same predicate itself.
    let names: Vec<String> = match repo {
        Some(raw) => {
            let absent = || {
                (
                    404,
                    serde_json::json!({ "error": "no such repository" }).to_string() + "\n",
                )
            };
            let Some(one) = repo_name(raw) else {
                return absent();
            };
            if !readable(&one) || !bare(root, &one).join("HEAD").is_file() {
                return absent();
            }
            vec![one]
        }
        None => repositories(root, readable),
    };
    // A revision only means something against one named repository.
    // Node-wide, each repository is searched at its own `HEAD`, because
    // `main` is not a ref every repository has and refusing the whole
    // request over one of them would be worse than searching what the
    // caller meant.
    if repo.is_none() && rev.is_some() {
        return refusal("rev applies to a single repo= and not to a node-wide search");
    }
    let rev = rev.unwrap_or("HEAD");

    let mut rows = Vec::new();
    let mut total = 0usize;
    let mut shown = 0usize;
    let mut truncated = false;
    for name in &names {
        let dir = bare(root, name);
        let Ok(oid) = resolve(&dir, rev) else {
            // An empty repository, or a rev this one does not carry. It
            // is reported with no matches rather than dropped, so the
            // count of repositories searched stays honest.
            rows.push(serde_json::json!({
                "repo": name,
                "oid": serde_json::Value::Null,
                "matches": [],
            }));
            continue;
        };
        let hits = Hits::find(&dir, &oid, q);
        let matches: Vec<serde_json::Value> = match scope {
            Scope::Files => hits
                .files
                .iter()
                .map(|path| serde_json::json!(path))
                .collect(),
            Scope::Code => hits
                .code
                .iter()
                .map(|(path, line, text)| {
                    serde_json::json!({
                        "path": path,
                        "line": line.parse::<u64>().unwrap_or_default(),
                        "text": text,
                    })
                })
                .collect(),
            Scope::Commits => hits
                .commits
                .iter()
                .map(|(oid, author, time, subject, _body)| {
                    serde_json::json!({
                        "oid": oid,
                        "author": author,
                        "time": time.parse::<i64>().unwrap_or_default(),
                        "subject": subject,
                    })
                })
                .collect(),
        };
        total += matches.len();
        // The budget is spent in repository order and what it cannot pay
        // for is dropped, but `total` counts every match found, so a
        // caller is told how much it is not seeing rather than being
        // shown a short list that looks complete.
        let kept = matches.len().min(limit - shown);
        shown += kept;
        if kept < matches.len() {
            truncated = true;
        }
        rows.push(serde_json::json!({
            "repo": name,
            "oid": oid,
            "matches": matches[..kept],
        }));
    }

    let body = serde_json::json!({
        "query": q,
        "in": scope.slug(),
        "rev": rev,
        "repositories": names.len(),
        "matches": total,
        "limit": limit,
        "truncated": truncated,
        "results": rows,
    });
    (200, body.to_string() + "\n")
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
    chrome: Chrome<'_>,
    origin: Option<&str>,
) -> Rendered {
    let oid = match resolve(dir, rev) {
        Ok(oid) => oid,
        Err(why) => return missing(repo, rev, &why, chrome),
    };
    let named = if rev == "HEAD" {
        default_branch(dir).unwrap_or_else(|| rev.to_string())
    } else {
        rev.to_string()
    };
    let rev = named.as_str();

    let mut h = shell(
        &format!("{repo}: search"),
        Bar::searched(repo, rev, q, chrome),
    );
    repo_header(&mut h, repo, rev, &oid, "", "search", origin);
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
            html: close(h, chrome.signed_in),
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
        html: close(h, chrome.signed_in),
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

/// Short ref names under one pattern, in git's own order.
///
/// Branches and tags stay two lists rather than one because they answer
/// different questions: a branch is where work is happening, a tag is a
/// release. Presenting them in one flat list is how a reader ends up
/// browsing `v0.1.0` believing it is current. They are fetched
/// separately as well, so a page that needs only branches pays for only
/// branches.
fn ref_names(dir: &Path, pattern: &str) -> Vec<String> {
    git_text(dir, &["for-each-ref", "--format=%(refname:short)", pattern])
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Every branch, short names, in git's own order.
fn branch_names(dir: &Path) -> Vec<String> {
    ref_names(dir, "refs/heads/")
}

/// Every tag, short names, in git's own order.
fn tag_names(dir: &Path) -> Vec<String> {
    ref_names(dir, "refs/tags/")
}

/// How far back one walk looks before falling back to per-path queries.
///
/// A listing's rows are almost always touched by recent commits, so a
/// bounded walk answers nearly all of them; the cap is what keeps a
/// repository with a hundred thousand commits from paying for all of
/// them to render one page.
const TOUCH_WALK: usize = 400;

/// Last-touch for every row of a listing, in one `git log` walk.
///
/// This used to be one `git log -1` per row, which is a subprocess per
/// row: a forty-file directory spawned forty-one of them where it now
/// spawns three, on a page Phase 1 asks to serve inside 100 ms. The
/// measurement that found it is `tests/phase1_reads.rs`; the number that
/// records it is the spawn budget in `tests/phase1_spawns.rs`, because
/// the milliseconds move with the machine and the spawn count does not.
/// (The wall-clock figure this comment used to quote was taken when the
/// sampler still forked a `curl` per request, so it was mostly measuring
/// that.)
///
/// One walk attributes each path to the newest commit touching it.
/// Anything the walk does not reach — a file untouched in the last
/// [`TOUCH_WALK`] commits — falls back to the per-path query, so the
/// answer is the same one the slow version gave and only the cost
/// changed. `last_touches_agree_with_the_per_path_query` is that claim.
fn last_touches(
    dir: &Path,
    oid: &str,
    rows: &[String],
) -> std::collections::BTreeMap<String, (String, i64)> {
    let mut found: std::collections::BTreeMap<String, (String, i64)> =
        std::collections::BTreeMap::new();
    if rows.is_empty() {
        return found;
    }
    let depth = format!("-{TOUCH_WALK}");
    // `\x01` opens a record so commit headers can be told from the file
    // names that follow them; `%at%x00%s` is the same pair the per-path
    // query parses, so the two cannot disagree about what they read.
    let walk = |extra: &[&str]| -> Result<String, String> {
        let mut args = vec!["log", &depth, "--format=%x01%at%x00%s", "--name-only"];
        args.extend_from_slice(extra);
        args.push(oid);
        git_text(dir, &args)
    };
    let Ok(text) = walk(&["--diff-merges=first-parent"]).or_else(|_| walk(&[])) else {
        return found;
    };

    let mut current: Option<(String, i64)> = None;
    for line in text.lines() {
        if let Some(header) = line.strip_prefix('\x01') {
            current = header.split_once('\0').and_then(|(at, subject)| {
                Some((subject.to_string(), at.trim().parse::<i64>().ok()?))
            });
            continue;
        }
        if line.is_empty() {
            continue;
        }
        let Some(touched) = current.as_ref() else {
            continue;
        };
        // A commit names files; a row may be a directory. The first row
        // this file sits under is the one it dates, and only the first
        // commit to mention a row counts, because the walk is newest
        // first.
        for row in rows {
            let hit = line == row || line.starts_with(&format!("{row}/"));
            if hit && !found.contains_key(row) {
                found.insert(row.clone(), touched.clone());
            }
        }
        if found.len() == rows.len() {
            break;
        }
    }

    // Whatever the walk could not reach, asked for directly. Rare by
    // construction, and the reason this is a speed-up rather than a
    // change of answer.
    for row in rows {
        if !found.contains_key(row) {
            if let Some(touch) = last_touch(dir, oid, row) {
                found.insert(row.clone(), touch);
            }
        }
    }
    found
}

/// How far back the front page looks for who has written here.
///
/// Bounded for the reason [`TOUCH_WALK`] is bounded: the rail is a
/// summary, and a repository with a hundred thousand commits in it must
/// not make its own front page linear in them. A history longer than
/// this is reported as the authors of its most recent commits, and the
/// rail says `+` so the number is not read as the whole truth.
const AUTHOR_WALK: usize = 2000;

/// The commit a listing is of: who wrote it, what they called it, when.
struct Tip {
    author: String,
    subject: String,
    at: i64,
}

/// The tip commit and everyone who has written here, in one walk.
///
/// Two answers from one subprocess because they come from the same
/// place: `git log`, newest first, over one revision. The first record
/// is the commit this listing is of; every record's author is the set
/// the rail names. Asking separately would be two spawns on a page whose
/// cost is counted in them (`tests/phase1_spawns.rs`), and this page had
/// exactly one to spend.
///
/// The `bool` is whether the walk hit [`AUTHOR_WALK`], so a caller can
/// say "at least this many" rather than state a count it did not finish
/// counting.
fn tip_and_authors(dir: &Path, oid: &str) -> (Option<Tip>, Vec<String>, bool) {
    let depth = format!("-{AUTHOR_WALK}");
    let Ok(text) = git_text(dir, &["log", &depth, "--format=%an%x00%at%x00%s", oid]) else {
        return (None, Vec::new(), false);
    };
    let mut tip: Option<Tip> = None;
    let mut authors: Vec<String> = Vec::new();
    let mut walked = 0usize;
    for line in text.lines() {
        let mut fields = line.splitn(3, '\0');
        let (Some(author), Some(at), Some(subject)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        walked += 1;
        if tip.is_none() {
            tip = Some(Tip {
                author: author.to_string(),
                subject: subject.to_string(),
                at: at.trim().parse().unwrap_or_default(),
            });
        }
        // Linear rather than a set: the list this builds is rendered in
        // first-seen order, which is most-recently-active first, and a
        // repository's author count is small enough that the scan costs
        // less than the ordering would cost to put back.
        if !authors.iter().any(|seen| seen == author) {
            authors.push(author.to_string());
        }
    }
    (tip, authors, walked >= AUTHOR_WALK)
}

/// The one row above a listing that says what state it is in.
///
/// Who last wrote, what they called it, and when -- the three facts a
/// reader checks before reading anything else, and the three that were
/// previously spread between the header, the ref bar and the first row
/// of the table.
fn commit_bar(h: &mut String, repo: &str, oid: &str, tip: &Tip, now: i64) {
    h.push_str("<div class=\"commitbar\"><span class=\"who\">");
    h.push_str(&esc(&tip.author));
    h.push_str("</span><a class=\"what\" href=\"/r/");
    h.push_str(&esc(repo));
    h.push_str("/commit/");
    h.push_str(&esc(oid));
    h.push_str("\">");
    h.push_str(&esc(&tip.subject));
    h.push_str("</a><span class=\"when muted\">");
    h.push_str(&esc(&ago(now, tip.at)));
    h.push_str("</span></div>");
}

/// The repository's own one-line description, if its operator wrote one.
///
/// `git init` leaves a `description` file carrying a sentence telling you
/// to replace it, and gitweb has read that file for twenty years, so this
/// is the conventional place rather than a new one this node invented.
/// The placeholder is treated as absent, because a front page repeating
/// "Unnamed repository; edit this file" is worse than one saying nothing.
fn described(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("description")).ok()?;
    let line = text.lines().next()?.trim();
    if line.is_empty() || line.starts_with("Unnamed repository") {
        return None;
    }
    Some(line.to_string())
}

/// The documents a repository is expected to carry, as the rail finds
/// them: by name, in the listing already read, so this costs no spawn.
///
/// Exact names rather than a pattern, because `LICENSE-APACHE` is a
/// licence and `LICENSED_UNDER_PROTEST.md` is somebody's essay, and a
/// rail that links the second one under the word "License" is stating
/// something untrue about the repository.
fn doc_link(names: &[String], wanted: &[&str]) -> Option<String> {
    wanted
        .iter()
        .find(|name| names.iter().any(|have| have == *name))
        .map(|name| (*name).to_string())
}

/// Every licence a repository carries, each under the name of the
/// licence rather than the name of the file.
///
/// Dual licensing is the normal case in this ecosystem and a rail that
/// picked one file and called it "License" would be describing the
/// repository wrongly -- it says both, in the order the file names sort,
/// and falls back to the bare word only for a repository carrying one
/// unnamed `LICENSE`.
fn licences(names: &[String]) -> Vec<(&'static str, String)> {
    [
        ("MIT", "LICENSE-MIT"),
        ("Apache-2.0", "LICENSE-APACHE"),
        ("License", "LICENSE"),
        ("License", "LICENSE.md"),
        ("License", "COPYING"),
    ]
    .into_iter()
    .filter(|(_, file)| names.iter().any(|have| have == file))
    .map(|(label, file)| (label, file.to_string()))
    .collect()
}

/// The rail beside the listing: what this repository is, what it is
/// licensed as, where it is up to, and who has written it.
///
/// Everything here is already on disk or already read, except the author
/// list, which shares [`tip_and_authors`]' one walk with the commit bar.
#[allow(clippy::too_many_arguments)]
fn about_pane(
    h: &mut String,
    dir: &Path,
    repo: &str,
    rev: &str,
    names: &[String],
    tags: &[String],
    authors: &[String],
    more_authors: bool,
) {
    let about = described(dir);
    let licences = licences(names);
    let contributing = doc_link(names, &["CONTRIBUTING.md", "CONTRIBUTING"]);
    let security = doc_link(names, &["SECURITY.md", "SECURITY"]);
    let latest = tags.last();
    // A rail with one item in it is a rail worth drawing; a rail with
    // none is the empty card this page just stopped drawing elsewhere.
    if about.is_none()
        && licences.is_empty()
        && contributing.is_none()
        && security.is_none()
        && latest.is_none()
        && authors.is_empty()
    {
        return;
    }
    h.push_str("<aside class=\"pane pane-about\"><h2>About</h2>");
    if let Some(about) = about.as_deref() {
        h.push_str("<p class=\"says\">");
        h.push_str(&esc(about));
        h.push_str("</p>");
    }
    h.push_str("<ul class=\"facts\">");
    if !licences.is_empty() {
        h.push_str("<li><span class=\"muted\">");
        h.push_str(if licences.len() == 1 {
            "License"
        } else {
            "Licenses"
        });
        h.push_str("</span> ");
        for (n, (label, file)) in licences.iter().enumerate() {
            if n > 0 {
                h.push_str(" \u{b7} ");
            }
            h.push_str("<a href=\"/r/");
            h.push_str(&esc(repo));
            h.push_str("/blob/");
            h.push_str(&esc(rev));
            h.push('/');
            h.push_str(&esc(&url_path(file)));
            h.push_str("\">");
            h.push_str(&esc(label));
            h.push_str("</a>");
        }
        h.push_str("</li>");
    }
    let mut doc = |label: &str, file: &Option<String>| {
        if let Some(file) = file.as_deref() {
            h.push_str("<li><a href=\"/r/");
            h.push_str(&esc(repo));
            h.push_str("/blob/");
            h.push_str(&esc(rev));
            h.push('/');
            h.push_str(&esc(&url_path(file)));
            h.push_str("\">");
            h.push_str(&esc(label));
            h.push_str("</a></li>");
        }
    };
    doc("Contributing", &contributing);
    doc("Security policy", &security);
    if let Some(tag) = latest {
        h.push_str("<li><span class=\"muted\">Latest tag</span> <a href=\"/r/");
        h.push_str(&esc(repo));
        h.push_str("/tree/");
        h.push_str(&esc(&url_path(tag)));
        h.push_str("\" class=\"mono\">");
        h.push_str(&esc(tag));
        h.push_str("</a></li>");
    }
    h.push_str("</ul>");
    if !authors.is_empty() {
        h.push_str("<h2>Written by<span class=\"count\">");
        h.push_str(&authors.len().to_string());
        if more_authors {
            h.push('+');
        }
        h.push_str("</span></h2><ul class=\"who\">");
        // Most recently active first, which is the order the walk found
        // them in, and the order a reader asking "who is here now" wants.
        for author in authors.iter().take(PANE_ROWS) {
            h.push_str("<li>");
            h.push_str(&esc(author));
            h.push_str("</li>");
        }
        h.push_str("</ul>");
    }
    h.push_str("</aside>");
}

/// The strip over the README: the other documents, one click away.
///
/// Links rather than tabs that switch in place, because this surface
/// runs no script. What it buys is the same thing GitHub's tab strip
/// buys -- a reader who wants the licence or the contributing guide
/// finds it without going back to the listing to look for the file.
fn readme_tabs(h: &mut String, repo: &str, rev: &str, names: &[String]) {
    let mut others: Vec<(&str, String)> = Vec::new();
    if let Some(file) = doc_link(names, &["CONTRIBUTING.md", "CONTRIBUTING"]) {
        others.push(("Contributing", file));
    }
    others.extend(licences(names));
    if let Some(file) = doc_link(names, &["SECURITY.md", "SECURITY"]) {
        others.push(("Security", file));
    }
    if others.is_empty() {
        return;
    }
    h.push_str("<nav class=\"doctabs\">");
    for (label, file) in others {
        h.push_str("<a href=\"/r/");
        h.push_str(&esc(repo));
        h.push_str("/blob/");
        h.push_str(&esc(rev));
        h.push('/');
        h.push_str(&esc(&url_path(&file)));
        h.push_str("\">");
        h.push_str(&esc(label));
        h.push_str("</a>");
    }
    h.push_str("</nav>");
}

/// The commit one path was last changed by: its subject and its timestamp.
///
/// A subprocess per call, so a listing does not use it per row any more
/// — [`last_touches`] answers a whole listing in one walk. This is what
/// that walk falls back to for a row it did not reach within
/// [`TOUCH_WALK`] commits, and it is the definition of the right answer
/// that the walk is tested against.
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
    chrome: Chrome<'_>,
    origin: Option<&str>,
) -> Rendered {
    let oid = match resolve(dir, rev) {
        Ok(oid) => oid,
        // A repository that exists but resolves nothing is empty, not
        // missing — and it is the first thing an operator browses after
        // creating one, so answering `404` there says the create failed
        // when it did not.
        Err(_) if dir.is_dir() && rev == "HEAD" => return empty(repo, chrome),
        Err(why) => return missing(repo, rev, &why, chrome),
    };
    // `HEAD` is how the front door is *addressed*, not what a reader
    // wants to be told they are looking at, and it is not a link anyone
    // can usefully share. Every link this page emits therefore names the
    // branch, so a reader who copies one gets a URL that keeps meaning
    // the same thing.
    //
    // One read of `refs/heads/` serves both questions this page asks of
    // it — what to call `HEAD`, and what to offer in the ref picker —
    // because they used to spawn a `for-each-ref` each, and a spawn is
    // around a tenth of the page's whole latency budget. Fetched only
    // when something actually asks: a subdirectory at a named revision
    // wants neither.
    let branches = if rev == "HEAD" || path.is_empty() {
        branch_names(dir)
    } else {
        Vec::new()
    };
    let named = if rev == "HEAD" {
        default_from(&branches).unwrap_or_else(|| rev.to_string())
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
        Err(why) => return missing(repo, rev, &why, chrome),
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
        Bar::repo(repo, rev, chrome),
    );
    repo_header(&mut h, repo, rev, &oid, path, "tree", origin);
    // The bar a reader uses to orient: which revision they are on, how
    // much history is under it, and what else they could switch to. Only
    // at the repository root — inside a directory it is the breadcrumb
    // that answers "where am I", and repeating the picker there just
    // pushes the listing further down the page.
    // Read once, used three times: the ref bar names the tags, the rail
    // names the newest, the commit bar and the rail split one author
    // walk. Hoisted out of the ref-bar block below because that block is
    // where they used to be read and the rail is drawn after it.
    let (tip, authors, more_authors, tags, root_names) = if path.is_empty() {
        let (tip, authors, more) = tip_and_authors(dir, &oid);
        (
            tip,
            authors,
            more,
            tag_names(dir),
            rows.iter().map(|(_, name, _)| name.clone()).collect(),
        )
    } else {
        (None, Vec::new(), false, Vec::new(), Vec::new())
    };
    if path.is_empty() {
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
    // The repository root is the listing, a rail beside it, and the
    // README under both. The listing leads: it is what a reader came for
    // and it is the pane that can use the width, and the rail's two
    // cards each draw nothing when they have nothing rather than holding
    // a share of the page open for news that has not arrived. Inside a
    // directory there is no rail to draw, so those pages carry the
    // listing and the README alone.
    let file_count = rows.len();
    if path.is_empty() {
        h.push_str("<div class=\"panes\">");
        h.push_str("<section class=\"pane pane-files\"><h2>Files<span class=\"count\">");
        h.push_str(&file_count.to_string());
        h.push_str("</span></h2>");
        if let Some(tip) = tip.as_ref() {
            commit_bar(&mut h, repo, &oid, tip, now_secs());
        }
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
        // One walk for the whole listing rather than one query per row.
        // See [`last_touches`]: this loop used to spawn a subprocess per
        // file, which is most of what a directory page cost.
        let names: Vec<String> = rows.iter().map(|(_, name, _)| name.clone()).collect();
        let touches = last_touches(dir, &oid, &names);
        h.push_str("<table class=\"listing\"><tbody>");
        for (is_dir, name, size) in rows {
            let touched = touches.get(&name).cloned();
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
            h.push_str("\" class=\"");
            h.push_str(if is_dir { "dir" } else { "file" });
            h.push_str("\">");
            h.push_str(&esc(leaf));
            h.push_str("</a>");
            // Name, subject, age, size: one row per entry at every
            // level. The subject is the column that turns a file list
            // into a description of what is happening; it truncates
            // rather than wraps. A path git cannot date is left blank
            // rather than filled with a guess.
            h.push_str("</td><td class=\"subject muted\">");
            if let Some((subject, _)) = touched.as_ref() {
                h.push_str(&esc(subject));
            }
            h.push_str("</td><td class=\"when muted\">");
            if let Some((_, at)) = touched.as_ref() {
                h.push_str(&esc(&ago(now, *at)));
            }
            h.push_str("</td><td class=\"num muted\">");
            if !is_dir {
                h.push_str(&esc(&size));
            }
            h.push_str("</td></tr>");
        }
        h.push_str("</tbody></table>");
    }
    h.push_str("</section>");
    // The rail: what is in flight, and what this repository is. One
    // column rather than two panes, so the listing keeps the width and
    // these stack beside it the way a sidebar does. Either half draws
    // nothing when it has nothing, and a rail with neither is a rail
    // that never opens.
    if path.is_empty() {
        let mut rail = String::new();
        reviews_pane(&mut rail, dir, repo, platform);
        about_pane(
            &mut rail,
            dir,
            repo,
            rev,
            &root_names,
            &tags,
            &authors,
            more_authors,
        );
        if !rail.is_empty() {
            h.push_str("<div class=\"rail\">");
            h.push_str(&rail);
            h.push_str("</div>");
        }
    }
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
            if path.is_empty() {
                readme_tabs(&mut h, repo, rev, &root_names);
            }
            h.push_str(&crate::readme::render(
                &body,
                crate::readme::Base {
                    repo,
                    rev,
                    dir: path,
                },
            ));
            h.push_str("</section>");
        } else if path.is_empty() {
            // An empty third pane reads as a broken layout. Saying what
            // is missing, and what would fill it, does not.
            h.push_str("<p class=\"muted\">No README at this revision.</p>");
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
        html: close(h, chrome.signed_in),
    }
}

/// One file.
fn blob(
    dir: &Path,
    repo: &str,
    rev: &str,
    path: &str,
    chrome: Chrome<'_>,
    origin: Option<&str>,
) -> Rendered {
    let oid = match resolve(dir, rev) {
        Ok(oid) => oid,
        Err(why) => return missing(repo, rev, &why, chrome),
    };
    let spec = format!("{oid}:{path}");
    let size: u64 = match git_text(dir, &["cat-file", "-s", &spec]) {
        Ok(text) => text.trim().parse().unwrap_or(0),
        Err(why) => return missing(repo, rev, &why, chrome),
    };

    let mut h = shell(&format!("{repo}: {path}"), Bar::repo(repo, rev, chrome));
    repo_header(&mut h, repo, rev, &oid, path, "blob", origin);
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
                let regions = split_conflicts(&text);
                if regions.iter().any(|r| matches!(r, Region::Conflict { .. })) {
                    conflicted_file(&mut h, &regions);
                } else {
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
    }
    h.push_str("</section>");
    Rendered {
        status: 200,
        etag: Some(tag(&oid, path)),
        html: close(h, chrome.signed_in),
    }
}

/// One stretch of a file that is either ordinary text or a committed
/// conflict.
///
/// A conflict is a value here, not an error (invariant 6): a merge that
/// nothing could resolve is committed with all three sides kept, and
/// later work builds on top of it. That is the state
/// [`choir_view::TreeEntry::Conflict`] names in the log's own substrate,
/// and it is the state git's conflict markers name in a repository this
/// node serves — the same fact in the two places this node stores facts.
/// The browse surface reads git, so this reads the markers.
#[derive(Debug, PartialEq, Eq)]
enum Region<'a> {
    /// Ordinary lines, with the one-based number of the first of them.
    Plain { first: usize, lines: Vec<&'a str> },
    /// A committed conflict, all three sides kept.
    Conflict {
        /// The one-based line the `<<<<<<<` marker sits on.
        first: usize,
        /// What the marker named as the left side, usually a ref.
        left_label: &'a str,
        /// What the closing marker named as the right side.
        right_label: &'a str,
        /// The base, present only in a `diff3`-style conflict. `None` is
        /// not "no base": it is "this file was written by a merge driver
        /// that did not record one", and the column says so rather than
        /// rendering empty as though the base were blank.
        base: Option<Vec<&'a str>>,
        left: Vec<&'a str>,
        right: Vec<&'a str>,
    },
}

/// Splits a file into plain stretches and committed conflicts.
///
/// Deliberately forgiving. A conflict that never closes, or one whose
/// markers arrive out of order, is *text*: a file that happens to
/// contain the string `<<<<<<<` in a code fence must render as itself,
/// and a half-parsed conflict view over ordinary prose would be worse
/// than no conflict view at all. So the scan only commits to a region
/// once it has seen the whole `<<<<<<< … ======= … >>>>>>>` shape, and
/// otherwise emits the lines it consumed as plain text.
///
/// The seven-character marker length is git's, and the space after it is
/// required: `<<<<<<<<` (eight) is not a marker, and neither is a line of
/// nothing but angle brackets.
fn split_conflicts(text: &str) -> Vec<Region<'_>> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<Region<'_>> = Vec::new();
    let mut plain: Vec<&str> = Vec::new();
    let mut plain_first = 1usize;
    let mut i = 0usize;
    while i < lines.len() {
        let Some(left_label) = marker(lines[i], "<<<<<<<") else {
            if plain.is_empty() {
                plain_first = i + 1;
            }
            plain.push(lines[i]);
            i += 1;
            continue;
        };
        // Walk forward for the rest of the shape. `base_at` is the
        // `|||||||` line if this driver wrote one.
        let mut base_at: Option<usize> = None;
        let mut split_at: Option<usize> = None;
        let mut end_at: Option<usize> = None;
        let mut right_label = "";
        for (at, line) in lines.iter().enumerate().skip(i + 1) {
            // A second opening marker before this one closed means the
            // first was never a conflict. Stop and let it be text.
            if marker(line, "<<<<<<<").is_some() {
                break;
            }
            if base_at.is_none() && split_at.is_none() && marker(line, "|||||||").is_some() {
                base_at = Some(at);
            } else if split_at.is_none() && line.trim_end() == "=======" {
                split_at = Some(at);
            } else if let Some(label) = marker(line, ">>>>>>>") {
                if split_at.is_some() {
                    right_label = label;
                    end_at = Some(at);
                }
                break;
            }
        }
        let (Some(split), Some(end)) = (split_at, end_at) else {
            if plain.is_empty() {
                plain_first = i + 1;
            }
            plain.push(lines[i]);
            i += 1;
            continue;
        };
        if !plain.is_empty() {
            out.push(Region::Plain {
                first: plain_first,
                lines: std::mem::take(&mut plain),
            });
        }
        let left_end = base_at.unwrap_or(split);
        out.push(Region::Conflict {
            first: i + 1,
            left_label,
            right_label,
            base: base_at.map(|at| lines[at + 1..split].to_vec()),
            left: lines[i + 1..left_end].to_vec(),
            right: lines[split + 1..end].to_vec(),
        });
        i = end + 1;
    }
    if !plain.is_empty() {
        out.push(Region::Plain {
            first: plain_first,
            lines: plain,
        });
    }
    out
}

/// The label on a conflict marker line, or `None` when this is not one.
///
/// A bare marker with no label is still a marker — git writes one for
/// `=======` and can write one for the others — so an exact match
/// returns the empty label rather than `None`.
fn marker<'a>(line: &'a str, mark: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(mark)?;
    if rest.is_empty() {
        return Some("");
    }
    Some(rest.strip_prefix(' ')?.trim_end())
}

/// A file with at least one committed conflict in it.
///
/// **This is the view nobody else renders.** Every other forge treats a
/// conflict as a thing that must not be committed, so the only way any
/// of them will show you one is as marker soup in a text file — three
/// versions of the same lines interleaved, in one column, with the
/// reader doing the alignment in their head. Here a conflict is a
/// committed value, so it gets a reading surface: base, left and right
/// as three columns, the same conflict line on the same row across all
/// three, and the ordinary text of the file above and below it.
fn conflicted_file(h: &mut String, regions: &[Region<'_>]) {
    let count = regions
        .iter()
        .filter(|r| matches!(r, Region::Conflict { .. }))
        .count();
    h.push_str("<p class=\"note\">");
    h.push_str(&esc(&plural(
        count,
        "unresolved conflict",
        "unresolved conflicts",
    )));
    h.push_str(
        " in this file. This is a committed state, not a failure: work can be built on top \
         of it, and resolving it is a later commit that links back to this one.</p>",
    );
    for region in regions {
        match region {
            Region::Plain { first, lines } => {
                if lines.iter().all(|line| line.trim().is_empty()) {
                    continue;
                }
                h.push_str("<pre class=\"code\">");
                for (n, line) in lines.iter().enumerate() {
                    h.push_str("<span class=\"ln\">");
                    h.push_str(&(first + n).to_string());
                    h.push_str("</span>");
                    h.push_str(&esc(line));
                    h.push('\n');
                }
                h.push_str("</pre>");
            }
            Region::Conflict {
                first,
                left_label,
                right_label,
                base,
                left,
                right,
            } => {
                h.push_str("<div class=\"conflict\"><div class=\"conflict-head\">");
                h.push_str("<span class=\"mark\">");
                h.push_str(CONFLICT_GLYPH);
                h.push_str(" conflict</span><span>line ");
                h.push_str(&first.to_string());
                h.push_str("</span></div>");
                h.push_str("<div class=\"sides\">");
                // Read once and handed to all three, because it is the
                // property that makes the view readable rather than a
                // per-column detail: every side is padded to the longest
                // one, so row N here is beside row N there.
                let rows = rows_of(base, left, right);
                // Base first, because it is what both sides changed and
                // is the only column that answers "what was there
                // before". A conflict view that omits it shows a reader
                // two alternatives and no way to judge between them.
                match base {
                    Some(lines) => side(h, "side-base", "base", lines, rows),
                    None => {
                        h.push_str("<div class=\"side side-base\"><h4>base</h4>");
                        h.push_str("<pre><span class=\"row\">not recorded</span></pre></div>");
                    }
                }
                side(h, "side-left", left_label, left, rows);
                side(h, "side-right", right_label, right, rows);
                h.push_str("</div></div>");
            }
        }
    }
}

/// How many rows every column of one conflict is padded to.
///
/// Alignment across the three columns is the whole point of the view, so
/// row N in one column must be beside row N in the next. The columns are
/// different lengths — that is what a conflict *is* — so the short ones
/// are padded with blank rows rather than allowed to end early and let
/// the next column's content slide up beside the wrong line.
fn rows_of(base: &Option<Vec<&str>>, left: &[&str], right: &[&str]) -> usize {
    base.as_ref()
        .map_or(0, Vec::len)
        .max(left.len())
        .max(right.len())
}

/// One column of a conflict: a label and exactly `rows` lines.
fn side(h: &mut String, class: &str, label: &str, lines: &[&str], rows: usize) {
    h.push_str("<div class=\"side ");
    h.push_str(class);
    h.push_str("\"><h4>");
    // An unlabelled side is still a side. Git labels the two outer
    // markers with refs, but a hand-written conflict or an unusual merge
    // driver may not, and a heading reading as blank is worse than one
    // naming which side it is.
    h.push_str(&esc(if label.is_empty() { "unnamed" } else { label }));
    h.push_str("</h4><pre>");
    for n in 0..rows {
        match lines.get(n) {
            Some(line) if !line.is_empty() => {
                h.push_str("<span class=\"row has\">");
                h.push_str(&esc(line));
            }
            // An empty line the side genuinely has, and a row the side
            // does not reach, are different facts and are drawn
            // differently: the first is content, the second is absence.
            Some(_) => h.push_str("<span class=\"row has\">"),
            None => h.push_str("<span class=\"row gap\">"),
        }
        h.push_str("</span>");
    }
    h.push_str("</pre></div>");
}

/// The one drawn glyph on this surface: a conflict marker.
///
/// Inline SVG, authored here, sized in `em` so it rides the text it sits
/// beside. It is drawn rather than fetched for the reason nothing else
/// here is fetched — the node's `default-src 'none'` blocks an image and
/// a font file would be a file to ship — and it is the only one, because
/// a set of icons is a vocabulary a reader has to learn and this surface
/// would rather spend the same space on a word.
///
/// Two paths diverging from one: the shape of the thing it marks.
const CONFLICT_GLYPH: &str = "<svg viewBox=\"0 0 16 16\" width=\"1em\" height=\"1em\" \
     aria-hidden=\"true\" focusable=\"false\">\
     <path d=\"M7 1h2v5H7z\"/>\
     <path d=\"M8 5.5 3.5 10v5h2v-4.2L8 8.3l2.5 2.5V15h2v-5z\"/></svg>";

/// Recent history.
fn commits(
    dir: &Path,
    repo: &str,
    rev: &str,
    chrome: Chrome<'_>,
    origin: Option<&str>,
) -> Rendered {
    let oid = match resolve(dir, rev) {
        Ok(oid) => oid,
        Err(why) => return missing(repo, rev, &why, chrome),
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
        Err(why) => return missing(repo, rev, &why, chrome),
    };
    let rows: Vec<&str> = log.lines().collect();
    let truncated = rows.len() > COMMIT_PAGE;

    let mut h = shell(&format!("{repo}: commits"), Bar::repo(repo, rev, chrome));
    repo_header(&mut h, repo, rev, &oid, "", "commits", origin);
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
        html: close(h, chrome.signed_in),
    }
}

/// One commit, with its diff.
fn commit(dir: &Path, repo: &str, oid: &str, chrome: Chrome<'_>, origin: Option<&str>) -> Rendered {
    let format = "--format=%H%x1f%an%x1f%aI%x1f%s%x1f%b";
    let header = match git_text(dir, &["show", "--no-patch", format, oid]) {
        Ok(text) => text,
        Err(why) => return missing(repo, oid, &why, chrome),
    };
    let mut fields = header.split('\u{1f}');
    let id = fields.next().unwrap_or(oid).trim().to_string();
    let author = fields.next().unwrap_or("").to_string();
    let when = fields.next().unwrap_or("").to_string();
    let subject = fields.next().unwrap_or("").to_string();

    let mut h = shell(
        &format!("{repo}: {}", &id[..id.len().min(12)]),
        Bar::repo(repo, oid, chrome),
    );
    repo_header(&mut h, repo, &id, &id, "", "commit", origin);
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
        html: close(h, chrome.signed_in),
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
    // A pane with nothing in it is not information, it is furniture. A
    // node with no platform has no reviews to have, and a repository with
    // an empty queue is saying so by having no rail -- neither is news
    // worth a card, and both used to take a share of the page from the
    // listing, which is the thing the reader came for. What this node
    // offers still belongs somewhere a reader can find it, and that is
    // `/status`, which answers about the node rather than about every
    // repository on it.
    let Some(platform) = platform else {
        return;
    };
    let all = platform.reviews_for_repo(repo);
    // Open and settled are different questions, and one total answers
    // neither: a repository with forty archived reviews and nothing in
    // flight is quiet, and a bare "40" says the opposite.
    let (archived, open): (Vec<_>, Vec<_>) = all
        .iter()
        .partition(|(_, r)| r["archived"].as_bool().unwrap_or(false));
    if open.is_empty() && archived.is_empty() {
        return;
    }
    h.push_str("<aside class=\"pane pane-reviews\"><h2>Reviews");
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
        h.push_str("<p class=\"muted\">No open reviews.</p>");
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
        h.push_str("\" title=\"");
        h.push_str(&esc(id));
        h.push_str("\">");
        h.push_str(&esc(&short_change_id(id)));
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
/// The three commands that turn a clone into a proposal.
///
/// Generated from the CLI's own command table (see
/// `choir_cli::surface::contribute_html`) and included at build time, so
/// this page cannot document a `choir` other than the one it ships
/// beside. That matters more here than anywhere else on the browse
/// surface: its entire readership is people with no way to tell a stale
/// flag from a current one.
///
/// Served per repository rather than once per node because the two
/// concrete things a newcomer needs — the clone URL and the branch they
/// are proposing onto — are properties of the repository, and a page
/// that made them fill those in themselves would be prose rather than
/// instructions.
const CONTRIBUTE_STEPS: &str = include_str!("contribute.html");

fn contribute(
    root: &Path,
    repo: &str,
    chrome: Chrome<'_>,
    origin: Option<&str>,
    self_service: bool,
) -> Rendered {
    let mut h = shell(
        &format!("{repo}: how to contribute"),
        Bar::repo(repo, "HEAD", chrome),
    );
    h.push_str("<header class=\"top\"><h1><a href=\"/r/");
    h.push_str(&esc(repo));
    h.push_str("\">");
    h.push_str(&esc(repo));
    h.push_str("</a></h1><div class=\"sub\"><span class=\"pill\">how to contribute</span>");
    h.push_str("<span class=\"pill\"><a href=\"/r/");
    h.push_str(&esc(repo));
    h.push_str("/reviews\">reviews</a></span></div></header><main id=\"main\"><section>");
    h.push_str(
        "<p class=\"lede\">There is no fork and no pull request. You push a branch and the \
         node opens a proposal for it, drawing your reviewers itself. Three commands, and \
         only the third is run more than once.</p>",
    );

    // The node's own address, so the commands are copyable rather than
    // illustrative. `NODE` is left standing when this page is served
    // from a request that carried no Host header, which is a client
    // problem and reads as one -- better than substituting a guess a
    // reader would paste.
    // Named from the repository rather than assumed to be `main`: a
    // page that prints a branch this repository does not have is a page
    // whose one copyable command fails.
    let default_onto = default_branch(&bare(root, repo)).unwrap_or_else(|| "main".to_string());
    let steps = CONTRIBUTE_STEPS
        .replace("NODE", &esc(&node_url(origin)))
        .replace("REPO", &esc(repo));
    h.push_str("<ol class=\"steps\">");
    h.push_str(&steps);
    h.push_str("</ol>");

    h.push_str(
        "<h2>Or: git and nothing else</h2>\
         <p>If you would rather not install anything, push to the magic refspec. The node \
         opens the review and draws the reviewers itself. Your own name comes first, then \
         a topic: together they make the proposal yours rather than a ref shared with \
         everyone else proposing onto the same branch, so both are required. A contributor \
         holding <code>propose</code> is held to this; anyone with <code>write</code> is \
         not, and should still follow it.</p>",
    );
    h.push_str("<pre class=\"cmd\">git push origin HEAD:refs/for/");
    h.push_str(&esc(&default_onto));
    h.push_str("/your-name/my-topic</pre>");
    h.push_str(
        "<p class=\"muted\">Pushing the same topic again updates that proposal. You still \
         need a credential to push, which is what step 1 gets you.</p>",
    );

    h.push_str("<h2>Three things that are not like GitHub</h2><ul>");
    for (what, why) in [
        (
            "You do not choose your reviewers.",
            "Name none and the node draws them. A review with no reviewers never counts as \
             approved, and it will not draw anyone who shares your operator prefix.",
        ),
        (
            "A merge conflict is a value, not an error.",
            "A conflicted merge is a committed state you can build on. Commit it, keep \
             working, and resolve it in a later commit.",
        ),
        (
            "Never force-push over a rejection.",
            "Every push is compare-and-set against one total order. A rejection means \
             somebody moved the ref first: fetch, rebase, and propose again.",
        ),
    ] {
        h.push_str("<li><strong>");
        h.push_str(what);
        h.push_str("</strong> ");
        h.push_str(why);
        h.push_str("</li>");
    }
    h.push_str("</ul>");
    // Two different nodes, and the difference is not cosmetic: on a node
    // with no credential self-service, step 1 names a command that has no
    // endpoint to call. Printing it anyway would send a newcomer to a
    // failure whose cause is the operator's configuration and whose
    // symptom looks like their own mistake.
    if self_service {
        crate::ui::next_action(
            &mut h,
            "No invite yet? Admission is invite-only and there is no registration to fill in — \
             ask the operator of this node for one. That is the only step here a person has to \
             perform for you.",
        );
    } else {
        crate::ui::next_action(
            &mut h,
            "This node issues no invites, so <code>choir join</code> has nothing to redeem \
             here. Ask the operator for a credential and to register your key — \
             <code>choir key &lt;key-file&gt; &lt;your-channel&gt;</code> prints the line they \
             need. Steps 2 and 3 are unchanged once you hold one.",
        );
    }
    h.push_str("</section>");
    Rendered {
        status: 200,
        etag: None,
        html: close(h, chrome.signed_in),
    }
}

/// The node address to print in the instructions.
///
/// Taken from the origin the reader reached this page on, because that
/// is the one address known to work for them: a node behind a reverse
/// proxy, on a private network, or under a name this process has never
/// been told does not know its own public URL, and a guess printed into
/// a copyable command is worse than no command.
///
/// `None` is a request that carried no `Host`, which HTTP/1.1 requires.
/// It leaves a visibly unfinished placeholder rather than a plausible
/// address, because a reader who pastes the placeholder gets an error
/// and a reader who pastes a wrong host gets a mystery.
fn node_url(origin: Option<&str>) -> String {
    origin.map_or_else(|| "<this node>".to_string(), ToString::to_string)
}

fn reviews(
    repo: &str,
    platform: Option<&crate::platform::Platform>,
    chrome: Chrome<'_>,
) -> Rendered {
    let Some(platform) = platform else {
        return unavailable(repo, chrome);
    };
    let rows = platform.reviews_for_repo(repo);
    // Reviewer seats are channels, and a channel is an opaque handle
    // after D46, so the list resolves them the same way the D28 page
    // does. This page carries no `ETag`, so unlike that one it has no
    // cache identity to fold the store generation into.
    let (_, roster) = platform.roster();

    let mut h = shell(&format!("{repo}: reviews"), Bar::repo(repo, "HEAD", chrome));
    h.push_str("<header class=\"top\"><h1><a href=\"/r/");
    h.push_str(&esc(repo));
    h.push_str("\">");
    h.push_str(&esc(repo));
    h.push_str("</a></h1><div class=\"sub\"><span class=\"pill\">");
    h.push_str(&plural(rows.len(), "review", "reviews"));
    h.push_str("</span><span class=\"pill\"><a href=\"/r/\">all repositories</a></span>");
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
            h.push_str("\" title=\"");
            h.push_str(&esc(id));
            h.push_str("\">");
            h.push_str(&esc(&short_change_id(id)));
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
        html: close(h, chrome.signed_in),
    }
}

/// The three facts about the *reader* that the review page needs.
///
/// Grouped for the reason [`Viewer`] and [`Chrome`] are: they arrive
/// together, they are all about who is asking rather than about the
/// review, and passing them one at a time is how a signature grows until
/// nobody can read a call site. `owners` was the eighth argument, which
/// is where clippy stops accepting the habit and it is right to.
#[derive(Clone, Copy)]
struct Reader<'a> {
    /// The authenticated reader, whose seat on the review decides
    /// whether the verdict controls render.
    user: &'a str,
    /// Whether the browser may offer mutation controls at all.
    browser_writes: bool,
    /// Who holds `own` over a repository. See [`Viewer::owners`].
    owners: &'a dyn Fn(&str) -> Vec<String>,
}

/// Every review this reader was drawn for and has not answered.
///
/// The reviewer's own queue. It existed in the view from the day reviews
/// did and on no page: the only way to find a review you had been asked
/// about was to already know which repository it was against, which is
/// exactly the thing a person coming back after a day away does not
/// know. It is the second link in the bar for that reason.
///
/// Filtered twice. [`crate::platform::Platform::reviews_awaiting`] does
/// the "drawn, unanswered, live" half, and `readable` does the
/// authorization half here, where the ACL already lives — a reader is
/// never shown a review against a repository they may not read, even one
/// they were somehow drawn for.
fn owed(
    readable: &dyn Fn(&str) -> bool,
    platform: Option<&crate::platform::Platform>,
    user: &str,
    chrome: Chrome<'_>,
) -> Rendered {
    let mut h = shell("choir: reviews you owe", Bar::index(chrome));
    h.push_str("<header class=\"top\"><h1>Reviews you owe</h1>");
    let Some(platform) = platform else {
        h.push_str("</header><main id=\"main\"><section>");
        h.push_str(
            "<p class=\"lede\">This node serves git, but its platform API is switched off, \
             so it holds no reviews to owe you.</p>",
        );
        h.push_str("</section>");
        return Rendered {
            status: 200,
            etag: None,
            html: close(h, chrome.signed_in),
        };
    };
    let rows: Vec<(String, String, serde_json::Value)> = platform
        .reviews_awaiting(user)
        .into_iter()
        .filter(|(_, repo, _)| readable(repo))
        .collect();
    h.push_str("<div class=\"sub\"><span class=\"pill\">");
    h.push_str(&plural(rows.len(), "review", "reviews"));
    h.push_str(" waiting on you</span></div>");
    h.push_str("</header><main id=\"main\"><section>");
    if rows.is_empty() {
        // Not the same sentence as "there are no reviews". A person with
        // an empty queue has answered everything they were asked, and
        // the page should say that rather than look broken.
        h.push_str(
            "<p class=\"lede\">Nothing is waiting on you. A review appears here when you \
             are drawn for it and leaves the moment you answer.</p>",
        );
    } else {
        h.push_str("<table><thead><tr><th>review</th><th>repository</th><th>onto</th>");
        h.push_str("<th>state</th><th>answered</th></tr></thead><tbody>");
        for (id, repo, review) in &rows {
            h.push_str("<tr><td><a href=\"/r/");
            h.push_str(&esc(repo));
            h.push_str("/review/");
            h.push_str(&esc(id));
            h.push_str("\" title=\"");
            h.push_str(&esc(id));
            h.push_str("\">");
            h.push_str(&esc(&short_change_id(id)));
            h.push_str("</a></td><td><a href=\"/r/");
            h.push_str(&esc(repo));
            h.push_str("\">");
            h.push_str(&esc(repo));
            h.push_str("</a></td><td class=\"mono muted\">");
            h.push_str(&esc(ref_name(review)));
            h.push_str("</td><td>");
            state_tag(&mut h, review);
            h.push_str("</td><td class=\"num\">");
            // How far along the rest of the review is, so a reader can
            // tell "waiting only on me" from "nobody has looked".
            let assigned = review["reviewers"].as_array().cloned().unwrap_or_default();
            let answered = assigned
                .iter()
                .filter_map(serde_json::Value::as_str)
                .filter(|who| review["verdicts"][who]["verdict"].as_str().is_some())
                .count();
            h.push_str(&answered.to_string());
            h.push_str(" of ");
            h.push_str(&assigned.len().to_string());
            h.push_str("</td></tr>");
        }
        h.push_str("</tbody></table>");
    }
    h.push_str("</section>");
    Rendered {
        status: 200,
        etag: None,
        html: close(h, chrome.signed_in),
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
    reader: Reader<'_>,
    chrome: Chrome<'_>,
) -> Rendered {
    let Reader {
        user,
        browser_writes,
        owners,
    } = reader;
    let Some(platform) = platform else {
        return unavailable(repo, chrome);
    };
    let Some(state) = platform.review_json(id) else {
        return no_such_review(repo, id, chrome);
    };
    let commit_oid = git_oid_of(state["target"].as_str().unwrap_or("")).unwrap_or_default();
    // Reviewer seats and comment authors are channels, and a channel is
    // an opaque handle after D46. This page is where a person reads who
    // said what, so it resolves them the same way the D28 page does.
    let (_, roster) = platform.roster();

    let mut h = shell(
        &format!("{repo}: review {id}"),
        Bar::repo(repo, "HEAD", chrome),
    );
    h.push_str("<header class=\"top\"><h1><a href=\"/r/");
    h.push_str(&esc(repo));
    h.push_str("\">");
    h.push_str(&esc(repo));
    h.push_str("</a> · review ");
    // The heading names the review; the `change` row below carries the id
    // whole. A D52 id ends in a 64-character fingerprint, and a title
    // that is mostly fingerprint says nothing at a glance while pushing
    // the repository it belongs to off the first line.
    h.push_str(&esc(&short_change_id(id)));
    h.push_str("</h1><div class=\"sub\">");
    state_tag(&mut h, &state);
    h.push_str("<span class=\"pill\">weight ");
    h.push_str(&esc(&state["approval_weight"].to_string()));
    h.push_str("</span><span class=\"pill\"><a href=\"/r/");
    h.push_str(&esc(repo));
    h.push_str("/reviews\">all reviews</a></span>");
    h.push_str("</div></header><main id=\"main\">");

    // ---------------------------------------------------------------
    // ABOVE THE FOLD. Four things, in the order a reviewer needs them:
    // what is proposed, who was asked, what would authorize the landing,
    // and — if this reader is one of the people being asked — the two
    // buttons. The diff follows, and the discussion follows that.
    //
    // The order these used to be in was proposal, reviewers,
    // *discussion*, diff, controls. So a reviewer arriving to answer had
    // to scroll past everybody else's comments to reach the change, and
    // past the change to reach the buttons: the two things the page
    // exists for were its last two sections.
    // ---------------------------------------------------------------

    // The relationship, as a sentence. It was four labelled cells in a
    // two-column table, which made a reader assemble "this commit, onto
    // that ref" out of parts instead of reading it.
    h.push_str("<section><h2>Proposal</h2>");
    h.push_str("<p class=\"proposal\">Land <a class=\"mono\" href=\"/r/");
    h.push_str(&esc(repo));
    h.push_str("/commit/");
    h.push_str(&esc(&commit_oid));
    h.push_str("\">");
    h.push_str(&esc(&short_oid(&commit_oid)));
    h.push_str("</a>");
    let onto_ref = ref_name(&state);
    if onto_ref.is_empty() {
        h.push_str(" — this review names no destination ref.");
    } else {
        h.push_str(" onto <span class=\"mono\">");
        h.push_str(&esc(onto_ref));
        h.push_str("</span>.");
    }
    h.push_str("</p>");
    // The id whole, because it is what a reviewer pastes into `choir
    // verdict` and what `view.changes` is keyed by. Shortening it here
    // would be an identity change wearing a display change's clothes.
    h.push_str("<table class=\"kv\"><tbody><tr><td>change</td><td class=\"mono\">");
    h.push_str(&esc(id));
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
        h.push_str("<table class=\"who\"><thead><tr><th>reviewer</th><th>verdict</th>");
        h.push_str("<th>note</th></tr></thead><tbody>");
        for who in reviewers.iter().filter_map(serde_json::Value::as_str) {
            let verdict = &state["verdicts"][who];
            let slashed = state["slashes"]
                .get(who)
                .and_then(serde_json::Value::as_str);
            // The reviewer's name links to what this node records about
            // them (D63). This is the page the question "who is this and
            // why should their approval count" is asked from, so it is
            // the page the answer has to be one click from; a profile
            // nobody can reach from a verdict is a profile nobody reads.
            h.push_str("<tr><td class=\"mono\"><a href=\"/p/");
            h.push_str(&esc(&url_path(who)));
            h.push_str("\">");
            h.push_str(&esc(&crate::ui::person(who, &roster)));
            h.push_str("</a></td><td class=\"said\">");
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
            h.push_str("</td><td class=\"remark\">");
            h.push_str(&esc(verdict["note"].as_str().unwrap_or("")));
            h.push_str("</td></tr>");
        }
        h.push_str("</tbody></table>");
    }
    authority(&mut h, repo, &state, owners, &roster);
    h.push_str("</section>");

    // The two buttons, above the diff rather than after it. A reviewer
    // who has read the change scrolls back up to answer, which is one
    // scroll rather than two and is also where they were when they
    // decided. `write_sections` returns them and the comment box as
    // separate fragments precisely so the two can sit in different
    // places on the page.
    let (verdict_html, comment_html) = if browser_writes {
        write_sections(id, &state, user)
    } else {
        (
            String::new(),
            "<p class=\"note\">This beta keeps browser access read-only. Use the signed CLI \
             for verdicts and comments.</p>"
                .to_string(),
        )
    };
    h.push_str(&verdict_html);

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

    // The discussion (D38), under the diff. A comment here is about the
    // change as a whole, so it belongs after the thing it is about; a
    // column of comments beside a patch competes with the patch for the
    // same eye, and loses, which is the worst of both.
    //
    // Rendered, never accepted: a comment is a signed operation, so the
    // only way one reaches the log is the submit endpoint. This surface
    // stays read-only, exactly as D34 built it.
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
        // A thread, not a table. Each comment is a paragraph somebody
        // wrote, and a three-column grid set it as a database row: the
        // author in a monospace cell, the body wrapped to whatever width
        // was left, and the sequence number given a column of its own at
        // the front, where the eye lands first.
        h.push_str("<ol class=\"thread\">");
        for comment in &comments {
            h.push_str("<li><div class=\"who\"><span class=\"name\">");
            h.push_str(&esc(&crate::ui::person(
                comment["author"].as_str().unwrap_or(""),
                &roster,
            )));
            // The op position stays on the page — it is what orders the
            // thread and what a reader cites — but as the quiet half of
            // a byline rather than as the first column of a table.
            h.push_str("</span><span class=\"mono\">op ");
            h.push_str(&esc(&comment["at"].to_string()));
            h.push_str("</span></div><p class=\"body\">");
            h.push_str(&esc(comment["body"].as_str().unwrap_or("")));
            h.push_str("</p></li>");
        }
        h.push_str("</ol>");
    }
    h.push_str(&comment_html);
    h.push_str("</section>");
    // One script element, if either write affordance rendered. It is
    // emitted here rather than inside either fragment so that a page
    // carrying both does not load it twice.
    if wants_ceremony(&verdict_html, &comment_html) {
        h.push_str(crate::ui::CEREMONY_SCRIPT);
    }
    Rendered {
        status: 200,
        etag: None,
        html: close(h, chrome.signed_in),
    }
}

/// Both write affordances, as two fragments the caller places
/// separately: the verdict controls belong above the diff and the
/// comment box belongs at the end of the thread, which is under it.
///
/// A reader with no verdict to cast and no right to comment gets two
/// empty strings, and `review` then emits no script element and makes no
/// request. That is the read surface's guarantee, and it is a function
/// rather than four lines inside `review` so a test can hold it without
/// standing up a platform.
///
/// It stopped emitting the script itself when the two halves stopped
/// being adjacent: the one element has to be written once, by whoever
/// knows both fragments landed, and that is now the caller.
fn write_sections(id: &str, state: &serde_json::Value, user: &str) -> (String, String) {
    let mut verdict = String::new();
    let mut comment = String::new();
    verdict_buttons(&mut verdict, id, state, user);
    comment_box(&mut comment, id, state, user);
    (verdict, comment)
}

/// Whether a review page needs the one `<script>` element at all.
///
/// The read surface's guarantee is that a reader with no verdict to cast
/// and no right to comment gets a page that fetches nothing: no element
/// and no request. That used to be `h.len() > before` inside
/// [`write_sections`], which stopped working the moment the two
/// fragments went to different places on the page.
///
/// The comment half is matched on its element id rather than on being
/// non-empty, because the read-only fallback is also a non-empty string
/// — a sentence naming the CLI — and a sentence needs no script.
fn wants_ceremony(verdict: &str, comment: &str) -> bool {
    !verdict.is_empty() || comment.contains("id=\"comment\"")
}

/// What would authorize this landing, in one plain sentence (D42, D43).
///
/// Two rules, and which one applies is a property of the *repository*
/// rather than of the actor: somebody owns it and an owner's assent is
/// necessary and sufficient, or nobody does and approval weight decides.
/// A reviewer looking at a page of verdicts cannot tell which of those
/// they are participating in, and the difference is whether their
/// approval can ever be what lands the change.
///
/// **This describes the rule; it does not decide anything.** The one
/// function that admits a landing is `Platform::authorization_for`, and
/// it runs at submit time against the ACL as of that moment. The
/// sentence here is written in those terms deliberately — "an owner's
/// assent lands this", not "this is authorized" — so that a page which
/// is a few seconds stale about a grant is still saying something true.
fn authority(
    h: &mut String,
    repo: &str,
    state: &serde_json::Value,
    owners: &dyn Fn(&str) -> Vec<String>,
    roster: &crate::ui::Roster,
) {
    let owners = owners(repo);
    h.push_str("<p class=\"authority\">");
    if owners.is_empty() {
        // The pre-D42 rule, still in force wherever nobody has been
        // granted `own`. The required weight is not rendered from a
        // constant here because the page has no honest access to the
        // node's own threshold; the weight standing is what the view
        // holds, and the sentence says what it is rather than judging it.
        h.push_str(
            "Nobody holds <b>own</b> on this repository, so approval weight is what \
             lands it: this review stands at <b>",
        );
        h.push_str(&esc(&state["approval_weight"].to_string()));
        h.push_str("</b>, from approvals by distinct operators.");
    } else {
        // Which of the owners has already said yes, if any. A reader
        // whose own approval cannot land the change should be able to
        // see who is actually being waited on.
        let assented: Vec<String> = owners
            .iter()
            .filter(|owner| {
                state["verdicts"][owner.as_str()]["verdict"].as_str() == Some("Approve")
            })
            .map(|owner| crate::ui::person(owner, roster))
            .collect();
        h.push_str(
            "This repository has an owner, so an owner's assent is what lands it and no \
             amount of other approval substitutes. ",
        );
        // Each name is escaped and *then* joined with markup. Escaping
        // the joined string instead would render the separator as
        // literal angle brackets, which is the bug this comment exists
        // to stop somebody re-introducing while tidying.
        let names = |list: &[String]| {
            list.iter()
                .map(|one| format!("<b>{}</b>", esc(one)))
                .collect::<Vec<_>>()
                .join(", ")
        };
        if assented.is_empty() {
            // "Waiting on ana." and "Waiting on ana, bo." — one form that
            // is grammatical at every count. The first draft wrote "None
            // of ana has approved yet", which is correct English only
            // when there are at least two of them and reads as a bug
            // when there is one, which is the ordinary case.
            h.push_str("Waiting on ");
            h.push_str(&names(&owners));
            h.push('.');
        } else {
            h.push_str("Approved by ");
            h.push_str(&names(&assented));
            h.push('.');
        }
    }
    h.push_str("</p>");
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
    h.push_str("<div class=\"verdicts\">");
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
        h.push_str("</button>");
    }
    h.push_str("</div>");
    h.push_str("<p id=\"verdict-said\" class=\"note\" hidden></p></div>");
    h.push_str("</section>");
}

/// The comment box (D39 × D38), offered to anyone who may write here
/// rather than to reviewers only.
///
/// Discussion is deliberately wider than judgement: a verdict is an
/// authorization and belongs to the people asked for one, while a comment
/// is a statement and belongs to anyone the ACL already lets read this
/// repository (D55). The node makes that call at `/api/submit`; the page
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
    // No `<section>` of its own: this lands at the end of the thread,
    // inside the Discussion section, because "say something" is the last
    // turn of the conversation above it rather than a separate topic.
    h.push_str("<h3>Say something</h3>");
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
fn unavailable(repo: &str, chrome: Chrome<'_>) -> Rendered {
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
            chrome,
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
/// A channel name out of a `/p/` path, or `None` when it is not one.
///
/// A channel is `owner/name` in some deployments and one segment in
/// others, so the whole remainder is taken and only decoded -- but it is
/// decoded per segment, which is what keeps a `%2f` from inventing a
/// boundary, and it is length-capped because it is about to be compared
/// against every key in the view.
pub(crate) fn decode_channel(rest: &str) -> Option<String> {
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() || rest.len() > 128 {
        return None;
    }
    let mut out = Vec::new();
    for segment in rest.split('/') {
        out.push(decode(segment)?);
    }
    Some(out.join("/"))
}

/// The page for a name this node has nothing to say about.
///
/// Separate from [`no_such_repository`] because the two are different
/// questions with different fixes, and shared with the malformed-name
/// case on purpose: a reader is never told which of the two they hit.
pub(crate) fn no_such_actor(chrome: Chrome<'_>) -> Rendered {
    let chrome = chrome.refusal();
    Rendered {
        status: 404,
        etag: None,
        html: crate::ui::refusal(
            "No actor here",
            404,
            &crate::ui::Refusal {
                code: "no_such_actor",
                error: "This node has nothing recorded under that name, or nothing this \
                        credential may see. The two look identical from here, on purpose.",
                expected: Some("the name an actor signs as"),
                actual: None,
                next: "Open a review and follow the link on a verdict — every name on a \
                       review page is one this node can tell you about.",
            },
            &[],
            chrome,
        ),
    }
}

/// One actor's standing as a page (D63).
///
/// Takes the profile document rather than building it, so the page and
/// `/api/profile` are the same reading of the same view. Two surfaces
/// counting separately is how they come to disagree, and here a
/// disagreement would be a disclosure: the ACL narrowing lives in the
/// view this was derived from, not in either renderer.
///
/// It is deliberately a table of inputs and not a verdict. The question
/// a reader brings is "should I trust this reviewer", and the honest
/// answer is the inputs and their units. Two of D24's four now exist --
/// key age and the vouch graph (D65) -- and a score would still be a
/// weighting of one against the other that nobody has measured.
pub(crate) fn profile_page(
    channel: &str,
    profile: &serde_json::Value,
    chrome: Chrome<'_>,
) -> Rendered {
    let n = |path: &[&str]| -> u64 {
        let mut at = profile;
        for step in path {
            at = &at[*step];
        }
        at.as_u64().unwrap_or_default()
    };
    let known = profile["known"].as_bool().unwrap_or_default();

    let mut h = shell(channel, Bar::index(chrome));
    h.push_str("<header class=\"top\"><h1>");
    h.push_str(&esc(channel));
    h.push_str("</h1><div class=\"sub\">");
    if known {
        h.push_str("<span class=\"pill\">known to this node</span>");
    } else {
        h.push_str("<span class=\"pill\">no record</span>");
    }
    h.push_str("</div></header><main id=\"main\">");

    if !known {
        h.push_str(
            "<section><p class=\"empty\">This node holds no key, change, review or check \
             for that name. That is not the same as an actor who has done nothing: a name \
             you cannot see the records of looks identical from here.</p></section>",
        );
        return Rendered {
            status: 200,
            etag: None,
            html: close(h, chrome.signed_in),
        };
    }

    h.push_str("<section><h2>keys</h2>");
    match profile["keys"].as_array() {
        Some(keys) if !keys.is_empty() => {
            h.push_str("<table><thead><tr><th>key</th><th>operator</th><th>bound at</th>");
            h.push_str("<th>ops since</th><th>state</th></tr></thead><tbody>");
            for key in keys {
                h.push_str("<tr><td><code>");
                h.push_str(&esc(key["key"].as_str().unwrap_or("?")));
                h.push_str("</code></td><td>");
                h.push_str(&esc(key["operator"].as_str().unwrap_or("?")));
                h.push_str("</td><td>");
                h.push_str(&key["bound_at"].as_u64().unwrap_or_default().to_string());
                h.push_str("</td><td>");
                h.push_str(
                    &key["ops_since_binding"]
                        .as_u64()
                        .unwrap_or_default()
                        .to_string(),
                );
                h.push_str("</td><td>");
                if key["revoked"].is_null() {
                    h.push_str("<b class=\"tag ok\">live</b>");
                } else {
                    h.push_str("<b class=\"tag bad\">revoked</b>");
                }
                h.push_str("</td></tr>");
            }
            h.push_str("</tbody></table>");
            // The unit, said once, where the number is. "Ops since" is
            // meaningless without it and misleading with the wrong one:
            // a node idle for a month and a node that took a thousand
            // pushes in an hour are not comparable on a clock.
            h.push_str(
                "<p class=\"lede\">Age is counted in sequenced ops, the only clock the log \
                 has. It orders keys by standing in a way a replay reproduces exactly.</p>",
            );
        }
        _ => h.push_str("<p class=\"empty\">No key bound to this name, or none you may see.</p>"),
    }
    h.push_str("</section>");

    h.push_str("<section><h2>record</h2><table><tbody>");
    for (label, value) in [
        ("changes owned", n(&["changes", "owned"])),
        ("reviews assigned", n(&["reviews", "assigned"])),
        ("approved", n(&["reviews", "approved"])),
        ("changes requested", n(&["reviews", "changes_requested"])),
        ("approvals slashed", n(&["reviews", "slashed"])),
        ("comments written", n(&["reviews", "comments"])),
        ("checks reported", n(&["checks", "reported"])),
        ("checks failed", n(&["checks", "failed"])),
    ] {
        h.push_str("<tr><td>");
        h.push_str(label);
        h.push_str("</td><td>");
        h.push_str(&value.to_string());
        h.push_str("</td></tr>");
    }
    h.push_str("</tbody></table>");
    h.push_str(
        "<p class=\"lede\">Counted from the view this credential may read, so another \
         reader may see different numbers for the same actor.</p>",
    );
    h.push_str("</section>");

    // D65. The heading names the operator rather than the channel: a
    // vouch is an edge between operator identities, so a page about
    // `ops/agent` is showing what was vouched to `ops`, and a reader who
    // had to work that out from a name that does not match the heading
    // would reasonably read it as somebody else's record.
    let operator = profile["vouches"]["operator"].as_str().unwrap_or(channel);
    h.push_str(
        "<section><h2>vouches</h2><p class=\"lede\">Vouches are between operators. \
                These are the ones held by <code>",
    );
    h.push_str(&esc(operator));
    h.push_str("</code>.</p>");
    match profile["vouches"]["received"].as_array() {
        Some(received) if !received.is_empty() => {
            h.push_str("<table><thead><tr><th>voucher</th><th>at</th><th>note</th>");
            h.push_str("<th>direction</th></tr></thead><tbody>");
            for edge in received {
                h.push_str("<tr><td class=\"mono\"><a href=\"/p/");
                h.push_str(&esc(edge["voucher"].as_str().unwrap_or("")));
                h.push_str("\">");
                h.push_str(&esc(edge["voucher"].as_str().unwrap_or("?")));
                h.push_str("</a></td><td>");
                h.push_str(&edge["at"].as_u64().unwrap_or_default().to_string());
                h.push_str("</td><td>");
                h.push_str(&esc(edge["note"].as_str().unwrap_or("")));
                h.push_str("</td><td>");
                // Shown, never scored. A mutual edge is the shape a
                // farm of identities makes and also the shape a real
                // team makes; which one a reader is looking at is their
                // call, and it is not one this page can make for them.
                if edge["reciprocal"].as_bool().unwrap_or_default() {
                    h.push_str("<b class=\"tag\">mutual</b>");
                } else {
                    h.push_str("one way");
                }
                h.push_str("</td></tr>");
            }
            h.push_str("</tbody></table>");
        }
        _ => h.push_str(
            "<p class=\"empty\">Nobody vouches for this operator on the records you may \
             read. A vouch needs both ends to hold a key bound in the log, so a node whose \
             operator has bound no keys has none to show.</p>",
        ),
    }
    h.push_str("<p class=\"lede\">Vouched for ");
    h.push_str(
        &profile["vouches"]["given"]
            .as_u64()
            .unwrap_or_default()
            .to_string(),
    );
    h.push_str(
        " other operator(s). A vouch authorizes nothing on its own: no threshold \
                reads it, and there is no score. D24 wants key age, vouches, scoped grants \
                and bonds. Bonds do not exist; scoped grants do (D66), but they live in the \
                node's authorization files rather than in the log, so no number here can \
                count them.</p></section>",
    );

    Rendered {
        status: 200,
        etag: None,
        html: close(h, chrome.signed_in),
    }
}

pub(crate) fn no_such_repository(chrome: Chrome<'_>) -> Rendered {
    // Every reader-specific fact is dropped here, and dropping it is the
    // property rather than a tidy-up: this page is rendered both for a
    // repository that does not exist and for one the reader may not
    // read, and the two must be byte-identical or a reader learns which
    // names exist by diffing them. See `Chrome::refusal` for what
    // survives and why.
    let chrome = chrome.refusal();
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
            chrome,
        ),
    }
}

/// The page for a repository with no commits yet.
///
/// Deliberately a `200`: the repository is exactly what the operator
/// asked for, and the only thing missing is a push. Saying so beats
/// reporting git's "Needed a single revision", which reads like a fault.
fn empty(repo: &str, chrome: Chrome<'_>) -> Rendered {
    // A repository with no commits has nothing to search, so the box
    // falls back to the list rather than offering an empty tree.
    let mut h = shell(&format!("{repo}: empty"), Bar::index(chrome));
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
        html: close(h, chrome.signed_in),
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
fn missing(repo: &str, rev: &str, why: &str, chrome: Chrome<'_>) -> Rendered {
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
            chrome,
        ),
    }
}

/// The page for a review id the view does not carry.
///
/// Separate from [`missing`] because a review id is not a revision: the
/// fix is a different command, and telling somebody to check their branch
/// names when they mistyped a review id wastes the trip.
fn no_such_review(repo: &str, id: &str, chrome: Chrome<'_>) -> Rendered {
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
            chrome,
        ),
    }
}

/// Document head and opening tags, shared with the D28 page so the two
/// surfaces cannot drift into looking like different products.
fn shell(title: &str, bar: Bar<'_>) -> String {
    let mut h = String::with_capacity(8 * 1024);
    h.push_str("<!doctype html><html lang=\"en\"");
    theme_attribute(&mut h, bar.theme);
    h.push_str("><head><meta charset=\"utf-8\">");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    h.push_str("<title>");
    h.push_str(&esc(title));
    h.push_str("</title>");
    // The card a link to this page renders as, wherever it is pasted.
    //
    // Every address on this surface is now shareable — D78 published the
    // repository and D76 put it at the apex — and a shared address with
    // no preview is the grey rectangle `ui::CARD` was drawn to replace.
    // The invite page has had one since it existed; the source did not,
    // which is the wrong way round for the half a stranger actually
    // reaches.
    //
    // **Not `title`.** The obvious version of this put the page's own
    // title in `og:title`, and that is a disclosure: `/p/<name>`
    // renders a narrowed profile byte-identically to an unknown one so
    // a reader cannot tell "withheld" from "no such actor", and it does
    // that by swapping the heading -- which reaches the text of the
    // page and not an attribute in its head. The name came straight
    // back out in the card. `vouches::a_per_repository_reader_is_shown_
    // no_part_of_the_graph` caught it.
    //
    // So the card names the repository, from the same field the search
    // box already scopes to, and nothing on a page that is about no
    // repository. Both are public on any page that has them, and a page
    // that must be indistinguishable from its sibling has neither.
    //
    // The description is fixed and node-wide for the reason the title is
    // not per-page: a preview is rendered by somebody else's server into
    // a channel that may hold more people than the link was sent to, and
    // the repository's own words are one fetch away for anybody who
    // opens it.
    h.push_str(
        "<meta property=\"og:type\" content=\"website\">\
         <meta property=\"og:description\" content=\"Source on a choir node. Every write is a \
         signed operation in one ordered log, and a merge conflict is a committed value.\">\
         <meta name=\"twitter:card\" content=\"summary_large_image\">\
         <meta property=\"og:title\" content=\"",
    );
    h.push_str(&esc(bar.scope.map_or("choir", |(repo, _)| repo)));
    h.push_str("\">");
    // Absolute, because the server fetching it has no page to resolve a
    // relative path against. Omitted rather than guessed when the
    // request carried no `Host`, on the same reasoning as `node_url`.
    if let Some(origin) = bar.origin {
        h.push_str("<meta property=\"og:image\" content=\"");
        h.push_str(&esc(origin));
        // A card drawn for this repository where there is one, and the
        // node's own where there is not. Both are public and neither
        // reads the repository, so this says nothing the page does not.
        match bar.scope {
            Some((repo, _)) => h.push_str(&esc(&crate::card::path(repo))),
            None => h.push_str(crate::ui::CARD_PATH),
        }
        h.push_str("\">");
    }
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
/// The three facts the fixed bar needs that are not about the page:
/// whether this node is one repository, which palette this reader
/// chose, and where they are so a palette link can bring them back.
///
/// Grouped for the reason [`Viewer`] is: they were arriving one at a
/// time as another parameter on ten signatures that pass them straight
/// through to [`Bar`], and the tenth would have been the one somebody
/// forgot on one page.
#[derive(Clone, Copy, Default)]
pub(crate) struct Chrome<'a> {
    /// The one repository this node presents, if it presents one.
    pub(crate) site: Option<&'a str>,
    /// The palette this reader chose, or `None` for the system's.
    pub(crate) theme: Option<&'a str>,
    /// The address the palette links return to.
    pub(crate) here: &'a str,
    /// Whether `/account` renders for this node, which is what decides
    /// whether the bar links to it.
    ///
    /// The page is gated on `--passkeys` (D71), so a node without them
    /// answers `503` there — and a bar that linked to it anyway would be
    /// the dead link this module's own test forbids. It is not gated on
    /// *this reader* having an account: the page is where a token for
    /// git is minted (D75), and that is the step between signing in and
    /// pushing anything, which nothing linked to before.
    pub(crate) account: bool,
    /// Whether this reader holds `@node write`, which is what `/people`
    /// is gated on.
    ///
    /// An operator who cannot reach their own console except by typing
    /// the path is the same defect as a dead link, one direction round.
    pub(crate) console: bool,
    /// Where the book for this node lives, if its operator has said.
    ///
    /// The other half of the site (D76). `None` on a node whose operator
    /// has published none, because a link to nowhere is worse than no
    /// link. The address is untracked operator state for the reason
    /// every host address here is.
    pub(crate) docs: Option<&'a str>,
    /// Whether this request carries a credential.
    ///
    /// The bar has two shapes and this is the only thing that decides
    /// between them: signed out it offers the way in, signed in it
    /// offers the three destinations a person actually uses. Before this
    /// the bar was the same either way, so a node with no ACL — where a
    /// reader without a credential is served every page — showed a
    /// stranger `account` and `people` and no way to become somebody.
    ///
    /// It is about the *request*, not about the node: `account` and
    /// `console` above answer "does this page exist here" and "does this
    /// reader hold the grant it needs", which are different questions
    /// and stay separate fields.
    ///
    /// `None` means the question has no answer on this page, and the bar
    /// then offers neither shape. That is the refusals' state: half of
    /// them are rendered for a caller who has not been identified yet,
    /// and a bar that guessed would put a `sign in` link in front of
    /// somebody already signed in, on the page where they have just been
    /// told something went wrong.
    pub(crate) signed_in: Option<bool>,
    /// The scheme and host this request arrived on, for the absolute
    /// URLs a social preview needs.
    ///
    /// A preview is fetched by somebody else's server, which has no page
    /// to resolve a relative path against, so `og:image` has to be
    /// absolute. Omitted rather than guessed when the request carried no
    /// `Host`, on the same reasoning as [`node_url`] and the invite
    /// page: this node deliberately learns no address of its own, and a
    /// guessed one in a card is a card pointing at somewhere else.
    ///
    /// Not reader-specific, so unlike every other field here it survives
    /// [`Chrome::refusal`]: two readers on the same host are owed the
    /// same card, and a refusal page that dropped it would still be
    /// byte-identical to its sibling.
    pub(crate) origin: Option<&'a str>,
}

impl<'a> Chrome<'a> {
    /// The same chrome with every reader-specific fact dropped.
    ///
    /// A refusal page is rendered both for a repository that does not
    /// exist and for one the reader may not read, and the two must be
    /// **byte-identical** or a reader learns which names exist by
    /// diffing them. That makes every field here that varies with the
    /// reader a leak: their address, whether they hold `@node write`,
    /// whether they are signed in at all.
    ///
    /// It is a constructor rather than a field list at each refusal
    /// because the property is structural. `here` was blanked by hand at
    /// both call sites and the next field to arrive — `signed_in` — was
    /// not, so a signed-in reader's refusal grew a `reviews` link an
    /// anonymous one's did not have, and the integration test that
    /// exists for exactly this caught it. Adding a field to [`Chrome`]
    /// now means deciding here whether it survives a refusal.
    ///
    /// `theme` and `docs` do survive: the palette is the reader's own
    /// choice and would flip at the worst possible moment if it were
    /// dropped, and the book's address is node-wide and identical for
    /// everybody.
    fn refusal(self) -> Chrome<'a> {
        Chrome {
            site: None,
            theme: self.theme,
            here: "",
            account: false,
            console: false,
            docs: self.docs,
            signed_in: None,
            origin: self.origin,
        }
    }
}

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
    /// The palette this reader chose, or `None` for the system's.
    pub(crate) theme: Option<&'a str>,
    /// The address the theme links return to, so choosing a palette
    /// leaves the reader on the page they were reading rather than at
    /// the front door. Empty means "the front door", which is what a
    /// page rendered outside a request has.
    pub(crate) here: &'a str,
    /// Whether to offer `/account`. See [`Chrome::account`].
    pub(crate) account: bool,
    /// Whether to offer `/people`. See [`Chrome::console`].
    pub(crate) console: bool,
    /// Where the book is, if anywhere. See [`Chrome::docs`].
    pub(crate) docs: Option<&'a str>,
    /// Whether this request carries a credential. See
    /// [`Chrome::signed_in`].
    pub(crate) signed_in: Option<bool>,
    /// Where this request arrived. See [`Chrome::origin`].
    pub(crate) origin: Option<&'a str>,
}

impl<'a> Bar<'a> {
    /// The bar for a page about no particular repository.
    pub(crate) fn index(chrome: Chrome<'a>) -> Bar<'a> {
        Bar {
            scope: None,
            q: "",
            site: false,
            theme: chrome.theme,
            here: chrome.here,
            account: chrome.account,
            console: chrome.console,
            docs: chrome.docs,
            signed_in: chrome.signed_in,
            origin: chrome.origin,
        }
    }

    /// The bar on the repository index, showing the filter in force.
    fn filtered(q: &'a str, chrome: Chrome<'a>) -> Bar<'a> {
        Bar {
            scope: None,
            q,
            site: false,
            theme: chrome.theme,
            here: chrome.here,
            account: chrome.account,
            console: chrome.console,
            docs: chrome.docs,
            signed_in: chrome.signed_in,
            origin: chrome.origin,
        }
    }

    /// The bar on a repository search, showing the term in force.
    fn searched(repo: &'a str, rev: &'a str, q: &'a str, chrome: Chrome<'a>) -> Bar<'a> {
        Bar {
            scope: Some((repo, rev)),
            q,
            site: chrome.site.is_some(),
            theme: chrome.theme,
            here: chrome.here,
            account: chrome.account,
            console: chrome.console,
            docs: chrome.docs,
            signed_in: chrome.signed_in,
            origin: chrome.origin,
        }
    }

    /// The bar for a page about one repository at one revision.
    fn repo(repo: &'a str, rev: &'a str, chrome: Chrome<'a>) -> Bar<'a> {
        Bar {
            scope: Some((repo, rev)),
            q: "",
            site: chrome.site.is_some(),
            theme: chrome.theme,
            here: chrome.here,
            account: chrome.account,
            console: chrome.console,
            docs: chrome.docs,
            signed_in: chrome.signed_in,
            origin: chrome.origin,
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
    // because there is no list to go back to. A word, not a mark: the
    // rotated square this used to draw beside it was decoration, and
    // decoration is what this surface is spending less of.
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
    // cannot perform from the page body once they are deep in a tree —
    // and, since D74 put signing in on a page of our own, the way to the
    // three destinations that finish the job. Before this the bar
    // offered `repositories` and `node` and nothing else, so a person who
    // had just signed in could reach every page about the *node* and no
    // page about *themselves*: the account that mints the token git
    // speaks was reachable only by typing `/account`.
    //
    // Two shapes, decided by `signed_in`. Signed out the bar offers the
    // one thing a stranger can do here; signed in it offers where the
    // work is: the repositories, the reviews this reader owes an answer
    // to, and their own account. `reviews` is the link this surface was
    // missing — a reviewer's own queue existed in the view and on no
    // page, so the only way to find a review you had been drawn for was
    // to already know which repository it was against.
    h.push_str("<nav class=\"chrome-nav\">");
    // No `sign in` link. The bar renders it for a reader the node has
    // identified as nobody, and after D78 that reader is the ordinary
    // case rather than the exceptional one: a stranger reading published
    // source. Offering them the way in put the most prominent control on
    // the page in front of the one person it does nothing for -- this
    // node issues no accounts from that page, so the link led to a form
    // that would refuse them.
    //
    // `/signin` is untouched and still answers, with D74's `401`, to
    // anybody who types it. That is the whole of the operator's path in
    // and the reason this is a rendering change rather than a removal:
    // what a node needs is a way in, not a way in advertised to readers
    // who have no account to sign into.
    if bar.scope.is_some() && !bar.site {
        h.push_str("<a href=\"/r/\">repositories</a>");
    }
    if bar.signed_in == Some(true) {
        h.push_str("<a href=\"/reviews\">reviews</a>");
    }
    // The node's own telemetry, gated like the queue above it. D78's
    // reachable set is an allowlist and `/status` is not in it, so a bar
    // that offered it to every stranger was offering the sign-in page
    // under another name. Publishing the page instead would be widening
    // what an unauthenticated caller reads, which is a decision for the
    // ACL and not for a link.
    if bar.signed_in == Some(true) {
        h.push_str("<a href=\"/status\">node</a>");
    }
    if bar.account && bar.signed_in == Some(true) {
        h.push_str("<a href=\"/account\">account</a>");
    }
    if bar.console {
        h.push_str("<a href=\"/people\">people</a>");
    }
    if let Some(docs) = bar.docs {
        // `rel="external"` and nothing else: the book is the other half
        // of this site (D76) and is trusted, but it is a different
        // origin, and a reader should be able to tell that from the
        // markup as well as from the address.
        h.push_str("<a class=\"docs\" rel=\"external\" href=\"");
        h.push_str(&esc(docs));
        h.push_str("\">docs</a>");
    }
    theme_control(h, bar);
    h.push_str("</nav></div></div>");
}

/// Writes ` data-theme="…"` when the reader has chosen one.
///
/// Nothing at all when they have not, which is what leaves
/// `prefers-color-scheme` in charge: the stylesheet's dark rules are
/// written as `:root:not([data-theme="dark"])` under that query, so an
/// absent attribute and a correct attribute are two different states and
/// only the absent one follows the system.
pub(crate) fn theme_attribute(h: &mut String, theme: Option<&str>) {
    if let Some(theme) = theme {
        h.push_str(" data-theme=\"");
        h.push_str(&esc(theme));
        h.push('"');
    }
}

/// The palette control: three states, the current one not a link.
///
/// Written as links rather than as a form or a toggle for the reason
/// the whole surface is: this page runs no script, a form here would be
/// a `POST` and a button in a row of navigation, and a two-state toggle
/// cannot express "follow the system", which is the default and has to
/// stay reachable.
///
/// The current state renders as `<b>` and is not clickable, so it reads
/// as the state it is without colour being the only thing saying so —
/// the same rule, and the same markup, as the search scope tabs.
fn theme_control(h: &mut String, bar: Bar<'_>) {
    let here = if bar.here.is_empty() { "/" } else { bar.here };
    h.push_str("<span class=\"themes\" role=\"group\" aria-label=\"Colour theme\">");
    for (value, label) in [("auto", "auto"), ("dark", "dark"), ("light", "light")] {
        let current = match bar.theme {
            Some(chosen) => chosen == value,
            None => value == "auto",
        };
        if current {
            h.push_str("<b class=\"theme here\" aria-current=\"true\">");
            h.push_str(label);
            h.push_str("</b>");
            continue;
        }
        h.push_str("<a class=\"theme\" href=\"/theme?set=");
        h.push_str(value);
        h.push_str("&amp;to=");
        h.push_str(&esc(&url_query_value(here)));
        h.push_str("\">");
        h.push_str(label);
        h.push_str("</a>");
    }
    h.push_str("</span>");
}

/// Percent-encodes a path so it survives being a query parameter.
///
/// `&`, `#` and `?` are the ones that matter: a repository or ref
/// carrying any of them would otherwise end the `to=` value early and
/// send the reader somewhere they did not ask to go.
fn url_query_value(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Repository name, revision, and the breadcrumb back up the tree.
fn repo_header(
    h: &mut String,
    repo: &str,
    rev: &str,
    oid: &str,
    path: &str,
    here: &str,
    origin: Option<&str>,
) {
    h.push_str("<header class=\"top\"><h1><a href=\"/r/");
    h.push_str(&esc(repo));
    h.push_str("\">");
    // `owner/` and `name` as two runs, so the sheet can set the owner
    // back a step: the name is the thing and the owner is its address,
    // and a title that weights them equally reads as one long word.
    match repo.split_once('/') {
        Some((owner, name)) => {
            h.push_str("<span class=\"owner\">");
            h.push_str(&esc(owner));
            h.push_str("/</span>");
            h.push_str(&esc(name));
        }
        None => h.push_str(&esc(repo)),
    }
    h.push_str("</a></h1><div class=\"sub\">");
    // The repository root draws a ref picker under this header, and the
    // picker's first control is the revision's name. Printing it here as
    // well put `main` twice on one screen, a hand's width apart, which
    // reads as two different facts rather than one repeated.
    if !(path.is_empty() && here == "tree") {
        h.push_str("<span class=\"pill\">");
        h.push_str(&esc(rev));
        h.push_str("</span>");
    }
    h.push_str("<span class=\"pill mono\">");
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
    // Beside the clone path rather than buried in a README, because the
    // reader who needs it is the one who has just arrived and does not
    // yet know this node has no pull requests.
    h.push_str("<span class=\"pill\"><a href=\"/r/");
    h.push_str(&esc(repo));
    h.push_str("/contribute\">how to contribute</a></span>");
    // No "all repositories" pill: the fixed bar carries that link on
    // every page, and two links to one list in one header is noise.
    // The path a reader clones, which nothing on this surface showed. It
    // is also the other half of the node's two names for one repository:
    // this page is `/r/<repo>` and the clone is `/<repo>.git`, and a
    // reader who only ever saw one of them had to guess the other.
    //
    // Written out whole whenever the request carried a `Host`, because
    // the string a reader needs is the argument to `git clone`, and a
    // relative path is only half of it. It is the reader's own origin
    // rather than a configured name — the same rule, and the same
    // reason, as the address the contribute page prints into its
    // commands: it is the one address known to reach this node for this
    // reader, proxy and all. A request carrying no `Host` — which
    // HTTP/1.1 forbids, so this is the malformed case — keeps the
    // relative path, which is still true.
    h.push_str("<span class=\"pill mono clone\">clone ");
    match origin {
        Some(origin) => h.push_str(&esc(&format!("{origin}/{repo}.git"))),
        None => h.push_str(&esc(&format!("/{repo}.git"))),
    }
    h.push_str("</span>");
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

/// Closing tags, the footer promise the D28 page makes, and the way in.
///
/// The way in lives here rather than in the bar, which is where it used
/// to be and where it did not belong. `chrome` drew `sign in` as the
/// most prominent control on the page for any reader the node had
/// identified as nobody, and after D78 that reader is ordinarily a
/// stranger reading published source who cannot have an account here --
/// so the loudest thing on the page was a door that would refuse them.
///
/// It is still a door and somebody still needs it: an operator arriving
/// at their own node has to get in, and "type the path" is a worse
/// answer than a quiet link. The footer is where a link nobody is being
/// sold belongs.
///
/// `None` renders nothing. That is the refusal pages' state, where the
/// caller has not been identified and a guess would put `sign in` in
/// front of somebody already signed in, on the page where they have just
/// been told something went wrong.
fn close(mut h: String, signed_in: Option<bool>) -> String {
    // Not "read-only" any more, and the merge is where that became
    // false: the review page now carries D39's verdict and comment
    // controls, so a footer under them claiming the surface takes no
    // writes contradicts the buttons directly above it. The half that is
    // still true is the half worth keeping — those buttons do not bypass
    // the signed-op API, they use it. The D28 node page keeps the full
    // sentence, because that page really does offer nothing.
    h.push_str("</main><footer>Every write here is a signed operation.");
    if signed_in == Some(false) {
        h.push_str(" <a href=\"/signin\">sign in</a>");
    }
    h.push_str("</footer>");
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

    // ---------------------------------------------------------------
    // The conflict view.
    // ---------------------------------------------------------------

    /// A file with no markers in it is one plain region, whatever else
    /// it holds.
    ///
    /// The negative case first, because it is the one that costs a
    /// reader something when it is wrong: every file on the surface goes
    /// through this scan, and a parser that saw conflicts in ordinary
    /// text would replace the file listing with a three-column view of
    /// nothing on some fraction of the repository.
    #[test]
    fn ordinary_text_is_never_read_as_a_conflict() {
        for text in [
            "fn main() {}\n",
            // The markers, but not as markers: too long, too short, and
            // with no space after them.
            "<<<<<<<<ours\n=======\n>>>>>>>>theirs\n",
            "<<<<<<ours\n=======\n>>>>>>theirs\n",
            "<<<<<<<ours\n=======\n>>>>>>>theirs\n",
            // Opened and never closed, which is a file that happens to
            // contain the string.
            "<<<<<<< a\nsome text\n",
            // Closed without ever splitting.
            "<<<<<<< a\nsome text\n>>>>>>> b\n",
        ] {
            let regions = split_conflicts(text);
            assert!(
                regions.iter().all(|r| matches!(r, Region::Plain { .. })),
                "read a conflict out of ordinary text: {text:?} -> {regions:?}"
            );
        }
    }

    /// The two-sided shape git writes by default: left, a split, right.
    ///
    /// `base` is `None` and that is a fact rather than an omission — the
    /// merge driver recorded no base — which is why the column says so
    /// on the page instead of rendering empty.
    #[test]
    fn a_two_sided_conflict_keeps_both_sides_and_says_it_has_no_base() {
        let regions = split_conflicts(
            "before\n<<<<<<< HEAD\nours one\nours two\n=======\ntheirs\n>>>>>>> topic\nafter\n",
        );
        assert_eq!(regions.len(), 3, "{regions:?}");
        assert_eq!(
            regions[0],
            Region::Plain {
                first: 1,
                lines: vec!["before"]
            }
        );
        assert_eq!(
            regions[1],
            Region::Conflict {
                first: 2,
                left_label: "HEAD",
                right_label: "topic",
                base: None,
                left: vec!["ours one", "ours two"],
                right: vec!["theirs"],
            }
        );
        // The plain region after a conflict resumes at the right line
        // number, which is what keeps the gutter honest across a file
        // with several of them.
        assert_eq!(
            regions[2],
            Region::Plain {
                first: 8,
                lines: vec!["after"]
            }
        );
    }

    /// The `diff3` shape, which is the one worth having: with a base,
    /// the view answers "what was there before" and not merely "here are
    /// two alternatives".
    #[test]
    fn a_diff3_conflict_keeps_the_base_as_its_own_side() {
        let regions =
            split_conflicts("<<<<<<< ours\nA\n||||||| base\nB\n=======\nC\n>>>>>>> theirs\n");
        assert_eq!(
            regions,
            vec![Region::Conflict {
                first: 1,
                left_label: "ours",
                right_label: "theirs",
                base: Some(vec!["B"]),
                left: vec!["A"],
                right: vec!["C"],
            }]
        );
    }

    /// Every column of one conflict has the same number of rows.
    ///
    /// This is the whole property of the view: three columns side by
    /// side are only readable if the same conflict line is on the same
    /// row across all three, and the sides are different lengths by
    /// definition. A short column that ended early would slide the next
    /// column's content up beside the wrong line, which is worse than no
    /// alignment at all because it looks like an answer.
    #[test]
    fn every_side_of_a_conflict_is_padded_to_the_same_row_count() {
        let regions = split_conflicts(
            "<<<<<<< ours\nA\nB\nC\n||||||| base\nX\n=======\nY\nZ\n>>>>>>> theirs\n",
        );
        let mut h = String::new();
        conflicted_file(&mut h, &regions);
        let columns: Vec<&str> = h.split("<div class=\"side ").skip(1).collect();
        assert_eq!(columns.len(), 3, "three columns: {h}");
        let rows: Vec<usize> = columns
            .iter()
            .map(|column| column.matches("class=\"row").count())
            .collect();
        assert_eq!(rows, vec![3, 3, 3], "rows out of step: {h}");
        // And the padding is marked as absence rather than as an empty
        // line the side actually has.
        assert_eq!(h.matches("row gap").count(), 3, "{h}");
    }

    /// Text around a conflict is still the file, with its own line
    /// numbers, and the conflict names the line it starts on.
    #[test]
    fn a_conflicted_file_still_renders_the_text_around_the_conflict() {
        let regions = split_conflicts("one\ntwo\n<<<<<<< a\nL\n=======\nR\n>>>>>>> b\nlast\n");
        let mut h = String::new();
        conflicted_file(&mut h, &regions);
        assert!(h.contains("<span class=\"ln\">1</span>one"), "{h}");
        assert!(h.contains("<span class=\"ln\">8</span>last"), "{h}");
        assert!(h.contains("line 3"), "the conflict names where it is: {h}");
        assert!(h.contains("1 unresolved conflict"), "{h}");
        // The page says what a conflict *is* here, because a reader
        // arriving from any other forge has only ever seen one as a
        // thing that had to be cleared before anything could proceed.
        assert!(h.contains("committed state"), "{h}");
    }

    /// Everything in a conflict is escaped, on every side.
    ///
    /// The three columns are three separate write paths and a miss in
    /// any one of them is a hole; the labels come off a marker line,
    /// which is file content, so they are attacker-controlled too.
    #[test]
    fn every_side_and_label_of_a_conflict_is_escaped() {
        let regions = split_conflicts(
            "<<<<<<< <img src=x>\n<script>a</script>\n||||||| b\n<b>base</b>\n\
             =======\n<script>c</script>\n>>>>>>> <svg onload=y>\n",
        );
        let mut h = String::new();
        conflicted_file(&mut h, &regions);
        assert!(!h.contains("<script>"), "unescaped markup: {h}");
        assert!(!h.contains("<img src=x>"), "unescaped label: {h}");
        assert!(!h.contains("<svg onload=y>"), "unescaped label: {h}");
        assert!(h.contains("&lt;script&gt;a&lt;/script&gt;"), "{h}");
    }

    // ---------------------------------------------------------------
    // What authorizes a landing.
    // ---------------------------------------------------------------

    /// The two landing rules read as two different sentences, and each
    /// says which one is in force (D42).
    ///
    /// A reviewer cannot otherwise tell whether their approval can ever
    /// be the thing that lands the change, which is the difference
    /// between being asked and being consulted.
    #[test]
    fn the_authority_sentence_names_the_rule_in_force() {
        let roster = crate::ui::Roster::new();
        let state = serde_json::json!({
            "approval_weight": 1,
            "verdicts": {"alice": {"verdict": "Approve"}},
        });

        let mut unowned = String::new();
        authority(&mut unowned, "o/r", &state, &|_| Vec::new(), &roster);
        assert!(unowned.contains("approval weight"), "{unowned}");
        assert!(
            unowned.contains("<b>1</b>"),
            "the weight standing: {unowned}"
        );
        assert!(!unowned.contains("owner"), "{unowned}");

        let mut owned = String::new();
        authority(
            &mut owned,
            "o/r",
            &state,
            &|_| vec!["alice".into(), "bob".into()],
            &roster,
        );
        assert!(owned.contains("an owner's assent"), "{owned}");
        assert!(
            owned.contains("Approved by <b>alice</b>") && !owned.contains("<b>bob</b>"),
            "only the owner who actually approved: {owned}"
        );

        // And an owned repository nobody has assented to names who is
        // being waited on, rather than reading as merely "not yet".
        let mut waiting = String::new();
        authority(
            &mut waiting,
            "o/r",
            &serde_json::json!({"approval_weight": 9, "verdicts": {}}),
            &|_| vec!["alice".into()],
            &roster,
        );
        assert!(waiting.contains("Waiting on <b>alice</b>"), "{waiting}");
        // The weight is not offered as an alternative on a repository
        // where it is not one, however high it has climbed.
        assert!(!waiting.contains('9'), "{waiting}");
    }

    /// An owner's name is escaped, because it comes out of a file the
    /// operator edits and lands inside markup this function assembles by
    /// hand.
    #[test]
    fn an_owner_name_cannot_inject_markup() {
        let mut h = String::new();
        authority(
            &mut h,
            "o/r",
            &serde_json::json!({"approval_weight": 0, "verdicts": {}}),
            &|_| vec!["<script>x</script>".into()],
            &crate::ui::Roster::new(),
        );
        assert!(!h.contains("<script>"), "{h}");
        assert!(h.contains("&lt;script&gt;"), "{h}");
    }

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

    /// A change id is not a name, and holding it to the name grammar
    /// 404-ed the whole onboarding path.
    ///
    /// `choir propose` mints `propose:<owner>/<repo>:<fingerprint>` (D52).
    /// Both the reviews table and the repository pane link to that id,
    /// and both links answered 404 because `safe_segment` refuses a
    /// colon and a slash. Asserted on the exact shape the client
    /// produces, since a shorter id kept passing while every real one
    /// failed.
    #[test]
    fn a_change_id_reaches_its_review_page() {
        let id = "propose:demo/hello:\
                  fcd6a585169449d3c4579093d3cf60d335bdece958e2163a2538f6d2657403fc";
        assert_eq!(
            route(&format!("/r/demo/hello/review/{id}")),
            Some(Page::Review {
                repo: "demo/hello".into(),
                id: id.into(),
            })
        );
        // Percent-encoded arrives at the same place: a client that
        // escapes the separators is not asking for a different review.
        assert_eq!(
            route("/r/demo/hello/review/propose%3Ademo%2Fhello%3Aabc"),
            Some(Page::Review {
                repo: "demo/hello".into(),
                id: "propose:demo/hello:abc".into(),
            })
        );
        // And an id that could be a path still cannot be one.
        for url in [
            "/r/demo/hello/review/../../etc/passwd",
            "/r/demo/hello/review/a%2F..%2F..%2Fb",
            "/r/demo/hello/review/a%00b",
            "/r/demo/hello/review/",
        ] {
            assert_eq!(route(url), None, "a dangerous review id parsed: {url}");
        }
    }

    /// The pane renders the id at the width of a sentence, and the
    /// fingerprint is what gets elided rather than the namespace.
    #[test]
    fn a_change_id_is_shortened_from_the_fingerprint_end() {
        let id = "propose:demo/hello:\
                  fcd6a585169449d3c4579093d3cf60d335bdece958e2163a2538f6d2657403fc";
        assert_eq!(
            short_change_id(id),
            "propose:demo/hello:fcd6a5851694\u{2026}"
        );
        // Nothing to elide: returned whole rather than claiming an
        // elision it did not make.
        assert_eq!(short_change_id("main-d6055a9"), "main-d6055a9");
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
    /// and no curl-driven test can reach it. Hence here, on the few
    /// lines that decide.
    ///
    /// It reads two functions rather than one now, because the redesign
    /// put the verdict controls above the diff and the comment box at
    /// the end of the thread below it: `write_sections` returns the two
    /// fragments and `wants_ceremony` is the decision that used to be
    /// "did either of them write anything". The property asserted is
    /// unchanged — nothing to do, nothing fetched — and it is asserted
    /// on the same two inputs.
    #[test]
    fn a_review_nobody_can_act_on_asks_for_no_script() {
        let settled = serde_json::json!({
            "reviewers": ["carol"], "verdicts": {}, "archived": true,
        });
        let (verdict, comment) = super::write_sections("review-1", &settled, "carol");
        assert!(
            verdict.is_empty() && comment.is_empty(),
            "an archived review drew a control: {verdict}{comment}"
        );
        assert!(
            !super::wants_ceremony(&verdict, &comment),
            "an archived review fetched the ceremony"
        );

        // And the live case still does, once, however many fragments
        // rendered — the element is written by the caller, exactly once,
        // whichever of the two produced it.
        let live = serde_json::json!({
            "reviewers": ["carol"], "verdicts": {}, "archived": false,
        });
        let (verdict, comment) = super::write_sections("review-1", &live, "carol");
        assert!(super::wants_ceremony(&verdict, &comment));
        assert!(
            verdict.contains("Your verdict") && comment.contains("comment-go"),
            "{verdict}{comment}"
        );
        // The read-only fallback is a sentence, and a sentence needs no
        // script: this is the case the old `is_empty` check would have
        // got wrong once the fragment stopped being empty.
        assert!(!super::wants_ceremony(
            "",
            "<p class=\"note\">Use the signed CLI.</p>"
        ));
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

    /// The batched walk answers what the per-path query answers.
    ///
    /// This is the whole licence for the batching: it was introduced to
    /// make a directory page fit inside Phase 1's read budget, and a
    /// faster function that dates files differently is not an
    /// optimization, it is a regression nobody would notice — a listing
    /// with slightly wrong dates looks exactly like a listing.
    ///
    /// The fixture deliberately includes a file touched by an older
    /// commit and a directory whose newest change is below it, because
    /// those are the two cases where a walk and a path query can part
    /// company.
    #[test]
    fn last_touches_agree_with_the_per_path_query() {
        let work = std::env::temp_dir().join(format!("choir-touch-agree-{}", std::process::id()));
        std::fs::remove_dir_all(&work).ok();
        std::fs::create_dir_all(&work).expect("temp root");

        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args([
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "init.defaultBranch=main",
                ])
                .args(args)
                .current_dir(&work)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .expect("git runs")
        };
        assert!(git(&["init", "-q"]).status.success());

        // Three commits, so the rows are dated by different ones.
        std::fs::write(work.join("old.txt"), "first\n").unwrap();
        std::fs::create_dir_all(work.join("nested")).unwrap();
        std::fs::write(work.join("nested/deep.txt"), "first\n").unwrap();
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "the older commit"])
            .status
            .success());

        std::fs::write(work.join("nested/deep.txt"), "second\n").unwrap();
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "touch only what is nested"])
            .status
            .success());

        std::fs::write(work.join("new.txt"), "third\n").unwrap();
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-q", "-m", "the newest commit"])
            .status
            .success());

        // The git directory, not the work tree: these helpers address a
        // repository the way the daemon does, which is bare.
        let repo = work.join(".git");
        let oid = resolve(&repo, "HEAD").expect("HEAD resolves");
        let rows: Vec<String> = vec!["old.txt".into(), "nested".into(), "new.txt".into()];
        let batched = last_touches(&repo, &oid, &rows);
        for row in &rows {
            assert_eq!(
                batched.get(row).cloned(),
                last_touch(&repo, &oid, row),
                "the walk and the per-path query disagree about {row}"
            );
        }
        // And the fixture really does separate them, or the agreement
        // above would be three copies of one answer.
        assert_eq!(
            batched["old.txt"].0, "the older commit",
            "the fixture did not produce distinct dates: {batched:?}"
        );
        assert_eq!(batched["nested"].0, "touch only what is nested");
        assert_eq!(batched["new.txt"].0, "the newest commit");

        std::fs::remove_dir_all(&work).ok();
    }
}
