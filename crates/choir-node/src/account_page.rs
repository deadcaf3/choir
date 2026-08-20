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

/// The account page for `user`.
///
/// `store` is `None` on a node without `--accounts-file`, which is not an
/// error: it is a node where nobody has an account, and the page says
/// that rather than 404ing on a path that exists.
pub(crate) fn render(
    store: Option<&Accounts>,
    user: &str,
    chrome: crate::browse::Chrome<'_>,
) -> Page {
    let mut h = String::with_capacity(4 * 1024);
    h.push_str("<!doctype html><html lang=\"en\"");
    crate::browse::theme_attribute(&mut h, chrome.theme);
    h.push_str("><head><meta charset=\"utf-8\">");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    h.push_str("<title>choir: your account</title>");
    h.push_str(crate::ui::STYLE);
    h.push_str("</head><body>");
    h.push_str("<a class=\"skip\" href=\"#main\">Skip to content</a>");
    h.push_str("<header class=\"top\"><h1>");
    h.push_str(&esc(user));
    h.push_str("</h1><div class=\"sub\"><span class=\"pill\"><a href=\"/r/\">repositories</a>");
    h.push_str("</span><span class=\"pill\"><a href=\"/\">node</a></span></div></header>");
    h.push_str("<main id=\"main\">");

    let Some(store) = store else {
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
        return Page {
            status: 200,
            html: close(h),
        };
    }
    if enrolled.is_empty() {
        h.push_str("<p class=\"empty\">None yet. A passkey lets you approve reviews from this ");
        h.push_str("browser, with your fingerprint, face or hardware key. Your token keeps ");
        h.push_str("working either way.</p>");
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
    h.push_str("<div id=\"enrol\" hidden data-user=\"");
    h.push_str(&esc(user));
    h.push_str("\"><button id=\"enrol-go\">Add a passkey</button> ");
    h.push_str("<input id=\"enrol-label\" maxlength=\"64\" placeholder=\"this laptop\">");
    h.push_str("<p id=\"enrol-said\" class=\"note\" hidden></p></div>");
    h.push_str(crate::ui::CEREMONY_SCRIPT);
    h.push_str("</section>");
    Page {
        status: 200,
        html: close(h),
    }
}

/// Closing tags, matching the browse shell.
fn close(mut h: String) -> String {
    h.push_str("</main></body></html>");
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

    /// A node with no store, and a credential with no account, are
    /// different situations with different repairs, and neither is an
    /// error page.
    #[test]
    fn the_page_distinguishes_no_store_from_no_account() {
        let page = super::render(None, "alice", crate::browse::Chrome::default());
        assert_eq!(page.status, 200);
        assert!(page.html.contains("does not run account self-service"));
        assert!(!page.html.contains("<script"), "no store, no ceremony");
    }
}
