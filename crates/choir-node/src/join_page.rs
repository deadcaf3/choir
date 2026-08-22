//! The front door: the public landing page and the invite link (D57).
//!
//! Until this existed, a stranger pointed at a choir node got `401` and a
//! browser password box, and an invite was redeemed by running `curl` by
//! hand against `/api/accounts/redeem`. The machinery was complete and
//! had no door: [`crate::accounts::Accounts::invite`] already mints a
//! single-use, expiring credential carrying a username and a set of
//! grants the issuer chose. What was missing was a page.
//!
//! These are the only routes on the node that answer before a credential
//! is checked, apart from D39's script constant, so the reasoning behind
//! each is written down here rather than inferred.
//!
//! # The invite link is a bearer credential, deliberately
//!
//! `GET /join?i=<id>&k=<secret>` is one clickable link because that is
//! what makes it usable: it gets pasted into a chat window and the person
//! on the other end clicks it. Anyone holding the link can redeem it.
//!
//! That is a real cost and it is bounded rather than denied. The username
//! and the grants are frozen by the issuer at minting, so a stolen link
//! cannot choose who it becomes or what it may do; it is single-use, it
//! expires (24 hours by default), and revoking it removes the account and
//! the invite together. The alternative that removes the risk entirely —
//! binding the invite to an identity the node already knows, the way a
//! forge binds one to an email address — needs a delivery channel this
//! node does not have. Naming the compensating control is honest; adding
//! a mail server to avoid saying so would not be.
//!
//! # `GET` never consumes
//!
//! The link will be fetched by machines that were not invited: chat
//! clients unfurl a preview, mail scanners follow links, browsers
//! prefetch. This is the single most common way a real invite system
//! breaks, and it breaks silently — the human clicks and is told their
//! invite was already used, by their own chat client.
//!
//! So `GET` renders and nothing else. Redemption is the `POST` a person
//! performs by pressing a button. A preview bot may fetch this page a
//! thousand times and the invite is still there.
//!
//! # Every refusal is the same refusal
//!
//! An unknown id, a wrong secret, an expired invite and a spent one all
//! render one page, byte for byte. Distinguishing them would answer
//! "does this invite id exist?" for anyone who asks, which is a directory
//! of the accounts pending on this node. The one page is rendered by one
//! function taking no arguments, so the property holds by construction
//! rather than by four call sites agreeing on a sentence.
//!
//! # The secret rides in the query string
//!
//! Not in the path. [`crate::limits::Access::start`] truncates a URL at
//! `?` before the record struct is built, so a query parameter cannot
//! reach `--request-log` by any later edit, while a path segment is
//! written to disk verbatim on every hit. Same link, one character
//! different, and the node stops writing invites to a file.
//!
//! # No chrome, and no node state
//!
//! These pages carry a brand header rather than the navigation bar every
//! other page carries. The bar's controls — the search box, the
//! repository list — all answer `401` to a reader who has no credential,
//! and a row of dead ends reads worse than no row at all.
//!
//! The landing page is static text. No sequence number, no repository
//! count, no build stamp, no names: a stranger learns that this is a
//! choir node and how to get in, which is everything the DNS record
//! already told them.

use crate::accounts::Accounts;
use crate::browse::Chrome;
use crate::ui::esc;

/// A rendered page: status and HTML, matching [`crate::browse::Rendered`]
/// without borrowing its repository-shaped fields, the same way
/// [`crate::account_page::Page`] does.
pub(crate) struct Page {
    /// HTTP status.
    pub status: u16,
    /// The document.
    pub html: String,
}

/// Opens the document and the brand header.
///
/// Shared by every state below so that the four of them cannot drift into
/// looking like four different products, and so the identical-refusal
/// property does not depend on two functions agreeing.
fn shell(title: &str, theme: Option<&str>) -> String {
    let mut h = String::with_capacity(4 * 1024);
    h.push_str("<!doctype html><html lang=\"en\"");
    crate::browse::theme_attribute(&mut h, theme);
    h.push_str("><head><meta charset=\"utf-8\">");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    h.push_str("<title>");
    h.push_str(&esc(title));
    h.push_str("</title>");
    h.push_str(crate::ui::STYLE);
    h
}

/// Closes the head, opens the body, and writes the brand header.
fn body(h: &mut String, headline: &str, sub: &str) {
    h.push_str("</head><body>");
    h.push_str("<a class=\"skip\" href=\"#main\">Skip to content</a>");
    h.push_str("<header class=\"top\"><h1>");
    h.push_str(&esc(headline));
    h.push_str("</h1>");
    if !sub.is_empty() {
        h.push_str("<div class=\"sub\"><span class=\"pill\">");
        h.push_str(&esc(sub));
        h.push_str("</span></div>");
    }
    h.push_str("</header><main id=\"main\">");
}

/// Closing tags and the footer every page on this surface carries.
fn close(mut h: String) -> String {
    h.push_str("</main><footer>Every write here is a signed operation.</footer>");
    h.push_str("</body></html>");
    h
}

/// The one refusal, for every reason an invite might not work.
///
/// Takes no argument that could vary the output. That is the whole
/// design: an unknown id, a wrong secret, an expired invite and one
/// already redeemed produce identical bytes, so possession of the page
/// tells a stranger nothing about which invite ids this node holds.
///
/// `status` is the caller's, not this function's, because a `GET` and a
/// failed `POST` differ in what the *protocol* should say while the
/// reader sees the same page. A `GET` answers `200`: a preview bot
/// retrying a `4xx` is noise, and there is no error here to report — the
/// page is the answer.
pub(crate) fn not_valid(status: u16, theme: Option<&str>) -> Page {
    let mut h = shell("choir: invite", theme);
    body(&mut h, "This invite is not valid", "");
    h.push_str("<section>");
    h.push_str(
        "<p class=\"lede\">It may have expired, or it may already have been used. \
         Invites work once and are not reusable.</p>",
    );
    crate::ui::next_action(
        &mut h,
        "Ask the person who invited you for a fresh link. Nothing you did caused this, \
         and nothing about your link was recorded here.",
    );
    h.push_str("</section>");
    Page {
        status,
        html: close(h),
    }
}

/// The page for a node that does not run account self-service.
///
/// Distinct from [`not_valid`] on purpose: this one is not about the
/// reader's link, and telling somebody their invite is bad when the node
/// simply has the feature switched off sends them back to an issuer who
/// cannot help. It also reveals nothing, because a node either offers
/// invites or does not and that is not a secret.
fn no_self_service(theme: Option<&str>) -> Page {
    let mut h = shell("choir: invite", theme);
    body(&mut h, "This node does not accept invites", "");
    h.push_str("<section>");
    h.push_str(
        "<p class=\"lede\">Accounts here are issued by the operator directly rather than \
         through a link.</p>",
    );
    crate::ui::next_action(
        &mut h,
        "Ask whoever runs this node for a credential. There is nothing for you to do on \
         this page.",
    );
    h.push_str("</section>");
    Page {
        status: 200,
        html: close(h),
    }
}

/// Roughly how long until `expires_at`, in words.
///
/// Rounded down and coarse on purpose. The exact second is not a fact the
/// reader can act on, and "in about 23 hours" is read correctly at a
/// glance where a unix timestamp is not read at all.
fn expires_in_words(expires_at: u64, now: u64) -> String {
    let left = expires_at.saturating_sub(now);
    if left == 0 {
        return "expired".to_string();
    }
    let hours = left / 3600;
    if hours >= 48 {
        return format!("in about {} days", hours / 24);
    }
    if hours >= 2 {
        return format!("in about {hours} hours");
    }
    let minutes = (left / 60).max(1);
    format!("in about {minutes} minutes")
}

/// One grant as a sentence rather than as a table row.
///
/// `agents/demo.git read` is precise and means nothing to somebody who
/// has never used this node. "read agents/demo" is the same fact in the
/// reader's language.
///
/// A deadline (D66) is said out loud here rather than left in the fourth
/// column, because the one person who most needs to know that an access
/// ends is the one being handed it, and this page is the only place they
/// are ever shown what they were given.
fn grant_in_words(grant: &str, now: u64) -> String {
    let mut columns = grant.split_whitespace();
    let (Some(target), Some(level)) = (columns.next(), columns.next()) else {
        return grant.to_string();
    };
    let until = columns
        .next()
        .and_then(|column| column.strip_prefix("until="))
        .and_then(|value| value.parse::<u64>().ok())
        .map(|deadline| format!(", until {}", expires_in_words(deadline, now)))
        .unwrap_or_default();
    let target = target.strip_suffix(".git").unwrap_or(target);
    let what = if target == "*" {
        "every repository on this node".to_string()
    } else {
        target.to_string()
    };
    let can = match level {
        "write" => format!("push to {what}"),
        "propose" => format!("propose changes to {what}"),
        "read" => format!("read {what}"),
        other => format!("{other} {what}"),
    };
    format!("{can}{until}")
}

/// The invite, shown to somebody who holds its secret.
///
/// `id` and `secret` are echoed into the form as hidden fields so the
/// `POST` carries them in a body rather than in a URL. The secret is
/// already in this reader's address bar; putting it in the body of the
/// request that spends it keeps it out of the next one.
fn offer(
    summary: &crate::accounts::InviteSummary,
    id: &str,
    secret: &str,
    ssh: bool,
    theme: Option<&str>,
    origin: Option<&str>,
    now: u64,
) -> Page {
    let mut h = shell("choir: you're invited", theme);
    // The card a chat client shows. Deliberately generic: it names no
    // repository, no issuer and no username, because the preview is
    // rendered by a third party's servers and pasted into a channel that
    // may have more people in it than the invite was meant for. The
    // specifics are in the body, which only the holder of the link sees.
    h.insert_str(
        h.find("<title>").unwrap_or(h.len()),
        "<meta property=\"og:type\" content=\"website\">\
         <meta property=\"og:title\" content=\"You're invited to a choir node\">\
         <meta property=\"og:description\" content=\"An invite to a repository on a choir \
         node, where agents and people work together under one ordered log. Open the link \
         to see what it gives you.\">\
         <meta name=\"robots\" content=\"noindex, nofollow\">",
    );
    // Absolute, because a preview is fetched by a server that has no
    // page to resolve a relative path against. Omitted rather than
    // guessed when the request carried no `Host`, on the same reasoning
    // as `browse::node_url`.
    if let Some(origin) = origin {
        h.insert_str(
            h.find("<title>").unwrap_or(h.len()),
            &format!(
                "<meta property=\"og:image\" content=\"{}{}\">",
                esc(origin),
                crate::ui::CARD_PATH
            ),
        );
    }
    body(&mut h, "You're invited", "expires once used");
    h.push_str("<section><h2>What this gives you</h2>");
    h.push_str("<table class=\"kv\"><tbody>");
    h.push_str("<tr><td>you will be</td><td class=\"mono\">");
    h.push_str(&esc(&summary.user));
    if let Some(name) = summary.display_name.as_deref() {
        h.push_str("</td></tr><tr><td>shown as</td><td>");
        h.push_str(&esc(name));
    }
    h.push_str("</td></tr><tr><td>invited by</td><td class=\"mono\">");
    h.push_str(&esc(&summary.issued_by));
    h.push_str("</td></tr><tr><td>expires</td><td>");
    h.push_str(&esc(&expires_in_words(summary.expires_at, now)));
    h.push_str("</td></tr>");
    // The label sits on the first grant only. Repeating "you may" down a
    // column reads as a stutter rather than as a list, and the rows are
    // already visibly one group.
    for (n, grant) in summary.grants.iter().enumerate() {
        h.push_str("<tr><td>");
        h.push_str(if n == 0 { "you may" } else { "" });
        h.push_str("</td><td>");
        h.push_str(&esc(&grant_in_words(grant, now)));
        h.push_str("</td></tr>");
    }
    h.push_str("</tbody></table>");
    h.push_str(
        "<p class=\"note\">Accepting creates that account and shows you a password once. \
         The link works a single time.</p>",
    );
    h.push_str("<form method=\"post\" action=\"/join\">");
    h.push_str("<input type=\"hidden\" name=\"i\" value=\"");
    h.push_str(&esc(id));
    h.push_str("\"><input type=\"hidden\" name=\"k\" value=\"");
    h.push_str(&esc(secret));
    h.push_str("\">");
    if ssh {
        // Offered rather than required: a person who has no key, or does
        // not know what one is, must still be able to finish. The token
        // on the next page is a complete credential on its own.
        h.push_str(
            "<p><label for=\"sshkey\">Your SSH public key, if you want to push over \
                    ssh. Optional, and you can add one later.</label></p>",
        );
        h.push_str(
            "<textarea id=\"sshkey\" name=\"ssh_key\" rows=\"3\" \
             placeholder=\"ssh-ed25519 AAAAC3N... you@your-laptop\"></textarea>",
        );
    }
    h.push_str("<p><button class=\"go\" type=\"submit\">Accept and create my account</button></p>");
    h.push_str("</form></section>");
    Page {
        status: 200,
        html: close(h),
    }
}

/// The page that hands over the credential, after a successful `POST`.
///
/// The token is shown here and nowhere else — the node keeps only its
/// hash — so this page's job is to make copying it the obvious next act
/// and to say plainly that there is no second chance.
fn welcome(
    user: &str,
    token: &str,
    grants: &[String],
    origin: Option<&str>,
    theme: Option<&str>,
) -> Page {
    let node = origin.unwrap_or("<this node>");
    let mut h = shell("choir: you're in", theme);
    body(&mut h, "You're in", user);
    h.push_str("<section><h2>Your password</h2>");
    h.push_str(
        "<p class=\"lede\">Copy this now. It is shown once and this node keeps only a hash \
         of it, so nobody — including the operator — can show it to you again.</p>",
    );
    h.push_str("<pre class=\"cmd\">");
    h.push_str(&esc(token));
    h.push_str("</pre>");
    crate::ui::next_action(
        &mut h,
        "Put it in your password manager before you close this tab. If you lose it, ask for \
         a new invite; there is no reset.",
    );
    h.push_str("</section>");

    h.push_str("<section><h2>Start working</h2><ol class=\"steps\">");
    let mut step = 1;
    for grant in grants {
        let target = grant.split_whitespace().next().unwrap_or("");
        if target == "*" {
            continue;
        }
        let repo = target.strip_suffix(".git").unwrap_or(target);
        h.push_str("<li><h3><span class=\"step\">");
        h.push_str(&step.to_string());
        h.push_str("</span>Clone <code>");
        h.push_str(&esc(repo));
        h.push_str("</code></h3><pre class=\"cmd\">git clone ");
        h.push_str(&esc(node));
        h.push('/');
        h.push_str(&esc(repo));
        h.push_str(".git</pre>");
        h.push_str("<p class=\"mono\">username <b>");
        h.push_str(&esc(user));
        h.push_str("</b>, password the one above</p></li>");
        step += 1;
    }
    h.push_str("<li><h3><span class=\"step\">");
    h.push_str(&step.to_string());
    h.push_str("</span>Teach your agent</h3>");
    h.push_str("<p>This node describes itself in a form a coding agent can read.</p>");
    h.push_str("<pre class=\"cmd\">curl -u ");
    h.push_str(&esc(user));
    h.push(' ');
    h.push_str(&esc(node));
    h.push_str("/sync.md &gt;&gt; CLAUDE.md</pre></li>");
    h.push_str("</ol></section>");

    h.push_str("<section><h2>One more thing</h2>");
    h.push_str(
        "<p>Add a passkey and you can approve reviews from this browser with your \
         fingerprint, face or hardware key, instead of pasting that password.</p>",
    );
    h.push_str("<p><span class=\"pill\"><a href=\"/account\">add a passkey</a></span> ");
    h.push_str("<span class=\"pill\"><a href=\"/r/\">browse repositories</a></span></p>");
    h.push_str("</section>");
    Page {
        status: 200,
        html: close(h),
    }
}

/// `GET /join`.
///
/// Renders and never changes anything. See the module docs: the fetch
/// this page most often serves is a preview bot, not a person.
pub(crate) fn get(
    store: Option<&Accounts>,
    id: Option<&str>,
    secret: Option<&str>,
    ssh: bool,
    chrome: Chrome<'_>,
    origin: Option<&str>,
    now: u64,
) -> Page {
    let Some(store) = store else {
        return no_self_service(chrome.theme);
    };
    let (Some(id), Some(secret)) = (id, secret) else {
        return not_valid(200, chrome.theme);
    };
    // Possession is proved here, before anything is read out of the
    // store, and `authenticate` is the same constant-time check every
    // other credential on this node goes through. Only an *invite*
    // proceeds: an account's own token pasted into this URL is not a
    // half-successful login, it is a request that makes no sense.
    match store.authenticate(id, secret) {
        Some(crate::accounts::Principal::Invite(id)) => match store.invite_summary(&id) {
            Some(summary) => offer(&summary, &id, secret, ssh, chrome.theme, origin, now),
            // Defence in depth, and measured to be exactly that, the same
            // way `Accounts::redeem`'s retired-name check was. No request
            // can reach this arm: `authenticate` already refuses an
            // expired invite, so an expiry landing between these two
            // calls is the only way here, and that window is shorter than
            // the clock's resolution. Rewriting this arm to name the
            // expired case leaves the whole suite green — proved by
            // mutation on 2026-08-21, not assumed — so no test can be
            // said to hold it.
            //
            // It is kept because it is the line that would notice if
            // `authenticate` ever stopped checking expiry, and because
            // the alternative to a refusal here is showing a live invite
            // page for a dead invite.
            None => not_valid(200, chrome.theme),
        },
        _ => not_valid(200, chrome.theme),
    }
}

/// `POST /join`: the act that spends the invite.
///
/// The id handed to [`Accounts::redeem`] is the one
/// [`Accounts::authenticate`] returned, never the one the form sent, so
/// a body naming one invite and authenticating as another redeems the
/// invite it proved it holds.
pub(crate) fn post(
    store: Option<&Accounts>,
    body: &str,
    origin: Option<&str>,
    chrome: Chrome<'_>,
) -> Page {
    let Some(store) = store else {
        return no_self_service(chrome.theme);
    };
    let (Some(id), Some(secret)) = (form_value(body, "i"), form_value(body, "k")) else {
        return not_valid(403, chrome.theme);
    };
    let Some(crate::accounts::Principal::Invite(id)) = store.authenticate(&id, &secret) else {
        return not_valid(403, chrome.theme);
    };
    let mut request = serde_json::Map::new();
    if let Some(key) = form_value(body, "ssh_key") {
        if !key.trim().is_empty() {
            request.insert("ssh_key".to_string(), serde_json::Value::String(key));
        }
    }
    let (status, answer) = store.redeem(&id, &serde_json::Value::Object(request));
    let parsed: serde_json::Value = serde_json::from_str(&answer).unwrap_or_default();
    if status != 200 {
        // A refusal that is about what the reader sent — an ssh key that
        // is not a key — must say so, or they will retry the same paste
        // forever. `redeem` validates the key *before* it looks the
        // invite up, so the invite is still redeemable and saying "not
        // valid" here would be a lie that costs them the invite.
        return bad_submission(
            parsed["error"]
                .as_str()
                .unwrap_or("that could not be accepted"),
            chrome.theme,
        );
    }
    let grants: Vec<String> = parsed["grants"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.as_str().map(ToString::to_string))
                .collect()
        })
        .unwrap_or_default();
    welcome(
        parsed["user"].as_str().unwrap_or(&id),
        parsed["token"].as_str().unwrap_or(""),
        &grants,
        origin,
        chrome.theme,
    )
}

/// A submission the node understood and refused, with the invite intact.
fn bad_submission(why: &str, theme: Option<&str>) -> Page {
    let mut h = shell("choir: invite", theme);
    body(&mut h, "That did not go through", "");
    h.push_str("<section><p class=\"lede\">");
    h.push_str(&esc(why));
    h.push_str("</p>");
    crate::ui::next_action(
        &mut h,
        "Your invite has not been used. Go back and try again, and leave the ssh key box \
         empty if you are not sure what belongs in it.",
    );
    h.push_str("</section>");
    Page {
        status: 400,
        html: close(h),
    }
}

/// The public landing page: what a stranger sees at the bare address.
///
/// Static by construction. It takes no store, no platform and no view, so
/// there is nothing here that could later grow a repository name or a
/// sequence number without someone adding a parameter on purpose.
pub(crate) fn landing(theme: Option<&str>) -> Page {
    let mut h = shell("choir", theme);
    h.insert_str(
        h.find("<title>").unwrap_or(h.len()),
        "<meta property=\"og:type\" content=\"website\">\
         <meta property=\"og:title\" content=\"A choir node\">\
         <meta property=\"og:description\" content=\"Agents and people working on one \
         repository at a time, ordered by a single writer. Every write is a signed \
         operation in one log.\">",
    );
    body(&mut h, "choir", "");
    h.push_str("<section>");
    h.push_str(
        "<p class=\"lede\">A node where agents and people work on the same repositories at \
         the same time. Every write is a signed operation in one ordered log, and a merge \
         conflict is a state the history can hold rather than an error somebody has to \
         clear.</p>",
    );
    // Three claims rather than a paragraph. This is the one page read by
    // somebody who has not decided to care yet, and the steps list is the
    // surface's existing way of saying "here are the parts of this".
    h.push_str("<ol class=\"steps\">");
    h.push_str(
        "<li><h3>One order</h3><p>A single writer sequences every change, so two agents \
         pushing at once get one history rather than a race.</p></li>",
    );
    h.push_str(
        "<li><h3>Signed, not trusted</h3><p>Each operation carries its author's signature, \
         and the log is hash-chained. What happened is checkable rather than asserted.</p></li>",
    );
    h.push_str(
        "<li><h3>Conflicts are values</h3><p>An unresolved merge is a committed state that \
         later work can build on, not an error that blocks the queue.</p></li>",
    );
    h.push_str("</ol>");
    h.push_str(
        "<p>Reading anything here needs an account, so there is nothing to look at yet.</p>",
    );
    crate::ui::next_action(
        &mut h,
        "Have an invite link? Open it and it will set you up. Otherwise \
         <a href=\"/r/\">sign in</a> with the credentials you were given.",
    );
    h.push_str("</section>");
    Page {
        status: 200,
        html: close(h),
    }
}

/// One value out of a URL's query string.
///
/// [`crate::browse::param`] reads the same grammar and is the right
/// reader for a path-shaped parameter; this one exists so the query and
/// the form body of the same submission go through the same code. A
/// secret that decodes one way in the link and another way in the form
/// would be a link that renders and then refuses itself.
pub(crate) fn param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    form_value(query.split('#').next().unwrap_or(query), key)
}

/// Unix seconds now, for expiry arithmetic.
///
/// Saturating at the epoch rather than panicking: a clock set before 1970
/// is a broken machine, and the repair for it is not a crashed daemon.
pub(crate) fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// One value out of an `application/x-www-form-urlencoded` body.
///
/// [`crate::browse::param`] reads the same grammar and cannot be used
/// here: it refuses a value whose decoding contains `/`, which is right
/// for a path segment and wrong for an ssh public key, where `/` and `+`
/// are ordinary base64. This is the narrower job — a body rather than a
/// URL, and no path to protect — so it allows `/` and refuses what a page
/// actually cannot survive: control characters and invalid UTF-8.
fn form_value(body: &str, key: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        if name != key {
            return None;
        }
        let decoded = percent_decode(&value.replace('+', " "))?;
        (!decoded.trim().is_empty()).then_some(decoded)
    })
}

/// Percent-decoding for form values.
///
/// Refuses control characters and anything that is not UTF-8, so a value
/// that reaches a page cannot carry a newline into a header or a lone
/// surrogate into the store.
fn percent_decode(raw: &str) -> Option<String> {
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
    let text = String::from_utf8(out).ok()?;
    text.chars()
        .all(|c| !c.is_control() || c == '\n')
        .then_some(text)
}

#[cfg(test)]
mod tests {
    /// The document with its inlined stylesheet removed.
    ///
    /// Every page on this surface carries the whole of `ui.css` inline,
    /// and that sheet contains English words in its comments. Asserting
    /// `!html.contains("seq")` against the raw document therefore fails
    /// on the word "consequence" in a comment about something else
    /// entirely — a test that is not reading the page it thinks it is.
    fn without_style(html: &str) -> String {
        let (before, rest) = html
            .split_once("<style>")
            .expect("a page carries the sheet");
        let (_, after) = rest.split_once("</style>").expect("the sheet closes");
        format!("{before}{after}")
    }

    /// Just the `<main>` region: the words a reader actually reads.
    ///
    /// The head is excluded because `initial-scale=1` is a digit that has
    /// nothing to do with this node, and a test that had to allow it
    /// would be allowing the class of thing it exists to catch.
    fn main_text(html: &str) -> String {
        let (_, rest) = html.split_once("<main").expect("a page has a main");
        let (inner, _) = rest.split_once("</main>").expect("main closes");
        // Tags out, because the checks below are about what a reader
        // reads. `<h3>` is a digit that says nothing about this node, and
        // a test that had to permit it would be permitting the shape of
        // the thing it exists to catch.
        let mut text = String::with_capacity(inner.len());
        let mut inside = false;
        for c in inner.chars() {
            match c {
                '<' => inside = true,
                '>' => inside = false,
                other if !inside => text.push(other),
                _ => {}
            }
        }
        text
    }

    /// The property the whole design rests on: four different reasons an
    /// invite might not work produce one page. Compared as bytes, because
    /// a difference of one word is still an oracle.
    #[test]
    fn every_reason_an_invite_fails_renders_the_same_page() {
        let a = super::not_valid(200, None).html;
        let b = super::not_valid(200, Some("dark")).html;
        let c = super::not_valid(403, None).html;
        assert_eq!(a, c, "the status changed the page a reader sees");
        assert_ne!(a, b, "the palette is meant to change the document element");
        // The page may say what *might* have happened — listing the two
        // possibilities is the reader's repair, and says nothing about
        // which one occurred. What it must not carry is a fact: an id, a
        // timestamp, a count. Checked as "no digits at all", because that
        // is mechanical, and anything the node knows about this link
        // arrives as one.
        let text = main_text(&without_style(&a));
        assert!(
            !text.chars().any(|c| c.is_ascii_digit()),
            "the refusal carries a number, which can only have come from this link: {text}"
        );
    }

    /// A grant is a table row in the ACL and a sentence on this page.
    #[test]
    fn grants_are_rendered_in_the_readers_language() {
        let now = 1_000_000;
        assert_eq!(
            super::grant_in_words("agents/demo.git write", now),
            "push to agents/demo"
        );
        assert_eq!(
            super::grant_in_words("agents/demo read", now),
            "read agents/demo"
        );
        assert_eq!(
            super::grant_in_words("* write", now),
            "push to every repository on this node"
        );
        // Anything unrecognised is shown as it stands rather than dropped:
        // a grant a reader cannot see is a permission they did not accept.
        assert_eq!(super::grant_in_words("weird", now), "weird");
    }

    /// A grant that ends says so here (D66), in the same words the
    /// invite's own expiry uses. Being handed an access without being
    /// told it lapses is the failure this rules out.
    #[test]
    fn a_grant_that_ends_says_when() {
        let now = 1_000_000;
        assert_eq!(
            super::grant_in_words("agents/demo.git write until=1864000", now),
            "push to agents/demo, until in about 10 days"
        );
        assert_eq!(
            super::grant_in_words("agents/demo.git write until=1003600", now),
            "push to agents/demo, until in about 60 minutes"
        );
        // Already past, and still shown: the row is the record of what
        // the issuer chose, not a guess at what is useful today.
        assert_eq!(
            super::grant_in_words("agents/demo.git write until=1", now),
            "push to agents/demo, until expired"
        );
        // A fourth column the ACL parser would refuse never reaches a
        // reader as a half-sentence.
        assert_eq!(
            super::grant_in_words("agents/demo.git write nonsense", now),
            "push to agents/demo"
        );
    }

    /// The ssh key is the reason this parser exists rather than
    /// [`crate::browse::param`]: real keys contain `/` and `+`.
    #[test]
    fn a_form_value_may_contain_the_characters_an_ssh_key_contains() {
        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI/x+y/z= someone@laptop";
        // Encoded the way a browser encodes a textarea, which is the only
        // way this value ever arrives: space becomes `+`, and a literal
        // `+` becomes `%2B`. Encoding it any other way would test a
        // request nothing sends. The order matters — escaping `+` after
        // substituting it for space would re-escape the separators.
        let body = format!(
            "i=invite-abc&k=deadbeef&ssh_key={}",
            key.replace('%', "%25")
                .replace('+', "%2B")
                .replace('=', "%3D")
                .replace(' ', "+")
        );
        assert_eq!(super::form_value(&body, "i").as_deref(), Some("invite-abc"));
        assert_eq!(super::form_value(&body, "ssh_key").as_deref(), Some(key));
        assert_eq!(super::form_value(&body, "absent"), None);
    }

    /// A control character in a form value would travel into a page and,
    /// worse, into the accounts file.
    #[test]
    fn a_form_value_carrying_a_control_character_is_refused() {
        assert_eq!(super::form_value("k=one%00two", "k"), None);
        assert_eq!(super::form_value("k=one%0Dtwo", "k"), None);
        // Truncated and non-hex escapes are refused rather than guessed.
        assert_eq!(super::form_value("k=%2", "k"), None);
        assert_eq!(super::form_value("k=%zz", "k"), None);
    }

    /// Expiry is for reading at a glance, so the wording changes with the
    /// magnitude rather than printing a timestamp.
    #[test]
    fn expiry_is_said_in_words_a_reader_can_act_on() {
        assert_eq!(super::expires_in_words(0, 100), "expired");
        assert_eq!(super::expires_in_words(100, 100), "expired");
        assert_eq!(super::expires_in_words(1_000, 100), "in about 15 minutes");
        assert_eq!(super::expires_in_words(86_400, 0), "in about 24 hours");
        assert_eq!(super::expires_in_words(30 * 86_400, 0), "in about 30 days");
    }

    /// The landing page is the one page a stranger can always reach, so
    /// what it does *not* say is the specification.
    #[test]
    fn the_landing_page_names_nothing_about_this_node() {
        let html = super::landing(None).html;
        assert!(html.contains("signed operation"), "{html}");
        assert!(!html.contains("<script"), "{html}");
        // Constant by construction: the only input is the palette, so
        // two renders with the same palette are the same bytes. If this
        // page ever grows a parameter, this is the test that argues
        // about it.
        assert_eq!(html, super::landing(None).html);
        // And it states no fact about this node. Every such fact — the
        // log position, a repository count, a version — arrives as a
        // number, so "no digits in the body" is the mechanical form of
        // "says nothing about what is hosted here".
        let text = main_text(&without_style(&html));
        assert!(
            !text.chars().any(|c| c.is_ascii_digit()),
            "the landing page carries a number, which can only describe this node: {text}"
        );
    }
}
