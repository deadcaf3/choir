//! The page a person signs in on, in place of the browser's own
//! credential dialog (D71, D74).
//!
//! Before this, an unauthenticated browser request was answered with
//! `WWW-Authenticate: Basic`, and what the reader saw was a grey box
//! drawn by their browser: no explanation of what this node is, no way to
//! offer a passkey, and nothing a page can style. It also asked for a
//! password on a node whose *write* credential is already a passkey, so a
//! person could approve an operation with a fingerprint and then be asked
//! to type a secret to look at the result.
//!
//! D71 replaced it for the passkey half and left the other half where it
//! was: a link back to the browser dialog, for the one credential
//! everybody has before they have a passkey. So the first sign-in on a
//! node -- the only one that happens to every single person -- was the
//! grey box, and cancelling it left them on the word `unauthorized` in
//! Times New Roman. The route that mattered most was the one that was
//! never replaced.
//!
//! This page now carries both: the ceremony for somebody who has enrolled
//! a passkey, and an ordinary username and password form for somebody who
//! has not. The form is plain HTML, so it works with scripting off and
//! the browser's own password manager offers to remember it -- which is
//! the second half of "never type this again", the first being the
//! passkey the account page enrols.

/// A rendered page, matching [`crate::account_page::Page`].
pub(crate) struct Page {
    /// HTTP status.
    pub status: u16,
    /// The document.
    pub html: String,
}

/// Escapes text for HTML, the same five characters the sibling pages do.
fn esc(text: &str) -> String {
    crate::ui::esc(text)
}

/// What the page has to say about the attempt that produced it.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Said {
    /// Nobody has tried yet: this is the page an unauthenticated request
    /// was answered with.
    Nothing,
    /// A username and password were sent and did not match.
    ///
    /// Deliberately one variant for both halves. Telling somebody their
    /// username exists but the password is wrong is telling anybody with
    /// a list of names which ones this node has issued.
    NoMatch,
}

/// The sign-in page, returning to `next` once either route succeeds.
///
/// `passkeys` is whether this node offers the ceremony at all (D71). With
/// it off the page still renders and the password form is still the way
/// in, because that form is what an unauthenticated browser needs
/// whatever else the node offers.
pub(crate) fn render(
    passkeys: bool,
    next: &str,
    said: Said,
    chrome: crate::browse::Chrome<'_>,
) -> Page {
    let mut h = String::with_capacity(4 * 1024);
    h.push_str("<!doctype html><html lang=\"en\"");
    crate::browse::theme_attribute(&mut h, chrome.theme);
    h.push_str("><head><meta charset=\"utf-8\">");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    h.push_str("<title>choir: sign in</title>");
    h.push_str(crate::ui::STYLE);
    h.push_str("</head><body class=\"task gate\">");
    h.push_str("<a class=\"skip\" href=\"#main\">Skip to content</a>");
    h.push_str("<header class=\"top\"><h1>Sign in</h1><div class=\"sub\">");
    h.push_str("<span class=\"pill\"><a href=\"/\">node</a></span></div></header>");
    h.push_str("<main id=\"main\"><section>");

    if said == Said::NoMatch {
        // Above everything, because a reader who just failed is looking
        // at the top of the page and not at the bottom of the form.
        h.push_str(
            "<p class=\"note\">That username and password did not match. \
             Check them and try again.</p>",
        );
    }

    if passkeys {
        // The fast path first, for the person who has one: nothing to
        // type, and no field to tab past on the way to a button.
        h.push_str(
            "<p>If you have enrolled a passkey here, use it. There is nothing to type: \
             your browser will ask for the fingerprint, face, or device PIN it is held \
             behind.</p>",
        );
        // `next` is rendered onto the element rather than read from the
        // query string by the script, so one place decides where a
        // sign-in returns to and that place is the server.
        h.push_str("<div id=\"signin\" hidden data-next=\"");
        h.push_str(&esc(next));
        h.push_str("\"><button id=\"signin-go\">Use a passkey</button>");
        h.push_str("<p id=\"signin-said\" class=\"note\" hidden></p></div>");
        h.push_str("<h2>Or with the credential you were issued</h2>");
    }

    // The form every person meets once, whatever else this node offers,
    // because a passkey is enrolled by somebody already signed in.
    h.push_str("<form method=\"post\" action=\"/signin\">");
    h.push_str("<input type=\"hidden\" name=\"next\" value=\"");
    h.push_str(&esc(next));
    h.push_str("\">");
    // `autocomplete` is the whole reason these carry the names they do:
    // it is what makes a browser's own password manager offer to keep
    // this, which is what stops the credential being retyped from a chat
    // window every morning.
    h.push_str(
        "<p><label for=\"signin-user\">Username</label><br>\
         <input id=\"signin-user\" name=\"user\" autocomplete=\"username\" \
         autocapitalize=\"none\" spellcheck=\"false\" required></p>",
    );
    h.push_str(
        "<p><label for=\"signin-secret\">Password</label><br>\
         <input id=\"signin-secret\" name=\"secret\" type=\"password\" \
         autocomplete=\"current-password\" required></p>",
    );
    // The one decisive control on the page every person meets exactly
    // once. `go` is opted into rather than taken by every submit button,
    // because a console full of them would then be a page full of
    // primary actions.
    h.push_str("<p><button class=\"go\" type=\"submit\">Sign in</button></p>");
    h.push_str("</form>");

    if passkeys {
        h.push_str(
            "<p class=\"note\">First time? Sign in with the username and password you were \
             given, and this node will take you straight to the page that enrols a passkey. \
             After that there is nothing to type.</p>",
        );
        h.push_str(
            "<noscript><p class=\"note\">The passkey button needs scripting, which is off in \
             this browser. The form above does not, and works exactly as well.</p></noscript>",
        );
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
    use super::Said;

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
    /// refused. The form is not a ceremony and is still there.
    #[test]
    fn a_node_without_passkeys_offers_no_ceremony_and_still_offers_a_way_in() {
        let page = super::render(false, "/", Said::Nothing, crate::browse::Chrome::default());
        assert_eq!(page.status, 401);
        assert!(!page.html.contains("signin-go"), "no switch, no button");
        assert!(!page.html.contains("<script"), "no ceremony, no script");
        assert!(
            page.html.contains("name=\"secret\""),
            "no way in: {}",
            page.html
        );
        assert!(page.html.contains("action=\"/signin\""), "{}", page.html);
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
        let page = super::render(true, "/", Said::Nothing, crate::browse::Chrome::default());
        assert!(page.html.contains("signin-go"), "the control is rendered");
        assert!(
            page.html.contains(crate::ui::CEREMONY_SCRIPT),
            "and the script that unhides it is loaded: {}",
            page.html
        );
    }

    /// D74. A first passkey is enrolled by somebody already signed in, so
    /// the route to a first passkey is a route to a first *session* --
    /// and it is on this page, not behind a link to the browser's own
    /// dialog. That link is what the screenshots of the ugly flow were.
    #[test]
    fn the_first_sign_in_happens_on_this_page_and_not_in_browser_chrome() {
        let page = super::render(true, "/r/", Said::Nothing, crate::browse::Chrome::default());
        assert!(
            !page.html.contains("/signin/credential"),
            "the page still hands the reader back to the browser dialog: {}",
            page.html
        );
        assert!(
            page.html.contains("autocomplete=\"current-password\""),
            "{}",
            page.html
        );
        // `next` survives both routes, so a person who was going
        // somewhere still gets there.
        assert!(page.html.contains("data-next=\"/r/\""), "{}", page.html);
        assert!(page.html.contains("value=\"/r/\""), "{}", page.html);
    }

    /// A refusal says one thing for both halves of a wrong credential.
    /// "No such user" and "wrong password" told apart is a way to ask
    /// this node which names it has issued.
    #[test]
    fn a_refusal_does_not_say_which_half_was_wrong() {
        let page = super::render(true, "/", Said::NoMatch, crate::browse::Chrome::default());
        assert_eq!(page.status, 401);
        assert!(page.html.contains("did not match"), "{}", page.html);
        for leak in [
            "no such user",
            "unknown user",
            "wrong password",
            "no account",
        ] {
            assert!(
                !page.html.to_ascii_lowercase().contains(leak),
                "the refusal names which half was wrong: {}",
                page.html
            );
        }
    }

    /// The status is 401 and not 200: the request that produced this page
    /// was unauthorized, and a cache or a client reading the code must
    /// see that rather than a successful page.
    #[test]
    fn the_page_is_an_unauthorized_answer_not_a_successful_one() {
        let page = super::render(true, "/r/", Said::Nothing, crate::browse::Chrome::default());
        assert_eq!(page.status, 401);
    }
}
