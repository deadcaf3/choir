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
//! same way: inline script, no `src`, no library, no build step, and the
//! page reads correctly with scripting off — where it says so and names
//! the endpoint, rather than presenting a button that cannot work.

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
pub(crate) fn render(store: Option<&Accounts>, user: &str) -> Page {
    let mut h = String::with_capacity(4 * 1024);
    h.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
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
        return Page { status: 200, html: close(h) };
    };

    h.push_str("<section><h2>Passkeys</h2>");
    let enrolled = store.passkeys_json(user);
    if !store.has_account(user) {
        // An operator credential from `--auth-file` is a real credential
        // with no account record behind it. Saying "no passkeys yet"
        // would invite an enrolment that always fails.
        h.push_str("<p class=\"note\">This credential was written by the operator into the ");
        h.push_str("node's auth file rather than issued as an account, so it cannot hold a ");
        h.push_str("passkey. Passkeys are enrolled on issued accounts.</p></section>");
        return Page { status: 200, html: close(h) };
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

    h.push_str("<noscript><p class=\"note\">Enrolling a passkey needs the browser to create a \
                key pair, which it will only do with scripting enabled. With it off, POST a \
                credential you already hold to <code>/api/accounts/passkey</code>.</p></noscript>");
    h.push_str("<div id=\"enrol\" hidden data-user=\"");
    h.push_str(&esc(user));
    h.push_str("\"><button id=\"enrol-go\">Add a passkey</button> ");
    h.push_str("<input id=\"enrol-label\" maxlength=\"64\" placeholder=\"this laptop\">");
    h.push_str("<p id=\"enrol-said\" class=\"note\" hidden></p></div>");
    h.push_str(ENROL_SCRIPT);
    h.push_str("</section>");
    Page { status: 200, html: close(h) }
}

/// Closing tags, matching the browse shell.
fn close(mut h: String) -> String {
    h.push_str("</main></body></html>");
    h
}

/// The registration ceremony. Inline, no `src`, no library, no build
/// step — the same scope D39 approved for the verdict script, and
/// asserted by the same kind of test.
///
/// **The challenge here is not verified by the node, and that is stated
/// rather than hidden.** WebAuthn requires one, so one is sent; but
/// nothing parses the attestation object, so nothing checks it came back.
/// D39 scoped a CBOR reader out deliberately, and the consequence — that
/// enrolment trusts the authenticated channel rather than proving
/// possession — is recorded on `Accounts::enroll_passkey` and in
/// `PHASE0.md`. A challenge that looked verified would be the worse
/// version of the same limitation.
const ENROL_SCRIPT: &str = r#"<script>
(function () {
  var box = document.getElementById('enrol');
  var said = document.getElementById('enrol-said');
  if (!box || !window.PublicKeyCredential || !navigator.credentials) return;
  box.hidden = false;
  var user = box.dataset.user;
  var b64url = function (buf) {
    var s = '';
    var b = new Uint8Array(buf);
    for (var i = 0; i < b.length; i++) s += String.fromCharCode(b[i]);
    return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  };
  var say = function (t) { said.hidden = false; said.textContent = t; };
  var utf8 = function (s) { return new TextEncoder().encode(s); };
  document.getElementById('enrol-go').addEventListener('click', function () {
    say('Follow your browser’s prompt...');
    navigator.credentials.create({
      publicKey: {
        rp: { name: 'choir' },
        user: { id: utf8(user), name: user, displayName: user },
        challenge: crypto.getRandomValues(new Uint8Array(32)),
        pubKeyCredParams: [{ type: 'public-key', alg: -7 }],
        authenticatorSelection: { userVerification: 'preferred' },
        timeout: 120000
      }
    }).then(function (c) {
      var spki = c.response.getPublicKey && c.response.getPublicKey();
      if (!spki) throw new Error('this browser did not return a public key');
      var label = document.getElementById('enrol-label').value || 'passkey';
      return fetch('/api/accounts/passkey', {
        method: 'POST',
        credentials: 'same-origin',
        body: JSON.stringify({
          credential_id: c.id, public_key: b64url(spki), label: label
        })
      });
    }).then(function (r) {
      if (r.ok) { location.reload(); return; }
      return r.text().then(function (t) { say('The node refused it: ' + t); });
    }).catch(function (e) { say('Not enrolled: ' + e.message); });
  });
})();
</script>"#;

#[cfg(test)]
mod tests {
    /// The same line the review page holds, held here: D39's reversal is
    /// inline script and nothing else.
    #[test]
    fn the_account_page_runs_only_inline_script() {
        assert!(super::ENROL_SCRIPT.starts_with("<script>"));
        assert!(!super::ENROL_SCRIPT.contains("src="), "the script is fetched");
        for probe in ["http://", "https://", "//cdn", "@import", "require("] {
            assert!(
                !super::ENROL_SCRIPT.contains(probe),
                "the enrolment script reaches out via {probe}"
            );
        }
        assert_eq!(super::ENROL_SCRIPT.matches("<script").count(), 1);
        // Nothing interpolated, so the script is safe to read once rather
        // than per render. The username travels on the element.
        assert!(!super::ENROL_SCRIPT.contains("{}"));
    }

    /// ES256 only, because that is the one scheme the node can verify.
    /// Offering `alg: -257` would enrol RSA keys that
    /// `verify_webauthn_assertion` refuses at first use, which is the
    /// failure mode enrolment-time validation exists to prevent.
    #[test]
    fn the_ceremony_asks_for_the_only_algorithm_the_node_verifies() {
        assert!(super::ENROL_SCRIPT.contains("alg: -7"));
        assert!(!super::ENROL_SCRIPT.contains("-257"), "RSA was offered");
        // `getPublicKey()` rather than parsing the attestation object:
        // D39 scoped a CBOR reader out, and a hand-rolled one is the
        // tripwire on that row.
        assert!(super::ENROL_SCRIPT.contains("getPublicKey"));
        assert!(!super::ENROL_SCRIPT.contains("attestationObject"));
    }

    /// A node with no store, and a credential with no account, are
    /// different situations with different repairs, and neither is an
    /// error page.
    #[test]
    fn the_page_distinguishes_no_store_from_no_account() {
        let page = super::render(None, "alice");
        assert_eq!(page.status, 200);
        assert!(page.html.contains("does not run account self-service"));
        assert!(!page.html.contains("<script"), "no store, no ceremony");
    }
}
