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

use pulldown_cmark::{CowStr, Event, LinkType, Options, Parser, Tag, TagEnd};

/// The filenames looked for at a repository root, in order of preference.
///
/// Case is not folded: git is case-sensitive, and a repository may hold
/// both. The list is the set actually in use, not every spelling that is
/// conceivable.
pub(crate) const NAMES: [&str; 4] = ["README.md", "README.markdown", "README", "readme.md"];

/// Renders README source to the HTML fragment the repository page embeds.
pub(crate) fn render(markdown: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_SMART_PUNCTUATION);

    let mut html = String::with_capacity(markdown.len());
    pulldown_cmark::html::push_html(
        &mut html,
        Parser::new_ext(markdown, options).filter_map(sanitize),
    );
    html
}

/// One event, as it is allowed to appear on the page — or `None`.
fn sanitize(event: Event<'_>) -> Option<Event<'_>> {
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
            dest_url: if linkable(&dest_url) {
                dest_url
            } else {
                // An anchor with no destination rather than no anchor:
                // dropping the tag here would leave its `TagEnd` to close
                // an element that was never opened.
                CowStr::Borrowed("")
            },
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
            dest_url: if linkable(&dest_url) {
                dest_url
            } else {
                CowStr::Borrowed("")
            },
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
            html.contains("href=\"docs/guide.md\""),
            "a relative link was refused: {html}"
        );
    }

    /// An image cannot load under this page's CSP, so it is rendered as
    /// something a reader can act on instead of a broken icon.
    #[test]
    fn an_image_becomes_a_link_carrying_its_alt_text() {
        let html = render("![the architecture diagram](docs/arch.png)\n");
        assert!(
            !html.contains("<img"),
            "an image tag reached the page: {html}"
        );
        assert!(
            html.contains("href=\"docs/arch.png\""),
            "the image is not reachable at all: {html}"
        );
        assert!(
            html.contains("the architecture diagram"),
            "the alt text was dropped: {html}"
        );
    }
}
