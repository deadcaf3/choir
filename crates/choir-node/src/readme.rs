//! A repository's README, rendered.
//!
//! Markdown parsing is [`pulldown_cmark`]; the policy is here. Two rules
//! decide everything below, and both exist because a README is content
//! this node did not author:
//!
//! 1. **No raw HTML reaches the page.** CommonMark passes HTML through by
//!    design. That is the injection vector, and the page's
//!    `default-src 'none'` would only stop the half of it that fetches.
//!    Raw blocks and inline spans are dropped, not escaped, because a
//!    reader is better served by a missing badge than by a page that
//!    shows them markup.
//! 2. **Only schemes a reader can be handed are linked.** `javascript:`
//!    and `data:` are not links, they are code; a link carrying one keeps
//!    its text and loses its anchor.
//!
//! Images become links for a third reason, which is not security: the
//! CSP forbids loading them, so an `<img>` would render as a broken icon
//! on every README that has a badge or a screenshot. A link says what is
//! there and can be followed.
//!
//! A fourth rule arrived with the route table: **a relative link is
//! resolved against the repository, not against the page**. `[guide](
//! docs/guide.md)` in the root README is a link to a file that is right
//! there, and a browser on `/r/<owner>/<repo>` resolves it to
//! `/r/<owner>/docs/guide.md` — a repository named `docs` under an owner
//! named after this one. Every relative link in every README on this
//! node was dead, and dead in a way that looked like a missing
//! repository rather than a bad link.

use pulldown_cmark::{CowStr, Event, LinkType, Options, Parser, Tag, TagEnd};

/// Where a README sits, so a relative link in it can be resolved.
///
/// Carried rather than derived, because a README renders at every level
/// of the tree and `docs/guide.md` inside `src/README.md` means
/// `src/docs/guide.md`.
#[derive(Clone, Copy)]
pub(crate) struct Base<'a> {
    /// `owner/repo`, without the `.git`.
    pub repo: &'a str,
    /// The revision the page is being read at.
    pub rev: &'a str,
    /// The directory this README is in, `""` at the repository root, with
    /// no leading or trailing slash.
    pub dir: &'a str,
}

/// Resolves one relative destination against `base`, or `None` when it
/// is not a relative path this should touch.
///
/// Left alone: anything with a scheme, anything already rooted at `/`
/// (which is a path on this node and so already resolves), and a bare
/// fragment or query, which addresses the page the reader is on.
///
/// Everything else becomes a `blob` URL. A relative link naming a
/// *directory* still 404s — telling the two apart needs a `git` call per
/// link, and the answer would be a lookup per README rather than per
/// page — but a 404 on one link is what this replaces node-wide.
fn resolve(dest: &str, base: Base<'_>) -> Option<String> {
    if dest.is_empty() || dest.starts_with('/') || dest.starts_with(['#', '?']) {
        return None;
    }
    // The same scheme test `linkable` makes, for the same reason: a colon
    // inside a path is not a scheme.
    if let Some(colon) = dest.find(':') {
        let head = &dest[..colon];
        if !head.contains('/') && !head.contains('?') && !head.contains('#') {
            return None;
        }
    }
    let (path, tail) = match dest.find(['#', '?']) {
        Some(at) => (&dest[..at], &dest[at..]),
        None => (dest, ""),
    };
    let mut segments: Vec<&str> = base
        .dir
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                // A link that climbs past the repository root is not a
                // path in this repository, and inventing one would point
                // the reader at somebody else's.
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    if segments.is_empty() {
        return None;
    }
    let joined = segments.join("/");
    Some(format!(
        "/r/{}/blob/{}/{}{tail}",
        base.repo,
        crate::browse::url_path(base.rev),
        crate::browse::url_path(&joined)
    ))
}

/// The filenames looked for at a repository root, in order of preference.
///
/// Case is not folded: git is case-sensitive, and a repository may hold
/// both. The list is the set actually in use, not every spelling that is
/// conceivable.
pub(crate) const NAMES: [&str; 4] = ["README.md", "README.markdown", "README", "readme.md"];

/// Renders README source to the HTML fragment the repository page embeds.
pub(crate) fn render(markdown: &str, base: Base<'_>) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_SMART_PUNCTUATION);

    let mut html = String::with_capacity(markdown.len());
    pulldown_cmark::html::push_html(
        &mut html,
        Parser::new_ext(markdown, options).filter_map(|event| sanitize(event, base)),
    );
    html
}

/// One event, as it is allowed to appear on the page — or `None`.
fn sanitize<'a>(event: Event<'a>, base: Base<'_>) -> Option<Event<'a>> {
    /// The destination this link is served with: refused, resolved
    /// against the repository, or exactly as written.
    fn destination<'a>(dest: CowStr<'a>, base: Base<'_>) -> CowStr<'a> {
        if !linkable(&dest) {
            // An anchor with no destination rather than no anchor:
            // dropping the tag here would leave its `TagEnd` to close
            // an element that was never opened.
            return CowStr::Borrowed("");
        }
        match resolve(&dest, base) {
            Some(resolved) => CowStr::Boxed(resolved.into_boxed_str()),
            None => dest,
        }
    }

    match event {
        // Rule 1. Both halves: `Html` is a block, `InlineHtml` is a span,
        // and dropping only one of them leaves the other as the hole.
        Event::Html(_) | Event::InlineHtml(_) => None,
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Some(Event::Start(Tag::Link {
            link_type,
            dest_url: destination(dest_url, base),
            title,
            id,
        })),
        // Rule 3. The alt text is the events between here and `TagEnd`,
        // so re-tagging the pair turns it into the link's text.
        Event::Start(Tag::Image {
            dest_url,
            title,
            id,
            ..
        }) => Some(Event::Start(Tag::Link {
            link_type: LinkType::Inline,
            dest_url: destination(dest_url, base),
            title,
            id,
        })),
        Event::End(TagEnd::Image) => Some(Event::End(TagEnd::Link)),
        other => Some(other),
    }
}

/// Whether a destination is one this page will hand a reader.
///
/// Relative URLs pass: they resolve against this origin, which is the
/// node the reader is already authenticated to. Anything carrying a
/// scheme must name one of three, because the set of schemes that execute
/// rather than navigate is open-ended and an allowlist is the only side of
/// that question with a finite answer.
fn linkable(url: &str) -> bool {
    // `java\tscript:` and `java\nscript:` are the same URL to a browser
    // and a different string to a naive parser, so the comparison is made
    // against a form with no whitespace or control characters at all.
    let cleaned: String = url
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_control())
        .collect();
    let lower = cleaned.to_ascii_lowercase();
    match lower.find(':') {
        None => true,
        Some(colon) => {
            let scheme = &lower[..colon];
            // A colon inside a path or query is not a scheme:
            // `docs/a:b` and `?x=1:2` are ordinary relative URLs.
            if scheme.contains('/') || scheme.contains('?') || scheme.contains('#') {
                return true;
            }
            matches!(scheme, "http" | "https" | "mailto")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The root README of `agents/demo` at `main`, which is where every
    /// relative link in these tests is written from.
    const ROOT: Base<'static> = Base {
        repo: "agents/demo",
        rev: "main",
        dir: "",
    };

    /// The rule that matters most, stated as the four shapes it has to
    /// cover: a block of HTML, an inline span, a script URL, and a script
    /// URL wearing whitespace to get past a naive check.
    #[test]
    fn no_readme_can_put_markup_or_a_script_url_on_the_page() {
        let html = render(
            "<script>alert(1)</script>\n\n\
             Text with <img src=x onerror=alert(1)> inline.\n\n\
             [click](javascript:alert(1))\n\n\
             [sneaky](java\tscript:alert(1))\n\n\
             [data](data:text/html;base64,PHNjcmlwdD4=)\n",
            ROOT,
        );
        assert!(!html.contains("<script"), "raw HTML block survived: {html}");
        assert!(!html.contains("onerror"), "inline HTML survived: {html}");
        assert!(
            !html.to_ascii_lowercase().contains("javascript:"),
            "a script URL survived: {html}"
        );
        assert!(
            !html.to_ascii_lowercase().contains("data:"),
            "a data URL survived: {html}"
        );
        // The text is still there — this is a filter, not a deletion.
        assert!(html.contains("click"), "the link text was lost: {html}");
        assert!(html.contains("sneaky"), "the link text was lost: {html}");
    }

    /// The ordinary case has to keep working, or the rule above is being
    /// enforced by rendering nothing.
    #[test]
    fn ordinary_markdown_still_renders() {
        let html = render(
            "# Title\n\nSome **bold** text and `code`.\n\n\
             - one\n- two\n\n\
             ```rust\nfn main() {}\n```\n\n\
             [docs](https://example.invalid/a) and [rel](docs/guide.md)\n\n\
             | a | b |\n|---|---|\n| 1 | 2 |\n",
            ROOT,
        );
        assert!(html.contains("<h1>"), "no heading: {html}");
        assert!(html.contains("<strong>bold</strong>"), "no bold: {html}");
        assert!(html.contains("<code>"), "no code span: {html}");
        assert!(html.contains("<li>"), "no list: {html}");
        assert!(html.contains("<table>"), "no table: {html}");
        assert!(
            html.contains("href=\"https://example.invalid/a\""),
            "an https link was refused: {html}"
        );
        assert!(
            html.contains("href=\"/r/agents/demo/blob/main/docs/guide.md\""),
            "a relative link was not resolved against the repository: {html}"
        );
    }

    /// An image cannot load under this page's CSP, so it is rendered as
    /// something a reader can act on instead of a broken icon.
    #[test]
    fn an_image_becomes_a_link_carrying_its_alt_text() {
        let html = render("![the architecture diagram](docs/arch.png)\n", ROOT);
        assert!(
            !html.contains("<img"),
            "an image tag reached the page: {html}"
        );
        assert!(
            html.contains("href=\"/r/agents/demo/blob/main/docs/arch.png\""),
            "the image is not reachable at all: {html}"
        );
        assert!(
            html.contains("the architecture diagram"),
            "the alt text was dropped: {html}"
        );
    }

    /// A README below the root resolves against its own directory, which
    /// is the whole reason the base is carried rather than assumed.
    #[test]
    fn a_readme_in_a_subdirectory_resolves_against_that_directory() {
        let base = Base {
            dir: "src/net",
            ..ROOT
        };
        let html = render("[sibling](client.rs) and [up](../lib.rs)\n", base);
        assert!(
            html.contains("href=\"/r/agents/demo/blob/main/src/net/client.rs\""),
            "a sibling link did not stay in the directory: {html}"
        );
        assert!(
            html.contains("href=\"/r/agents/demo/blob/main/src/lib.rs\""),
            "a link climbing one level did not climb: {html}"
        );
    }

    /// The three shapes that must not be rewritten, because each already
    /// addresses something: an absolute URL, a path on this node, and a
    /// heading on the page the reader is on.
    #[test]
    fn a_link_that_already_resolves_is_left_exactly_as_written() {
        let html = render(
            "[out](https://example.invalid/a) [abs](/r/agents/other) [here](#usage)\n",
            ROOT,
        );
        assert!(
            html.contains("href=\"https://example.invalid/a\""),
            "{html}"
        );
        assert!(html.contains("href=\"/r/agents/other\""), "{html}");
        assert!(html.contains("href=\"#usage\""), "{html}");
    }

    /// A fragment on a relative link survives the rewrite; a climb past
    /// the repository root does not become a path in another one.
    #[test]
    fn a_fragment_is_kept_and_a_climb_out_of_the_repository_is_refused() {
        let html = render("[a](docs/guide.md#install) [b](../../etc/passwd)\n", ROOT);
        assert!(
            html.contains("href=\"/r/agents/demo/blob/main/docs/guide.md#install\""),
            "the fragment was lost: {html}"
        );
        assert!(
            !html.contains("passwd\" ") && !html.contains("/r/agents/demo/blob/main/etc"),
            "a link climbing out of the repository was given a path in it: {html}"
        );
    }

    /// A name outside ASCII is encoded the way the listing encodes it —
    /// otherwise the link a README renders and the link the listing
    /// renders disagree about the same file.
    ///
    /// `?` and `#` are deliberately *not* in this test: in a URL they
    /// start a query and a fragment, and a README author writing one
    /// means the URL syntax. A filename containing either is reachable
    /// from the listing, which knows it is holding a name rather than a
    /// URL.
    #[test]
    fn a_relative_link_is_encoded_the_way_the_listing_encodes_one() {
        let html = render("[c](docs/café.md)\n", ROOT);
        assert!(
            html.contains("href=\"/r/agents/demo/blob/main/docs/caf%C3%A9.md\""),
            "a name outside ASCII was left unencoded: {html}"
        );
    }
}
