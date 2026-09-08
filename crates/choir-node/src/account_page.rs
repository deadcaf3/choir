//! The page a person manages their own passkeys on (D39).
//!
//! D39's write path needed a credential before it could be used, and
//! until this existed the only way to get one was a client that already
//! held a P-256 key — `openssl` in every test, and nothing at all for a
//! human with a browser. The endpoint was reachable and the ceremony that
//! calls it was missing, which is a complete feature with no door.
//!
//! It lives in its own module rather than in [`crate::browse`] because
//! the two answer different questions. `browse` is the repository
//! surface, keyed by repository and gated by repository read; this is
//! keyed by the caller and gated by nothing but being authenticated,
//! since the only account it ever shows is your own.
//!
//! The same scope rules as the review page apply and are asserted the
//! same way: one same-origin script, no library, no build step, nothing
//! off this origin, and the page reads correctly with scripting off —
//! where it says so and names the endpoint, rather than presenting a
//! button that cannot work.

use crate::accounts::Accounts;

/// A rendered page: status and HTML, matching [`crate::browse::Rendered`]
/// without borrowing its repository-shaped fields.
pub(crate) struct Page {
    /// HTTP status.
    pub status: u16,
    /// The document.
    pub html: String,
}

/// Escapes text for HTML. The same five characters
/// [`crate::browse`] escapes, kept local so this module has no reason to
/// reach into that one.
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

/// Opens the document, the chrome and the header every page here shares.
///
/// One function because two of them would be two places for the console
/// pill to appear on one page and not the other, and a reader who found
/// it on one would reasonably conclude the other had lost it.
fn open_page(h: &mut String, user: &str, console: bool, chrome: crate::browse::Chrome<'_>) {
    h.push_str("<!doctype html><html lang=\"en\"");
    crate::browse::theme_attribute(h, chrome.theme);
    h.push_str("><head><meta charset=\"utf-8\">");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    h.push_str("<title>choir: your account</title>");
    h.push_str(crate::ui::STYLE);
    h.push_str("</head><body>");
    h.push_str("<a class=\"skip\" href=\"#main\">Skip to content</a>");
    // The same fixed bar every other page carries. Without it this page
    // was the one surface with no way back except the two pills below
    // it, no search box, and — once the palette became a control rather
    // than a system setting — no way to change it. A page that drops
    // the chrome reads as a different product, and this one is reached
    // from a link in that chrome.
    crate::browse::chrome(h, crate::browse::Bar::index(chrome));
    h.push_str("<header class=\"top\"><h1>");
    h.push_str(&esc(user));
    h.push_str("</h1><div class=\"sub\"><span class=\"pill\"><a href=\"/r/\">repositories</a>");
    h.push_str("</span><span class=\"pill\"><a href=\"/\">node</a></span>");
    // The operator's console (D72), offered only to somebody who may use
    // it. A link everybody can see and only one person can follow is a
    // link that teaches most readers what they are not.
    if console {
        h.push_str("<span class=\"pill\"><a href=\"/people\">people</a></span>");
    }
    h.push_str("</div></header>");
    h.push_str("<main id=\"main\">");
}

/// The account page for `user`.
///
/// `store` is `None` on a node without `--accounts-file`, which is not an
/// error: it is a node where nobody has an account, and the page says
/// that rather than 404ing on a path that exists.
///
/// `in_session` is whether the caller arrived on a browser session
/// rather than a credential (D71). The sign-out control is rendered only
/// then, because signing out of a Basic-auth request does nothing the
/// reader can see: the browser holds the header and sends it again on the
/// next request. Offering a button that appears to do nothing is worse
/// than offering none.
pub(crate) fn render(
    store: Option<&Accounts>,
    user: &str,
    in_session: bool,
    console: bool,
    node: Option<&str>,
    chrome: crate::browse::Chrome<'_>,
) -> Page {
    let mut h = String::with_capacity(4 * 1024);
    open_page(&mut h, user, console, chrome);
    h.push_str("<main id=\"main\">");

    let Some(store) = store else {
        sign_out(&mut h, in_session);
        h.push_str("<section><p class=\"note\">This node does not run account self-service, ");
        h.push_str("so there is nothing here to manage. Credentials are whatever the operator ");
        h.push_str("wrote in the node's auth file.</p></section>");
        return Page {
            status: 200,
            html: close(h),
        };
    };

    h.push_str("<section><h2>Passkeys</h2>");
    let enrolled = store.passkeys_json(user);
    if !store.has_account(user) {
        // An operator credential from `--auth-file` is a real credential
        // with no account record behind it. Saying "no passkeys yet"
        // would invite an enrolment that always fails.
        h.push_str("<p class=\"note\">This credential was written by the operator into the ");
        h.push_str("node's auth file rather than issued as an account, so it cannot hold a ");
        h.push_str("passkey. Passkeys are enrolled on issued accounts.</p>");
        // Saying only what someone cannot do is a dead end, and this
        // page had no way out of it. An operator's write path is the one
        // it always was, which is worth saying rather than leaving them
        // to infer.
        h.push_str("<p class=\"note\">Your write path is the CLI, signed with your actor key. ");
        h.push_str("That is unchanged and stays available whatever anyone enrols.</p></section>");
        sign_out(&mut h, in_session);
        return Page {
            status: 200,
            html: close(h),
        };
    }
    if enrolled.is_empty() {
        // The one thing a person who has just signed in with a password
        // is here to do (D74), so it is stated as an instruction rather
        // than as a description of a feature.
        h.push_str("<p class=\"empty\">None yet. Add one now and you will not type that ");
        h.push_str("password again: your browser will ask for a fingerprint, face or ");
        h.push_str("hardware key instead. The password keeps working for git and the CLI ");
        h.push_str("either way.</p>");
    } else {
        h.push_str("<table><thead><tr><th>name</th><th>credential</th><th>added</th>");
        h.push_str("</tr></thead><tbody>");
        for key in &enrolled {
            h.push_str("<tr><td>");
            h.push_str(&esc(key["label"].as_str().unwrap_or("passkey")));
            h.push_str("</td><td class=\"mono\">");
            let id = key["credential_id"].as_str().unwrap_or("");
            h.push_str(&esc(&id.chars().take(16).collect::<String>()));
            h.push_str("</td><td class=\"mono muted\">");
            h.push_str(&esc(&key["created_at"].to_string()));
            h.push_str("</td></tr>");
        }
        h.push_str("</tbody></table>");
    }

    h.push_str(
        "<noscript><p class=\"note\">Enrolling a passkey needs the browser to create a \
                key pair, which it will only do with scripting enabled. With it off, POST a \
                credential you already hold to <code>/api/accounts/passkey</code>.</p></noscript>",
    );
    // Field first and then the button, with the label above the field.
    // It was the other way round — a button, then the box naming the
    // thing the button had already made — which reads as an afterthought
    // and, with no label on it at all, as a search box.
    h.push_str("<div id=\"enrol\" hidden data-user=\"");
    h.push_str(&esc(user));
    h.push_str(
        "\"><p><label for=\"enrol-label\">What to call this device</label><br>\
         <input id=\"enrol-label\" maxlength=\"64\" placeholder=\"this laptop\"></p>",
    );
    h.push_str("<p><button class=\"go\" id=\"enrol-go\">Add a passkey</button></p>");
    h.push_str("<p id=\"enrol-said\" class=\"note\" hidden></p></div>");
    h.push_str(crate::ui::CEREMONY_SCRIPT);
    h.push_str("</section>");
    tokens(&mut h, node, user, store.has_token(user));
    sign_out(&mut h, in_session);
    Page {
        status: 200,
        html: close(h),
    }
}

/// The credential git and the CLI need, made on request (D75).
///
/// A passwordless account has none, and that is not a gap to be filled
/// at redemption: git speaks basic auth and cannot present a passkey, so
/// a secret is needed for exactly that job and for nothing else. Minting
/// it here means the person asking has already been authenticated by
/// this node, and that they are asking because something actually
/// wanted one.
///
/// One token per account, so a second mint replaces the first. Said out
/// loud, because the old one stops working the moment the button is
/// pressed and somebody with a working `git push` deserves to know that
/// before they press it.
fn tokens(h: &mut String, node: Option<&str>, user: &str, has_one: bool) {
    h.push_str("<section><h2>Tokens</h2>");
    h.push_str(
        "<p>Git and the <code>choir</code> command line authenticate with a password, \
         because neither can present a passkey. This is that password, and it is the only \
         thing it is for.</p>",
    );
    if has_one {
        h.push_str(
            "<p class=\"note\">You have one. Making another replaces it, and whatever is \
             using the old one stops working until you paste the new one in.</p>",
        );
    } else {
        h.push_str("<p class=\"empty\">None. Nothing needs one until you clone or push.</p>");
    }
    h.push_str("<form method=\"post\" action=\"/account/token\"><p>");
    h.push_str("<button type=\"submit\">");
    h.push_str(if has_one {
        "Replace my token"
    } else {
        "Make a token"
    });
    h.push_str("</button></p></form>");
    let _ = (node, user);
    h.push_str("</section>");
}

/// The page that hands one over, after the `POST` that made it.
///
/// Rendered directly rather than redirected to, like the invite link on
/// the console: this node kept only a hash, so a redirect would drop the
/// one copy that exists.
pub(crate) fn minted(
    token: &str,
    user: &str,
    node: Option<&str>,
    replaced: bool,
    chrome: crate::browse::Chrome<'_>,
) -> Page {
    let mut h = String::with_capacity(4 * 1024);
    open_page(&mut h, user, false, chrome);
    h.push_str("<section><h2>Your token</h2>");
    h.push_str(
        "<p class=\"lede\">Copy it now. It is shown once and this node keeps only a hash of \
         it, so nobody -- including the operator -- can show it to you again.</p>",
    );
    h.push_str("<pre class=\"cmd\">");
    h.push_str(&esc(token));
    h.push_str("</pre>");
    if replaced {
        h.push_str(
            "<p class=\"note\">This replaced the one you had. Anything still using that one \
             is now refused.</p>",
        );
    }
    if let Some(node) = node {
        h.push_str("<h3>So git stops asking</h3>");
        h.push_str(
            "<p>Git will ask for this on every push unless it has somewhere to keep it. \
             macOS and Windows come with somewhere; on Linux, run <code>git config --global \
             credential.helper</code> first to check you have one.</p>",
        );
        h.push_str("<pre class=\"cmd\">");
        h.push_str(&esc(&crate::join_page::approve_command(node, user, token)));
        h.push_str("</pre>");
    }
    // A forward action as well as the way back. This was the one page in
    // the walk whose only link went backwards, which leaves a reader who
    // has just copied their token with nothing on the page to do next
    // but use the browser's own controls.
    crate::ui::next_action(
        &mut h,
        "Your token is kept now. <a href=\"/r/\">Open a repository</a> to read the reviews \
         you were granted.",
    );
    h.push_str("<p><span class=\"pill\"><a href=\"/account\">back to your account</a></span></p>");
    h.push_str("</section>");
    Page {
        status: 200,
        html: close(h),
    }
}

/// The way out of a browser session, when the reader arrived on one.
///
/// A plain form, so the control works with scripting off like every
/// other one here; the endpoint answers `303`, which is why this needs
/// no script to follow it.
///
/// Rendered only for a session, because signing out of a Basic-auth
/// request does nothing the reader can see: the browser holds the header
/// and sends it again on the next request. It is also last on the page
/// rather than first -- somebody who has just arrived is here to enrol a
/// passkey, not to leave.
fn sign_out(h: &mut String, in_session: bool) {
    if !in_session {
        return;
    }
    h.push_str("<section><h2>This browser</h2><p class=\"note\">Signed in. Signing out ");
    h.push_str("forgets the session on the node, so this browser stops being you.</p>");
    h.push_str("<form method=\"post\" action=\"/api/signout\">");
    h.push_str("<button type=\"submit\">Sign out</button></form></section>");
}

/// Closing tags, matching the browse shell — including its footer,
/// which this page used to omit. The sentence is the surface's one
/// standing claim about itself, and it was absent from the page where a
/// person enrols the thing that signs on their behalf.
fn close(mut h: String) -> String {
    h.push_str("</main><footer>Every write here is a signed operation.</footer>");
    h.push_str("</body></html>");
    h
}

#[cfg(test)]
mod tests {
    /// The ids are the whole contract between this page and
    /// [`crate::ui::WEBAUTHN_JS`], now that the script is a shared file
    /// rather than a constant sitting next to the markup it drives.
    ///
    /// Rename one on either side and enrolment stops working silently:
    /// the button renders, nothing binds to it, and the page looks
    /// exactly as it should. Both halves name the same four strings
    /// here, so the rename fails a test instead of a person.
    #[test]
    fn the_shared_script_looks_for_the_ids_this_page_emits() {
        for id in ["enrol", "enrol-go", "enrol-label", "enrol-said"] {
            assert!(
                crate::ui::WEBAUTHN_JS.contains(&format!("'{id}'")),
                "the shared script never looks for {id}"
            );
        }
    }

    /// The sign-out control appears only for a session, because signing
    /// out of a Basic-auth request does nothing a reader can see.
    #[test]
    fn signing_out_is_offered_only_to_a_session() {
        let with = super::render(
            None,
            "alice",
            true,
            false,
            None,
            crate::browse::Chrome::default(),
        );
        assert!(with.html.contains("/api/signout"));
        let without = super::render(
            None,
            "alice",
            false,
            false,
            None,
            crate::browse::Chrome::default(),
        );
        assert!(!without.html.contains("/api/signout"));
    }

    /// A node with no store, and a credential with no account, are
    /// different situations with different repairs, and neither is an
    /// error page.
    #[test]
    fn the_page_distinguishes_no_store_from_no_account() {
        let page = super::render(
            None,
            "alice",
            false,
            false,
            None,
            crate::browse::Chrome::default(),
        );
        assert_eq!(page.status, 200);
        assert!(page.html.contains("does not run account self-service"));
        assert!(!page.html.contains("<script"), "no store, no ceremony");
    }
}
