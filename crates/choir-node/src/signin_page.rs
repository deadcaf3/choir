//! The page a person signs in on, in place of the browser's own
//! credential dialog (D71).
//!
//! Before this, an unauthenticated browser request was answered with
//! `WWW-Authenticate: Basic`, and what the reader saw was a grey box
//! drawn by their browser: no explanation of what this node is, no way to
//! offer a passkey, and nothing a page can style. It also asked for a
//! password on a node whose *write* credential is already a passkey, so a
//! person could approve an operation with a fingerprint and then be asked
//! to type a secret to look at the result.
//!
//! The page reads correctly with scripting off, like every other surface
//! here, and says so plainly rather than presenting a button that cannot
//! work: without script there is no ceremony to run, and the honest
//! fallback is the credential the operator issued.

/// A rendered page, matching [`crate::account_page::Page`].
pub(crate) struct Page {
    /// HTTP status.
    pub status: u16,
    /// The document.
    pub html: String,
}

/// Escapes text for HTML, the same five characters the sibling pages do.
fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// The sign-in page, returning to `next` once the ceremony succeeds.
///
/// `passkeys` is whether this node offers the ceremony at all (D71). With
/// it off the page still renders, because it is what an unauthenticated
/// browser is now shown, and it names the credential that does work here
/// instead of a button that would 503.
pub(crate) fn render(passkeys: bool, next: &str, chrome: crate::browse::Chrome<'_>) -> Page {
    let mut h = String::with_capacity(4 * 1024);
    h.push_str("<!doctype html><html lang=\"en\"");
    crate::browse::theme_attribute(&mut h, chrome.theme);
    h.push_str("><head><meta charset=\"utf-8\">");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    h.push_str("<title>choir: sign in</title>");
    h.push_str(crate::ui::STYLE);
    h.push_str("</head><body>");
    h.push_str("<a class=\"skip\" href=\"#main\">Skip to content</a>");
    h.push_str("<header class=\"top\"><h1>Sign in</h1><div class=\"sub\">");
    h.push_str("<span class=\"pill\"><a href=\"/\">node</a></span></div></header>");
    h.push_str("<main id=\"main\"><section>");

    if passkeys {
        h.push_str("<p>This node knows you by a passkey. There is nothing to type: your ");
        h.push_str("browser will ask for the fingerprint, face, or device PIN the passkey ");
        h.push_str("is held behind.</p>");
        // `next` is rendered onto the element rather than read from the
        // query string by the script, so one place decides where a
        // sign-in returns to and that place is the server.
        h.push_str("<div id=\"signin\" hidden data-next=\"");
        h.push_str(&esc(next));
        h.push_str("\"><button id=\"signin-go\">Use a passkey</button>");
        h.push_str("<p id=\"signin-said\" class=\"note\" hidden></p></div>");
        h.push_str("<noscript><p class=\"note\">A passkey needs scripting, which is off in ");
        h.push_str("this browser. Everything here is also reachable with the credential you ");
        h.push_str("were issued, through the <code>choir</code> command line.</p></noscript>");
        h.push_str("<p class=\"note\">No passkey yet? Enrol one from your account page after ");
        h.push_str("signing in with the credential you were issued.</p>");
    } else {
        h.push_str("<p class=\"note\">This node does not offer passkeys, so there is no ");
        h.push_str("ceremony to run here. Use the credential you were issued, through the ");
        h.push_str("<code>choir</code> command line or an ordinary HTTP client.</p>");
    }

    h.push_str("</section></main>");
    h.push_str("<footer>Every write here is a signed operation.</footer>");
    // Only when there is a ceremony to drive. A page with no controls
    // pulling the script anyway would widen its own policy for nothing,
    // and the test that checks which pages are allowed to run script
    // reads exactly this.
    if passkeys {
        h.push_str(crate::ui::CEREMONY_SCRIPT);
    }
    h.push_str("</body></html>");
    Page {
        status: 401,
        html: h,
    }
}

#[cfg(test)]
mod tests {
    /// The ids are the whole contract between this page and
    /// [`crate::ui::WEBAUTHN_JS`], the same contract the account page
    /// keeps and for the same reason: rename one on either side and the
    /// button renders, nothing binds to it, and the page looks exactly as
    /// it should.
    #[test]
    fn the_shared_script_looks_for_the_ids_this_page_emits() {
        for id in ["signin", "signin-go", "signin-said"] {
            assert!(
                crate::ui::WEBAUTHN_JS.contains(&format!("'{id}'")),
                "the shared script never looks for {id}"
            );
        }
    }

    /// A node without passkeys still has to answer an unauthenticated
    /// browser, and what it must not do is offer a ceremony that would be
    /// refused.
    #[test]
    fn a_node_without_passkeys_offers_no_ceremony() {
        let page = super::render(false, "/", crate::browse::Chrome::default());
        assert_eq!(page.status, 401);
        assert!(!page.html.contains("signin-go"), "no switch, no button");
        assert!(page.html.contains("does not offer passkeys"));
        assert!(!page.html.contains("<script"), "no ceremony, no script");
    }

    /// The control starts hidden and the shared script is what reveals
    /// it, so a page carrying the markup without the tag renders a
    /// sign-in page with no way to sign in.
    ///
    /// That is exactly what shipped: the page was right, the ceremony was
    /// right, and the two were never introduced. A person opening it saw
    /// an explanation of passkeys and no button.
    #[test]
    fn the_page_that_offers_the_ceremony_also_loads_it() {
        let page = super::render(true, "/", crate::browse::Chrome::default());
        assert!(page.html.contains("signin-go"), "the control is rendered");
        assert!(
            page.html.contains(crate::ui::CEREMONY_SCRIPT),
            "and the script that unhides it is loaded: {}",
            page.html
        );
    }

    /// The status is 401 and not 200: the request that produced this page
    /// was unauthorized, and a cache or a client reading the code must
    /// see that rather than a successful page.
    #[test]
    fn the_page_is_an_unauthorized_answer_not_a_successful_one() {
        let page = super::render(true, "/r/", crate::browse::Chrome::default());
        assert_eq!(page.status, 401);
        assert!(page.html.contains("data-next=\"/r/\""));
    }
}
