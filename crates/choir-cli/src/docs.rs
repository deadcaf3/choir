//! `choir docs`: the book, and the API documentation inside it.
//!
//! Two renderers over one set of files. `docs/*.md` is the book's source
//! and is also pulled into the crates with `#![doc = include_str!]`, so a
//! page cannot say one thing in the book and another in `cargo doc`.
//!
//! rustdoc is copied to `book/api/` rather than left in the target
//! directory. That is the whole reason this is one command instead of
//! two: a book whose API links point outside the tree it was built into
//! is a book whose API links 404 the moment it is copied anywhere.
//!
//! # Why this is not a shell script
//!
//! It was one. The script computed the documentation directory as
//! `${CARGO_TARGET_DIR:-target}/doc`, which is right when the variable
//! is exported, right when nothing sets it, and wrong when `target-dir`
//! comes from a `[build]` table in a `.cargo/config.toml`, which cargo
//! reads from every *ancestor* of the working directory. `cargo doc`
//! then succeeded and the copy after it reported that cargo had produced
//! nothing. [`target_dir_from_metadata`] asks cargo instead, and is
//! tested against the shape cargo actually emits.

use std::path::{Path, PathBuf};

/// What a successful build produced.
#[derive(Debug)]
pub struct Built {
    /// The checkout the book was built from.
    pub root: PathBuf,
    /// The rendered book.
    pub book: PathBuf,
    /// The API documentation, inside the book.
    pub api: PathBuf,
    /// Crate pages rustdoc produced, in the order the index lists them.
    pub crates: Vec<String>,
}

/// Why a build did not happen, phrased as the thing to do about it.
#[derive(Debug)]
pub enum Failure {
    /// No `book.toml` in this directory or any parent.
    NotACheckout,
    /// A tool this needs is not on `PATH`.
    MissingTool {
        /// The executable that was not found.
        tool: &'static str,
        /// The command that installs it.
        install: &'static str,
    },
    /// A stage ran and exited nonzero. Its own output has already been
    /// written to the terminal, so this carries no captured text: the
    /// compiler's errors belong on the screen, not inside a JSON string.
    Stage {
        /// Which stage.
        name: &'static str,
        /// Its exit code, or `None` when a signal killed it.
        code: Option<i32>,
    },
    /// Something failed that is nobody's fault but the filesystem's.
    Io(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotACheckout => write!(
                f,
                "no book.toml in this directory or any parent.\n\
                 `choir docs` builds this repository's own documentation, \
                 so it has to be run inside a checkout of it."
            ),
            Self::MissingTool { tool, install } => {
                write!(f, "{tool} is not on PATH.\n  {install}")
            }
            Self::Stage { name, code } => match code {
                Some(code) => write!(f, "{name} exited {code}"),
                None => write!(f, "{name} was killed by a signal"),
            },
            Self::Io(message) => write!(f, "{message}"),
        }
    }
}

/// The checkout `from` sits in, found by the file that defines the book.
///
/// `book.toml` rather than `Cargo.toml`: this workspace has sixteen
/// manifests and exactly one book, so walking up for a manifest would
/// stop at whichever crate the caller happened to be standing in.
#[must_use]
pub fn find_root(from: &Path) -> Option<PathBuf> {
    let mut dir = from.to_path_buf();
    loop {
        if dir.join("book.toml").is_file() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Cargo's target directory, read out of `cargo metadata` output.
///
/// A string scan rather than a JSON dependency: `target_directory` is a
/// top-level string in a document this crate never otherwise parses, and
/// the alternative is asking every consumer of this CLI to build serde's
/// derive for one field.
///
/// Returns `None` when the field is absent, which is the honest answer
/// for output this did not understand: the caller falls back rather
/// than guessing a path and reporting somebody else's bug.
#[must_use]
pub fn target_dir_from_metadata(json: &str) -> Option<&str> {
    const KEY: &str = "\"target_directory\":";
    let rest = &json[json.find(KEY)? + KEY.len()..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// The landing page rustdoc does not write.
///
/// rustdoc emits a root `index.html` only when it has a single root
/// crate to point at; documenting a workspace with `--no-deps` leaves
/// that root empty, so the book's `/api/` link would land on nothing.
///
/// Styled from `theme/choir-tokens.css`, which the build copies in
/// beside it, so this page and the book are the same colours rather than
/// merely adjacent. It carries no script and no external request, which
/// is the same rule the node's own pages follow.
#[must_use]
pub fn api_index(crates: &[String]) -> String {
    let mut items = String::new();
    for name in crates {
        items.push_str(&format!(
            "  <li><a href=\"{name}/index.html\"><code>{name}</code></a></li>\n"
        ));
    }
    format!(
        r#"<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>choir: API documentation</title>
<link rel="stylesheet" href="choir-tokens.css">
<style>
  :root {{ font-size: 17px; }}
  body {{ margin: 0; background: var(--ground); color: var(--ink);
         font-family: var(--font-sans); letter-spacing: var(--tr-body);
         line-height: var(--lh-copy); }}
  main {{ max-width: var(--measure); margin: 0 auto;
         padding: var(--sp-16) var(--sp-5); }}
  h1 {{ font-size: var(--fs-900); line-height: var(--lh-tight);
       letter-spacing: var(--tr-display); color: var(--strong);
       margin: 0 0 var(--sp-3); }}
  p.lede {{ font-size: var(--fs-600); color: var(--muted);
           margin: 0 0 var(--sp-10); }}
  a {{ color: var(--accent-ink); text-decoration-color: var(--accent-line);
      text-underline-offset: .18em; }}
  a:hover {{ color: var(--accent-hover); }}
  ul {{ list-style: none; padding: 0; margin: 0; display: grid;
       gap: var(--sp-2);
       grid-template-columns: repeat(auto-fill, minmax(13rem, 1fr)); }}
  li a {{ display: block; background: var(--card); border: var(--border);
         border-radius: var(--r-md); padding: var(--sp-3) var(--sp-4);
         text-decoration: none; color: var(--ink); }}
  li a code {{ font-family: var(--font-mono); font-size: var(--fs-300); }}
  li a:hover {{ border-color: var(--accent-line); background: var(--accent-tint);
               color: var(--accent-ink); }}
  footer {{ margin-top: var(--sp-12); padding-top: var(--sp-5);
           border-top: var(--bw-hair) solid var(--line);
           color: var(--faint); font-size: var(--fs-300); }}
</style>
<main>
<h1>choir</h1>
<p class="lede">API documentation, one page per crate. The prose that
explains how they fit is in <a href="../index.html">the book</a>.</p>
<ul>
{items}</ul>
<footer>Written by <code>choir docs</code>. Rebuild with
<code>choir docs --open</code>.</footer>
</main>
</html>
"#
    )
}

/// The marker `docs/README.md` carries where a link to the node belongs.
///
/// An HTML comment, so a build that does not know the node's address
/// renders nothing at all rather than a placeholder somebody has to
/// explain. mdBook passes raw HTML through, and a comment is the one
/// shape of it that is invisible until something replaces it.
pub const NODE_LINK_MARKER: &str = "<!--node-link-->";

/// Repoints the links that leave the book.
///
/// `docs/*.md` is read by three people (`book.toml` says so): somebody
/// browsing the checkout, somebody reading the book, and `cargo doc`.
/// The first and third want `../README.md`, because in a checkout that
/// file is right there. The second gets a 404: mdBook renders `src` and
/// nothing above it, so `../README.html` names a file the published
/// artifact does not contain — twenty-one such links, from four pages.
///
/// Rather than making the pages worse for two readers to fix the third,
/// the escaping links are repointed at the repository itself at publish
/// time. `base` is a browsable tree URL (the workflow builds one from the
/// commit it is publishing, so the link is pinned rather than tracking a
/// branch), and it never appears in a tracked file — the same rule
/// `site-url` follows.
///
/// `depth` is how many directories below the book root the page sits, so
/// `index.html` is 0 and `using/workflow.html` is 1. A link escapes when
/// it climbs further than that, and the first `..` past the book root is
/// the step from `book/` to the checkout — which is why one is stripped
/// before the rest becomes a repository path.
///
/// `.html` goes back to `.md` because mdBook rewrote it on the way in.
/// Anything that was not a markdown link is left exactly as it was: a
/// rewriter that guessed here would break the links that work.
#[must_use]
pub fn repoint_escaping_links(html: &str, depth: usize, base: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(at) = rest.find("href=\"") {
        out.push_str(&rest[..at + 6]);
        rest = &rest[at + 6..];
        let Some(end) = rest.find('"') else { break };
        let href = &rest[..end];
        match repoint_one(href, depth, base) {
            Some(moved) => out.push_str(&moved),
            None => out.push_str(href),
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// One `href`, or `None` when it stays where it is.
fn repoint_one(href: &str, depth: usize, base: &str) -> Option<String> {
    let (path, tail) = match href.find(['#', '?']) {
        Some(at) => (&href[..at], &href[at..]),
        None => (href, ""),
    };
    let climbs = path.split('/').take_while(|seg| *seg == "..").count();
    // Not a climb, or a climb the book contains: nothing to do. The
    // `depth + 1` is the book root itself — a page at depth 0 may climb
    // once before it has left, and that once lands on the checkout.
    if climbs == 0 || climbs <= depth || climbs > depth + 1 {
        return None;
    }
    let target = path.split('/').skip(climbs).collect::<Vec<_>>().join("/");
    let target = target.strip_suffix(".html")?;
    Some(format!("{}/{target}.md{tail}", base.trim_end_matches('/')))
}

/// Puts a link to the node into the page that carries
/// [`NODE_LINK_MARKER`].
///
/// The two halves of the site are on two hosts (D76), so neither can
/// reach the other with a relative path and neither may name the other in
/// a tracked file. The marker is tracked; the address is not.
#[must_use]
pub fn stamp_node_link(html: &str, node: &str) -> String {
    let node = node.trim_end_matches('/');
    html.replace(
        NODE_LINK_MARKER,
        &format!("<a class=\"api-link\" href=\"{node}\">The node itself &rarr;</a>"),
    )
}

/// Applies both publish-time rewrites to every page mdBook rendered.
///
/// **`api/` is skipped by name**, not by running first. It is rustdoc's
/// output, whose thousands of pages are full of `../` links that are
/// already right; a rewriter with an opinion about them would be a
/// second bug. Ordering alone does not skip them, because this runs
/// against `book/` in place and a previous build's `api/` is still
/// sitting there — the copy that replaces it happens further down.
fn rewrite_pages(book: &Path, base: Option<&str>, node: Option<&str>) -> std::io::Result<()> {
    fn walk(
        dir: &Path,
        depth: usize,
        base: Option<&str>,
        node: Option<&str>,
    ) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                if depth == 0 && entry.file_name() == "api" {
                    continue;
                }
                walk(&path, depth + 1, base, node)?;
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "html") {
                continue;
            }
            let before = std::fs::read_to_string(&path)?;
            let mut after = match base {
                Some(base) => repoint_escaping_links(&before, depth, base),
                None => before.clone(),
            };
            if let Some(node) = node {
                after = stamp_node_link(&after, node);
            }
            if after != before {
                std::fs::write(&path, after)?;
            }
        }
        Ok(())
    }
    walk(book, 0, base, node)
}

fn tool_exists(tool: &str) -> bool {
    std::process::Command::new(tool)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Runs one stage, letting its output reach the terminal.
fn stage(name: &'static str, mut command: std::process::Command) -> Result<(), Failure> {
    let status = command
        .status()
        .map_err(|e| Failure::Io(format!("cannot run {name}: {e}")))?;
    if status.success() {
        return Ok(());
    }
    Err(Failure::Stage {
        name,
        code: status.code(),
    })
}

/// Copies a directory's *contents* into `dst`.
///
/// Contents rather than the directory itself, which is the difference
/// between `book/api/index.html` and `book/api/doc/index.html`.
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Builds the book and the API documentation inside it.
///
/// # Errors
///
/// [`Failure`], which is written for a person to read: a missing tool
/// names the command that installs it, and a stage that failed has
/// already put its own errors on the terminal.
pub fn build(root: &Path) -> Result<Built, Failure> {
    if !tool_exists("cargo") {
        return Err(Failure::MissingTool {
            tool: "cargo",
            install: "install Rust from https://rustup.rs/",
        });
    }
    if !tool_exists("mdbook") {
        return Err(Failure::MissingTool {
            tool: "mdbook",
            install: "cargo install mdbook --locked",
        });
    }

    // The same flags the gate's rustdoc stage uses. A doc build that
    // warns here and is denied there is a difference nobody wants to
    // find at commit time, and `docs/` is compiled by both.
    let mut doc = std::process::Command::new("cargo");
    doc.current_dir(root)
        .env("RUSTDOCFLAGS", "-Dwarnings")
        .args(["doc", "--workspace", "--no-deps"]);
    stage("cargo doc", doc)?;

    let mut book = std::process::Command::new("mdbook");
    book.current_dir(root).arg("build");
    stage("mdbook build", book)?;

    // The two publish-time rewrites (D76). Both come from the
    // environment for the reason `site-url` does: they name hosts, and a
    // host in a tracked file is the thing this repository does not do.
    // Absent, which is every local build, the pages are left exactly as
    // mdBook rendered them.
    let base = std::env::var("CHOIR_DOCS_REPO_BASE").ok();
    let node = std::env::var("CHOIR_DOCS_NODE_URL").ok();
    rewrite_pages(
        &root.join("book"),
        base.as_deref().filter(|s| !s.is_empty()),
        node.as_deref().filter(|s| !s.is_empty()),
    )
    .map_err(|e| Failure::Io(format!("cannot rewrite the book's links: {e}")))?;

    let metadata = std::process::Command::new("cargo")
        .current_dir(root)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .map_err(|e| Failure::Io(format!("cannot run cargo metadata: {e}")))?;
    let metadata = String::from_utf8_lossy(&metadata.stdout);
    let target = target_dir_from_metadata(&metadata)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from))
        .unwrap_or_else(|| root.join("target"));

    let from = target.join("doc");
    if !from.is_dir() {
        return Err(Failure::Io(format!(
            "cargo doc reported success but {} does not exist\n\
             cargo says its target directory is: {}",
            from.display(),
            target.display()
        )));
    }

    let book_dir = root.join("book");
    let api = book_dir.join("api");
    if let Err(e) = std::fs::remove_dir_all(&api) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(Failure::Io(format!("cannot clear {}: {e}", api.display())));
        }
    }
    copy_tree(&from, &api).map_err(|e| {
        Failure::Io(format!(
            "cannot copy {} to {}: {e}",
            from.display(),
            api.display()
        ))
    })?;

    // Every directory rustdoc produced a page for. Binary targets are in
    // this list because they are real documentation pages, not because
    // it failed to filter them out.
    let mut crates = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&api) {
        for entry in entries.flatten() {
            if entry.path().join("index.html").is_file() {
                crates.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    crates.sort();

    // Only when rustdoc did not write one: if a future cargo starts
    // emitting a workspace index, its page is better than this one.
    let index = api.join("index.html");
    if !index.is_file() {
        let tokens = root.join("theme/choir-tokens.css");
        std::fs::copy(&tokens, api.join("choir-tokens.css"))
            .map_err(|e| Failure::Io(format!("cannot copy {}: {e}", tokens.display())))?;
        std::fs::write(&index, api_index(&crates))
            .map_err(|e| Failure::Io(format!("cannot write {}: {e}", index.display())))?;
    }

    Ok(Built {
        root: root.to_path_buf(),
        book: book_dir,
        api,
        crates,
    })
}

/// Opens a built page in whatever the desktop uses, if anything does.
///
/// Best-effort on purpose: a build that succeeded and could not open a
/// browser has still built the documentation, and reporting it as a
/// failure would make `--open` less reliable than not passing it.
pub fn open(path: &Path) -> bool {
    for opener in ["open", "xdg-open"] {
        let opened = std::process::Command::new(opener)
            .arg(path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if opened {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_directory_is_read_out_of_cargos_own_shape() {
        // The shape `cargo metadata --format-version 1` emits: one line,
        // compact, the field among many others.
        let json = r#"{"packages":[],"workspace_members":[],"resolve":null,
            "target_directory":"/w/shared-target","version":1,"workspace_root":"/w"}"#;
        assert_eq!(
            target_dir_from_metadata(json),
            Some("/w/shared-target"),
            "the field cargo actually emits must be readable"
        );
    }

    #[test]
    fn unreadable_metadata_is_none_rather_than_a_guess() {
        assert_eq!(target_dir_from_metadata("{}"), None);
        assert_eq!(target_dir_from_metadata(""), None);
        // Truncated mid-value: a scan that returned the rest of the
        // buffer would produce a path, and a wrong path is worse here
        // than no answer, because the caller has a correct fallback.
        assert_eq!(target_dir_from_metadata(r#"{"target_directory":"/w"#), None);
    }

    #[test]
    fn the_root_is_the_directory_holding_the_book() {
        let dir = std::env::temp_dir().join("choir-docs-find-root");
        let deep = dir.join("crates/choir-cli/src");
        std::fs::create_dir_all(&deep).expect("scratch tree");
        std::fs::write(dir.join("book.toml"), "").expect("book.toml");
        // A manifest in between must not stop the walk: this workspace
        // has fifteen of them and one book.
        std::fs::write(dir.join("crates/choir-cli/Cargo.toml"), "").expect("manifest");

        assert_eq!(find_root(&deep).as_deref(), Some(dir.as_path()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_tree_with_no_book_has_no_root() {
        let dir = std::env::temp_dir().join("choir-docs-no-book");
        std::fs::create_dir_all(&dir).expect("scratch tree");
        // `/` has no book.toml either, so the walk ends at None rather
        // than looping.
        assert!(find_root(&dir).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_index_links_every_crate_and_nothing_else() {
        let html = api_index(&["choir_node".to_string(), "choir_view".to_string()]);
        assert!(html.contains("href=\"choir_node/index.html\""));
        assert!(html.contains("href=\"choir_view/index.html\""));
        assert!(
            html.contains("href=\"../index.html\""),
            "no way back to the book"
        );
        assert!(
            html.contains("choir-tokens.css"),
            "the index must use the book's palette"
        );
        // The rule the node's own pages follow, and the reason this page
        // works from a file:// URL with no network.
        assert!(!html.contains("<script"), "the index must run nothing");
        assert!(!html.contains("http://"), "no external request");
    }

    #[test]
    fn an_empty_workspace_still_renders_a_page() {
        let html = api_index(&[]);
        assert!(html.contains("<ul>"), "the list is still well-formed");
        assert!(html.contains("</html>"));
    }

    const BASE: &str = "https://forge.invalid/o/choir/blob/abc123";

    /// The four shapes the book actually contains, taken from the pages
    /// that were broken: a climb from the root, a climb from one level
    /// down, one with a fragment, and one that stays inside.
    #[test]
    fn a_link_that_leaves_the_book_is_repointed_at_the_repository() {
        assert_eq!(
            repoint_one("../DECISIONS.html", 0, BASE).as_deref(),
            Some("https://forge.invalid/o/choir/blob/abc123/DECISIONS.md")
        );
        assert_eq!(
            repoint_one("../../templates/README.html", 1, BASE).as_deref(),
            Some("https://forge.invalid/o/choir/blob/abc123/templates/README.md")
        );
        assert_eq!(
            repoint_one("../ERRORS.html#e_no_such_ref", 0, BASE).as_deref(),
            Some("https://forge.invalid/o/choir/blob/abc123/ERRORS.md#e_no_such_ref")
        );
    }

    /// A link inside the book is the common case and must not move. The
    /// second of these is the one that would break a whole chapter: from
    /// `using/workflow.html`, `../architecture.html` is a page the book
    /// does contain.
    #[test]
    fn a_link_that_stays_inside_the_book_is_left_alone() {
        assert_eq!(repoint_one("architecture.html", 0, BASE), None);
        assert_eq!(repoint_one("../architecture.html", 1, BASE), None);
        assert_eq!(repoint_one("operating/limits.html", 0, BASE), None);
        assert_eq!(repoint_one("#start-here", 0, BASE), None);
        assert_eq!(repoint_one("https://example.invalid/x", 0, BASE), None);
    }

    /// A climb past the checkout is not a repository path, and guessing
    /// one would be a link to somewhere nobody named.
    #[test]
    fn a_link_that_leaves_the_checkout_too_is_left_alone() {
        assert_eq!(repoint_one("../../../elsewhere.html", 0, BASE), None);
    }

    /// Only what mdBook rewrote on the way in comes back. A stylesheet or
    /// an image that climbs is not a markdown page.
    #[test]
    fn only_a_page_mdbook_renamed_is_renamed_back() {
        assert_eq!(repoint_one("../theme/custom.css", 0, BASE), None);
    }

    #[test]
    fn the_rewrite_touches_hrefs_and_nothing_else() {
        let html = "<p>see ../DECISIONS.html</p><a href=\"../DECISIONS.html\">x</a>";
        let out = repoint_escaping_links(html, 0, BASE);
        assert!(
            out.contains("<p>see ../DECISIONS.html</p>"),
            "prose was rewritten: {out}"
        );
        assert!(
            out.contains("href=\"https://forge.invalid/o/choir/blob/abc123/DECISIONS.md\""),
            "the link was not rewritten: {out}"
        );
    }

    #[test]
    fn the_node_link_replaces_its_marker_and_nothing_when_unmarked() {
        let stamped = stamp_node_link(&format!("<p>{NODE_LINK_MARKER}</p>"), "https://n.invalid/");
        assert!(
            stamped.contains("href=\"https://n.invalid\""),
            "the trailing slash survived: {stamped}"
        );
        assert!(!stamped.contains(NODE_LINK_MARKER));
        // A page without the marker is returned unchanged, which is what
        // makes it safe to run over every page in the book.
        assert_eq!(stamp_node_link("<p>x</p>", "https://n.invalid"), "<p>x</p>");
    }
}
