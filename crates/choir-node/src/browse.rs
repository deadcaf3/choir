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

/// One browse request, after parsing and validation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Page {
    /// The repository index: everything the reader may see.
    Index,
    /// A directory listing at `rev`, rooted at `path` (empty for the
    /// repository root).
    Tree { repo: String, rev: String, path: String },
    /// One file's contents at `rev`.
    Blob { repo: String, rev: String, path: String },
    /// Recent history reachable from `rev`.
    Commits { repo: String, rev: String },
    /// One commit and its diff.
    Commit { repo: String, oid: String },
    /// Every review proposing to land on this repository.
    Reviews { repo: String },
    /// One review: what it proposes, who was asked, what they said, and
    /// the diff between the proposal and where it would land.
    Review { repo: String, id: String },
}

impl Page {
    /// The repository this page reads, in the D29 canonical spelling, or
    /// `None` for the index — which is not about one repository and is
    /// filtered per reader instead of gated.
    pub(crate) fn repo(&self) -> Option<&str> {
        match self {
            Page::Index => None,
            Page::Tree { repo, .. }
            | Page::Blob { repo, .. }
            | Page::Commits { repo, .. }
            | Page::Commit { repo, .. }
            | Page::Reviews { repo }
            | Page::Review { repo, .. } => Some(repo),
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
    let rest = path.strip_prefix("/r")?;
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    if rest.is_empty() {
        return Some(Page::Index);
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
        None | Some("") => return Some(Page::Tree { repo, rev: "HEAD".into(), path: String::new() }),
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
        "commits" if path.is_empty() => Some(Page::Commits { repo, rev: safe_rev(&rev_or_oid)? }),
        "commit" if path.is_empty() => Some(Page::Commit { repo, oid: safe_oid(&rev_or_oid)? }),
        _ => None,
    }
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
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Same, decoded as UTF-8 with replacement — for git's own output,
/// which is metadata rather than file content.
fn git_text(dir: &Path, args: &[&str]) -> Result<String, String> {
    git(dir, args).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// The commit oid a page is really about, which is its cache identity.
fn resolve(dir: &Path, rev: &str) -> Result<String, String> {
    let out = git_text(dir, &["rev-parse", "--verify", &format!("{rev}^{{commit}}")])?;
    let oid = out.trim().to_string();
    if oid.is_empty() {
        return Err("no such revision".to_string());
    }
    Ok(oid)
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
pub(crate) fn render(
    root: &Path,
    page: &Page,
    readable: &dyn Fn(&str) -> bool,
    platform: Option<&crate::platform::Platform>,
) -> Rendered {
    match page {
        Page::Index => index(root, readable),
        Page::Tree { repo, rev, path } => tree(&bare(root, repo), repo, rev, path),
        Page::Blob { repo, rev, path } => blob(&bare(root, repo), repo, rev, path),
        Page::Commits { repo, rev } => commits(&bare(root, repo), repo, rev),
        Page::Commit { repo, oid } => commit(&bare(root, repo), repo, oid),
        Page::Reviews { repo } => reviews(repo, platform),
        Page::Review { repo, id } => review(&bare(root, repo), repo, id, platform),
    }
}

/// On-disk location of a repository's bare directory.
fn bare(root: &Path, repo: &str) -> PathBuf {
    root.join(format!("{repo}.git"))
}

/// The repository index, filtered to what the reader may see.
fn index(root: &Path, readable: &dyn Fn(&str) -> bool) -> Rendered {
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

    let mut h = shell("repositories");
    h.push_str("<header class=\"top\"><h1>repositories</h1><div class=\"sub\">");
    h.push_str("<span class=\"pill\">");
    h.push_str(&repos.len().to_string());
    h.push_str(" readable</span>");
    h.push_str("<span class=\"pill\"><a href=\"/\">node state</a></span>");
    h.push_str("</div></header><main id=\"main\"><section>");
    if repos.is_empty() {
        h.push_str("<p class=\"empty\">No repositories you can read.</p>");
    } else {
        h.push_str("<table><tbody>");
        for repo in &repos {
            h.push_str("<tr><td><a href=\"/r/");
            h.push_str(&esc(repo));
            h.push_str("\">");
            h.push_str(&esc(repo));
            h.push_str("</a></td></tr>");
        }
        h.push_str("</tbody></table>");
    }
    h.push_str("</section>");
    Rendered { status: 200, etag: None, html: close(h) }
}

/// A directory listing.
fn tree(dir: &Path, repo: &str, rev: &str, path: &str) -> Rendered {
    let oid = match resolve(dir, rev) {
        Ok(oid) => oid,
        // A repository that exists but resolves nothing is empty, not
        // missing — and it is the first thing an operator browses after
        // creating one, so answering `404` there says the create failed
        // when it did not.
        Err(_) if dir.is_dir() && rev == "HEAD" => return empty(repo),
        Err(why) => return missing(repo, rev, &why),
    };
    // The trailing slash is what makes `ls-tree` list a directory's
    // children rather than the directory entry itself.
    let spec = if path.is_empty() { String::new() } else { format!("{path}/") };
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

    let mut h = shell(&format!("{repo}: {}", if path.is_empty() { "/" } else { path }));
    repo_header(&mut h, repo, rev, &oid, path, "tree");
    h.push_str("<section>");
    if rows.is_empty() {
        h.push_str("<p class=\"empty\">Nothing here at this revision.</p>");
    } else {
        h.push_str("<table><tbody>");
        for (is_dir, name, size) in rows {
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
            h.push_str(&esc(&name));
            h.push_str("\">");
            if is_dir {
                h.push_str("<span class=\"muted\">/</span>");
            }
            h.push_str(&esc(leaf));
            h.push_str("</a></td><td class=\"num muted\">");
            h.push_str(&esc(&size));
            h.push_str("</td></tr>");
        }
        h.push_str("</tbody></table>");
    }
    h.push_str("</section>");
    Rendered { status: 200, etag: Some(tag(&oid, path)), html: close(h) }
}

/// One file.
fn blob(dir: &Path, repo: &str, rev: &str, path: &str) -> Rendered {
    let oid = match resolve(dir, rev) {
        Ok(oid) => oid,
        Err(why) => return missing(repo, rev, &why),
    };
    let spec = format!("{oid}:{path}");
    let size: u64 = match git_text(dir, &["cat-file", "-s", &spec]) {
        Ok(text) => text.trim().parse().unwrap_or(0),
        Err(why) => return missing(repo, rev, &why),
    };

    let mut h = shell(&format!("{repo}: {path}"));
    repo_header(&mut h, repo, rev, &oid, path, "blob");
    h.push_str("<section>");
    if size > MAX_BLOB_BYTES {
        h.push_str("<p class=\"note\">");
        h.push_str(&esc(&human(&size.to_string())));
        h.push_str(" — too large to show. Clone the repository to read it.</p>");
    } else {
        match git(dir, &["cat-file", "blob", &spec]) {
            Err(why) => {
                h.push_str("<p class=\"empty\">");
                h.push_str(&esc(&why));
                h.push_str("</p>");
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
    Rendered { status: 200, etag: Some(tag(&oid, path)), html: close(h) }
}

/// Recent history.
fn commits(dir: &Path, repo: &str, rev: &str) -> Rendered {
    let oid = match resolve(dir, rev) {
        Ok(oid) => oid,
        Err(why) => return missing(repo, rev, &why),
    };
    // Unit separators rather than spaces: a subject can contain
    // anything, including whatever delimiter looked safe.
    let format = "--format=%H%x1f%an%x1f%aI%x1f%s";
    let log = match git_text(
        dir,
        &["log", &format!("--max-count={COMMIT_PAGE}"), format, &oid],
    ) {
        Ok(text) => text,
        Err(why) => return missing(repo, rev, &why),
    };

    let mut h = shell(&format!("{repo}: commits"));
    repo_header(&mut h, repo, rev, &oid, "", "commits");
    h.push_str("<section><table><thead><tr><th>commit</th><th>subject</th>");
    h.push_str("<th>author</th><th>when</th></tr></thead><tbody>");
    for line in log.lines() {
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
    h.push_str("</tbody></table></section>");
    Rendered { status: 200, etag: Some(tag(&oid, "commits")), html: close(h) }
}

/// One commit, with its diff.
fn commit(dir: &Path, repo: &str, oid: &str) -> Rendered {
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

    let mut h = shell(&format!("{repo}: {}", &id[..id.len().min(12)]));
    repo_header(&mut h, repo, &id, &id, "", "commit");
    h.push_str("<section><h2>");
    h.push_str(&esc(&subject));
    h.push_str("</h2><p class=\"muted\">");
    h.push_str(&esc(&author));
    h.push_str(" · ");
    h.push_str(&esc(&when));
    h.push_str("</p>");

    match git_text(dir, &["show", "--patch", "--stat", "--format=", oid]) {
        Ok(diff) => patch(&mut h, &diff),
        Err(why) => {
            h.push_str("<p class=\"empty\">");
            h.push_str(&esc(&why));
            h.push_str("</p>");
        }
    }
    h.push_str("</section>");
    Rendered { status: 200, etag: Some(tag(&id, "commit")), html: close(h) }
}

/// Renders a unified diff, coloured by line kind and bounded in length.
///
/// Shared by the commit page and the review page so a diff cannot come
/// to mean two different things depending on which one a reader opened.
fn patch(h: &mut String, diff: &str) {
    h.push_str("<pre class=\"code diff\">");
    for (n, line) in diff.lines().enumerate() {
        if n >= MAX_DIFF_LINES {
            h.push_str("\n<span class=\"muted\">… diff truncated at ");
            h.push_str(&MAX_DIFF_LINES.to_string());
            h.push_str(" lines. Clone the repository to read the rest.</span>");
            break;
        }
        let class = match line.as_bytes().first() {
            Some(b'+') if !line.starts_with("+++") => "add",
            Some(b'-') if !line.starts_with("---") => "del",
            Some(b'@') => "hunk",
            _ => "",
        };
        if class.is_empty() {
            h.push_str(&esc(line));
        } else {
            h.push_str("<span class=\"");
            h.push_str(class);
            h.push_str("\">");
            h.push_str(&esc(line));
            h.push_str("</span>");
        }
        h.push('\n');
    }
    h.push_str("</pre>");
}

/// Every review proposing to land on this repository.
fn reviews(repo: &str, platform: Option<&crate::platform::Platform>) -> Rendered {
    let Some(platform) = platform else {
        return unavailable(repo);
    };
    let rows = platform.reviews_for_repo(repo);

    let mut h = shell(&format!("{repo}: reviews"));
    h.push_str("<header class=\"top\"><h1><a href=\"/r/");
    h.push_str(&esc(repo));
    h.push_str("\">");
    h.push_str(&esc(repo));
    h.push_str("</a></h1><div class=\"sub\"><span class=\"pill\">");
    h.push_str(&rows.len().to_string());
    h.push_str(" reviews</span><span class=\"pill\"><a href=\"/r/\">all repositories</a></span>");
    h.push_str("</div></header><main id=\"main\"><section>");
    if rows.is_empty() {
        h.push_str("<p class=\"empty\">No review proposes to land on this repository.</p>");
    } else {
        h.push_str("<table><thead><tr><th>review</th><th>onto</th><th>state</th>");
        h.push_str("<th class=\"num\">weight</th></tr></thead><tbody>");
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
            h.push_str("</td></tr>");
        }
        h.push_str("</tbody></table>");
    }
    h.push_str("</section>");
    Rendered { status: 200, etag: None, html: close(h) }
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
) -> Rendered {
    let Some(platform) = platform else {
        return unavailable(repo);
    };
    let Some(state) = platform.review_json(id) else {
        return missing(repo, id, "no such review");
    };
    let commit_oid = git_oid_of(state["target"].as_str().unwrap_or("")).unwrap_or_default();

    let mut h = shell(&format!("{repo}: review {id}"));
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
        h.push_str("<p class=\"empty\">Unassigned.</p>");
    } else {
        h.push_str("<table><thead><tr><th>reviewer</th><th>verdict</th><th>note</th>");
        h.push_str("</tr></thead><tbody>");
        for who in reviewers.iter().filter_map(serde_json::Value::as_str) {
            let verdict = &state["verdicts"][who];
            let slashed = state["slashes"].get(who).and_then(serde_json::Value::as_str);
            h.push_str("<tr><td class=\"mono\">");
            h.push_str(&esc(who));
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
            h.push_str(&esc(comment["author"].as_str().unwrap_or("")));
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
            match git_text(dir, &["diff", "--stat", "--patch", &range]) {
                Ok(diff) if diff.trim().is_empty() => {
                    h.push_str("<p class=\"empty\">Nothing to land: the destination already ");
                    h.push_str("contains this commit.</p>");
                }
                Ok(diff) => patch(&mut h, &diff),
                Err(why) => {
                    h.push_str("<p class=\"note\">");
                    h.push_str(&esc(&why));
                    h.push_str("</p>");
                }
            }
        }
    }
    h.push_str("</section>");
    Rendered { status: 200, etag: None, html: close(h) }
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
    let mut h = shell(&format!("{repo}: unavailable"));
    h.push_str("<header class=\"top\"><h1>");
    h.push_str(&esc(repo));
    h.push_str("</h1></header><main id=\"main\"><section><p class=\"note\">");
    h.push_str("The platform API is not enabled on this node, so it holds no reviews.");
    h.push_str("</p></section>");
    Rendered { status: 503, etag: None, html: close(h) }
}

/// The page for a repository with no commits yet.
///
/// Deliberately a `200`: the repository is exactly what the operator
/// asked for, and the only thing missing is a push. Saying so beats
/// reporting git's "Needed a single revision", which reads like a fault.
fn empty(repo: &str) -> Rendered {
    let mut h = shell(&format!("{repo}: empty"));
    h.push_str("<header class=\"top\"><h1>");
    h.push_str(&esc(repo));
    h.push_str("</h1><div class=\"sub\"><span class=\"pill\">empty</span>");
    h.push_str("<span class=\"pill\"><a href=\"/r/\">all repositories</a></span>");
    h.push_str("</div></header><main id=\"main\"><section><p class=\"note\">");
    h.push_str("No commits yet. Push a branch and it will appear here.</p></section>");
    Rendered { status: 200, etag: None, html: close(h) }
}

/// The page for a revision that does not resolve.
///
/// A `404` with git's own words, because "no such revision" and "not a
/// tree object" send a reader to different fixes, and flattening both
/// into "not found" costs them the difference.
fn missing(repo: &str, rev: &str, why: &str) -> Rendered {
    let mut h = shell(&format!("{repo}: not found"));
    h.push_str("<header class=\"top\"><h1>");
    h.push_str(&esc(repo));
    h.push_str("</h1></header><main id=\"main\"><section><p class=\"note\">");
    h.push_str(&esc(rev));
    h.push_str(": ");
    h.push_str(&esc(why));
    h.push_str("</p></section>");
    Rendered { status: 404, etag: None, html: close(h) }
}

/// Document head and opening tags, shared with the D28 page so the two
/// surfaces cannot drift into looking like different products.
fn shell(title: &str) -> String {
    let mut h = String::with_capacity(8 * 1024);
    h.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    h.push_str("<title>");
    h.push_str(&esc(title));
    h.push_str("</title>");
    h.push_str(crate::ui::STYLE);
    h.push_str("</head><body>");
    h.push_str("<a class=\"skip\" href=\"#main\">Skip to content</a>");
    h
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
    h.push_str("<span class=\"pill\"><a href=\"/r/\">all repositories</a></span>");
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
                h.push_str(&esc(&walked));
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
    h.push_str("</main><footer>Read-only. Every write goes through the signed-op API.</footer>");
    h.push_str("</body></html>");
    h
}

/// The `ETag` for content at one commit oid.
///
/// Weak, and scoped by what the page is about as well as the commit, so
/// two pages of the same tree never share a tag.
fn tag(oid: &str, what: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in what.as_bytes() {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(route("/r"), Some(Page::Index));
        assert_eq!(route("/r/"), Some(Page::Index));
        assert_eq!(
            route("/r/o/p/tree/main/src/lib"),
            Some(Page::Tree { repo: "o/p".into(), rev: "main".into(), path: "src/lib".into() })
        );
        assert_eq!(
            route("/r/o/p/blob/main/src/lib.rs"),
            Some(Page::Blob { repo: "o/p".into(), rev: "main".into(), path: "src/lib.rs".into() })
        );
        assert_eq!(
            route("/r/o/p/commits/main"),
            Some(Page::Commits { repo: "o/p".into(), rev: "main".into() })
        );
        let oid = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            route(&format!("/r/o/p/commit/{oid}")),
            Some(Page::Commit { repo: "o/p".into(), oid: oid.into() })
        );
        // A query string is not part of the route.
        assert_eq!(
            route("/r/o/p/tree/main?x=1"),
            Some(Page::Tree { repo: "o/p".into(), rev: "main".into(), path: String::new() })
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

    #[test]
    fn byte_counts_read_like_sizes() {
        assert_eq!(human("0"), "0 B");
        assert_eq!(human("1023"), "1023 B");
        assert_eq!(human("1024"), "1.0 KiB");
        assert_eq!(human("1048576"), "1.0 MiB");
        // `ls-tree --long` prints `-` for a tree's size column.
        assert_eq!(human("-"), "-");
    }
}
