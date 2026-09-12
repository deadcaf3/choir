//! The operator's console: who is here, and who is asking (D72).
//!
//! Everything on this page was a terminal command before it existed.
//! Minting an invite was `choirctl invite <name> <repo> <level>`, which
//! is three positional arguments an operator has to remember the order
//! of and a `.git` suffix they have to remember to leave off; answering
//! somebody who wanted access was not a command at all, because there
//! was nothing to answer. The node already knew how to do both. What was
//! missing was a page.
//!
//! **Plain forms, no script.** Every other write surface on this node is
//! passkey-signed because it puts an operation in the op log, where a
//! forged one cannot be taken back. Nothing here reaches the log: the
//! store is a node-owned file and revocation is deletion (see
//! [`crate::accounts`]). So these are `<form method="post">` and a `303`,
//! which works in every browser, needs no ceremony, and cannot ship a
//! button the CSP will not run.
//!
//! What that costs is a cross-site request forgery surface, and it is
//! paid for rather than ignored: the session cookie is `SameSite=Lax`,
//! so no cross-site `POST` carries it, and [`crate::same_origin`] refuses
//! a `POST` whose `Origin` names anywhere else -- which also closes the
//! same hole on the JSON endpoints an operator's cached Basic credential
//! could have been steered into.

use crate::accounts::{Accounts, RequestSummary};

/// A rendered page: status and HTML, matching [`crate::browse::Rendered`]
/// without borrowing its repository-shaped fields, the same way
/// [`crate::account_page::Page`] does.
pub(crate) struct Page {
    /// HTTP status.
    pub status: u16,
    /// The document.
    pub html: String,
}

/// Escapes text for HTML, the same five characters every other page on
/// this surface escapes.
fn esc(text: &str) -> String {
    crate::ui::esc(text)
}

/// Opens the document, the chrome and the header.
fn shell(h: &mut String, chrome: crate::browse::Chrome<'_>) {
    h.push_str("<!doctype html><html lang=\"en\"");
    crate::browse::theme_attribute(h, chrome.theme);
    h.push_str("><head><meta charset=\"utf-8\">");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    h.push_str("<title>choir: people</title>");
    h.push_str(crate::ui::STYLE);
    // This page draws the bar below, so it takes the key with it (D81).
    // The bar and the palette ship together everywhere: a page that
    // renders the search box and not the shortcut is the one page where
    // a learned key does nothing.
    h.push_str(crate::ui::PALETTE_SCRIPT);
    h.push_str("</head><body>");
    h.push_str("<a class=\"skip\" href=\"#main\">Skip to content</a>");
    crate::browse::chrome(h, crate::browse::Bar::index(chrome));
    h.push_str("<header class=\"top\"><h1>People</h1>");
    h.push_str("<div class=\"sub\"><span class=\"pill\"><a href=\"/r/\">repositories</a></span>");
    h.push_str("<span class=\"pill\"><a href=\"/account\">your account</a></span></div></header>");
    h.push_str("<main id=\"main\">");
}

/// Closes the document.
fn close(mut h: String) -> String {
    h.push_str("</main><footer>Every write here is a signed operation.</footer>");
    h.push_str("</body></html>");
    h
}

/// A `<select>` over the repositories on this node, plus the levels.
///
/// The repository list is rendered rather than typed because the two
/// mistakes an operator makes here are both spelling: `owner/name` with
/// the wrong owner mints an invite to nothing, and leaving the `.git`
/// off used to mint an invite granting nothing at all. A list cannot be
/// misspelled.
///
/// `read` is selected because both callers hand authority to somebody
/// who does not have it yet, and the level nobody thought about should
/// be the smaller one. Defaulting to `write` made the safe choice the
/// one an operator had to remember, which is backwards for the common
/// case: somebody invited to look at the work.
fn grant_controls(h: &mut String, repos: &[String], id_prefix: &str) {
    h.push_str("<label for=\"");
    h.push_str(id_prefix);
    h.push_str("-repo\">Repository</label> ");
    h.push_str("<select id=\"");
    h.push_str(id_prefix);
    h.push_str("-repo\" name=\"repo\" required>");
    for repo in repos {
        h.push_str("<option value=\"");
        h.push_str(&esc(repo));
        h.push_str("\">");
        h.push_str(&esc(repo));
        h.push_str("</option>");
    }
    h.push_str("</select> ");
    h.push_str("<label for=\"");
    h.push_str(id_prefix);
    h.push_str("-level\">may</label> ");
    h.push_str("<select id=\"");
    h.push_str(id_prefix);
    h.push_str("-level\" name=\"level\">");
    h.push_str("<option value=\"read\" selected>read</option>");
    h.push_str("<option value=\"write\">write</option>");
    h.push_str("</select> ");
}

/// The console.
///
/// `said` is the one-line outcome of the `POST` that redirected here, or
/// empty. It is chosen from a fixed set by the handler rather than echoed
/// from the query string: a page that prints back what the address bar
/// said is a page that can be made to say anything by a link.
pub(crate) fn render(
    store: &Accounts,
    repos: &[String],
    contact: Option<&str>,
    said: &str,
    chrome: crate::browse::Chrome<'_>,
) -> Page {
    let mut h = String::with_capacity(8 * 1024);
    shell(&mut h, chrome);
    if !said.is_empty() {
        h.push_str("<p class=\"lede\">");
        h.push_str(&esc(said));
        h.push_str("</p>");
    }

    let waiting = store.pending_requests();
    h.push_str("<section><h2>Waiting to be let in</h2>");
    if waiting.is_empty() {
        h.push_str("<p>Nobody is asking. Requests arrive from the front page.</p>");
    } else if repos.is_empty() {
        // A grant needs a repository to name, and a node with none can
        // only mint a credential that reaches nothing. Saying so beats
        // rendering an empty `<select>` whose button answers 400.
        h.push_str(
            "<p>There are no repositories on this node yet, so there is nothing to \
                    grant. Create one and these become answerable.</p>",
        );
    }
    if !waiting.is_empty() {
        // A table, not a stack of cards. The console is a list of
        // decisions and the operator is scanning it — who asked, what
        // they said, and the two answers — so it is the one shape on
        // this surface built for scanning. As cards, four requests were
        // four bordered boxes each with its own heading, and the thing
        // being compared across them was two lines of text.
        h.push_str("<table><thead><tr><th>who</th><th>what they said</th>");
        h.push_str("<th>answer</th></tr></thead><tbody>");
        for request in &waiting {
            request_row(&mut h, request, repos);
        }
        h.push_str("</tbody></table>");
    }
    h.push_str("</section>");

    h.push_str("<section><h2>Invite somebody directly</h2>");
    h.push_str(
        "<p>Mints a link to send. Use this when you already know who they are; the \
         queue above is for people who found the node on their own.</p>",
    );
    if repos.is_empty() {
        h.push_str("<p>No repositories on this node yet.</p>");
    } else {
        h.push_str("<form method=\"post\" action=\"/people\">");
        h.push_str("<input type=\"hidden\" name=\"action\" value=\"invite\">");
        h.push_str(
            "<p><label for=\"invite-name\">What to call them</label><br>\
             <input id=\"invite-name\" name=\"display_name\" maxlength=\"64\" required></p><p>",
        );
        grant_controls(&mut h, repos, "invite");
        h.push_str("</p><p><button type=\"submit\">Mint a link</button></p>");
        h.push_str("</form>");
    }
    h.push_str("</section>");

    roster(&mut h, store);
    contact_form(&mut h, contact);
    Page {
        status: 200,
        html: close(h),
    }
}

/// What the front page tells a stranger who has no invite and no account.
///
/// Here rather than in a file the operator edits over ssh, which is what
/// it was: a value that takes a shell and a restart to change is a value
/// nobody changes. It is still a file -- `<root>/.choir/contact`, one
/// line -- because a personal identifier belongs in the operator's own
/// untracked state and never compiled into a published binary.
fn contact_form(h: &mut String, contact: Option<&str>) {
    h.push_str("<section><h2>How strangers reach you</h2>");
    h.push_str(
        "<p>Offered on the front page to anybody who arrives with no invite, beside the \
         form that puts them in the queue above. An address, a link, or a handle; leave \
         it empty and the front page offers nothing.</p>",
    );
    h.push_str("<form method=\"post\" action=\"/people\"><p>");
    h.push_str("<input type=\"hidden\" name=\"action\" value=\"contact\">");
    h.push_str("<label for=\"contact\">Contact</label> ");
    h.push_str("<input id=\"contact\" name=\"contact\" maxlength=\"200\" value=\"");
    h.push_str(&esc(contact.unwrap_or("")));
    h.push_str("\"> ");
    h.push_str("<button type=\"submit\">Save</button>");
    h.push_str("</p></form></section>");
}

/// One pending request as a row, with the two answers to it.
///
/// Two separate forms rather than two buttons in one, so that declining
/// cannot pick up a half-filled repository selection and so that neither
/// button can be reached by pressing return in the other's field. Both
/// forms live in the last cell, which is what makes this a table of
/// decisions rather than a table with a stray control beside it.
fn request_row(h: &mut String, request: &RequestSummary, repos: &[String]) {
    h.push_str("<tr><td>");
    h.push_str(&esc(&request.display_name));
    h.push_str("</td><td>");
    if request.about.is_empty() {
        h.push_str("<span class=\"muted\">They wrote nothing.</span>");
    } else {
        h.push_str(&esc(&request.about));
    }
    h.push_str("</td><td>");
    if !repos.is_empty() {
        h.push_str("<form method=\"post\" action=\"/people\"><p>");
        h.push_str("<input type=\"hidden\" name=\"action\" value=\"grant\">");
        h.push_str("<input type=\"hidden\" name=\"request_id\" value=\"");
        h.push_str(&esc(&request.id));
        h.push_str("\">");
        grant_controls(h, repos, &esc(&request.id));
        h.push_str("<button type=\"submit\">Let them in</button></p></form>");
    }
    h.push_str("<form method=\"post\" action=\"/people\"><p>");
    h.push_str("<input type=\"hidden\" name=\"action\" value=\"decline\">");
    h.push_str("<input type=\"hidden\" name=\"request_id\" value=\"");
    h.push_str(&esc(&request.id));
    h.push_str("\">");
    h.push_str("<button type=\"submit\">Decline</button>");
    h.push_str(
        " <span class=\"muted\">their link stops working, and says only that it is not \
         valid</span>",
    );
    h.push_str("</p></form>");
    h.push_str("</td></tr>");
}

/// Who has an account, and what each of them may reach.
fn roster(h: &mut String, store: &Accounts) {
    let listing = store.list_json();
    h.push_str("<section><h2>Accounts</h2>");
    let accounts = listing["accounts"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if accounts.is_empty() {
        h.push_str("<p>Nobody has redeemed an invite yet.</p>");
    } else {
        h.push_str("<table><thead><tr><th>who</th><th>handle</th><th>may reach</th>");
        h.push_str("<th>passkeys</th></tr></thead><tbody>");
        for account in accounts {
            let user = account["user"].as_str().unwrap_or("");
            h.push_str("<tr><td>");
            h.push_str(&esc(account["display_name"].as_str().unwrap_or(user)));
            h.push_str("</td><td class=\"mono\">");
            h.push_str(&esc(user));
            h.push_str("</td><td class=\"mono\">");
            h.push_str(&esc(&grants_in_words(&account["grants"])));
            h.push_str("</td><td class=\"mono\">");
            let keys = account["passkeys"].as_array().map_or(0, Vec::len);
            h.push_str(&keys.to_string());
            h.push_str("</td></tr>");
        }
        h.push_str("</tbody></table>");
    }

    let invites = listing["invites"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if !invites.is_empty() {
        h.push_str("<h2>Invited, not yet arrived</h2>");
        h.push_str("<table><thead><tr><th>who</th><th>may reach</th></tr></thead><tbody>");
        for invite in invites {
            h.push_str("<tr><td>");
            h.push_str(&esc(invite["display_name"]
                .as_str()
                .unwrap_or_else(|| invite["user"].as_str().unwrap_or(""))));
            h.push_str("</td><td class=\"mono\">");
            h.push_str(&esc(&grants_in_words(&invite["grants"])));
            h.push_str("</td></tr>");
        }
        h.push_str("</tbody></table>");
    }
    h.push_str("</section>");
}

/// A grant list as one readable cell.
fn grants_in_words(grants: &serde_json::Value) -> String {
    grants
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(serde_json::Value::as_str)
                .map(|grant| grant.replace(".git", ""))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

/// The page that hands over a freshly minted invite link.
///
/// Rendered directly by the `POST` rather than redirected to, unlike
/// every other action here: the link is the whole artefact and this node
/// keeps only a hash of its secret half, so a redirect would drop the one
/// copy that exists.
pub(crate) fn minted(link: &str, who: &str, chrome: crate::browse::Chrome<'_>) -> Page {
    let mut h = String::with_capacity(4 * 1024);
    shell(&mut h, chrome);
    h.push_str("<section><h2>A link for ");
    h.push_str(&esc(who));
    h.push_str("</h2>");
    h.push_str("<pre class=\"cmd\">");
    h.push_str(&esc(link));
    h.push_str("</pre>");
    crate::ui::next_action(
        &mut h,
        "Send it however you already talk to them. It is shown once: this node kept only a \
         hash of it, so nobody -- you included -- can show it again. If it is lost, mint \
         another.",
    );
    h.push_str("<p><span class=\"pill\"><a href=\"/people\">back to people</a></span></p>");
    h.push_str("</section>");
    Page {
        status: 200,
        html: close(h),
    }
}
